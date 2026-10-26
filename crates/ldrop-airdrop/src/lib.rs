// SPDX-License-Identifier: GPL-3.0-or-later
//! AirDrop helpers that need no I/O (SPEC §6): what ldrop shows for an Apple peer, where it
//! is on the AWDL link, and how its share sheet shows up over BLE. The protocol itself is
//! `owly-airdrop`.

pub mod addr;
pub mod ble;
pub mod model;
pub mod uti;
