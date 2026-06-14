#[cfg(any(target_os = "macos", test))]
use anyhow::bail;
use anyhow::Result;
use clap::{Parser, Subcommand};
#[cfg(any(target_os = "macos", test))]
use std::path::Component;
#[cfg(any(target_os = "macos", test))]
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "agent-overlayfs",
    about = "macOS FUSE overlay filesystem for path-preserving agent views"
)]
struct Cli {
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Debug, Subcommand)]
enum CommandKind {
    Mount(MountArgs),
    Check,
}

#[derive(Debug, Parser, Clone)]
struct MountArgs {
    #[arg(long)]
    mount_point: PathBuf,
    #[arg(long)]
    lower: PathBuf,
    #[arg(long)]
    upper: PathBuf,
    #[arg(long)]
    whiteouts: PathBuf,
    #[arg(long = "fallback-root")]
    fallback_roots: Vec<PathBuf>,
    #[arg(long, default_value = "agent-overlayfs")]
    fs_name: String,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    match Cli::parse().command {
        CommandKind::Mount(args) => platform::mount(args),
        CommandKind::Check => platform::check(),
    }
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone)]
struct FallbackRoots {
    roots: Vec<FallbackRoot>,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone)]
struct FallbackRoot {
    visible: PathBuf,
    real: PathBuf,
}

#[cfg(any(target_os = "macos", test))]
impl FallbackRoots {
    fn new(mut roots: Vec<PathBuf>) -> Self {
        roots.sort();
        roots.dedup();
        let roots = roots
            .into_iter()
            .filter_map(|visible| {
                std::fs::canonicalize(&visible)
                    .ok()
                    .map(|real| FallbackRoot { visible, real })
            })
            .collect();
        Self { roots }
    }

    fn contains_visible(&self, visible: &Path) -> bool {
        self.roots
            .iter()
            .any(|root| visible == root.visible || visible.starts_with(&root.visible))
    }

    fn is_virtual_dir(&self, visible: &Path) -> bool {
        visible != Path::new("/")
            && self
                .roots
                .iter()
                .any(|root| root.visible != visible && root.visible.starts_with(visible))
    }

    fn child_names(&self, visible: &Path) -> Vec<std::ffi::OsString> {
        let mut names = Vec::new();
        for root in &self.roots {
            let name = if visible == Path::new("/") {
                root.visible
                    .components()
                    .nth(1)
                    .map(|component| component.as_os_str().to_os_string())
            } else if root.visible != visible && root.visible.starts_with(visible) {
                root.visible
                    .strip_prefix(visible)
                    .ok()
                    .and_then(|relative| relative.components().next())
                    .map(|component| component.as_os_str().to_os_string())
            } else {
                None
            };
            if let Some(name) = name {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names
    }

    fn path(&self, visible: &Path) -> Option<PathBuf> {
        let root = self
            .roots
            .iter()
            .find(|root| visible == root.visible || visible.starts_with(&root.visible))?;
        if !visible.exists() {
            return None;
        }
        let real = std::fs::canonicalize(visible).ok()?;
        (real == root.real || real.starts_with(&root.real)).then(|| visible.to_path_buf())
    }

    #[cfg(test)]
    fn roots(&self) -> impl Iterator<Item = &Path> {
        self.roots.iter().map(|root| root.visible.as_path())
    }
}

#[cfg(any(target_os = "macos", test))]
fn validate_mount_layout(
    mount_point: &Path,
    lower: &Path,
    upper: &Path,
    whiteouts: &Path,
) -> Result<()> {
    for (name, path) in [
        ("mount-point", mount_point),
        ("lower", lower),
        ("upper", upper),
        ("whiteouts", whiteouts),
    ] {
        if !path.is_absolute() {
            bail!("{name} must be absolute: {}", path.display());
        }
        reject_dot_components(name, path)?;
        reject_existing_symlink_components(name, path)?;
    }

    let env_dir = mount_point
        .parent()
        .ok_or_else(|| anyhow::anyhow!("mount-point must be inside an env directory"))?;
    let envs_dir = env_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("mount-point env directory must be inside envs"))?;
    if envs_dir.file_name().and_then(|name| name.to_str()) != Some("envs") {
        bail!(
            "mount-point must be under an agentfs envs directory: {}",
            mount_point.display()
        );
    }
    for (name, path, expected_basename) in [
        ("mount-point", mount_point, "view-root"),
        ("lower", lower, "lower"),
        ("upper", upper, "upper"),
        ("whiteouts", whiteouts, "whiteouts"),
    ] {
        if path.parent() != Some(env_dir) {
            bail!(
                "{name} must be a sibling inside {}, got {}",
                env_dir.display(),
                path.display()
            );
        }
        if path.file_name().and_then(|name| name.to_str()) != Some(expected_basename) {
            bail!(
                "{name} must be named {expected_basename}, got {}",
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn validate_fallback_roots(fallback_roots: &[PathBuf]) -> Result<()> {
    for path in fallback_roots {
        if !path.is_absolute() {
            bail!("fallback-root must be absolute: {}", path.display());
        }
        reject_dot_components("fallback-root", path)?;
        if !allowed_fallback_root(path) {
            bail!(
                "fallback-root is outside the macOS system allowlist: {}",
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn allowed_fallback_root(path: &Path) -> bool {
    path.to_str()
        .is_some_and(|path| FALLBACK_ROOT_ALLOWLIST.contains(&path))
}

#[cfg(any(target_os = "macos", test))]
fn reject_dot_components(name: &str, path: &Path) -> Result<()> {
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {}
            Component::CurDir | Component::ParentDir => {
                bail!(
                    "{name} must not contain . or .. components: {}",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn reject_existing_symlink_components(name: &str, path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            bail!(
                "{name} must not pass through symlink component {}",
                current.display()
            );
        }
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
const FALLBACK_ROOT_ALLOWLIST: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr/bin",
    "/usr/lib",
    "/usr/share",
    "/System",
    "/Library/Filesystems",
    "/dev/null",
    "/dev/random",
    "/dev/tty",
    "/dev/urandom",
    "/dev/zero",
    "/etc/hosts",
    "/etc/protocols",
    "/etc/resolv.conf",
    "/etc/services",
    "/etc/shells",
    "/etc/zprofile",
    "/etc/zshenv",
    "/etc/zshrc",
    "/var/tmp",
    "/tmp",
    "/private/etc/hosts",
    "/private/etc/protocols",
    "/private/etc/resolv.conf",
    "/private/etc/services",
    "/private/etc/shells",
    "/private/etc/zprofile",
    "/private/etc/zshenv",
    "/private/etc/zshrc",
    "/private/tmp",
    "/private/var/tmp",
];

#[cfg(any(target_os = "macos", test))]
fn fallback_is_special_file(path: &Path) -> std::io::Result<bool> {
    let file_type = std::fs::symlink_metadata(path)?.file_type();
    Ok(!file_type.is_file() && !file_type.is_dir() && !file_type.is_symlink())
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{fallback_is_special_file, FallbackRoots, MountArgs};
    use agent_core::path_overlay::{OverlayLookup, PathOverlay};
    use anyhow::{bail, Context, Result};
    use fuser::{
        BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
        Generation, INodeNo, KernelConfig, MountOption, ReplyAttr, ReplyCreate, ReplyData,
        ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, SessionACL,
        TimeOrNow,
    };
    use std::collections::{BTreeMap, HashMap};
    use std::ffi::{OsStr, OsString};
    use std::fs::{self, File, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, SystemTime};

    const ROOT_INO: u64 = 1;
    const TTL: Duration = Duration::from_secs(1);
    const READY_FILE: &str = ".agent-overlayfs-ready";

    pub fn check() -> Result<()> {
        eprintln!("agent-overlayfs: checking macOS FUSE environment");
        for path in [
            "/Library/Filesystems/macfuse.fs",
            "/usr/local/bin/macfuse",
            "/dev/fuse",
            "/dev/macfuse0",
            "/dev/osxfuse0",
        ] {
            let status = match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_dir() => "dir",
                Ok(metadata) if metadata.is_file() => "file",
                Ok(_) => "other",
                Err(error) => {
                    eprintln!("agent-overlayfs: check {path}: {error}");
                    continue;
                }
            };
            eprintln!("agent-overlayfs: check {path}: {status}");
        }
        Ok(())
    }

    pub fn mount(args: MountArgs) -> Result<()> {
        eprintln!(
            "agent-overlayfs: validating mount-point={} lower={} upper={} whiteouts={} fallback-roots={}",
            args.mount_point.display(),
            args.lower.display(),
            args.upper.display(),
            args.whiteouts.display(),
            args.fallback_roots.len()
        );
        validate_mount_args(&args)?;
        eprintln!(
            "agent-overlayfs: starting fuser mount2 mount-point={} fs-name={}",
            args.mount_point.display(),
            args.fs_name
        );
        let fs = OverlayFs::new(args.lower, args.upper, args.whiteouts, args.fallback_roots);
        let mut config = Config::default();
        config.mount_options = vec![
            MountOption::FSName(args.fs_name),
            MountOption::Subtype("agent-overlayfs".to_string()),
            MountOption::RW,
            MountOption::NoSuid,
        ];
        config.acl = SessionACL::All;
        let result = fuser::mount2(fs, &args.mount_point, &config)
            .context("failed to mount agent-overlayfs");
        eprintln!(
            "agent-overlayfs: fuser mount2 returned mount-point={} result={:?}",
            args.mount_point.display(),
            result.as_ref().map(|_| ())
        );
        result
    }

    fn validate_mount_args(args: &MountArgs) -> Result<()> {
        super::validate_mount_layout(&args.mount_point, &args.lower, &args.upper, &args.whiteouts)?;
        super::validate_fallback_roots(&args.fallback_roots)?;
        if !args.mount_point.is_dir() {
            bail!("mount-point {} does not exist", args.mount_point.display());
        }
        fs::create_dir_all(&args.lower)?;
        fs::create_dir_all(&args.upper)?;
        fs::create_dir_all(&args.whiteouts)?;
        Ok(())
    }

    struct OverlayFs {
        overlay: PathOverlay,
        fallback_roots: FallbackRoots,
        paths: Mutex<InodeTable>,
        files: Mutex<HashMap<u64, File>>,
        next_fh: AtomicU64,
    }

    impl OverlayFs {
        fn new(
            lower: PathBuf,
            upper: PathBuf,
            whiteouts: PathBuf,
            fallback_roots: Vec<PathBuf>,
        ) -> Self {
            let mut table = InodeTable::default();
            table.intern(PathBuf::from("/"));
            Self {
                overlay: PathOverlay::new(lower, upper, whiteouts),
                fallback_roots: FallbackRoots::new(fallback_roots),
                paths: Mutex::new(table),
                files: Mutex::new(HashMap::new()),
                next_fh: AtomicU64::new(2),
            }
        }

        fn path_for_ino(&self, ino: INodeNo) -> Result<PathBuf, Errno> {
            self.paths
                .lock()
                .map_err(|_| Errno::EIO)?
                .path(ino)
                .ok_or(Errno::ENOENT)
        }

        fn ino_for_path(&self, path: &Path) -> Result<INodeNo, Errno> {
            Ok(self
                .paths
                .lock()
                .map_err(|_| Errno::EIO)?
                .intern(path.to_path_buf()))
        }

        fn child_path(&self, parent: INodeNo, name: &OsStr) -> Result<PathBuf, Errno> {
            if name.as_bytes().contains(&b'/') || name.is_empty() {
                return Err(Errno::EINVAL);
            }
            let parent = self.path_for_ino(parent)?;
            Ok(if parent == Path::new("/") {
                PathBuf::from("/").join(name)
            } else {
                parent.join(name)
            })
        }

        fn lookup_path(&self, visible: &Path) -> Result<(INodeNo, FileAttr), Errno> {
            if visible == Path::new("/") {
                return Ok((INodeNo(ROOT_INO), root_attr()));
            }
            if visible == Path::new("/").join(READY_FILE) {
                let ino = self.ino_for_path(visible)?;
                return Ok((ino, virtual_ready_attr(ino)));
            }
            match self.overlay.resolve(visible).map_err(|_| Errno::EIO)? {
                OverlayLookup::Found(found) => {
                    let ino = self.ino_for_path(visible)?;
                    let attr = file_attr(ino, &found.storage_path).map_err(|_| Errno::EIO)?;
                    Ok((ino, attr))
                }
                OverlayLookup::Whiteout(_) => Err(Errno::ENOENT),
                OverlayLookup::Missing => {
                    let ino = self.ino_for_path(visible)?;
                    if let Some(fallback) = self.fallback_path(visible) {
                        let attr = file_attr(ino, &fallback).map_err(|_| Errno::EIO)?;
                        Ok((ino, attr))
                    } else if self.fallback_roots.is_virtual_dir(visible) {
                        Ok((ino, virtual_dir_attr(ino)))
                    } else {
                        Err(Errno::ENOENT)
                    }
                }
            }
        }

        fn storage_path_for_read(&self, visible: &Path) -> Result<PathBuf, Errno> {
            match self.overlay.resolve(visible).map_err(|_| Errno::EIO)? {
                OverlayLookup::Found(found) => Ok(found.storage_path),
                OverlayLookup::Whiteout(_) => Err(Errno::ENOENT),
                OverlayLookup::Missing => self.fallback_path(visible).ok_or(Errno::ENOENT),
            }
        }

        fn ensure_upper_for_write(&self, visible: &Path) -> Result<PathBuf, Errno> {
            let upper = self.overlay.upper_path(visible).map_err(|_| Errno::EIO)?;
            if upper.exists() {
                return Ok(upper);
            }
            if let Some(parent) = upper.parent() {
                fs::create_dir_all(parent).map_err(|_| Errno::EIO)?;
            }
            match self.overlay.resolve(visible).map_err(|_| Errno::EIO)? {
                OverlayLookup::Found(found) => {
                    copy_up(&found.storage_path, &upper).map_err(|_| Errno::EIO)?
                }
                OverlayLookup::Missing if self.fallback_path(visible).is_some() => {
                    let fallback = self.fallback_path(visible).ok_or(Errno::ENOENT)?;
                    copy_up(&fallback, &upper).map_err(|_| Errno::EIO)?
                }
                OverlayLookup::Whiteout(_) | OverlayLookup::Missing => {
                    File::create(&upper).map_err(|_| Errno::EIO)?;
                }
            }
            Ok(upper)
        }

        fn storage_path_for_open(&self, visible: &Path, write: bool) -> Result<PathBuf, Errno> {
            if !write {
                return self.storage_path_for_read(visible);
            }
            if let Some(fallback) = self.fallback_path(visible) {
                if fallback_is_special_file(&fallback).map_err(|_| Errno::EIO)? {
                    return Ok(fallback);
                }
            }
            self.ensure_upper_for_write(visible)
        }

        fn merged_dir_entries(&self, visible: &Path) -> Result<Vec<(OsString, FileType)>, Errno> {
            let mut entries = BTreeMap::new();
            for root in self.fallback_entries_root(visible).into_iter().chain([
                self.overlay.lower_path(visible).map_err(|_| Errno::EIO)?,
                self.overlay.upper_path(visible).map_err(|_| Errno::EIO)?,
            ]) {
                if !root.exists() {
                    continue;
                }
                if !root.is_dir() {
                    return Err(Errno::ENOTDIR);
                }
                for entry in fs::read_dir(root).map_err(|_| Errno::EIO)? {
                    let entry = entry.map_err(|_| Errno::EIO)?;
                    let name = entry.file_name();
                    let child_visible = if visible == Path::new("/") {
                        PathBuf::from("/").join(&name)
                    } else {
                        visible.join(&name)
                    };
                    if self
                        .overlay
                        .whiteout_path(&child_visible)
                        .map_err(|_| Errno::EIO)?
                        .exists()
                    {
                        continue;
                    }
                    let kind = fuser_kind(entry.path()).map_err(|_| Errno::EIO)?;
                    entries.insert(name, kind);
                }
            }
            if visible == Path::new("/") {
                entries.insert(OsString::from(READY_FILE), FileType::RegularFile);
            }
            for name in self.fallback_roots.child_names(visible) {
                let child_visible = if visible == Path::new("/") {
                    PathBuf::from("/").join(&name)
                } else {
                    visible.join(&name)
                };
                if self
                    .overlay
                    .whiteout_path(&child_visible)
                    .map_err(|_| Errno::EIO)?
                    .exists()
                {
                    continue;
                }
                let kind = self
                    .fallback_path(&child_visible)
                    .map(fuser_kind)
                    .transpose()
                    .map_err(|_| Errno::EIO)?
                    .unwrap_or(FileType::Directory);
                entries.entry(name).or_insert(kind);
            }
            Ok(entries.into_iter().collect())
        }

        fn fallback_path(&self, visible: &Path) -> Option<PathBuf> {
            self.fallback_roots.path(visible)
        }

        fn fallback_entries_root(&self, visible: &Path) -> Option<PathBuf> {
            if visible == Path::new("/") {
                return None;
            }
            if !self.fallback_roots.contains_visible(visible) {
                return None;
            }
            self.fallback_path(visible).filter(|path| path.is_dir())
        }
    }

    impl Filesystem for OverlayFs {
        fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> std::io::Result<()> {
            Ok(())
        }

        fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
            let result = self
                .child_path(parent, name)
                .and_then(|path| self.lookup_path(&path));
            match result {
                Ok((_ino, attr)) => reply.entry(&TTL, &attr, Generation(0)),
                Err(errno) => reply.error(errno),
            }
        }

        fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
            let result = self
                .path_for_ino(ino)
                .and_then(|path| self.lookup_path(&path).map(|(_, attr)| attr));
            match result {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(errno) => reply.error(errno),
            }
        }

        fn setattr(
            &self,
            _req: &Request,
            ino: INodeNo,
            mode: Option<u32>,
            _uid: Option<u32>,
            _gid: Option<u32>,
            size: Option<u64>,
            _atime: Option<TimeOrNow>,
            _mtime: Option<TimeOrNow>,
            _ctime: Option<SystemTime>,
            _fh: Option<FileHandle>,
            _crtime: Option<SystemTime>,
            _chgtime: Option<SystemTime>,
            _bkuptime: Option<SystemTime>,
            _flags: Option<BsdFileFlags>,
            reply: ReplyAttr,
        ) {
            let result = (|| {
                let path = self.path_for_ino(ino)?;
                let upper = self.ensure_upper_for_write(&path)?;
                if let Some(size) = size {
                    OpenOptions::new()
                        .write(true)
                        .open(&upper)
                        .and_then(|file| file.set_len(size))
                        .map_err(|_| Errno::EIO)?;
                }
                if let Some(mode) = mode {
                    fs::set_permissions(&upper, fs::Permissions::from_mode(mode & 0o7777))
                        .map_err(|_| Errno::EIO)?;
                }
                file_attr(ino, &upper).map_err(|_| Errno::EIO)
            })();
            match result {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(errno) => reply.error(errno),
            }
        }

        fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
            let result = self
                .path_for_ino(ino)
                .and_then(|path| self.storage_path_for_read(&path))
                .and_then(|path| fs::read_link(path).map_err(|_| Errno::EIO));
            match result {
                Ok(target) => reply.data(target.as_os_str().as_bytes()),
                Err(errno) => reply.error(errno),
            }
        }

        fn mkdir(
            &self,
            _req: &Request,
            parent: INodeNo,
            name: &OsStr,
            mode: u32,
            _umask: u32,
            reply: ReplyEntry,
        ) {
            let result = (|| {
                let path = self.child_path(parent, name)?;
                let upper = self.overlay.upper_path(&path).map_err(|_| Errno::EIO)?;
                if upper.exists() {
                    return Err(Errno::EEXIST);
                }
                fs::create_dir_all(&upper).map_err(|_| Errno::EIO)?;
                fs::set_permissions(&upper, fs::Permissions::from_mode(mode & 0o7777))
                    .map_err(|_| Errno::EIO)?;
                let ino = self.ino_for_path(&path)?;
                let attr = file_attr(ino, &upper).map_err(|_| Errno::EIO)?;
                Ok(attr)
            })();
            match result {
                Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
                Err(errno) => reply.error(errno),
            }
        }

        fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
            let result = (|| {
                let path = self.child_path(parent, name)?;
                let upper = self.overlay.upper_path(&path).map_err(|_| Errno::EIO)?;
                if upper.exists() {
                    fs::remove_file(&upper).map_err(|_| Errno::EIO)?;
                }
                self.overlay.create_whiteout(&path).map_err(|_| Errno::EIO)
            })();
            reply_result(result, reply);
        }

        fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
            let result = (|| {
                let path = self.child_path(parent, name)?;
                if !self.merged_dir_entries(&path)?.is_empty() {
                    return Err(Errno::ENOTEMPTY);
                }
                let upper = self.overlay.upper_path(&path).map_err(|_| Errno::EIO)?;
                if upper.exists() {
                    fs::remove_dir(&upper).map_err(|_| Errno::EIO)?;
                }
                self.overlay.create_whiteout(&path).map_err(|_| Errno::EIO)
            })();
            reply_result(result, reply);
        }

        fn symlink(
            &self,
            _req: &Request,
            parent: INodeNo,
            link_name: &OsStr,
            target: &Path,
            reply: ReplyEntry,
        ) {
            let result = (|| {
                let path = self.child_path(parent, link_name)?;
                let upper = self.overlay.upper_path(&path).map_err(|_| Errno::EIO)?;
                if let Some(parent) = upper.parent() {
                    fs::create_dir_all(parent).map_err(|_| Errno::EIO)?;
                }
                std::os::unix::fs::symlink(target, &upper).map_err(|_| Errno::EIO)?;
                let ino = self.ino_for_path(&path)?;
                file_attr(ino, &upper).map_err(|_| Errno::EIO)
            })();
            match result {
                Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
                Err(errno) => reply.error(errno),
            }
        }

        fn rename(
            &self,
            _req: &Request,
            parent: INodeNo,
            name: &OsStr,
            newparent: INodeNo,
            newname: &OsStr,
            _flags: fuser::RenameFlags,
            reply: ReplyEmpty,
        ) {
            let result = (|| {
                let from = self.child_path(parent, name)?;
                let to = self.child_path(newparent, newname)?;
                self.overlay.rename(&from, &to).map_err(|_| Errno::EIO)
            })();
            reply_result(result, reply);
        }

        fn open(&self, _req: &Request, ino: INodeNo, flags: fuser::OpenFlags, reply: ReplyOpen) {
            let result = (|| {
                let visible = self.path_for_ino(ino)?;
                let write = open_flags_write(flags.0);
                let storage = self.storage_path_for_open(&visible, write)?;
                let file = open_file(&storage, flags.0, write).map_err(|_| Errno::EIO)?;
                let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
                self.files.lock().map_err(|_| Errno::EIO)?.insert(fh, file);
                Ok(fh)
            })();
            match result {
                Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
                Err(errno) => reply.error(errno),
            }
        }

        fn create(
            &self,
            _req: &Request,
            parent: INodeNo,
            name: &OsStr,
            mode: u32,
            _umask: u32,
            flags: i32,
            reply: ReplyCreate,
        ) {
            let result = (|| {
                let visible = self.child_path(parent, name)?;
                let upper = self.overlay.upper_path(&visible).map_err(|_| Errno::EIO)?;
                if let Some(parent) = upper.parent() {
                    fs::create_dir_all(parent).map_err(|_| Errno::EIO)?;
                }
                let file = OpenOptions::new()
                    .create(true)
                    .truncate(flags & libc::O_TRUNC != 0)
                    .read(true)
                    .write(true)
                    .mode(mode & 0o7777)
                    .open(&upper)
                    .map_err(|_| Errno::EIO)?;
                let ino = self.ino_for_path(&visible)?;
                let attr = file_attr(ino, &upper).map_err(|_| Errno::EIO)?;
                let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
                self.files.lock().map_err(|_| Errno::EIO)?.insert(fh, file);
                Ok((attr, fh))
            })();
            match result {
                Ok((attr, fh)) => reply.created(
                    &TTL,
                    &attr,
                    Generation(0),
                    FileHandle(fh),
                    FopenFlags::empty(),
                ),
                Err(errno) => reply.error(errno),
            }
        }

        fn read(
            &self,
            _req: &Request,
            ino: INodeNo,
            fh: FileHandle,
            offset: u64,
            size: u32,
            _flags: fuser::OpenFlags,
            _lock_owner: Option<fuser::LockOwner>,
            reply: ReplyData,
        ) {
            if let Some(data) = virtual_data_for_ino(self, ino) {
                let offset = offset as usize;
                let end = data.len().min(offset.saturating_add(size as usize));
                reply.data(data.get(offset..end).unwrap_or_default());
                return;
            }
            let result = (|| {
                let mut files = self.files.lock().map_err(|_| Errno::EIO)?;
                let file = files.get_mut(&fh.0).ok_or(Errno::EIO)?;
                let mut buf = vec![0; size as usize];
                file.seek(SeekFrom::Start(offset)).map_err(|_| Errno::EIO)?;
                let read = file.read(&mut buf).map_err(|_| Errno::EIO)?;
                buf.truncate(read);
                Ok(buf)
            })();
            match result {
                Ok(buf) => reply.data(&buf),
                Err(errno) => reply.error(errno),
            }
        }

        fn write(
            &self,
            _req: &Request,
            _ino: INodeNo,
            fh: FileHandle,
            offset: u64,
            data: &[u8],
            _write_flags: fuser::WriteFlags,
            _flags: fuser::OpenFlags,
            _lock_owner: Option<fuser::LockOwner>,
            reply: ReplyWrite,
        ) {
            let result = (|| {
                let mut files = self.files.lock().map_err(|_| Errno::EIO)?;
                let file = files.get_mut(&fh.0).ok_or(Errno::EIO)?;
                file.seek(SeekFrom::Start(offset)).map_err(|_| Errno::EIO)?;
                file.write_all(data).map_err(|_| Errno::EIO)?;
                Ok(data.len() as u32)
            })();
            match result {
                Ok(bytes) => reply.written(bytes),
                Err(errno) => reply.error(errno),
            }
        }

        fn fsync(
            &self,
            _req: &Request,
            _ino: INodeNo,
            fh: FileHandle,
            _datasync: bool,
            reply: ReplyEmpty,
        ) {
            let result = self.files.lock().map_err(|_| Errno::EIO).and_then(|files| {
                files
                    .get(&fh.0)
                    .ok_or(Errno::EIO)
                    .and_then(|file| file.sync_all().map_err(|_| Errno::EIO))
            });
            reply_result(result, reply);
        }

        fn release(
            &self,
            _req: &Request,
            _ino: INodeNo,
            fh: FileHandle,
            _flags: fuser::OpenFlags,
            _lock_owner: Option<fuser::LockOwner>,
            _flush: bool,
            reply: ReplyEmpty,
        ) {
            if let Ok(mut files) = self.files.lock() {
                files.remove(&fh.0);
            }
            reply.ok();
        }

        fn readdir(
            &self,
            _req: &Request,
            ino: INodeNo,
            _fh: FileHandle,
            offset: u64,
            mut reply: ReplyDirectory,
        ) {
            let result = (|| {
                let visible = self.path_for_ino(ino)?;
                let mut entries = vec![
                    (ino, FileType::Directory, OsString::from(".")),
                    (
                        parent_ino(&visible, self).unwrap_or(INodeNo(ROOT_INO)),
                        FileType::Directory,
                        OsString::from(".."),
                    ),
                ];
                for (name, kind) in self.merged_dir_entries(&visible)? {
                    let child = if visible == Path::new("/") {
                        PathBuf::from("/").join(&name)
                    } else {
                        visible.join(&name)
                    };
                    let child_ino = self.ino_for_path(&child)?;
                    entries.push((child_ino, kind, name));
                }
                Ok(entries)
            })();
            match result {
                Ok(entries) => {
                    for (index, (ino, kind, name)) in
                        entries.into_iter().enumerate().skip(offset as usize)
                    {
                        if reply.add(ino, (index + 1) as u64, kind, name) {
                            break;
                        }
                    }
                    reply.ok();
                }
                Err(errno) => reply.error(errno),
            }
        }
    }

    #[derive(Default)]
    struct InodeTable {
        next: u64,
        by_path: HashMap<PathBuf, u64>,
        by_ino: HashMap<u64, PathBuf>,
    }

    impl InodeTable {
        fn intern(&mut self, path: PathBuf) -> INodeNo {
            if let Some(ino) = self.by_path.get(&path) {
                return INodeNo(*ino);
            }
            let ino = if path == Path::new("/") {
                ROOT_INO
            } else {
                self.next = self.next.max(ROOT_INO + 1);
                let ino = self.next;
                self.next += 1;
                ino
            };
            self.by_path.insert(path.clone(), ino);
            self.by_ino.insert(ino, path);
            INodeNo(ino)
        }

        fn path(&self, ino: INodeNo) -> Option<PathBuf> {
            self.by_ino.get(&ino.0).cloned()
        }
    }

    fn file_attr(ino: INodeNo, path: &Path) -> std::io::Result<FileAttr> {
        let metadata = fs::symlink_metadata(path)?;
        let kind = FileType::from_std(metadata.file_type()).unwrap_or(FileType::RegularFile);
        Ok(FileAttr {
            ino,
            size: metadata.len(),
            blocks: metadata.blocks(),
            atime: unix_time(metadata.atime(), metadata.atime_nsec()),
            mtime: unix_time(metadata.mtime(), metadata.mtime_nsec()),
            ctime: unix_time(metadata.ctime(), metadata.ctime_nsec()),
            crtime: SystemTime::UNIX_EPOCH,
            kind,
            perm: (metadata.mode() & 0o7777) as u16,
            nlink: metadata.nlink() as u32,
            uid: metadata.uid(),
            gid: metadata.gid(),
            rdev: metadata.rdev() as u32,
            blksize: metadata.blksize() as u32,
            flags: 0,
        })
    }

    fn root_attr() -> FileAttr {
        dir_attr(INodeNo(ROOT_INO))
    }

    fn virtual_dir_attr(ino: INodeNo) -> FileAttr {
        dir_attr(ino)
    }

    fn dir_attr(ino: INodeNo) -> FileAttr {
        let now = SystemTime::now();
        FileAttr {
            ino,
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn virtual_ready_attr(ino: INodeNo) -> FileAttr {
        let now = SystemTime::now();
        FileAttr {
            ino,
            size: b"ready\n".len() as u64,
            blocks: 1,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileType::RegularFile,
            perm: 0o444,
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn virtual_data_for_ino(fs: &OverlayFs, ino: INodeNo) -> Option<&'static [u8]> {
        let path = fs.path_for_ino(ino).ok()?;
        (path == Path::new("/").join(READY_FILE)).then_some(b"ready\n")
    }

    fn fuser_kind(path: PathBuf) -> std::io::Result<FileType> {
        fs::symlink_metadata(path)?
            .file_type()
            .pipe(FileType::from_std)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "unsupported file type"))
    }

    trait Pipe: Sized {
        fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
            f(self)
        }
    }
    impl<T> Pipe for T {}

    fn unix_time(sec: i64, nsec: i64) -> SystemTime {
        if sec >= 0 {
            SystemTime::UNIX_EPOCH + Duration::new(sec as u64, nsec.max(0) as u32)
        } else {
            SystemTime::UNIX_EPOCH
        }
    }

    fn copy_up(src: &Path, dst: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(src)?;
        if metadata.is_dir() {
            fs::create_dir_all(dst)?;
        } else if metadata.file_type().is_symlink() {
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
        } else {
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(src, dst)?;
            fs::set_permissions(dst, fs::Permissions::from_mode(metadata.mode() & 0o7777))?;
        }
        Ok(())
    }

    fn open_flags_write(flags: i32) -> bool {
        matches!(flags & libc::O_ACCMODE, libc::O_WRONLY | libc::O_RDWR)
            || flags & libc::O_TRUNC != 0
            || flags & libc::O_APPEND != 0
    }

    fn open_file(path: &Path, flags: i32, write: bool) -> std::io::Result<File> {
        let mut options = OpenOptions::new();
        options.read(!write || flags & libc::O_ACCMODE == libc::O_RDWR);
        options.write(write);
        options.append(flags & libc::O_APPEND != 0);
        options.truncate(flags & libc::O_TRUNC != 0);
        options.open(path)
    }

    fn parent_ino(path: &Path, fs: &OverlayFs) -> Option<INodeNo> {
        path.parent()
            .and_then(|parent| fs.ino_for_path(parent).ok())
    }

    fn reply_result(result: Result<(), Errno>, reply: ReplyEmpty) {
        match result {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::MountArgs;
    use anyhow::{bail, Result};

    pub fn check() -> Result<()> {
        bail!("agent-overlayfs is supported only on macOS")
    }

    pub fn mount(_args: MountArgs) -> Result<()> {
        bail!("agent-overlayfs is supported only on macOS")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{
        fallback_is_special_file, validate_fallback_roots, validate_mount_layout, FallbackRoots,
    };
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    #[test]
    fn mount_layout_accepts_agentfs_env_siblings() {
        let temp = tempfile::tempdir().unwrap();
        let env_dir = temp.path().join("agentfs/envs/codex-1");
        fs::create_dir_all(&env_dir).unwrap();

        validate_mount_layout(
            &env_dir.join("view-root"),
            &env_dir.join("lower"),
            &env_dir.join("upper"),
            &env_dir.join("whiteouts"),
        )
        .unwrap();
    }

    #[test]
    fn mount_layout_rejects_unexpected_names_and_siblings() {
        let temp = tempfile::tempdir().unwrap();
        let env_dir = temp.path().join("agentfs/envs/codex-1");
        let other_env_dir = temp.path().join("agentfs/envs/codex-2");
        fs::create_dir_all(&env_dir).unwrap();
        fs::create_dir_all(&other_env_dir).unwrap();

        assert!(validate_mount_layout(
            &env_dir.join("mount"),
            &env_dir.join("lower"),
            &env_dir.join("upper"),
            &env_dir.join("whiteouts"),
        )
        .is_err());
        assert!(validate_mount_layout(
            &env_dir.join("view-root"),
            &other_env_dir.join("lower"),
            &env_dir.join("upper"),
            &env_dir.join("whiteouts"),
        )
        .is_err());
        assert!(validate_mount_layout(
            &temp.path().join("agentfs/codex-1/view-root"),
            &temp.path().join("agentfs/codex-1/lower"),
            &temp.path().join("agentfs/codex-1/upper"),
            &temp.path().join("agentfs/codex-1/whiteouts"),
        )
        .is_err());
    }

    #[test]
    fn mount_layout_rejects_dot_components_and_existing_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let env_dir = temp.path().join("agentfs/envs/codex-1");
        fs::create_dir_all(&env_dir).unwrap();

        assert!(validate_mount_layout(
            &env_dir.join("../codex-1/view-root"),
            &env_dir.join("lower"),
            &env_dir.join("upper"),
            &env_dir.join("whiteouts"),
        )
        .is_err());

        let link = temp.path().join("agent-link");
        std::os::unix::fs::symlink(temp.path().join("agentfs"), &link).unwrap();
        assert!(validate_mount_layout(
            &link.join("envs/codex-1/view-root"),
            &env_dir.join("lower"),
            &env_dir.join("upper"),
            &env_dir.join("whiteouts"),
        )
        .is_err());
    }

    #[test]
    fn fallback_root_validation_accepts_only_system_allowlist() {
        validate_fallback_roots(&[
            "/bin".into(),
            "/usr/bin".into(),
            "/private/etc/hosts".into(),
            "/private/var/tmp".into(),
        ])
        .unwrap();

        for path in [
            "relative",
            "/Users/alice",
            "/private/etc/ssh",
            "/etc",
            "/usr/local",
            "/private/etc/../var/db",
        ] {
            assert!(
                validate_fallback_roots(&[path.into()]).is_err(),
                "{path} should be rejected"
            );
        }
    }

    #[test]
    fn fallback_path_allows_files_inside_canonical_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("file.txt"), "ok").unwrap();

        let roots = FallbackRoots::new(vec![root.clone()]);

        assert!(roots.contains_visible(&root.join("file.txt")));
        assert_eq!(roots.roots().collect::<Vec<_>>(), vec![root.as_path()]);
        assert_eq!(
            roots.path(&root.join("file.txt")),
            Some(root.join("file.txt"))
        );
    }

    #[test]
    fn fallback_path_rejects_symlink_escape_outside_canonical_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("leak")).unwrap();

        let roots = FallbackRoots::new(vec![root.clone()]);

        assert!(roots.path(&root.join("leak")).is_none());
    }

    #[test]
    fn fallback_path_accepts_symlink_staying_inside_canonical_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(root.join("real")).unwrap();
        fs::write(root.join("real/file.txt"), "ok").unwrap();
        std::os::unix::fs::symlink(Path::new("real/file.txt"), root.join("link")).unwrap();

        let roots = FallbackRoots::new(vec![root.clone()]);

        assert_eq!(roots.path(&root.join("link")), Some(root.join("link")));
    }

    #[test]
    fn fallback_roots_expose_virtual_parent_dirs_without_sibling_access() {
        let temp = tempfile::tempdir().unwrap();
        let private = temp.path().join("private");
        let etc = private.join("etc");
        let secret = private.join("var/db");
        let var = temp.path().join("var");
        let var_tmp = var.join("tmp");
        let var_secret = var.join("db");
        let dev = temp.path().join("dev");
        let dev_null = dev.join("null");
        let dev_zero = dev.join("zero");
        let dev_disk = dev.join("disk0");
        fs::create_dir_all(&etc).unwrap();
        fs::create_dir_all(&secret).unwrap();
        fs::create_dir_all(&var_tmp).unwrap();
        fs::create_dir_all(&var_secret).unwrap();
        fs::create_dir_all(&dev).unwrap();
        fs::write(etc.join("zshrc"), "ok").unwrap();
        fs::write(secret.join("secret.txt"), "secret").unwrap();
        fs::write(var_tmp.join("scratch.txt"), "ok").unwrap();
        fs::write(var_secret.join("secret.txt"), "secret").unwrap();
        fs::write(&dev_null, "").unwrap();
        fs::write(&dev_zero, "").unwrap();
        fs::write(&dev_disk, "secret").unwrap();

        let roots = FallbackRoots::new(vec![
            etc.clone(),
            var_tmp.clone(),
            dev_null.clone(),
            dev_zero.clone(),
        ]);

        assert!(roots.is_virtual_dir(&private));
        assert_eq!(
            roots.child_names(&private),
            vec![std::ffi::OsString::from("etc")]
        );
        assert!(roots.is_virtual_dir(&var));
        assert_eq!(
            roots.child_names(&var),
            vec![std::ffi::OsString::from("tmp")]
        );
        assert_eq!(roots.path(&etc.join("zshrc")), Some(etc.join("zshrc")));
        assert_eq!(
            roots.path(&var_tmp.join("scratch.txt")),
            Some(var_tmp.join("scratch.txt"))
        );
        assert!(roots.is_virtual_dir(&dev));
        assert_eq!(
            roots.child_names(&dev),
            vec![
                std::ffi::OsString::from("null"),
                std::ffi::OsString::from("zero")
            ]
        );
        assert_eq!(roots.path(&dev_null), Some(dev_null));
        assert_eq!(roots.path(&dev_zero), Some(dev_zero));
        assert!(roots.path(&secret.join("secret.txt")).is_none());
        assert!(roots.path(&var_secret.join("secret.txt")).is_none());
        assert!(roots.path(&dev_disk).is_none());
        assert!(!roots.is_virtual_dir(&secret));
        assert!(!roots.is_virtual_dir(&var_secret));
        assert!(!roots.is_virtual_dir(&dev_disk));
    }

    #[test]
    fn fallback_special_file_detection_distinguishes_fifo_from_regular_paths() {
        let temp = tempfile::tempdir().unwrap();
        let regular = temp.path().join("regular");
        let dir = temp.path().join("dir");
        let symlink = temp.path().join("symlink");
        let fifo = temp.path().join("fifo");
        fs::write(&regular, "").unwrap();
        fs::create_dir(&dir).unwrap();
        std::os::unix::fs::symlink(&regular, &symlink).unwrap();
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        let rc = unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) };
        assert_eq!(rc, 0);

        assert!(!fallback_is_special_file(&regular).unwrap());
        assert!(!fallback_is_special_file(&dir).unwrap());
        assert!(!fallback_is_special_file(&symlink).unwrap());
        assert!(fallback_is_special_file(&fifo).unwrap());
    }
}
