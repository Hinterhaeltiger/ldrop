// SPDX-License-Identifier: GPL-3.0-or-later
//! `ldrop integrate`: adds or removes the per-user "Send with ldrop" entries (SPEC §11.6).
//! `ldrop-gui` adds them once on its own; this is for doing it by hand or undoing it.

use std::path::Path;

use ldrop_core::integrate::{self, Change};

use crate::{CmdResult, Exit, Fail};

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum FileManager {
    /// Custom action in ~/.config/Thunar/uca.xml.
    Thunar,
    /// Script in ~/.local/share/nautilus/scripts, for Nautilus without nautilus-python.
    NautilusScript,
}

pub fn run(file_manager: FileManager, remove: bool) -> CmdResult {
    let (name, path, result, restart) = match file_manager {
        FileManager::Thunar => ("Thunar", integrate::thunar_file(), integrate::thunar(remove), "thunar -q"),
        FileManager::NautilusScript => {
            ("Nautilus", integrate::nautilus_script_file(), integrate::nautilus_script(remove), "nautilus -q")
        }
    };
    report(name, &path, remove, restart, result)
}

fn report(name: &str, path: &Path, remove: bool, restart: &str, result: std::io::Result<Change>) -> CmdResult {
    match result.map_err(|e| Fail::new(Exit::Error, format!("{}: {e}", path.display())))? {
        Change::Unchanged => println!("{name} already {} \"Send with ldrop\"", if remove { "lacks" } else { "has" }),
        Change::Done => println!(
            "{} \"Send with ldrop\" {} {}; restart {name} to see it: {restart}",
            if remove { "Removed" } else { "Added" },
            if remove { "from" } else { "to" },
            path.display()
        ),
    }
    Ok(Exit::Ok)
}
