//! 与 inode 元数据（权限 / 属主 / 时间戳 / umask）打交道的底层封装。

use crate::cli::Preserve;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::RawFd;
use std::path::Path;

/// 读取当前进程的 umask。
///
/// 优先读 `/proc/self/status`（无副作用）；拿不到时退回 `umask(0)` 读后立即还原。
pub fn read_umask() -> u32 {
    if let Ok(text) = fs::read_to_string("/proc/self/status") {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("Umask:") {
                if let Ok(v) = u32::from_str_radix(rest.trim(), 8) {
                    return v & 0o777;
                }
            }
        }
    }
    let old = unsafe { libc::umask(0o022) };
    unsafe { libc::umask(old) };
    old & 0o777
}

/// 权限位（含 setuid/setgid/sticky）。
pub fn perm(mode: u32) -> u32 {
    mode & 0o7777
}

fn cpath(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn times(meta: &fs::Metadata) -> [libc::timespec; 2] {
    [
        libc::timespec {
            tv_sec: meta.atime(),
            tv_nsec: meta.atime_nsec(),
        },
        libc::timespec {
            tv_sec: meta.mtime(),
            tv_nsec: meta.mtime_nsec(),
        },
    ]
}

pub fn fchmod(fd: RawFd, mode: u32) -> io::Result<()> {
    if unsafe { libc::fchmod(fd, mode as libc::mode_t) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn fchown(fd: RawFd, uid: u32, gid: u32) -> io::Result<()> {
    if unsafe { libc::fchown(fd, uid, gid) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn futimens(fd: RawFd, meta: &fs::Metadata) -> io::Result<()> {
    if unsafe { libc::futimens(fd, times(meta).as_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn chmod_path(p: &Path, mode: u32) -> io::Result<()> {
    let c = cpath(p)?;
    if unsafe { libc::chmod(c.as_ptr(), mode as libc::mode_t) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn mkdir_path(p: &Path, mode: u32) -> io::Result<()> {
    let c = cpath(p)?;
    if unsafe { libc::mkdir(c.as_ptr(), perm(mode) as libc::mode_t) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn mkfifo_path(p: &Path, mode: u32) -> io::Result<()> {
    let c = cpath(p)?;
    if unsafe { libc::mkfifo(c.as_ptr(), perm(mode) as libc::mode_t) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 按源文件类型创建设备节点（需要 root）。
pub fn mknod_path(p: &Path, mode: u32, rdev: u64) -> io::Result<()> {
    let c = cpath(p)?;
    let mode = (mode & libc::S_IFMT) | perm(mode);
    if unsafe { libc::mknod(c.as_ptr(), mode as libc::mode_t, rdev as libc::dev_t) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn lchown_path(p: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let c = cpath(p)?;
    if unsafe { libc::lchown(c.as_ptr(), uid, gid) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 不跟随符号链接地设置访问/修改时间。
pub fn utimens_path_nofollow(p: &Path, meta: &fs::Metadata) -> io::Result<()> {
    let c = cpath(p)?;
    let t = times(meta);
    if unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c.as_ptr(),
            t.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 按 `Preserve` 把属主写回 fd。非 root 常见的 EPERM 静默忽略（与 GNU 行为一致）。
pub fn set_owner_fd(fd: RawFd, meta: &fs::Metadata) -> Vec<String> {
    match fchown(fd, meta.uid(), meta.gid()) {
        Ok(()) => Vec::new(),
        Err(e) if e.raw_os_error() == Some(libc::EPERM) => Vec::new(),
        Err(e) => vec![format!("failed to preserve ownership: {e}")],
    }
}

/// 按 `Preserve` 把属主写回路径（符号链接用，不跟随）。
pub fn set_owner_path(p: &Path, meta: &fs::Metadata) -> Vec<String> {
    match lchown_path(p, meta.uid(), meta.gid()) {
        Ok(()) => Vec::new(),
        Err(e) if e.raw_os_error() == Some(libc::EPERM) => Vec::new(),
        Err(e) => vec![format!(
            "failed to preserve ownership for '{}': {e}",
            p.display()
        )],
    }
}

/// 创建文件时使用的临时权限：保证自己能写，复制完成后再 chmod 回真实权限。
pub fn temporary_mode(final_mode: u32) -> u32 {
    perm(final_mode) | 0o200
}

/// 不保留权限时新文件的最终权限。
pub fn default_mode(src_mode: u32, umask: u32) -> u32 {
    perm(src_mode) & !umask
}

/// 是否需要写回属性。
pub fn needs_attr_writeback(p: &Preserve) -> bool {
    p.mode || p.ownership || p.timestamps
}
