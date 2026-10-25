// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::IsTerminal;

use clap::Parser;
use ldrop_core::paths::Paths;
use tracing_subscriber::EnvFilter;

/// ldrop user daemon: Quick Share and AirDrop for Linux (org.ldrop.Ldrop1).
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Simulated nearby devices and the org.ldrop.Ldrop1.Debug interface, for developing
    /// frontends without phones.
    #[arg(long)]
    dev_sim: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let fmt = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("LDROP_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal());
    // journald adds its own timestamps.
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        fmt.without_time().init();
    } else {
        fmt.init();
    }
    ldropd::run(ldropd::Options { paths: Paths::from_env(), sim: args.dev_sim, probe_hardware: true, network: true })
        .await
}
