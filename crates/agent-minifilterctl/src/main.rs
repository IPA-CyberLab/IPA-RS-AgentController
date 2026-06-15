#[cfg(windows)]
mod windows_app {
    use anyhow::{anyhow, bail, Result};
    use clap::{Parser, Subcommand};
    use std::ffi::OsStr;
    use std::mem::{size_of, zeroed};
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, GetExitCodeProcess, ResumeThread, WaitForSingleObject, CREATE_SUSPENDED,
        INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
    };

    const AGENTFS_PORT_NAME: &str = "\\AgentFsPort";
    const AGENTFS_MAX_CHARS: usize = 1024;
    const AGENTFS_IOCTL_VERSION: u32 = 1;
    const AGENTFS_REPLY_OK: u32 = 0;

    #[derive(Debug, Parser)]
    #[command(
        name = "agent-minifilterctl",
        about = "Control the IPA-RS Windows minifilter overlay"
    )]
    struct Cli {
        #[command(subcommand)]
        command: Command,
    }

    #[derive(Debug, Subcommand)]
    enum Command {
        Exec(RunArgs),
        Session(RunArgs),
        Register(RegisterArgs),
        Unregister {
            #[arg(long)]
            pid: u32,
        },
        Check,
    }

    #[derive(Debug, Parser)]
    struct RegisterArgs {
        #[arg(long)]
        pid: u32,
        #[arg(long)]
        env_id: String,
        #[arg(long)]
        source_root: PathBuf,
        #[arg(long)]
        lower: PathBuf,
        #[arg(long)]
        upper: PathBuf,
        #[arg(long)]
        whiteouts: PathBuf,
    }

    #[derive(Debug, Parser)]
    struct RunArgs {
        #[arg(long)]
        env_id: String,
        #[arg(long)]
        source_root: PathBuf,
        #[arg(long)]
        lower: PathBuf,
        #[arg(long)]
        upper: PathBuf,
        #[arg(long)]
        whiteouts: PathBuf,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long, default_value = "host")]
        network: String,
        #[arg(long)]
        log_path: Option<PathBuf>,
        #[arg(last = true)]
        command: Vec<String>,
    }

    #[repr(u32)]
    #[derive(Debug, Clone, Copy)]
    enum RequestKind {
        RegisterProcess = 1,
        UnregisterProcess = 2,
        Check = 3,
    }

    #[repr(C)]
    struct AgentFsRequest {
        version: u32,
        kind: u32,
        pid: u32,
        reserved: u32,
        env_id: [u16; 128],
        source_root: [u16; AGENTFS_MAX_CHARS],
        lower_root: [u16; AGENTFS_MAX_CHARS],
        upper_root: [u16; AGENTFS_MAX_CHARS],
        whiteout_root: [u16; AGENTFS_MAX_CHARS],
    }

    #[repr(C)]
    struct AgentFsReply {
        status: u32,
        win32_error: u32,
        message: [u16; 512],
    }

    impl Default for AgentFsReply {
        fn default() -> Self {
            Self {
                status: 0,
                win32_error: 0,
                message: [0; 512],
            }
        }
    }

    #[link(name = "fltlib")]
    extern "system" {
        fn FilterConnectCommunicationPort(
            lpPortName: *const u16,
            dwOptions: u32,
            lpContext: *const core::ffi::c_void,
            wSizeOfContext: u16,
            lpSecurityAttributes: *const core::ffi::c_void,
            hPort: *mut HANDLE,
        ) -> i32;

        fn FilterSendMessage(
            hPort: HANDLE,
            lpInBuffer: *const core::ffi::c_void,
            dwInBufferSize: u32,
            lpOutBuffer: *mut core::ffi::c_void,
            dwOutBufferSize: u32,
            lpBytesReturned: *mut u32,
        ) -> i32;
    }

    pub(crate) fn main() -> Result<()> {
        let cli = Cli::parse();
        match cli.command {
            Command::Exec(args) => std::process::exit(run(args, true)?),
            Command::Session(args) => {
                let pid = spawn_registered(args)?;
                println!("{pid}");
                Ok(())
            }
            Command::Register(args) => send_register(args),
            Command::Unregister { pid } => send_unregister(pid),
            Command::Check => send_check(),
        }
    }

    fn run(args: RunArgs, wait: bool) -> Result<i32> {
        if !wait {
            return Ok(spawn_registered(args)? as i32);
        }
        let child = create_registered_process(&args)?;
        let wait_result = unsafe { WaitForSingleObject(child.process, INFINITE) };
        if wait_result == u32::MAX {
            let error = unsafe { GetLastError() };
            let _ = send_unregister(child.pid);
            return Err(anyhow!("WaitForSingleObject failed with {error}"));
        }
        let mut exit_code = 128u32;
        let ok = unsafe { GetExitCodeProcess(child.process, &mut exit_code) };
        let _ = send_unregister(child.pid);
        drop(child);
        if ok == 0 {
            let error = unsafe { GetLastError() };
            return Err(anyhow!("GetExitCodeProcess failed with {error}"));
        }
        Ok(exit_code as i32)
    }

    fn spawn_registered(args: RunArgs) -> Result<u32> {
        let child = create_registered_process(&args)?;
        let pid = child.pid;
        std::mem::forget(child);
        Ok(pid)
    }

    struct RegisteredProcess {
        process: HANDLE,
        thread: HANDLE,
        job: HANDLE,
        pid: u32,
    }

    impl Drop for RegisteredProcess {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.thread);
                CloseHandle(self.process);
                CloseHandle(self.job);
            }
        }
    }

    fn create_registered_process(args: &RunArgs) -> Result<RegisteredProcess> {
        if args.command.is_empty() {
            bail!("command after -- is required");
        }
        if args.network != "host" {
            bail!("Windows minifilter overlay currently supports only network=host");
        }
        let command_line = windows_command_line(&args.command);
        let mut command_line = wide_mut(&command_line);
        let cwd = wide_null(args.cwd.as_os_str());
        let mut startup: STARTUPINFOW = unsafe { zeroed() };
        startup.cb = size_of::<STARTUPINFOW>() as u32;
        let mut process_info: PROCESS_INFORMATION = unsafe { zeroed() };
        let ok = unsafe {
            CreateProcessW(
                null(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                1,
                CREATE_SUSPENDED,
                null(),
                cwd.as_ptr(),
                &startup,
                &mut process_info,
            )
        };
        if ok == 0 {
            let error = unsafe { GetLastError() };
            return Err(anyhow!("CreateProcessW failed with {error}"));
        }

        let job = unsafe { CreateJobObjectW(null(), null()) };
        if job.is_null() {
            unsafe {
                CloseHandle(process_info.hThread);
                CloseHandle(process_info.hProcess);
            }
            let error = unsafe { GetLastError() };
            return Err(anyhow!("CreateJobObjectW failed with {error}"));
        }
        apply_job_limits(job)?;
        let assigned = unsafe { AssignProcessToJobObject(job, process_info.hProcess) };
        if assigned == 0 {
            unsafe {
                CloseHandle(job);
                CloseHandle(process_info.hThread);
                CloseHandle(process_info.hProcess);
            }
            let error = unsafe { GetLastError() };
            return Err(anyhow!("AssignProcessToJobObject failed with {error}"));
        }

        let register = RegisterArgs {
            pid: process_info.dwProcessId,
            env_id: args.env_id.clone(),
            source_root: args.source_root.clone(),
            lower: args.lower.clone(),
            upper: args.upper.clone(),
            whiteouts: args.whiteouts.clone(),
        };
        if let Err(error) = send_register(register) {
            unsafe {
                CloseHandle(job);
                CloseHandle(process_info.hThread);
                CloseHandle(process_info.hProcess);
            }
            return Err(error);
        }
        if unsafe { ResumeThread(process_info.hThread) } == u32::MAX {
            let error = unsafe { GetLastError() };
            let _ = send_unregister(process_info.dwProcessId);
            unsafe {
                CloseHandle(job);
                CloseHandle(process_info.hThread);
                CloseHandle(process_info.hProcess);
            }
            return Err(anyhow!("ResumeThread failed with {error}"));
        }
        Ok(RegisteredProcess {
            process: process_info.hProcess,
            thread: process_info.hThread,
            job,
            pid: process_info.dwProcessId,
        })
    }

    fn apply_job_limits(job: HANDLE) -> Result<()> {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &mut info as *mut _ as *mut core::ffi::c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            let error = unsafe { GetLastError() };
            Err(anyhow!("SetInformationJobObject failed with {error}"))
        } else {
            Ok(())
        }
    }

    fn send_register(args: RegisterArgs) -> Result<()> {
        let mut request = empty_request(RequestKind::RegisterProcess, args.pid);
        write_wide_field(&mut request.env_id, &args.env_id)?;
        write_wide_field(
            &mut request.source_root,
            &nt_kernel_path(&args.source_root)?,
        )?;
        write_wide_field(&mut request.lower_root, &nt_kernel_path(&args.lower)?)?;
        write_wide_field(&mut request.upper_root, &nt_kernel_path(&args.upper)?)?;
        write_wide_field(
            &mut request.whiteout_root,
            &nt_kernel_path(&args.whiteouts)?,
        )?;
        send_request(&request)
    }

    fn send_unregister(pid: u32) -> Result<()> {
        let request = empty_request(RequestKind::UnregisterProcess, pid);
        send_request(&request)
    }

    fn send_check() -> Result<()> {
        let request = empty_request(RequestKind::Check, 0);
        send_request(&request)
    }

    fn empty_request(kind: RequestKind, pid: u32) -> AgentFsRequest {
        AgentFsRequest {
            version: AGENTFS_IOCTL_VERSION,
            kind: kind as u32,
            pid,
            reserved: 0,
            env_id: [0; 128],
            source_root: [0; AGENTFS_MAX_CHARS],
            lower_root: [0; AGENTFS_MAX_CHARS],
            upper_root: [0; AGENTFS_MAX_CHARS],
            whiteout_root: [0; AGENTFS_MAX_CHARS],
        }
    }

    fn send_request(request: &AgentFsRequest) -> Result<()> {
        let mut port: HANDLE = null_mut();
        let port_name = wide_null(AGENTFS_PORT_NAME);
        let status = unsafe {
            FilterConnectCommunicationPort(port_name.as_ptr(), 0, null(), 0, null(), &mut port)
        };
        if status < 0 {
            bail!("failed to connect to {AGENTFS_PORT_NAME}; is AgentFs minifilter loaded? NTSTATUS=0x{status:08x}");
        }
        let mut reply = AgentFsReply::default();
        let mut returned = 0u32;
        let status = unsafe {
            FilterSendMessage(
                port,
                request as *const _ as *const core::ffi::c_void,
                size_of::<AgentFsRequest>() as u32,
                &mut reply as *mut _ as *mut core::ffi::c_void,
                size_of::<AgentFsReply>() as u32,
                &mut returned,
            )
        };
        unsafe {
            CloseHandle(port);
        }
        if status < 0 {
            bail!("AgentFs FilterSendMessage failed: NTSTATUS=0x{status:08x}");
        }
        if reply.status != AGENTFS_REPLY_OK {
            bail!(
                "AgentFs rejected request: status={} win32={} message={}",
                reply.status,
                reply.win32_error,
                wide_field_to_string(&reply.message)
            );
        }
        Ok(())
    }

    fn write_wide_field<const N: usize>(field: &mut [u16; N], value: &str) -> Result<()> {
        let wide = OsStr::new(value).encode_wide().collect::<Vec<_>>();
        if wide.len() >= N {
            bail!("value is too long for AgentFs request field: {value}");
        }
        field[..wide.len()].copy_from_slice(&wide);
        field[wide.len()] = 0;
        Ok(())
    }

    fn nt_kernel_path(path: &PathBuf) -> Result<String> {
        let text = path
            .canonicalize()
            .unwrap_or_else(|_| path.clone())
            .display()
            .to_string();
        let bytes = text.as_bytes();
        if bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
            let drive = &text[..2];
            let device = query_dos_device(drive)?;
            let suffix = text[2..].replace('/', "\\");
            return Ok(format!("{device}{suffix}"));
        }
        if text.starts_with("\\Device\\") {
            return Ok(text);
        }
        bail!("Windows minifilter paths must be absolute drive paths: {text}");
    }

    fn query_dos_device(drive: &str) -> Result<String> {
        let drive = wide_null(drive);
        let mut buffer = vec![0u16; 1024];
        let len =
            unsafe { QueryDosDeviceW(drive.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32) };
        if len == 0 {
            let error = unsafe { GetLastError() };
            return Err(anyhow!("QueryDosDeviceW failed with {error}"));
        }
        let len = buffer
            .iter()
            .position(|ch| *ch == 0)
            .unwrap_or(len as usize);
        Ok(String::from_utf16_lossy(&buffer[..len]))
    }

    fn wide_field_to_string(field: &[u16]) -> String {
        let len = field.iter().position(|ch| *ch == 0).unwrap_or(field.len());
        String::from_utf16_lossy(&field[..len])
    }

    fn wide_null(value: impl AsRef<OsStr>) -> Vec<u16> {
        value
            .as_ref()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn wide_mut(value: &str) -> Vec<u16> {
        OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn windows_command_line(args: &[String]) -> String {
        args.iter()
            .map(|arg| quote_windows_arg(arg))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn quote_windows_arg(arg: &str) -> String {
        if !arg.is_empty()
            && !arg
                .bytes()
                .any(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'"'))
        {
            return arg.to_string();
        }
        let mut quoted = String::from("\"");
        let mut backslashes = 0usize;
        for ch in arg.chars() {
            if ch == '\\' {
                backslashes += 1;
            } else if ch == '"' {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            } else {
                quoted.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                quoted.push(ch);
            }
        }
        quoted.push_str(&"\\".repeat(backslashes * 2));
        quoted.push('"');
        quoted
    }
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    windows_app::main()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("agent-minifilterctl is supported only on Windows");
    std::process::exit(1);
}
