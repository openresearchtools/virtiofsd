// Copyright 2024 Red Hat, Inc. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/*!
 * Serialization functionality (i.e. what happens in `SerializableFileSystem::serialize()`): Take
 * information that we have collected during preserialization and turn it into actually
 * serializable structs ('serialized' module), which are then turned into a plain vector of bytes.
 */

use crate::fuse;
use crate::passthrough::device_state::preserialization::{
    self, HandleMigrationInfo, InodeMigrationInfo,
};
use crate::passthrough::device_state::serialized;
use crate::passthrough::inode_store::InodeData;
use crate::passthrough::util::relative_path;
use crate::passthrough::{Handle, HandleData, PassthroughFs};
use crate::util::{other_io_error, ResultErrorContext};
use std::convert::TryFrom;
use std::ffi::CString;
use std::io;
use std::sync::atomic::Ordering;

impl TryFrom<serialized::PassthroughFs> for Vec<u8> {
    type Error = io::Error;

    /// Root of serialization: Turn the final `serialized::PassthroughFs` struct into plain bytes
    fn try_from(state: serialized::PassthroughFs) -> io::Result<Self> {
        postcard::to_stdvec(&state).map_err(other_io_error)
    }
}

impl TryFrom<&PassthroughFs> for serialized::PassthroughFsV1 {
    type Error = io::Error;

    /// Serialize `fs`, assuming it has been prepared for serialization (i.e. all inodes must have
    /// their migration info set)
    fn try_from(fs: &PassthroughFs) -> io::Result<Self> {
        let handles_map = fs.handles.read().unwrap();

        let inodes = if let Some(shared_dir) = fs.inodes.get(fuse::ROOT_ID) {
            let shared_dir_path = shared_dir.get_path(&fs.proc_self_fd);
            fs.inodes.iter().map(|inode| {
                inode
                    .as_ref()
                    .as_serialized(fs, &shared_dir, &shared_dir_path)
                    .unwrap_or_else(|err| {
                        warn!(
                            "Failed to serialize inode {} (st_dev={}, mnt_id={}, st_ino={}): {}; marking as invalid",
                            inode.inode, inode.ids.dev, inode.ids.mnt_id, inode.ids.ino, err
                        );
                        serialized::Inode {
                            id: inode.inode,
                            refcount: inode.refcount.load(Ordering::Relaxed),
                            location: serialized::InodeLocation::Invalid,
                            file_handle: None,
                        }
                    })
            }).collect()
        } else {
            // When unmounted, we will not have a root node, that's OK.  But there should not be
            // any other nodes either then.
            if !fs.inodes.is_empty() {
                return Err(other_io_error(
                    "Root node (shared directory) not in inode store".to_string(),
                ));
            };
            Vec::new()
        };

        let handles = handles_map
            .iter()
            .map(|(handle, data)| (*handle, data.as_ref()).into())
            .collect();

        Ok(serialized::PassthroughFsV1 {
            inodes,
            next_inode: fs.next_inode.load(Ordering::Relaxed),

            handles,
            next_handle: fs.next_handle.load(Ordering::Relaxed),

            negotiated_opts: fs.into(),
        })
    }
}

impl From<&PassthroughFs> for serialized::NegotiatedOpts {
    /// Serialize the options we have negotiated with the guest
    fn from(fs: &PassthroughFs) -> Self {
        serialized::NegotiatedOpts {
            writeback: fs.writeback.load(Ordering::Relaxed),
            announce_submounts: fs.announce_submounts.load(Ordering::Relaxed),
            posix_acl: fs.posix_acl.load(Ordering::Relaxed),
            sup_group_extension: fs.sup_group_extension.load(Ordering::Relaxed),
        }
    }
}

impl InodeData {
    /// Serialize an inode, which requires that its `migration_info` is set
    fn as_serialized(
        &self,
        fs: &PassthroughFs,
        shared_dir: &InodeData,
        shared_dir_path: &io::Result<CString>,
    ) -> io::Result<serialized::Inode> {
        let id = self.inode;
        let refcount = self.refcount.load(Ordering::Relaxed);

        // Note that we do not special-case invalid inodes here (`self.file_or_handle ==
        // FileOrHandle::Invalid(_)`), i.e. inodes that this instance failed to find on a prior
        // incoming migration.  We do not expect them to have migration info (we could not open
        // them, so we should not know where to find them), but if we do, there must be a reason
        // for it, so we might as well forward it to our destination.

        let migration_info_locked = self.migration_info.lock().unwrap();
        let migration_info = migration_info_locked
            .as_ref()
            .ok_or_else(|| other_io_error("Failed to reconstruct inode location"))?;

        // The root node (and only the root node) must have its special kind of placeholder info
        assert_eq!(
            (id == fuse::ROOT_ID),
            matches!(
                migration_info.location,
                preserialization::InodeLocation::RootNode
            )
        );

        // Serialize the information that tells the destination how to find this inode
        let location = migration_info.as_serialized(self, fs, shared_dir, shared_dir_path)?;

        let file_handle = if fs.cfg.migration_verify_handles {
            // We could construct the file handle now, but we don't want to do I/O here.  It should
            // have been prepared in the preserialization phase.  If it is not, that's an internal
            // programming error.
            let handle = migration_info
                .file_handle
                .as_ref()
                .ok_or_else(|| other_io_error("No prepared file handle found"))?;
            Some(handle.clone())
        } else {
            None
        };

        Ok(serialized::Inode {
            id,
            refcount,
            location,
            file_handle,
        })
    }
}

impl InodeMigrationInfo {
    /// Helper for serializing inodes: Turn their prepared `migration_info` into a
    /// `serialized::InodeLocation`
    fn as_serialized(
        &self,
        inode_data: &InodeData,
        fs: &PassthroughFs,
        shared_dir: &InodeData,
        shared_dir_path: &io::Result<CString>,
    ) -> io::Result<serialized::InodeLocation> {
        Ok(match &self.location {
            preserialization::InodeLocation::RootNode => serialized::InodeLocation::RootNode,

            preserialization::InodeLocation::Path(inode_path) => {
                let preserialization::find_paths::InodePath { parent, filename } = inode_path;

                if fs.cfg.migration_confirm_paths {
                    if let Err(err) = inode_path.check_presence(inode_data, self) {
                        warn!(
                            "Lost inode {} (former location: {}): {}; looking it up through /proc/self/fd",
                            inode_data.inode, filename, err
                        );
                        // Inode is gone (or replaced), look for it in /proc/self/fd
                        let path_in_shared_dir = self
                            .path_from_proc_self_fd(inode_data, fs, shared_dir, shared_dir_path)
                            .err_context(|| "Failed to get path from /proc/self/fd".to_string())?;
                        info!("Found inode {}: {}", inode_data.inode, path_in_shared_dir);
                        return Ok(serialized::InodeLocation::FullPath {
                            filename: path_in_shared_dir,
                        });
                    }
                }

                // Safe: We serialize everything before we will drop the serialized state (the
                // inode store), so the strong refcount in there will outlive this weak reference
                // (which means that the ID we get will remain valid until everything is
                // serialized, i.e. that parent node will be part of the serialized state)
                let parent = unsafe { parent.get_raw() };
                let filename = filename.clone();

                serialized::InodeLocation::Path { parent, filename }
            }
        })
    }

    /// Retrieve the inode's path relative to the shared directory from /proc/self/fd
    fn path_from_proc_self_fd(
        &self,
        inode_data: &InodeData,
        fs: &PassthroughFs,
        shared_dir: &InodeData,
        shared_dir_path: &io::Result<CString>,
    ) -> io::Result<String> {
        let path = inode_data.get_path(&fs.proc_self_fd)?;

        // Kernel will report nodes beyond our root as having path / -- but only the root node (the
        // shared directory) can actually have that path, so we can cut the rest short and spare
        // the user the more cryptic error generated by `check_presence()`
        if path.as_bytes() == b"/" && inode_data.inode != fuse::ROOT_ID {
            return Err(other_io_error(
                "Got empty path for non-root node, so it is outside the shared directory"
                    .to_string(),
            ));
        }

        let shared_dir_path = shared_dir_path.as_ref().map_err(|err| {
            io::Error::new(err.kind(), format!("Shared directory path unknown: {err}"))
        })?;

        let relative_path = relative_path(&path, shared_dir_path)?
            .to_str()
            .map_err(|err| other_io_error(format!("Path {path:?} is not a UTF-8 string: {err}")))?
            .to_string();

        preserialization::find_paths::InodePath {
            parent: fs.inodes.get_strong(shared_dir.inode).unwrap(),
            filename: relative_path.clone(),
        }
        .check_presence(inode_data, self)
        .map_err(|err| io::Error::new(err.kind(), format!("Inode not found at {path:?}: {err}")))?;

        Ok(relative_path)
    }
}

impl From<(Handle, &HandleData)> for serialized::Handle {
    /// Serialize a handle
    fn from(handle: (Handle, &HandleData)) -> Self {
        // Note that we will happily process invalid handles here (`handle.1.file ==
        // HandleDataFile::Invalid(_)`), i.e. handles that this instance failed to open on a prior
        // incoming migration.  A handle is identified by the inode to which it belongs, and
        // instructions on how to open that inode (e.g. `open()` flags).  If this instance failed
        // to open the inode in this way (on in-migration), that does not prevent us from
        // forwarding the same information to the next destination (on out-migration), and thus
        // allow it to re-try.

        let source = (&handle.1.migration_info).into();
        serialized::Handle {
            id: handle.0,
            inode: handle.1.inode,
            source,
        }
    }
}

impl From<&HandleMigrationInfo> for serialized::HandleSource {
    /// Helper for serializing handles: Turn their prepared `migration_info` into a
    /// `serialized::HandleSource`
    fn from(repr: &HandleMigrationInfo) -> Self {
        match repr {
            HandleMigrationInfo::OpenInode { flags } => {
                serialized::HandleSource::OpenInode { flags: *flags }
            }
        }
    }
}
