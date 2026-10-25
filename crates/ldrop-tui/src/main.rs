// SPDX-License-Identifier: GPL-3.0-or-later
//! `ldrop-tui`: terminal frontend for `ldropd` (SPEC §11.3), for window managers and minimal
//! setups. Talks only to the daemon, except settings that live in the config file.

mod app;
mod complete;
mod input;
mod runtime;
mod theme;
mod ui;

#[cfg(test)]
mod snapshots;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ldrop-tui", version, about = "Share files with nearby Android and Apple devices, in the terminal")]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start with these files picked; choose the device to send them to.
    Send {
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut preselected = Vec::new();
    if let Some(Cmd::Send { files }) = cli.command {
        for file in files {
            match std::path::absolute(&file) {
                Ok(path) if path.exists() => preselected.push(path),
                _ => {
                    eprintln!("ldrop-tui: {}: no such file or directory", file.display());
                    return ExitCode::from(2);
                }
            }
        }
    }
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        eprintln!("ldrop-tui: needs a terminal (use `ldrop` in scripts)");
        return ExitCode::from(2);
    }
    match runtime::run(preselected).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ldrop-tui: {e:#}");
            ExitCode::FAILURE
        }
    }
}
