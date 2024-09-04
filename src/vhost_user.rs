// Copyright 2019 Intel Corporation. All Rights Reserved.
//
// SPDX-License-Identifier: (Apache-2.0 AND BSD-3-Clause)

use std::{convert, error, fmt, io};

use crate::descriptor_utils::Error as VufDescriptorError;
use crate::util::other_io_error;
use crate::Error as VhostUserFsError;

/// The maximum length of the tag being used.
pub const MAX_TAG_LEN: usize = 36;

// The compiler warns that some wrapped values are never read, but they are in fact read by
// `<Error as fmt::Display>::fmt()` via the derived `Debug`.
#[allow(dead_code)]
#[derive(Debug)]
pub enum Error {
    /// Failed to create kill eventfd.
    CreateKillEventFd(io::Error),
    /// Failed to create thread pool.
    CreateThreadPool(io::Error),
    /// Failed to handle event other than input event.
    HandleEventNotEpollIn,
    /// Failed to handle unknown event.
    HandleEventUnknownEvent,
    /// Iterating through the queue failed.
    IterateQueue,
    /// No memory configured.
    NoMemoryConfigured,
    /// Processing queue failed.
    ProcessQueue(VhostUserFsError),
    /// Creating a queue reader failed.
    QueueReader(VufDescriptorError),
    /// Creating a queue writer failed.
    QueueWriter(VufDescriptorError),
    /// The unshare(CLONE_FS) call failed.
    UnshareCloneFs(io::Error),
    /// Invalid tag name
    InvalidTag,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use self::Error::UnshareCloneFs;
        match self {
            UnshareCloneFs(error) => {
                write!(
                    f,
                    "The unshare(CLONE_FS) syscall failed with '{error}'. \
                    If running in a container please check that the container \
                    runtime seccomp policy allows unshare."
                )
            }
            Self::InvalidTag => write!(
                f,
                "The tag may not be empty or longer than {MAX_TAG_LEN} bytes (encoded as UTF-8)."
            ),
            _ => write!(f, "{self:?}"),
        }
    }
}

impl error::Error for Error {}

impl convert::From<Error> for io::Error {
    fn from(e: Error) -> Self {
        other_io_error(e)
    }
}
