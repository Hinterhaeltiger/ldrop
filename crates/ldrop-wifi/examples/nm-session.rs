// SPDX-License-Identifier: GPL-3.0-or-later
//! Manual test for the temporary networks: `nm-session host SSID PASSWORD SECONDS` or
//! `nm-session join SSID PASSWORD SECONDS`. Holds the network that long, then restores.

use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter("debug").init();
    let args: Vec<String> = std::env::args().collect();
    let [_, mode, ssid, password, seconds] = args.as_slice() else {
        return Err("usage: nm-session host|join SSID PASSWORD SECONDS".into());
    };
    let wifi = ldrop_wifi::nm::wifi_device().await?.ok_or("no Wi-Fi device")?;
    println!("device {} can_host={} connected={}", wifi.interface, wifi.can_host, wifi.connected);
    let session = match mode.as_str() {
        "host" => wifi.host(ssid, password).await?,
        "join" => wifi.join(ssid, password).await?,
        _ => return Err("mode is host or join".into()),
    };
    println!("up: address {} gateway {:?}", session.address, session.gateway);
    tokio::time::sleep(Duration::from_secs(seconds.parse()?)).await;
    drop(session);
    // Let the restore run.
    tokio::time::sleep(Duration::from_secs(3)).await;
    println!("restored");
    Ok(())
}
