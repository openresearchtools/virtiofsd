// Copyright 2024 Red Hat, Inc. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/*!
 * Facilities for getting inodes’ paths from /proc/self/fd for migration.
 *
 * This module provides different objects that all share the same core for multiple purposes:
 * - Provide a preserialization migration info constructor for the find-paths migration mode
 * - Check migration info paths during migration and, if found incorrect, reconstruct them as we
 *   would for preserialization; this is used by --migration-confirm-paths, as well as an implicit
 *   double-check step after any path-based preserialization phase
 */

use super::InodeMigrationInfo;
use crate::fuse;
use crate::passthrough::inode_store::{InodeData, StrongInodeReference};
use crate::passthrough::util::relative_path;
use crate::passthrough::PassthroughFs;
use crate::util::{other_io_error, ResultErrorContext};
use std::ffi::{CStr, CString};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/**
 * Provides all core functionality.
 *
 * This module provides functionality for three different cases; all of it is implemented on this
 * single internal struct that is incorporated into different public structs depending on the use.
 *
 * `Walker::run()` is the core method, which walks over the inode store and can check paths in
 * inode migration info structures, and construct them by looking into /proc/self/fd.  What exactly
 * is done depends on `mode`.
 */
struct Walker<'a> {
    /// Reference to the filesystem state to check
    fs: &'a PassthroughFs,
    /// Specifies which functionality we are supposed to provide
    #[allow(dead_code)] // will be used once we provide more than one mode
    mode: Mode,
    /// Optional: Cancel early
    cancel: Option<Arc<AtomicBool>>,
}

/**
 * `--migration-confirm-paths` implementation.
 *
 * Implements checking inodes’ paths right before serialization, as requested by the user through
 * the `--migration-confirm-paths` switch: Give all inodes that either don’t have a migration info
 * set, or where it is found to be incorrect, a path from /proc/self/fd.  Furthermore, given the
 * user has specifically requested this check run, return any error as a hard error, preventing
 * migration.
 */
pub(in crate::passthrough::device_state) struct ConfirmPaths<'a> {
    /// `Walker` in `Mode::ConfirmPaths` mode.
    walker: Walker<'a>,
}

/**
 * Double-check inodes’ paths after preserialization.
 *
 * Similar to `ConfirmPaths`, but is an implicit double-check run after the first preserialization
 * phase, and as a result, is more relaxed:
 * - On a fundamental unrecoverable error (e.g. failing to find the shared directory’s base path),
 *   printing a warning an skipping the whole run is OK
 * - We only need to find new paths for inodes that have a path in their migration info when we
 *   found that path to be incorrect.  No need to try to find paths for inodes that don’t have any
 *   migration info attached to them.
 */
pub(in crate::passthrough::device_state) struct ImplicitPathCheck<'a> {
    /// `Walker` in `Mode::ImplicitPathCheck` mode.
    walker: Walker<'a>,
}

/// Selects how a `Walker` should behave.
pub(in crate::passthrough::device_state) enum Mode {
    /// Run the `--migration-confirm-paths` check.
    ConfirmPaths,

    /// Double-check inodes’ paths after preserialization.
    ImplicitPathCheck,
}

impl<'a> ConfirmPaths<'a> {
    /// Prepare to confirm paths collected for `fs`.
    pub fn new(fs: &'a PassthroughFs) -> Self {
        ConfirmPaths {
            walker: Walker::new(fs, Mode::ConfirmPaths, None),
        }
    }

    /**
     * Run the `--migration-confirm-paths` check.
     *
     * If necessary, try to fix the paths collected during the preserialization phase by looking
     * into /proc/self/fd.  Return errors.
     */
    pub fn confirm_paths(self) -> io::Result<()> {
        self.walker.run()
    }
}

impl<'a> ImplicitPathCheck<'a> {
    /// Prepare to double-check paths during preserialization.
    pub fn new(fs: &'a PassthroughFs, cancel: Arc<AtomicBool>) -> Self {
        ImplicitPathCheck {
            walker: Walker::new(fs, Mode::ImplicitPathCheck, Some(cancel)),
        }
    }

    /**
     * Double-check inodes’ paths after preserialization.
     *
     * Try to fix any paths that are wrong (by getting new paths from /proc/self/fd), but do not
     * return errors: This check is implicit, not requested by the user, so should be infallible,
     * not cancelling migration on error.
     */
    pub fn check_paths(self) {
        if let Err(err) = self.walker.run() {
            warn!("Double-check of all inode paths collected for migration failed: {err}")
        }
    }
}

impl<'a> Walker<'a> {
    /**
     * Create a `Walker` over `fs` with the given `mode`.
     *
     * If `cancel` is given, the operation will be cancelled when it is found to be set.
     */
    fn new(fs: &'a PassthroughFs, mode: Mode, cancel: Option<Arc<AtomicBool>>) -> Self {
        Walker { fs, mode, cancel }
    }

    /**
     * Run the `Walker` over all inodes in our store.
     *
     * Iterate through the store, check the paths we found (depending on the `mode`), and update
     * inodes’ migration info with paths from /proc/self/fd (depending on the `mode`).
     */
    fn run(self) -> io::Result<()> {
        let Some(root_node) = self.fs.inodes.get(fuse::ROOT_ID) else {
            // No root?  That’s fine if and only if we don’t have any inodes at all.
            return if self.fs.inodes.is_empty() {
                Ok(())
            } else {
                Err(other_io_error("Root node not found"))
            };
        };

        let shared_dir_path = root_node
            .get_path(&self.fs.proc_self_fd)
            .err_context(|| "Failed to get shared directory's path")?;

        for inode_data in self.fs.inodes.iter() {
            if self
                .cancel
                .as_ref()
                .map(|c| c.load(Ordering::Relaxed))
                .unwrap_or(false)
            {
                break;
            }

            if !self.should_update_inode(&inode_data) {
                continue;
            }

            let set_path_result =
                set_path_migration_info_from_proc_self_fd(&inode_data, self.fs, &shared_dir_path);
            match self.mode {
                // In check modes, we note inodes we found, and log all errors
                Mode::ConfirmPaths | Mode::ImplicitPathCheck => {
                    if let Err(err) = set_path_result {
                        error!("Inode {}: {}", inode_data.inode, err);
                    } else if let Some(new_info) =
                        inode_data.migration_info.lock().unwrap().as_ref()
                    {
                        info!("Found inode {}: {}", inode_data.inode, new_info.location);
                    }
                }
            }
        }

        Ok(())
    }

    /**
     * Check the given inode’s migration info.
     *
     * - Return `true` iff the info should be updated from /proc/self/fd.
     * - Return `false` iff the info seems fine, and should be left as-is.
     */
    fn should_update_inode(&self, inode_data: &InodeData) -> bool {
        let mut migration_info_locked = inode_data.migration_info.lock().unwrap();
        match (&self.mode, migration_info_locked.as_ref()) {
            // Do not touch inodes without migration info in the implicit/lax check mode
            (Mode::ImplicitPathCheck, None) => false,

            // In the explicit check mode, give migration info to those inodes that don’t already
            // have it
            (Mode::ConfirmPaths, None) => true,

            // In both check modes, when there is pre-existing migration info, we have to check its
            // path; update those we find to be incorrect
            (Mode::ConfirmPaths, Some(migration_info))
            | (Mode::ImplicitPathCheck, Some(migration_info)) => {
                if let Err(err) = migration_info.check_path_presence(inode_data) {
                    // Migration info is wrong, clear it unconditionally, regardless of whether we
                    // can find a better one
                    let migration_info = migration_info_locked.take().unwrap();
                    warn!(
                        "Lost inode {} (former location: {}): {}; looking it up through /proc/self/fd",
                        inode_data.inode, migration_info.location, err
                    );
                    true
                } else {
                    false
                }
            }
        }
    }
}

/**
 * Update inode migration info from /proc/self/fd.
 *
 * Fetch the given inode’s path from /proc/self/fd, split that path into components relative to
 * the shared directory root, and for all inodes along that path, if they don’t have a migration
 * info set, set it accordingly.
 *
 * Note that this is decidedly not a method of `Walker` so that we can easily reuse it in other
 * places; specifically, to re-establish a path for inodes that have been potentially invalidated.
 */
fn set_path_migration_info_from_proc_self_fd(
    inode_data: &InodeData,
    fs: &PassthroughFs,
    shared_dir_path: &CStr,
) -> io::Result<()> {
    let abs_path = inode_data
        .get_path(&fs.proc_self_fd)
        .err_context(|| "Failed to get path from /proc/self/fd")?;

    let rel_path = relative_path(&abs_path, shared_dir_path)?
        .to_str()
        .map_err(|err| other_io_error(format!("Path {abs_path:?} is not a UTF-8 string: {err}")))?
        .to_string();

    let path = Path::new(&rel_path);

    let mut parent = fs.inodes.get_strong(fuse::ROOT_ID)?;

    for element in path {
        // Both `unwrap()`s must succeed: We know the path is UTF-8, and we know it does not
        // contain internal NULs (because it used to be a CString before)
        let element_cstr = CString::new(element.to_str().unwrap()).unwrap();
        let entry = fs.do_lookup(parent.get().inode, &element_cstr)?;

        // `entry.inode` is effectively a strong reference, so this must succeed
        let entry_data = fs.inodes.get(entry.inode).unwrap();
        // Safe: Turns `entry.inode` back into a typed strong reference
        let entry_inode = unsafe { StrongInodeReference::new_no_increment(entry_data, &fs.inodes) };

        {
            let entry_data = entry_inode.get();
            let mut mig_info = entry_data.migration_info.lock().unwrap();
            if mig_info.is_none() {
                *mig_info = Some(InodeMigrationInfo::new(
                    &fs.cfg,
                    parent,
                    &element_cstr,
                    &entry_data.file_or_handle,
                )?);
            }
        }

        parent = entry_inode;
    }

    if parent.get().inode != inode_data.inode {
        return Err(other_io_error(format!(
            "Inode not found under path reported by /proc/self/fd ({rel_path:?})"
        )));
    }

    Ok(())
}
