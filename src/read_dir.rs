// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#[cfg(target_os = "macos")]
use crate::libc_compat as libc;

use crate::filesystem::{DirEntry, DirectoryIterator};

use std::ffi::CStr;
use std::io;
use std::mem::size_of;
use std::ops::{Deref, DerefMut};
use std::os::unix::io::AsRawFd;

use vm_memory::ByteValued;

#[cfg(target_os = "linux")]
#[repr(C, packed)]
#[derive(Default, Clone, Copy)]
struct LinuxDirent64 {
    d_ino: libc::ino64_t,
    d_off: libc::off64_t,
    d_reclen: libc::c_ushort,
    d_ty: libc::c_uchar,
}
#[cfg(target_os = "linux")]
unsafe impl ByteValued for LinuxDirent64 {}

/// macOS dirent structure for getdirentries.
/// On macOS, struct dirent has: d_fileno (ino_t), d_seekoff (u64), d_reclen (u16),
/// d_namlen (u16), d_type (u8), then d_name[].
// TODO(macos): The macOS dirent layout varies by version. This targets macOS 10.15+.
// On older macOS, the layout may differ. Also, d_seekoff is macOS-specific.
#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct MacDirent {
    d_ino: libc::ino_t,
    d_seekoff: u64,
    d_reclen: u16,
    d_namlen: u16,
    d_type: u8,
    // d_name follows (variable length)
}

#[cfg(target_os = "macos")]
impl Default for MacDirent {
    fn default() -> Self {
        MacDirent {
            d_ino: 0,
            d_seekoff: 0,
            d_reclen: 0,
            d_namlen: 0,
            d_type: 0,
        }
    }
}

#[cfg(target_os = "macos")]
unsafe impl ByteValued for MacDirent {}

#[derive(Default)]
pub struct ReadDir<P> {
    buf: P,
    current: usize,
    end: usize,
}

#[cfg(target_os = "linux")]
impl<P: DerefMut<Target = [u8]>> ReadDir<P> {
    pub fn new<D: AsRawFd>(dir: &D, offset: libc::off64_t, buf: P) -> io::Result<Self> {
        // Safe because this doesn't modify any memory and we check the return value.
        let res = unsafe { libc::lseek64(dir.as_raw_fd(), offset, libc::SEEK_SET) };
        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        // Safe because we used lseek() to get to the correct position
        unsafe { Self::new_no_seek(dir, buf) }
    }

    /// Continue reading from the current position in the directory without seeking.
    ///
    /// # Safety
    /// Caller must ensure the current position is valid, for example, by exclusively using this
    /// function on a given FD, potentially repeatedly.
    pub unsafe fn new_no_seek<D: AsRawFd>(dir: &D, mut buf: P) -> io::Result<Self> {
        // Safe because the kernel guarantees that it will only write to `buf` and we check the
        // return value.
        let res = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                dir.as_raw_fd(),
                buf.as_mut_ptr() as *mut LinuxDirent64,
                buf.len() as libc::c_int,
            )
        };
        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(ReadDir {
            buf,
            current: 0,
            end: res as usize,
        })
    }
}

/// macOS: Use __getdirentries64 to read directory entries.
// TODO(macos): __getdirentries64 is a private API on macOS. A more portable
// approach would be to use fdopendir/readdir_r, but that doesn't fit the
// buffer-based API as cleanly.
#[cfg(target_os = "macos")]
extern "C" {
    fn __getdirentries64(
        fd: libc::c_int,
        buf: *mut libc::c_char,
        bufsize: libc::size_t,
        basep: *mut libc::off_t,
    ) -> libc::ssize_t;
}

#[cfg(target_os = "macos")]
impl<P: DerefMut<Target = [u8]>> ReadDir<P> {
    pub fn new<D: AsRawFd>(dir: &D, offset: libc::off64_t, buf: P) -> io::Result<Self> {
        let res = unsafe { libc::lseek(dir.as_raw_fd(), offset as libc::off_t, libc::SEEK_SET) };
        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        unsafe { Self::new_no_seek(dir, buf) }
    }

    /// # Safety
    /// Caller must ensure the current position is valid.
    pub unsafe fn new_no_seek<D: AsRawFd>(dir: &D, mut buf: P) -> io::Result<Self> {
        let mut basep: libc::off_t = 0;
        let res = unsafe {
            __getdirentries64(
                dir.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                &mut basep,
            )
        };
        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(ReadDir {
            buf,
            current: 0,
            end: res as usize,
        })
    }
}

impl<P> ReadDir<P> {
    /// Returns the number of bytes from the internal buffer that have not yet been consumed.
    pub fn remaining(&self) -> usize {
        self.end.saturating_sub(self.current)
    }
}

#[cfg(target_os = "linux")]
impl<P: Deref<Target = [u8]>> DirectoryIterator for ReadDir<P> {
    fn next(&mut self) -> Option<DirEntry<'_>> {
        let rem = &self.buf[self.current..self.end];
        if rem.is_empty() {
            return None;
        }

        // We only use debug asserts here because these values are coming from the kernel and we
        // trust them implicitly.
        debug_assert!(
            rem.len() >= size_of::<LinuxDirent64>(),
            "not enough space left in `rem`"
        );

        let (front, back) = rem.split_at(size_of::<LinuxDirent64>());

        let dirent64 =
            LinuxDirent64::from_slice(front).expect("unable to get LinuxDirent64 from slice");

        let namelen = dirent64.d_reclen as usize - size_of::<LinuxDirent64>();
        debug_assert!(namelen <= back.len(), "back is smaller than `namelen`");

        // The kernel will pad the name with additional nul bytes until it is 8-byte aligned so
        // we need to strip those off here.
        let name = strip_padding(&back[..namelen]);
        let entry = DirEntry {
            ino: dirent64.d_ino,
            offset: dirent64.d_off as u64,
            type_: dirent64.d_ty as u32,
            name,
        };

        debug_assert!(
            rem.len() >= dirent64.d_reclen as usize,
            "rem is smaller than `d_reclen`"
        );
        self.current += dirent64.d_reclen as usize;

        Some(entry)
    }
}

#[cfg(target_os = "macos")]
impl<P: Deref<Target = [u8]>> DirectoryIterator for ReadDir<P> {
    fn next(&mut self) -> Option<DirEntry<'_>> {
        let rem = &self.buf[self.current..self.end];
        if rem.is_empty() || rem.len() < size_of::<MacDirent>() {
            return None;
        }

        // Read the fixed-size header
        let header = unsafe {
            std::ptr::read_unaligned(rem.as_ptr() as *const MacDirent)
        };

        if header.d_reclen == 0 || (header.d_reclen as usize) > rem.len() {
            return None;
        }

        // Name starts after the fixed header
        let name_start = size_of::<MacDirent>();
        let name_end = name_start + header.d_namlen as usize;
        if name_end > rem.len() {
            return None;
        }

        // Need to include the NUL byte
        let name_with_nul = if name_end < rem.len() && rem[name_end] == 0 {
            &rem[name_start..=name_end]
        } else {
            // Should not happen but be safe
            &rem[name_start..name_end]
        };

        let name = strip_padding(name_with_nul);
        let entry = DirEntry {
            ino: header.d_ino,
            offset: header.d_seekoff,
            type_: header.d_type as u32,
            name,
        };

        self.current += header.d_reclen as usize;

        Some(entry)
    }
}

// Like `CStr::from_bytes_with_nul` but strips any bytes after the first '\0'-byte. Panics if `b`
// doesn't contain any '\0' bytes.
fn strip_padding(b: &[u8]) -> &CStr {
    // It would be nice if we could use memchr here but that's locked behind an unstable gate.
    let pos = b
        .iter()
        .position(|&c| c == 0)
        .expect("`b` doesn't contain any nul bytes");

    // Safe because we are creating this string with the first nul-byte we found so we can
    // guarantee that it is nul-terminated and doesn't contain any interior nuls.
    unsafe { CStr::from_bytes_with_nul_unchecked(&b[..=pos]) }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn padded_cstrings() {
        assert_eq!(strip_padding(b".\0\0\0\0\0\0\0").to_bytes(), b".");
        assert_eq!(strip_padding(b"..\0\0\0\0\0\0").to_bytes(), b"..");
        assert_eq!(
            strip_padding(b"normal cstring\0").to_bytes(),
            b"normal cstring"
        );
        assert_eq!(strip_padding(b"\0\0\0\0").to_bytes(), b"");
        assert_eq!(
            strip_padding(b"interior\0nul bytes\0\0\0").to_bytes(),
            b"interior"
        );
    }

    #[test]
    #[should_panic(expected = "`b` doesn't contain any nul bytes")]
    fn no_nul_byte() {
        strip_padding(b"no nul bytes in string");
    }
}
