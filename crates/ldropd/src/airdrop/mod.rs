// SPDX-License-Identifier: GPL-3.0-or-later
//! AirDrop (SPEC §6): receiving from Apple devices while visible to Everyone, and sending to
//! those that are discoverable. The Apple link (AWDL) is leased from Owly's radio service,
//! owlyd, while something needs it ([`link`]); owly-airdrop speaks the protocol on it
//! ([`receive`], [`send`]).

mod link;
mod receive;
mod send;

use std::collections::HashMap;
use std::net::Ipv6Addr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use ldrop_core::Visibility;
use ldrop_core::capabilities::{BackendId, Support};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zbus::Variant;

use crate::prompts::Prompts;
use crate::state::Shared;

pub use send::{is_airdrop_device, start_outgoing};

/// The Apple link as frontends see it, for the `AirDropLink` property: `state` (`down`,
/// `starting`, `up`), `interface` while up, and `wifi_paused` while the link keeps the card off
/// its network.
pub fn link_status(s: &Shared) -> HashMap<&'static str, Variant<'static>> {
    let mut status = HashMap::new();
    let Some(ad) = backend(s) else {
        status.insert("state", Variant::from("down"));
        return status;
    };
    let link = ad.link.subscribe().borrow().clone();
    let state = match (&link, *ad.link.wanted().borrow()) {
        (Some(_), _) => "up",
        (None, true) => "starting",
        (None, false) => "down",
    };
    status.insert("state", Variant::from(state));
    if let Some(link) = link {
        status.insert("interface", Variant::from(link.interface.clone()));
        status.insert("wifi_paused", Variant::from(link::exclusive(s)));
    }
    status
}

/// Tells frontends whenever [`link_status`] changes.
async fn follow_link_status(s: Arc<Shared>, ad: Arc<Backend>) {
    let mut link = ad.link.subscribe();
    let mut wanted = ad.link.wanted();
    loop {
        tokio::select! {
            changed = link.changed() => if changed.is_err() { return },
            changed = wanted.changed() => if changed.is_err() { return },
        }
        crate::manager::emit_airdrop_link(&s).await;
    }
}

/// How often to look again whether AirDrop became possible: owlyd installed, a card plugged in.
const RECHECK: Duration = Duration::from_secs(10);
/// How long the daemon waits on its way out for the receiver to say goodbye.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(1);

pub struct Backend {
    link: Arc<link::AppleLink>,
    prompts: Arc<Prompts<Ipv6Addr>>,
    /// Whether Apple devices should see us now.
    shown: watch::Sender<bool>,
    /// The receive session: runs while shown, and after that until its transfers are done.
    session: Mutex<Option<JoinHandle<()>>>,
    /// Discovery, while any session wants it.
    discovery: tokio::sync::Mutex<Option<send::DiscoveryRun>>,
    /// Where each discovered receiver listens, by device id.
    receivers: Mutex<HashMap<String, send::Route>>,
    /// AWDL peers given with `--airdrop-peer`, asked instead of owlyd's.
    peers: Vec<[u8; 6]>,
}

impl Backend {
    /// Starts a receive session unless one is running.
    fn ensure_session(self: &Arc<Self>, s: &Arc<Shared>) {
        let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
        if session.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        *session = Some(tokio::spawn(receive::session(s.clone(), self.clone())));
    }
}

fn backend(s: &Shared) -> Option<Arc<Backend>> {
    s.airdrop.get().cloned()
}

/// Where archives are built before they go out: not `/tmp`, which may be a small tmpfs.
fn spool_dir(s: &Shared) -> PathBuf {
    s.paths.state_dir.join("spool")
}

fn enabled(s: &Shared) -> bool {
    s.state().config.airdrop.enabled
}

/// Whether there can be an Apple link: owlyd drives the card in a mode the config allows.
fn usable(s: &Shared) -> Result<(), String> {
    if s.airdrop_interface.is_some() {
        return Ok(());
    }
    let st = s.state();
    let awdl = [BackendId::AwdlShared, BackendId::AwdlActivemon, BackendId::AwdlExclusive];
    let backends: Vec<_> = st.report.backends.iter().filter(|b| awdl.contains(&b.id)).collect();
    match backends.iter().find(|b| b.support != Support::Unsupported) {
        Some(b) if b.id == BackendId::AwdlExclusive && !st.config.airdrop.allow_exclusive_radio => {
            Err("the Wi-Fi card leaves its network for AirDrop; allow that with \
             `ldrop config set airdrop.allow_exclusive_radio true`"
                .into())
        }
        Some(_) => Ok(()),
        None if st.report_at.is_none() => Err("the hardware check has not run yet".into()),
        None => Err(backends.first().map_or_else(|| "no Wi-Fi card".to_owned(), |b| b.reason.clone())),
    }
}

/// Starts following visibility. Needs `airdrop.enabled` at startup.
pub async fn start(s: &Arc<Shared>) -> anyhow::Result<()> {
    if !enabled(s) {
        tracing::info!("AirDrop is disabled in the config");
        return Ok(());
    }
    // owly-airdrop's TLS needs a process-wide crypto provider.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let fixed = s.airdrop_interface.as_deref().map(link::fixed).transpose()?;
    if let Some(link) = &fixed {
        tracing::warn!("AirDrop: using {} ({}) as the Apple link, without owlyd", link.interface, link.address);
    }
    let peers = s
        .airdrop_peers
        .iter()
        .map(|mac| ldrop_airdrop::addr::parse_mac(mac).with_context(|| format!("`{mac}` is not a MAC address")))
        .collect::<anyhow::Result<_>>()?;
    // Left over from a daemon that did not get to clean up.
    let spool = spool_dir(s);
    if let Err(e) = std::fs::remove_dir_all(&spool)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("AirDrop: cannot clear {}: {e}", spool.display());
    }
    let ad = Arc::new(Backend {
        link: link::AppleLink::new(),
        prompts: Prompts::new(),
        shown: watch::channel(false).0,
        session: Mutex::new(None),
        discovery: tokio::sync::Mutex::new(None),
        receivers: Mutex::default(),
        peers,
    });
    if s.airdrop.set(ad.clone()).is_err() {
        anyhow::bail!("AirDrop backend started twice");
    }
    tokio::spawn(link::run(s.clone(), ad.link.clone(), fixed));
    tokio::spawn(follow_link_status(s.clone(), ad.clone()));
    tokio::spawn(follow_visibility(s.clone(), ad));
    Ok(())
}

/// Runs discovery exactly while a session wants it, AirDrop is on, and the card can do it.
pub async fn sync_discovery(s: Arc<Shared>) {
    let Some(ad) = backend(&s) else { return };
    let mut run = ad.discovery.lock().await;
    let wanted = !s.state().sessions.is_empty() && enabled(&s);
    let wanted =
        wanted && usable(&s).inspect_err(|why| tracing::info!("AirDrop: cannot look for Apple devices: {why}")).is_ok();
    match (wanted, run.take()) {
        (true, None) => *run = Some(send::DiscoveryRun::start(&s, &ad)),
        (false, Some(stopping)) => stopping.stop().await,
        (_, running) => *run = running,
    }
}

/// Hides us and gives the receive session a moment to withdraw the mDNS service.
pub async fn shutdown(s: &Shared) {
    let Some(ad) = backend(s) else { return };
    ad.shown.send_replace(false);
    let task = ad.session.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(task) = task {
        let _ = tokio::time::timeout(SHUTDOWN_WAIT, task).await;
    }
}

/// Shows us to Apple devices while visible to Everyone, AirDrop is on, and the card can do it.
async fn follow_visibility(s: Arc<Shared>, ad: Arc<Backend>) {
    let mut rx = s.visibility_tx.subscribe();
    let mut said: Option<String> = None;
    loop {
        let (mode, _) = *rx.borrow_and_update();
        let wanted = mode == Visibility::Everyone && enabled(&s);
        let shown = wanted
            && match usable(&s) {
                Ok(()) => true,
                Err(why) => {
                    if said.as_ref() != Some(&why) {
                        tracing::info!("AirDrop: Apple devices cannot see us: {why}");
                        said = Some(why);
                    }
                    false
                }
            };
        if shown || !wanted {
            said = None;
        }
        ad.shown.send_if_modified(|v| std::mem::replace(v, shown) != shown);
        if shown {
            ad.ensure_session(&s);
        }
        tokio::select! {
            changed = rx.changed() => if changed.is_err() { return },
            () = tokio::time::sleep(RECHECK) => {}
        }
    }
}
