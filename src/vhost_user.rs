// Copyright 2019 Intel Corporation. All Rights Reserved.
//
// SPDX-License-Identifier: (Apache-2.0 AND BSD-3-Clause)

use std::convert::TryInto;
use std::sync::Arc;
use std::{convert, error, fmt, io};

use futures::executor::{ThreadPool, ThreadPoolBuilder};
use libc::EFD_NONBLOCK;
use log::*;

use vhost::vhost_user::Backend;
use vhost_user_backend::bitmap::BitmapMmapRegion;
use vhost_user_backend::{VringMutex, VringState, VringT};
use virtio_queue::{DescriptorChain, QueueOwnedT};
use vm_memory::{GuestAddressSpace, GuestMemoryAtomic, GuestMemoryLoadGuard, GuestMemoryMmap};
use vmm_sys_util::eventfd::EventFd;

use crate::descriptor_utils::{Error as VufDescriptorError, Reader, Writer};
use crate::filesystem::{FileSystem, SerializableFileSystem};
use crate::server::Server;
use crate::util::other_io_error;
use crate::Error as VhostUserFsError;

type LoggedMemory = GuestMemoryMmap<BitmapMmapRegion>;
type LoggedMemoryAtomic = GuestMemoryAtomic<LoggedMemory>;

// The guest queued an available buffer for the high priority queue.
const HIPRIO_QUEUE_EVENT: u16 = 0;
// The guest queued an available buffer for the request queue.
const REQ_QUEUE_EVENT: u16 = 1;

/// The maximum length of the tag being used.
pub const MAX_TAG_LEN: usize = 36;

type Result<T> = std::result::Result<T, Error>;

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

// TODO: Type and members should be private.
pub struct VhostUserFsThread<F: FileSystem + Send + Sync + 'static> {
    pub mem: Option<LoggedMemoryAtomic>,
    pub kill_evt: EventFd,
    pub server: Arc<Server<F>>,
    // handle request from backend to frontend
    pub vu_req: Option<Backend>,
    pub event_idx: bool,
    pub pool: Option<ThreadPool>,
}

impl<F: FileSystem + Send + Sync + 'static> Clone for VhostUserFsThread<F> {
    fn clone(&self) -> Self {
        VhostUserFsThread {
            mem: self.mem.clone(),
            kill_evt: self.kill_evt.try_clone().unwrap(),
            server: self.server.clone(),
            vu_req: self.vu_req.clone(),
            event_idx: self.event_idx,
            pool: self.pool.clone(),
        }
    }
}

impl<F: FileSystem + SerializableFileSystem + Send + Sync + 'static> VhostUserFsThread<F> {
    // TODO: Should be private.
    pub fn new(fs: F, thread_pool_size: usize) -> Result<Self> {
        let pool = if thread_pool_size > 0 {
            // Test that unshare(CLONE_FS) works, it will be called for each thread.
            // It's an unprivileged system call but some Docker/Moby versions are
            // known to reject it via seccomp when CAP_SYS_ADMIN is not given.
            //
            // Note that the program is single-threaded here so this syscall has no
            // visible effect and is safe to make.
            let ret = unsafe { libc::unshare(libc::CLONE_FS) };
            if ret == -1 {
                return Err(Error::UnshareCloneFs(std::io::Error::last_os_error()));
            }

            Some(
                ThreadPoolBuilder::new()
                    .after_start(|_| {
                        // unshare FS for xattr operation
                        let ret = unsafe { libc::unshare(libc::CLONE_FS) };
                        assert_eq!(ret, 0); // Should not fail
                    })
                    .pool_size(thread_pool_size)
                    .create()
                    .map_err(Error::CreateThreadPool)?,
            )
        } else {
            None
        };

        Ok(VhostUserFsThread {
            mem: None,
            kill_evt: EventFd::new(EFD_NONBLOCK).map_err(Error::CreateKillEventFd)?,
            server: Arc::new(Server::new(fs)),
            vu_req: None,
            event_idx: false,
            pool,
        })
    }

    fn return_descriptor(
        vring_state: &mut VringState<LoggedMemoryAtomic>,
        head_index: u16,
        event_idx: bool,
        len: usize,
    ) {
        let used_len: u32 = match len.try_into() {
            Ok(l) => l,
            Err(_) => panic!("Invalid used length, can't return used descritors to the ring"),
        };

        if vring_state.add_used(head_index, used_len).is_err() {
            warn!("Couldn't return used descriptors to the ring");
        }

        if event_idx {
            match vring_state.needs_notification() {
                Err(_) => {
                    warn!("Couldn't check if queue needs to be notified");
                    vring_state.signal_used_queue().unwrap();
                }
                Ok(needs_notification) => {
                    if needs_notification {
                        vring_state.signal_used_queue().unwrap();
                    }
                }
            }
        } else {
            vring_state.signal_used_queue().unwrap();
        }
    }

    fn process_queue_pool(&self, vring: VringMutex<LoggedMemoryAtomic>) -> Result<bool> {
        let mut used_any = false;
        let atomic_mem = match &self.mem {
            Some(m) => m,
            None => return Err(Error::NoMemoryConfigured),
        };

        while let Some(avail_desc) = vring
            .get_mut()
            .get_queue_mut()
            .iter(atomic_mem.memory())
            .map_err(|_| Error::IterateQueue)?
            .next()
        {
            used_any = true;

            // Prepare a set of objects that can be moved to the worker thread.
            let atomic_mem = atomic_mem.clone();
            let server = self.server.clone();
            let mut vu_req = self.vu_req.clone();
            let event_idx = self.event_idx;
            let worker_vring = vring.clone();
            let worker_desc = avail_desc.clone();

            self.pool.as_ref().unwrap().spawn_ok(async move {
                let mem = atomic_mem.memory();
                let head_index = worker_desc.head_index();

                let reader = Reader::new(&mem, worker_desc.clone())
                    .map_err(Error::QueueReader)
                    .unwrap();
                let writer = Writer::new(&mem, worker_desc.clone())
                    .map_err(Error::QueueWriter)
                    .unwrap();

                let len = server
                    .handle_message(reader, writer, vu_req.as_mut())
                    .map_err(Error::ProcessQueue)
                    .unwrap();

                Self::return_descriptor(&mut worker_vring.get_mut(), head_index, event_idx, len);
            });
        }

        Ok(used_any)
    }

    fn process_queue_serial(
        &self,
        vring_state: &mut VringState<LoggedMemoryAtomic>,
    ) -> Result<bool> {
        let mut used_any = false;
        let mem = match &self.mem {
            Some(m) => m.memory(),
            None => return Err(Error::NoMemoryConfigured),
        };
        let mut vu_req = self.vu_req.clone();

        let avail_chains: Vec<DescriptorChain<GuestMemoryLoadGuard<LoggedMemory>>> = vring_state
            .get_queue_mut()
            .iter(mem.clone())
            .map_err(|_| Error::IterateQueue)?
            .collect();

        for chain in avail_chains {
            used_any = true;

            let head_index = chain.head_index();

            let reader = Reader::new(&mem, chain.clone())
                .map_err(Error::QueueReader)
                .unwrap();
            let writer = Writer::new(&mem, chain.clone())
                .map_err(Error::QueueWriter)
                .unwrap();

            let len = self
                .server
                .handle_message(reader, writer, vu_req.as_mut())
                .map_err(Error::ProcessQueue)
                .unwrap();

            Self::return_descriptor(vring_state, head_index, self.event_idx, len);
        }

        Ok(used_any)
    }

    // TODO: Should be private.
    pub fn handle_event_pool(
        &self,
        device_event: u16,
        vrings: &[VringMutex<LoggedMemoryAtomic>],
    ) -> io::Result<()> {
        let idx = match device_event {
            HIPRIO_QUEUE_EVENT => {
                debug!("HIPRIO_QUEUE_EVENT");
                0
            }
            REQ_QUEUE_EVENT => {
                debug!("QUEUE_EVENT");
                1
            }
            _ => return Err(Error::HandleEventUnknownEvent.into()),
        };

        if self.event_idx {
            // vm-virtio's Queue implementation only checks avail_index
            // once, so to properly support EVENT_IDX we need to keep
            // calling process_queue() until it stops finding new
            // requests on the queue.
            loop {
                vrings[idx].disable_notification().unwrap();
                // we can't recover from an error here, so let's hope it's transient
                if let Err(e) = self.process_queue_pool(vrings[idx].clone()) {
                    error!("processing the vring {idx}: {e}");
                }
                if !vrings[idx].enable_notification().unwrap() {
                    break;
                }
            }
        } else {
            // Without EVENT_IDX, a single call is enough.
            self.process_queue_pool(vrings[idx].clone())?;
        }

        Ok(())
    }

    // TODO: Should be private.
    pub fn handle_event_serial(
        &self,
        device_event: u16,
        vrings: &[VringMutex<LoggedMemoryAtomic>],
    ) -> io::Result<()> {
        let mut vring_state = match device_event {
            HIPRIO_QUEUE_EVENT => {
                debug!("HIPRIO_QUEUE_EVENT");
                vrings[0].get_mut()
            }
            REQ_QUEUE_EVENT => {
                debug!("QUEUE_EVENT");
                vrings[1].get_mut()
            }
            _ => return Err(Error::HandleEventUnknownEvent.into()),
        };

        if self.event_idx {
            // vm-virtio's Queue implementation only checks avail_index
            // once, so to properly support EVENT_IDX we need to keep
            // calling process_queue() until it stops finding new
            // requests on the queue.
            loop {
                vring_state.disable_notification().unwrap();
                // we can't recover from an error here, so let's hope it's transient
                if let Err(e) = self.process_queue_serial(&mut vring_state) {
                    error!("processing the vring: {e}");
                }
                if !vring_state.enable_notification().unwrap() {
                    break;
                }
            }
        } else {
            // Without EVENT_IDX, a single call is enough.
            self.process_queue_serial(&mut vring_state)?;
        }

        Ok(())
    }
}
