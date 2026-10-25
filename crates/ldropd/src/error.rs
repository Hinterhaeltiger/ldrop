// SPDX-License-Identifier: GPL-3.0-or-later
//! `org.ldrop.Error.*` (SPEC §10).

use zbus::DBusError;

#[derive(Debug, DBusError)]
#[zbus(prefix = "org.ldrop.Error")]
pub enum Error {
    #[zbus(error)]
    ZBus(zbus::Error),
    NotSupported(String),
    NotFound(String),
    Busy(String),
    Unavailable(String),
    PermissionDenied(String),
    Failed(String),
}

pub type Result<T> = std::result::Result<T, Error>;
