// SPDX-License-Identifier: GPL-3.0-or-later
//! The Apple link (AWDL), leased from owlyd while something needs it: the receive wake, a
//! discovery session, a transfer. One lease serves all of them. It is taken when the first
//! asks and given back [`LINGER`] after the last is done, so a share sheet opened twice in a
//! row does not bring the link up twice.

use std::net::Ipv6Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use ldrop_core::{Protocol, TransferState};
use ldrop_wifi::owly::{self, Link, Owly};
use nix::ifaddrs::getifaddrs;
use tokio::sync::watch;

use crate::radio::{Claim, Use};
use crate::state::Shared;

/// How long the link stays up after the last user is done.
const LINGER: Duration = Duration::from_secs(10);
/// owlyd may have to bring the card into another mode first.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);
/// After a failed `Acquire` or a lost lease, doubling up to [`MAX_RETRY`].
const FIRST_RETRY: Duration = Duration::from_secs(2);
const MAX_RETRY: Duration = Duration::from_secs(60);
/// How often to look again while Quick Share has the card.
const RADIO_POLL: Duration = Duration::from_secs(2);

pub struct AppleLink {
    users: Mutex<usize>,
    want: watch::Sender<bool>,
    link: watch::Sender<Option<Arc<Link>>>,
}

/// One user of the link. The link goes down [`LINGER`] after the last one is dropped.
pub struct LinkUse {
    apple: Arc<AppleLink>,
}

impl AppleLink {
    pub fn new() -> Arc<Self> {
        Arc::new(AppleLink { users: Mutex::new(0), want: watch::channel(false).0, link: watch::channel(None).0 })
    }

    pub fn hold(self: &Arc<Self>) -> LinkUse {
        let mut users = self.users.lock().unwrap_or_else(|e| e.into_inner());
        *users += 1;
        if *users == 1 {
            self.want.send_replace(true);
        }
        LinkUse { apple: self.clone() }
    }

    /// The link while it is up, `None` while it is down.
    pub fn subscribe(&self) -> watch::Receiver<Option<Arc<Link>>> {
        self.link.subscribe()
    }

    /// Whether anything needs the link, up yet or not.
    pub fn wanted(&self) -> watch::Receiver<bool> {
        self.want.subscribe()
    }
}

impl Drop for LinkUse {
    fn drop(&mut self) {
        let mut users = self.apple.users.lock().unwrap_or_else(|e| e.into_inner());
        *users -= 1;
        if *users == 0 {
            self.apple.want.send_replace(false);
        }
    }
}

/// A link that is always there: an existing interface, for tests on a veth pair.
pub fn fixed(interface: &str) -> anyhow::Result<Link> {
    let index = nix::net::if_::if_nametoindex(interface).with_context(|| format!("no interface {interface}"))?;
    let addrs: Vec<_> =
        getifaddrs()?.filter(|ifa| ifa.interface_name == interface).filter_map(|ifa| ifa.address).collect();
    let mac = addrs
        .iter()
        .find_map(|a| a.as_link_addr()?.addr())
        .with_context(|| format!("{interface} has no MAC address"))?;
    let address = addrs
        .iter()
        .filter_map(|a| a.as_sockaddr_in6().map(|a| a.ip()))
        .find(Ipv6Addr::is_unicast_link_local)
        .with_context(|| format!("{interface} has no link-local IPv6 address"))?;
    let mac = mac.map(|b| format!("{b:02x}")).join(":");
    Ok(Link { interface: interface.to_owned(), index, mac, address, announce: None })
}

/// Keeps the link up exactly while it is wanted, plus [`LINGER`]. Runs for the daemon's life.
pub async fn run(s: Arc<Shared>, apple: Arc<AppleLink>, fixed: Option<Link>) {
    let mut want = apple.want.subscribe();
    let mut retry = FIRST_RETRY;
    loop {
        if want.wait_for(|w| *w).await.is_err() {
            return;
        }
        let Some(claim) = claim_radio(&s, &mut want).await else { continue };
        if let Some(link) = &fixed {
            apple.link.send_replace(Some(Arc::new(link.clone())));
            linger_out(&mut want).await;
            apple.link.send_replace(None);
            continue;
        }
        let acquired = match tokio::time::timeout(ACQUIRE_TIMEOUT, acquire()).await {
            Ok(acquired) => acquired,
            Err(_) => Err(owly::Error::Other("owlyd did not answer in time".into())),
        };
        let mut lease = match acquired {
            Ok(lease) => lease,
            Err(e) => {
                tracing::warn!("AirDrop: cannot bring up the Apple link: {}", explain(&e));
                drop(claim);
                back_off(&mut want, &mut retry).await;
                continue;
            }
        };
        let link = lease.link.clone();
        tracing::info!("AirDrop: Apple link up on {} ({})", link.interface, link.address);
        apple.link.send_replace(Some(Arc::new(link)));
        let ended = tokio::select! {
            why = lease.ended() => Some(why),
            () = linger_out(&mut want) => None,
        };
        apple.link.send_replace(None);
        match ended {
            Some(why) => {
                tracing::warn!("AirDrop: owlyd ended the Apple link: {why}");
                drop((lease, claim));
                back_off(&mut want, &mut retry).await;
            }
            None => {
                lease.release().await;
                drop(claim);
                retry = FIRST_RETRY;
                tracing::info!("AirDrop: Apple link down");
            }
        }
    }
}

async fn acquire() -> owly::Result<owly::Lease> {
    Owly::system().await?.acquire().await
}

fn explain(e: &owly::Error) -> String {
    match e {
        owly::Error::NotAuthorized(_) => format!(
            "{e}; owlyd lets programs in the active local session in, so over ssh add a polkit rule for \
             io.github.t4t5.Owly.acquire"
        ),
        _ => e.to_string(),
    }
}

/// The card for the link. Waits while Quick Share has it, which includes, when the link would
/// take Wi-Fi down, any Quick Share transfer. `None` once the link is no longer wanted.
async fn claim_radio(s: &Shared, want: &mut watch::Receiver<bool>) -> Option<Claim> {
    let mut said = false;
    loop {
        let busy = exclusive(s) && quickshare_busy(s);
        if let Some(claim) = (!busy).then(|| s.radio.try_claim(Use::AppleLink)).flatten() {
            return Some(claim);
        }
        if !said {
            tracing::info!("AirDrop: waiting for Quick Share to be done with Wi-Fi");
            said = true;
        }
        tokio::select! {
            () = tokio::time::sleep(RADIO_POLL) => {}
            _ = want.wait_for(|w| !*w) => return None,
        }
    }
}

/// Whether owlyd takes the card off its network while the link is up.
pub(super) fn exclusive(s: &Shared) -> bool {
    s.airdrop_interface.is_none() && s.state().report.owly.mode.as_deref() == Some("exclusive")
}

fn quickshare_busy(s: &Shared) -> bool {
    s.state().transfers.values().any(|r| {
        r.props.peer.protocol == Protocol::QuickShare
            && !r.props.state.is_final()
            && r.props.state != TransferState::AwaitingAcceptance
    })
}

/// Returns once nobody has wanted the link for [`LINGER`].
async fn linger_out(want: &mut watch::Receiver<bool>) {
    loop {
        if want.wait_for(|w| !*w).await.is_err() {
            return;
        }
        tokio::select! {
            () = tokio::time::sleep(LINGER) => return,
            _ = want.wait_for(|w| *w) => {}
        }
    }
}

/// Waits `retry`, or less if the link stops being wanted, and doubles it for next time.
async fn back_off(want: &mut watch::Receiver<bool>, retry: &mut Duration) {
    tokio::select! {
        () = tokio::time::sleep(*retry) => {}
        _ = want.wait_for(|w| !*w) => {}
    }
    *retry = (*retry * 2).min(MAX_RETRY);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn lingers_after_the_last_user() {
        let apple = AppleLink::new();
        let mut want = apple.want.subscribe();
        let first = apple.hold();
        let second = apple.hold();
        assert!(*want.borrow_and_update());
        drop(first);
        assert!(*want.borrow(), "still one user");
        drop(second);
        assert!(!*want.borrow());

        let start = tokio::time::Instant::now();
        let again = {
            let apple = apple.clone();
            tokio::spawn(async move {
                tokio::time::sleep(LINGER / 2).await;
                let held = apple.hold();
                tokio::time::sleep(LINGER / 2).await;
                drop(held);
            })
        };
        linger_out(&mut want).await;
        again.await.unwrap();
        assert!(start.elapsed() >= LINGER * 2, "a new user restarts the linger");
    }
}
