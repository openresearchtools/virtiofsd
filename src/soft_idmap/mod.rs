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

pub use id_types::{GuestGid, GuestId, GuestUid, HostGid, HostId, HostUid, Id};
