// SPDX-License-Identifier: GPL-3.0-or-later
//! `ldrop`: command-line frontend for `ldropd` (SPEC §11.2). Talks only to the daemon, except
//! `config`, which edits the config file the daemon watches.

mod commands;
mod doctor;
mod fmt;
mod receive;
mod send;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use ldrop_client::Error as ClientError;

#[derive(Parser)]
#[command(name = "ldrop", version, about = "Share files with nearby Android and Apple devices")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check what this computer's hardware supports.
    Doctor {
        #[arg(long)]
        json: bool,
        /// Try the Apple link on the Wi-Fi card (needs ldrop-netd, milestone M6).
        #[arg(long)]
        card_test: bool,
    },
    /// Show device name, visibility and active transfers.
    Status {
        /// Waybar-compatible JSON (text, class, tooltip, plus all fields).
        #[arg(long)]
        json: bool,
    },
    /// Let everyone nearby see this computer.
    Visible {
        /// How long, e.g. 90s, 10m, 1h (default from config: 10m).
        #[arg(long = "for", value_name = "DURATION", value_parser = parse_duration)]
        duration: Option<std::time::Duration>,
    },
    /// Stop being visible.
    Hidden,
    /// List nearby devices.
    Devices {
        /// Keep listing until interrupted.
        #[arg(long)]
        watch: bool,
        /// How long to search.
        #[arg(long, value_name = "DURATION", default_value = "10s", value_parser = parse_duration)]
        timeout: std::time::Duration,
        #[arg(long)]
        json: bool,
    },
    /// Send files, text or a link.
    Send(SendArgs),
    /// Answer incoming requests in this terminal.
    Receive,
    /// List transfers and recent history.
    Transfers {
        #[arg(long)]
        watch: bool,
        #[arg(long)]
        json: bool,
    },
    /// Accept an incoming transfer.
    Accept { transfer: String },
    /// Decline an incoming transfer.
    Reject { transfer: String },
    /// Cancel a transfer.
    Cancel { transfer: String },
    /// Clear the transfer history.
    ClearHistory,
    /// Open the terminal interface (`ldrop-tui`).
    Tui {
        /// Passed on, e.g. `send FILE…`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Read or change settings (`~/.config/ldrop/config.toml`).
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Args)]
pub struct SendArgs {
    /// Device name or id; asks interactively when omitted.
    #[arg(long, value_name = "DEVICE")]
    pub to: Option<String>,
    /// Send this text instead of files.
    #[arg(long, conflicts_with_all = ["url", "files"])]
    pub text: Option<String>,
    /// Send this link instead of files.
    #[arg(long, conflicts_with_all = ["text", "files"])]
    pub url: Option<String>,
    /// How long to look for the device.
    #[arg(long, value_name = "DURATION", default_value = "15s", value_parser = parse_duration)]
    pub timeout: std::time::Duration,
    #[arg(required_unless_present_any = ["text", "url"])]
    pub files: Vec<PathBuf>,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print one key, or the whole effective config.
    Get { key: Option<String> },
    /// Set a key; the daemon picks the change up.
    Set { key: String, value: String },
    /// Print the config file location.
    Path,
}

fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    ldrop_core::time::parse_duration(s).ok_or_else(|| format!("invalid duration `{s}` (try 90s, 10m, 1h)"))
}

/// SPEC §11.2 exit codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Ok = 0,
    Error = 1,
    Usage = 2,
    Declined = 3,
    Cancelled = 4,
    Unavailable = 5,
}

/// An error that ends the command with a specific exit code.
#[derive(Debug)]
pub struct Fail {
    pub code: Exit,
    pub message: String,
}

impl Fail {
    pub fn new(code: Exit, message: impl Into<String>) -> Self {
        Fail { code, message: message.into() }
    }
}

impl From<ClientError> for Fail {
    fn from(e: ClientError) -> Self {
        if e.is_daemon_missing() {
            return Fail::new(
                Exit::Error,
                "ldropd is not running and could not be started (try `systemctl --user start ldropd`)",
            );
        }
        let code = match e {
            ClientError::Unavailable(_) | ClientError::NotSupported(_) => Exit::Unavailable,
            _ => Exit::Error,
        };
        Fail::new(code, e.message())
    }
}

impl From<std::io::Error> for Fail {
    fn from(e: std::io::Error) -> Self {
        Fail::new(Exit::Error, e.to_string())
    }
}

impl From<ldrop_core::config::ConfigError> for Fail {
    fn from(e: ldrop_core::config::ConfigError) -> Self {
        Fail::new(Exit::Error, e.to_string())
    }
}

pub type CmdResult = Result<Exit, Fail>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("LDROP_LOG").unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();

    let cli = Cli::parse();
    let result = match cli.command {
        Command::Doctor { json, card_test } => doctor::run(json, card_test).await,
        Command::Status { json } => commands::status(json).await,
        Command::Visible { duration } => commands::visible(duration).await,
        Command::Hidden => commands::hidden().await,
        Command::Devices { watch, timeout, json } => commands::devices(watch, timeout, json).await,
        Command::Send(args) => send::run(args).await,
        Command::Receive => receive::run().await,
        Command::Transfers { watch, json } => commands::transfers(watch, json).await,
        Command::Accept { transfer } => commands::decide(&transfer, commands::Verb::Accept).await,
        Command::Reject { transfer } => commands::decide(&transfer, commands::Verb::Reject).await,
        Command::Cancel { transfer } => commands::decide(&transfer, commands::Verb::Cancel).await,
        Command::ClearHistory => commands::clear_history().await,
        Command::Tui { args } => commands::tui(&args),
        Command::Config { action } => match action {
            ConfigAction::Get { key } => commands::config_get(key.as_deref()),
            ConfigAction::Set { key, value } => commands::config_set(&key, &value),
            ConfigAction::Path => commands::config_path(),
        },
    };
    match result {
        Ok(code) => ExitCode::from(code as u8),
        Err(fail) => {
            eprintln!("ldrop: {}", fail.message);
            ExitCode::from(fail.code as u8)
        }
    }
}
