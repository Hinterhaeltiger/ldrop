// SPDX-License-Identifier: GPL-3.0-or-later
//! The QObject behind the share sheet's QML. The state crosses as JSON strings: the sheet
//! shows a handful of devices, so rebuilding the list on each change costs nothing, and it
//! keeps the C++ side free of list models.

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    extern "RustQt" {
        #[qobject]
        #[qml_element]
        #[qproperty(QString, summary)]
        #[qproperty(QString, devices)]
        #[qproperty(QString, status)]
        type ShareBackend = super::ShareBackendRust;

        /// Starts discovery; called once the sheet is shown.
        #[qinvokable]
        fn start(self: Pin<&mut ShareBackend>);

        #[qinvokable]
        fn send(self: &ShareBackend, device_id: &QString);

        #[qinvokable]
        fn cancel(self: &ShareBackend, device_id: &QString);
    }

    impl cxx_qt::Threading for ShareBackend {}

    unsafe extern "C++" {
        include!("ldrop-gui/src/share/app.h");

        /// Runs the share sheet's event loop until its window closes.
        fn run_share_app() -> i32;
    }
}

use std::pin::Pin;

use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::QString;
use tokio::sync::mpsc;

use super::{Command, worker};

pub struct ShareBackendRust {
    summary: QString,
    devices: QString,
    status: QString,
    commands: Option<mpsc::UnboundedSender<Command>>,
}

impl Default for ShareBackendRust {
    fn default() -> Self {
        ShareBackendRust {
            summary: QString::from(super::summary().as_str()),
            devices: QString::from("[]"),
            status: QString::from("Looking for nearby devices…"),
            commands: None,
        }
    }
}

impl qobject::ShareBackend {
    fn start(mut self: Pin<&mut Self>) {
        if self.commands.is_some() {
            return;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        self.as_mut().rust_mut().commands = Some(tx);
        let qt = self.qt_thread();
        std::thread::spawn(move || worker(qt, rx));
    }

    fn send(&self, device_id: &QString) {
        self.command(Command::Send(device_id.to_string()));
    }

    fn cancel(&self, device_id: &QString) {
        self.command(Command::Cancel(device_id.to_string()));
    }

    fn command(&self, command: Command) {
        if let Some(tx) = &self.commands {
            let _ = tx.send(command);
        }
    }
}

pub use qobject::{ShareBackend, run_share_app};
