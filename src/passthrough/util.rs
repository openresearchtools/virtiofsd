// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.

use crate::util::{other_io_error, ErrorContext, ResultErrorContext};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::{fmt, io};

/// Translate Linux open(2) flags to macOS open(2) flags.
///
/// The FUSE protocol sends open flags using Linux numeric values. On macOS,
/// some flags have different numeric values, so we must translate before
/// passing them to the macOS kernel.
#[cfg(target_os = "macos")]
pub fn translate_linux_open_flags(linux_flags: i32) -> i32 {
    // Linux flag constants (from <asm-generic/fcntl.h> / <bits/fcntl-linux.h>)
    const LINUX_O_RDONLY: i32 = 0o0;
    const LINUX_O_WRONLY: i32 = 0o1;
    const LINUX_O_RDWR: i32 = 0o2;
    const LINUX_O_ACCMODE: i32 = 0o3;
    const LINUX_O_CREAT: i32 = 0o100;
    const LINUX_O_EXCL: i32 = 0o200;
    const LINUX_O_NOCTTY: i32 = 0o400;
    const LINUX_O_TRUNC: i32 = 0o1000;
    const LINUX_O_APPEND: i32 = 0o2000;
    const LINUX_O_NONBLOCK: i32 = 0o4000;
    const LINUX_O_DSYNC: i32 = 0o10000;
    const LINUX_O_DIRECT: i32 = 0o40000;
    const LINUX_O_LARGEFILE: i32 = 0o100000;
    const LINUX_O_DIRECTORY: i32 = 0o200000;
    const LINUX_O_NOFOLLOW: i32 = 0o400000;
    const LINUX_O_NOATIME: i32 = 0o1000000;
    const LINUX_O_CLOEXEC: i32 = 0o2000000;
    const LINUX_O_SYNC: i32 = 0o4010000;

    let mut mac_flags: i32 = 0;

    // Access mode (low 2 bits are the same on both platforms)
    mac_flags |= match linux_flags & LINUX_O_ACCMODE {
        LINUX_O_RDONLY => libc::O_RDONLY,
        LINUX_O_WRONLY => libc::O_WRONLY,
        LINUX_O_RDWR => libc::O_RDWR,
        _ => libc::O_RDONLY,
    };

    // Map individual flags
    if linux_flags & LINUX_O_CREAT != 0 { mac_flags |= libc::O_CREAT; }
    if linux_flags & LINUX_O_EXCL != 0 { mac_flags |= libc::O_EXCL; }
    if linux_flags & LINUX_O_NOCTTY != 0 { mac_flags |= libc::O_NOCTTY; }
    if linux_flags & LINUX_O_TRUNC != 0 { mac_flags |= libc::O_TRUNC; }
    if linux_flags & LINUX_O_APPEND != 0 { mac_flags |= libc::O_APPEND; }
    if linux_flags & LINUX_O_NONBLOCK != 0 { mac_flags |= libc::O_NONBLOCK; }
    if linux_flags & LINUX_O_DIRECTORY != 0 { mac_flags |= libc::O_DIRECTORY; }
    if linux_flags & LINUX_O_NOFOLLOW != 0 { mac_flags |= libc::O_NOFOLLOW; }
    if linux_flags & LINUX_O_CLOEXEC != 0 { mac_flags |= libc::O_CLOEXEC; }
    if linux_flags & LINUX_O_SYNC != 0 { mac_flags |= libc::O_SYNC; }
    if linux_flags & LINUX_O_DSYNC != 0 { mac_flags |= libc::O_DSYNC; }
    // O_DIRECT: macOS has no direct equivalent, silently drop it
    // O_NOATIME: macOS has no equivalent, silently drop it
    // O_LARGEFILE: not meaningful on macOS (always 64-bit), drop it

    mac_flags
}

/// On Linux, flags are already native — pass through unchanged.
#[cfg(target_os = "linux")]
#[inline]
pub fn translate_linux_open_flags(linux_flags: i32) -> i32 {
    linux_flags
}

/// Safe wrapper around libc::openat().
pub fn openat(dir_fd: &impl AsRawFd, path: &str, flags: libc::c_int) -> io::Result<File> {
    let path_cstr =
        CString::new(path).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // Safe because:
    // - CString::new() has returned success and thus guarantees `path_cstr` is a valid
    //   NUL-terminated string
    // - this does not modify any memory
    // - we check the return value
    // We do not check `flags` because if the kernel cannot handle poorly specified flags then we
    // have much bigger problems.
    let fd = unsafe { libc::openat(dir_fd.as_raw_fd(), path_cstr.as_ptr(), flags) };
    if fd >= 0 {
        // Safe because we just opened this fd
        Ok(unsafe { File::from_raw_fd(fd) })
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Same as `openat()`, but produces more verbose errors.
///
/// Do not use this for operations where the error is returned to the guest, as the raw OS error
/// value will be clobbered.
pub fn openat_verbose(dir_fd: &impl AsRawFd, path: &str, flags: libc::c_int) -> io::Result<File> {
    openat(dir_fd, path, flags).err_context(|| path)
}

/// Open `/proc/self/fd/{fd}` with the given flags to effectively duplicate the given `fd` with new
/// flags (e.g. to turn an `O_PATH` file descriptor into one that can be used for I/O).
#[cfg(target_os = "linux")]
pub fn reopen_fd_through_proc(
    fd: &impl AsRawFd,
    flags: libc::c_int,
    proc_self_fd: &File,
) -> io::Result<File> {
    // Clear the `O_NOFOLLOW` flag if it is set since we need to follow the `/proc/self/fd` symlink
    // to get the file.
    openat(
        proc_self_fd,
        format!("{}", fd.as_raw_fd()).as_str(),
        flags & !libc::O_NOFOLLOW,
    )
}

/// macOS: /proc/self/fd does not exist. Use fcntl(F_GETPATH) to get the path,
/// then reopen it with the requested flags.
// TODO(macos): This approach has a TOCTOU race: the path could change between
// F_GETPATH and the subsequent open(). Also, F_GETPATH may fail for certain
// fd types (e.g. pipes, sockets).
#[cfg(target_os = "macos")]
pub fn reopen_fd_through_proc(
    fd: &impl AsRawFd,
    flags: libc::c_int,
    _proc_self_fd: &File,
) -> io::Result<File> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    let ret = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) };
    if ret == -1 {
        let err = io::Error::last_os_error();
        log::debug!("reopen_fd_through_proc: F_GETPATH failed for fd {}: {}", fd.as_raw_fd(), err);
        return Err(err);
    }
    let path = CStr::from_bytes_until_nul(&buf)
        .map_err(|_| other_io_error("F_GETPATH returned invalid path"))?;
    log::debug!(
        "reopen_fd_through_proc: fd {} -> path {:?}, flags=0x{:x}",
        fd.as_raw_fd(),
        path,
        flags & !libc::O_NOFOLLOW
    );
    let new_fd = unsafe { libc::open(path.as_ptr(), flags & !libc::O_NOFOLLOW) };
    if new_fd < 0 {
        let err = io::Error::last_os_error();
        log::debug!("reopen_fd_through_proc: open failed: {}", err);
        Err(err)
    } else {
        Ok(unsafe { File::from_raw_fd(new_fd) })
    }
}

/// Returns true if it's safe to open this inode without O_PATH.
pub fn is_safe_inode(mode: u32) -> bool {
    // Only regular files and directories are considered safe to be opened from the file
    // server without O_PATH.
    matches!(mode & libc::S_IFMT as u32, m if m == libc::S_IFREG as u32 || m == libc::S_IFDIR as u32)
}

pub fn ebadf() -> io::Error {
    io::Error::from_raw_os_error(libc::EBADF)
}

pub fn einval() -> io::Error {
    io::Error::from_raw_os_error(libc::EINVAL)
}

pub fn erofs() -> io::Error {
    io::Error::from_raw_os_error(libc::EROFS)
}

/**
 * Errors that `get_path_by_fd()` can encounter.
 *
 * This specialized error type exists so that
 * [`crate::passthrough::device_state::preserialization::proc_paths`] can decide which errors it
 * considers recoverable.
 */
#[derive(Debug)]
pub(crate) enum FdPathError {
    /// `readlinkat()` failed with the contained error.
    ReadLink(io::Error),

    /// Link name is too long.
    TooLong,

    /// Link name is not a valid C string.
    InvalidCString(io::Error),

    /// Returned path (contained string) is not a plain file path.
    NotAFile(String),

    /// Returned path (contained string) is reported to be deleted, i.e. no longer valid.
    Deleted(String),
}

/// Looks up an FD's path through /proc/self/fd
#[cfg(target_os = "linux")]
pub(crate) fn get_path_by_fd(
    fd: &impl AsRawFd,
    proc_self_fd: &impl AsRawFd,
) -> Result<CString, FdPathError> {
    let fname = format!("{}\0", fd.as_raw_fd());
    let fname_cstr = CStr::from_bytes_with_nul(fname.as_bytes()).unwrap();

    let max_len = libc::PATH_MAX as usize; // does not include final NUL byte
    let mut link_target = vec![0u8; max_len + 1]; // make space for NUL byte

    let ret = unsafe {
        libc::readlinkat(
            proc_self_fd.as_raw_fd(),
            fname_cstr.as_ptr(),
            link_target.as_mut_ptr().cast::<libc::c_char>(),
            max_len,
        )
    };
    if ret < 0 {
        return Err(FdPathError::ReadLink(io::Error::last_os_error()));
    } else if ret as usize == max_len {
        return Err(FdPathError::TooLong);
    }

    link_target.truncate(ret as usize + 1);
    let link_target_cstring = CString::from_vec_with_nul(link_target)
        .map_err(|err| FdPathError::InvalidCString(other_io_error(err)))?;
    let link_target_str = link_target_cstring.to_string_lossy();

    let pre_slash = link_target_str.split('/').next().unwrap();
    if pre_slash.contains(':') {
        return Err(FdPathError::NotAFile(link_target_str.into_owned()));
    }

    if let Some(path) = link_target_str.strip_suffix(" (deleted)") {
        return Err(FdPathError::Deleted(path.to_owned()));
    }

    Ok(link_target_cstring)
}

/// macOS: /proc/self/fd does not exist. Use fcntl(F_GETPATH) to resolve the path.
#[cfg(target_os = "macos")]
pub(crate) fn get_path_by_fd(
    fd: &impl AsRawFd,
    _proc_self_fd: &impl AsRawFd,
) -> Result<CString, FdPathError> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    let ret = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) };
    if ret == -1 {
        return Err(FdPathError::ReadLink(io::Error::last_os_error()));
    }

    // Find the NUL terminator
    let nul_pos = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    if nul_pos == 0 {
        return Err(FdPathError::ReadLink(other_io_error(
            "F_GETPATH returned empty path",
        )));
    }
    buf.truncate(nul_pos + 1); // include the NUL byte

    let path_cstring = CString::from_vec_with_nul(buf)
        .map_err(|err| FdPathError::InvalidCString(other_io_error(err)))?;
    let path_str = path_cstring.to_string_lossy();

    let pre_slash = path_str.split('/').next().unwrap();
    if pre_slash.contains(':') {
        return Err(FdPathError::NotAFile(path_str.into_owned()));
    }

    Ok(path_cstring)
}

impl From<FdPathError> for io::Error {
    fn from(err: FdPathError) -> Self {
        match err {
            FdPathError::ReadLink(err) => err.context("readlink"),
            FdPathError::TooLong => other_io_error("Path returned from readlink is too long"),
            FdPathError::InvalidCString(err) => err.context("readlink returned invalid path"),
            FdPathError::NotAFile(path) => other_io_error(format!("Not a file ({path})")),
            FdPathError::Deleted(path) => other_io_error(format!("Inode deleted ({path})")),
        }
    }
}

impl fmt::Display for FdPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FdPathError::ReadLink(err) => write!(f, "readlink: {err}"),
            FdPathError::TooLong => write!(f, "Path returned from readlink is too long"),
            FdPathError::InvalidCString(err) => write!(f, "readlink returned invalid path: {err}"),
            FdPathError::NotAFile(path) => write!(f, "Not a file ({path})"),
            FdPathError::Deleted(path) => write!(f, "Inode deleted ({path})"),
        }
    }
}

impl std::error::Error for FdPathError {}

/// Debugging helper function: Turn the given file descriptor into a string representation we can
/// show the user.  If `proc_self_fd` is given, try to obtain the actual path through the symlink
/// in /proc/self/fd; otherwise (or on error), just print the integer representation (as
/// "{fd:%i}").
pub fn printable_fd(fd: &impl AsRawFd, proc_self_fd: Option<&impl AsRawFd>) -> String {
    if let Some(Ok(path)) = proc_self_fd.map(|psf| get_path_by_fd(fd, psf)) {
        match path.into_string() {
            Ok(s) => s,
            Err(err) => err.into_cstring().to_string_lossy().into_owned(),
        }
    } else {
        format!("{{fd:{}}}", fd.as_raw_fd())
    }
}

pub fn relative_path<'a>(path: &'a CStr, prefix: &CStr) -> io::Result<&'a CStr> {
    let mut relative_path = path
        .to_bytes_with_nul()
        .strip_prefix(prefix.to_bytes())
        .ok_or_else(|| {
            other_io_error(format!(
                "Path {path:?} is outside the directory ({prefix:?})"
            ))
        })?;

    // Remove leading / if left
    while let Some(prefixless) = relative_path.strip_prefix(b"/") {
        relative_path = prefixless;
    }

    // Must succeed: Was a `CStr` before, converted to `&[u8]` via `to_bytes_with_nul()`, so must
    // still contain exactly one NUL byte at the end of the slice
    Ok(CStr::from_bytes_with_nul(relative_path).unwrap())
}
