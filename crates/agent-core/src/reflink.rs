use anyhow::{anyhow, Context, Result};
use std::path::Path;
use walkdir::WalkDir;

pub fn clone_file(src: &Path, dst: &Path) -> Result<()> {
    platform_clone_file(src, dst)
}

pub fn clone_tree(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
        return Err(anyhow!("{} is not a directory", src.display()));
    }
    if dst.exists() {
        return Err(anyhow!("{} already exists", dst.display()));
    }
    std::fs::create_dir_all(dst)?;
    for entry in WalkDir::new(src).follow_links(false).min_depth(1) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(src)?;
        let target = dst.join(relative);
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            std::fs::create_dir_all(&target)
                .with_context(|| format!("failed to create directory {}", target.display()))?;
        } else if metadata.file_type().is_symlink() {
            clone_symlink(entry.path(), &target)
                .with_context(|| format!("failed to clone symlink {}", entry.path().display()))?;
        } else if metadata.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            platform_clone_file(entry.path(), &target)
                .with_context(|| format!("failed to reflink {}", entry.path().display()))?;
        }
    }
    Ok(())
}

pub fn clone_tree_with_copy_fallback(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        return Err(anyhow!("{} already exists", dst.display()));
    }
    match clone_tree(src, dst) {
        Ok(()) => Ok(()),
        Err(clone_error) => {
            let _ = std::fs::remove_dir_all(dst);
            copy_tree(src, dst).with_context(|| {
                format!(
                    "failed to copy {} to {} after reflink failed: {clone_error:#}",
                    src.display(),
                    dst.display()
                )
            })
        }
    }
}

pub fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
        return Err(anyhow!("{} is not a directory", src.display()));
    }
    if dst.exists() {
        return Err(anyhow!("{} already exists", dst.display()));
    }
    std::fs::create_dir_all(dst)?;
    let root_metadata = std::fs::symlink_metadata(src)?;
    std::fs::set_permissions(dst, root_metadata.permissions())
        .with_context(|| format!("failed to copy permissions to {}", dst.display()))?;
    for entry in WalkDir::new(src).follow_links(false).min_depth(1) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(src)?;
        let target = dst.join(relative);
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            std::fs::create_dir_all(&target)
                .with_context(|| format!("failed to create directory {}", target.display()))?;
            std::fs::set_permissions(&target, metadata.permissions())
                .with_context(|| format!("failed to copy permissions to {}", target.display()))?;
        } else if metadata.file_type().is_symlink() {
            clone_symlink(entry.path(), &target)
                .with_context(|| format!("failed to copy symlink {}", entry.path().display()))?;
        } else if metadata.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target).with_context(|| {
                format!(
                    "failed to copy file {} to {}",
                    entry.path().display(),
                    target.display()
                )
            })?;
            std::fs::set_permissions(&target, metadata.permissions())
                .with_context(|| format!("failed to copy permissions to {}", target.display()))?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_clone_file(src: &Path, dst: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let src = CString::new(src.as_os_str().as_bytes())?;
    let dst = CString::new(dst.as_os_str().as_bytes())?;
    let result = unsafe { clonefile(src.as_ptr(), dst.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).context("clonefile failed")
    }
}

#[cfg(target_os = "macos")]
extern "C" {
    fn clonefile(
        src: *const std::os::raw::c_char,
        dst: *const std::os::raw::c_char,
        flags: u32,
    ) -> i32;
}

#[cfg(target_os = "windows")]
fn platform_clone_file(src: &Path, dst: &Path) -> Result<()> {
    use std::fs::OpenOptions;
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use std::ptr::null_mut;

    const FSCTL_DUPLICATE_EXTENTS_TO_FILE: u32 = 0x0009_8344;

    #[repr(C)]
    struct DuplicateExtentsData {
        file_handle: isize,
        source_file_offset: i64,
        target_file_offset: i64,
        byte_count: i64,
    }

    let src_file = OpenOptions::new()
        .read(true)
        .open(src)
        .with_context(|| format!("failed to open source {}", src.display()))?;
    let byte_count = src_file.metadata()?.len();
    let dst_file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(dst)
        .with_context(|| format!("failed to create target {}", dst.display()))?;
    dst_file.set_len(byte_count)?;
    if byte_count == 0 {
        return Ok(());
    }

    let mut input = DuplicateExtentsData {
        file_handle: src_file.as_raw_handle() as isize,
        source_file_offset: 0,
        target_file_offset: 0,
        byte_count: byte_count
            .try_into()
            .map_err(|_| anyhow!("{} is too large to reflink", src.display()))?,
    };
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            dst_file.as_raw_handle() as isize,
            FSCTL_DUPLICATE_EXTENTS_TO_FILE,
            &mut input as *mut DuplicateExtentsData as *mut std::ffi::c_void,
            size_of::<DuplicateExtentsData>() as u32,
            null_mut(),
            0,
            &mut returned,
            null_mut(),
        )
    };
    if ok != 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        let _ = std::fs::remove_file(dst);
        Err(error).context("FSCTL_DUPLICATE_EXTENTS_TO_FILE failed")
    }
}

#[cfg(target_os = "windows")]
extern "system" {
    fn DeviceIoControl(
        h_device: isize,
        dw_io_control_code: u32,
        lp_in_buffer: *mut std::ffi::c_void,
        n_in_buffer_size: u32,
        lp_out_buffer: *mut std::ffi::c_void,
        n_out_buffer_size: u32,
        lp_bytes_returned: *mut u32,
        lp_overlapped: *mut std::ffi::c_void,
    ) -> i32;
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_clone_file(src: &Path, _dst: &Path) -> Result<()> {
    Err(anyhow!(
        "native reflink is unsupported on this target for {}",
        src.display()
    ))
}

#[cfg(unix)]
fn clone_symlink(src: &Path, dst: &Path) -> Result<()> {
    let target = std::fs::read_link(src)?;
    std::os::unix::fs::symlink(target, dst)?;
    Ok(())
}

#[cfg(windows)]
fn clone_symlink(src: &Path, dst: &Path) -> Result<()> {
    let target = std::fs::read_link(src)?;
    let metadata = std::fs::metadata(src)?;
    if metadata.is_dir() {
        std::os::windows::fs::symlink_dir(target, dst)?;
    } else {
        std::os::windows::fs::symlink_file(target, dst)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::clone_tree;

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn clone_tree_reports_unsupported_target_without_copying() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let dst_path = dst.path().join("env");
        std::fs::create_dir(src.path().join("nested")).unwrap();
        std::fs::write(src.path().join("nested/file.txt"), "hello").unwrap();

        let error = clone_tree(src.path(), &dst_path).unwrap_err().to_string();

        assert!(error.contains("failed to reflink"));
    }
}
