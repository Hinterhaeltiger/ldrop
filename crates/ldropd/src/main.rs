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
    /// Use this network interface as the Apple link instead of asking owlyd, with no BLE wake:
    /// for developing AirDrop on a veth pair. Needs `airdrop.enabled`.
    #[arg(long, value_name = "INTERFACE")]
    airdrop_interface: Option<String>,
    /// Look for AirDrop receivers at this MAC address's link-local address, as if owlyd had
    /// heard it on AWDL. Repeatable; for development with `--airdrop-interface`.
    #[arg(long = "airdrop-peer", value_name = "MAC")]
    airdrop_peers: Vec<String>,
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
    ldropd::run(ldropd::Options {
        paths: Paths::from_env(),
        sim: args.dev_sim,
        probe_hardware: true,
        network: true,
        airdrop_interface: args.airdrop_interface,
        airdrop_peers: args.airdrop_peers,
    })
    .await
}
