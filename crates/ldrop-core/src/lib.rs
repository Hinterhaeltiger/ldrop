// SPDX-License-Identifier: GPL-3.0-or-later
//! ldrop domain model, configuration, storage and capability rules (SPEC §4.1, §8, §9, §12).
//!
//! Shared by the daemon and every frontend. Contains no I/O on the network or radio.

pub mod capabilities;
pub mod config;
pub mod dbus;
pub mod history;
pub mod hostname;
pub mod model;
pub mod paths;
pub mod storage;
pub mod time;
pub mod uri;

pub use model::*;
