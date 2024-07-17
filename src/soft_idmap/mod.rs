// Copyright 2024 Red Hat, Inc. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/*!
 * Facilities for mapping UIDs/GIDs within virtiofsd.
 *
 * This module provides various facilities to map UIDs/GIDs between host and guest, with separate
 * translation functions in either direction.
 */

pub mod id_types;

use btree_range_map::RangeMap;
pub use id_types::{GuestGid, GuestId, GuestUid, HostGid, HostId, HostUid, Id};
use std::fmt::{self, Display, Formatter};
use std::io;
use std::ops::{Add, Range, Sub};

/**
 * Provides mappings for UIDs or GIDs between host and guest.
 *
 * Each `IdMap` will only translate UIDs or GIDs, not both.  Translation in either direction (host
 * to guest, guest to host) is independent of the other direction, i.e. does not need to be
 * bijective (invertible).
 */
pub struct IdMap<Guest: GuestId<HostType = Host>, Host: HostId<GuestType = Guest>> {
    /// Guest-to-host mapping.
    guest_to_host: RangeMap<Guest::Inner, MapEntry<Guest, Host>>,
    /// Host-to-guest mapping.
    host_to_guest: RangeMap<Host::Inner, MapEntry<Host, Guest>>,
}

/**
 * Maps a range of IDs.
 *
 * Can be either UIDs or GIDs, and either host to guest or guest to host.
 */
#[derive(Clone, Debug, PartialEq)]
enum MapEntry<Source: Id, Target: Id> {
    /// Squash a range of IDs onto a single one.
    #[allow(dead_code)] // to be removed when we allow parsing from the command line
    Squash {
        /// Range of source IDs.
        from: Range<Source>,
        /// Single target ID.
        to: Target,
    },

    /// 1:1 map a range of IDs to another range (of the same length).
    #[allow(dead_code)] // to be removed when we allow parsing from the command line
    Range {
        /// Range of source IDs.
        from: Range<Source>,
        /// First ID in the target range (i.e. mapping for `from.start`).
        to_base: Target,
    },

    /// Disallow using this ID range: Return an error.
    #[allow(dead_code)] // to be removed when we allow parsing from the command line
    Fail {
        /// Range of source IDs.
        from: Range<Source>,
    },
}

#[derive(Clone, Debug)]
pub enum MapError<Source: Id> {
    ExplicitFailMapping { id: Source },
}

impl<Guest, Host> IdMap<Guest, Host>
where
    Guest: GuestId<HostType = Host>,
    Host: HostId<GuestType = Guest>,
{
    /**
     * Create an empty map.
     *
     * Note that unmapped ranges default to identity mapping, i.e. an empty map will map everything
     * to itself (numerically speaking).
     */
    pub fn empty() -> Self {
        IdMap {
            guest_to_host: RangeMap::new(),
            host_to_guest: RangeMap::new(),
        }
    }

    /// Map a guest UID/GID to one in the host domain.
    pub fn map_guest(&self, guest_id: Guest) -> Result<Host, MapError<Guest>> {
        self.guest_to_host
            .get(guest_id.into_inner())
            .map(|e| e.map(guest_id))
            .unwrap_or(Ok(guest_id.id_mapped()))
    }

    /// Map a host UID/GID to one in the guest domain.
    pub fn map_host(&self, host_id: Host) -> Result<Guest, MapError<Host>> {
        self.host_to_guest
            .get(host_id.into_inner())
            .map(|e| e.map(host_id))
            .unwrap_or(Ok(host_id.id_mapped()))
    }
}

impl<Source: Id, Target: Id> MapEntry<Source, Target>
where
    Source: Sub<Source>,
    Target: Add<<Source as Sub>::Output, Output = Target>,
{
    /// Map an element from the source domain into the target domain.
    fn map(&self, id: Source) -> Result<Target, MapError<Source>> {
        match self {
            MapEntry::Squash { from, to } => {
                assert!(from.contains(&id));
                Ok(*to)
            }

            MapEntry::Range { from, to_base } => {
                assert!(from.contains(&id));
                Ok(*to_base + (id - from.start))
            }

            MapEntry::Fail { from } => {
                assert!(from.contains(&id));
                Err(MapError::ExplicitFailMapping { id })
            }
        }
    }
}

impl<Source: Id> Display for MapError<Source> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            MapError::ExplicitFailMapping { id } => {
                write!(f, "Use of ID {id} has been configured to fail")
            }
        }
    }
}

impl<Source: Id> std::error::Error for MapError<Source> {}

impl<Source: Id> From<MapError<Source>> for io::Error {
    fn from(err: MapError<Source>) -> Self {
        io::Error::new(io::ErrorKind::PermissionDenied, err)
    }
}

impl<Source: Id, Target: Id> Display for MapEntry<Source, Target>
where
    Source: Sub<Source>,
    Target: Add<<Source as Sub>::Output, Output = Target>,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            MapEntry::Squash { from, to } => {
                write!(f, "squash [{}, {}) to {}", from.start, from.end, to)
            }
            MapEntry::Range { from, to_base } => {
                write!(
                    f,
                    "map [{}, {}) to [{}, {})",
                    from.start,
                    from.end,
                    to_base,
                    *to_base + (from.end - from.start)
                )
            }
            MapEntry::Fail { from } => {
                write!(f, "fail [{}, {})", from.start, from.end)
            }
        }
    }
}
