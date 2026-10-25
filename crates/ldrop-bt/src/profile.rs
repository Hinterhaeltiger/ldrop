// SPDX-License-Identifier: GPL-3.0-or-later
//! An RFCOMM service through BlueZ's `ProfileManager1`: we export an `org.bluez.Profile1`
//! object, and BlueZ hands it the socket of every connection, incoming or outgoing, through
//! `NewConnection`.
//!
//! One registration does both directions: without a `Role`, BlueZ listens (on the channel it
//! picks for `Channel = 0`, published in an SDP record under the UUID) and connects through
//! `Device1.ConnectProfile`, which looks the channel up over SDP first. No pairing and no
//! authorization, like Nearby's insecure RFCOMM sockets.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use zbus::{Connection, ObjectPath, OwnedFd, OwnedObjectPath, OwnedVariant, Variant, interface, proxy};

use crate::{Adapter, Error, Result, device_address};

/// Connections waiting to be taken by [`Incoming::next`]; more are refused.
const INCOMING_QUEUE: usize = 4;
/// `ConnectProfile` covers paging and the SDP lookup; BlueZ gives up on its own well before.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(40);
/// BlueZ calls `NewConnection` before it answers `ConnectProfile`, so this is only a safety net.
const HANDOVER_TIMEOUT: Duration = Duration::from_secs(5);

#[proxy(interface = "org.bluez.ProfileManager1", default_service = "org.bluez", default_path = "/org/bluez")]
trait ProfileManager {
    fn register_profile(
        &self,
        profile: &ObjectPath<'_>,
        uuid: &str,
        options: HashMap<&str, Variant<'_>>,
    ) -> zbus::Result<()>;
    fn unregister_profile(&self, profile: &ObjectPath<'_>) -> zbus::Result<()>;
}

/// An RFCOMM connection. The socket is driven as a [`UnixStream`]: tokio only uses plain
/// `read`/`write`/`shutdown` on it, which behave the same on RFCOMM sockets.
#[derive(Debug)]
pub struct RfcommStream {
    /// The peer's address, `AA:BB:CC:DD:EE:FF`.
    pub address: String,
    pub stream: UnixStream,
}

#[derive(Default)]
struct Routes {
    /// Outgoing connections in progress, by device path.
    outgoing: HashMap<OwnedObjectPath, oneshot::Sender<UnixStream>>,
}

struct ProfileIface {
    uuid: String,
    routes: Arc<Mutex<Routes>>,
    incoming: mpsc::Sender<RfcommStream>,
}

#[interface(name = "org.bluez.Profile1")]
impl ProfileIface {
    /// BlueZ dropped the profile, for example because bluetoothd stopped.
    fn release(&self) {
        tracing::info!("Bluetooth: BlueZ released the {} profile", self.uuid);
    }

    fn new_connection(
        &self,
        device: OwnedObjectPath,
        fd: OwnedFd,
        _properties: HashMap<String, OwnedVariant>,
    ) -> zbus::fdo::Result<()> {
        let stream = stream(fd).map_err(|e| zbus::fdo::Error::Failed(format!("unusable socket: {e}")))?;
        let waiter = self.routes.lock().unwrap_or_else(|e| e.into_inner()).outgoing.remove(&device);
        if let Some(waiter) = waiter {
            tracing::debug!("Bluetooth: connected to {device}");
            // The caller may have given up already; the socket closes with the stream then.
            let _ = waiter.send(stream);
            return Ok(());
        }
        let address = device_address(&device).unwrap_or_default();
        tracing::debug!("Bluetooth: incoming connection from {address}");
        self.incoming.try_send(RfcommStream { address, stream }).map_err(|e| {
            let why = match e {
                mpsc::error::TrySendError::Full(_) => "too many connections",
                mpsc::error::TrySendError::Closed(_) => "not accepting connections",
            };
            tracing::debug!("Bluetooth: refusing connection from {device}: {why}");
            zbus::fdo::Error::Failed(why.into())
        })
    }

    fn request_disconnection(&self, device: OwnedObjectPath) {
        // The socket belongs to a transfer; it ends when the transfer closes it.
        tracing::debug!("Bluetooth: BlueZ asked to disconnect {device}");
    }
}

fn stream(fd: OwnedFd) -> std::io::Result<UnixStream> {
    let std = std::os::unix::net::UnixStream::from(std::os::fd::OwnedFd::from(fd));
    std.set_nonblocking(true)?;
    UnixStream::from_std(std)
}

/// Keeps the profile registered; unregistered (in the background) when the last handle goes.
struct Registration {
    conn: Connection,
    path: OwnedObjectPath,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let (conn, path) = (self.conn.clone(), self.path.clone());
        rt.spawn(async move {
            if let Ok(manager) = ProfileManagerProxy::new(&conn).await
                && let Err(e) = manager.unregister_profile(&path).await
            {
                tracing::debug!("Bluetooth: cannot unregister {path}: {e}");
            }
            let _ = conn.object_server().remove::<ProfileIface, _>(&path).await;
        });
    }
}

/// Opens outgoing connections. Cheap to clone.
#[derive(Clone)]
pub struct Rfcomm {
    adapter: Adapter,
    uuid: String,
    routes: Arc<Mutex<Routes>>,
    _registration: Arc<Registration>,
}

/// Incoming connections, in arrival order.
pub struct Incoming {
    rx: mpsc::Receiver<RfcommStream>,
    _registration: Arc<Registration>,
}

impl Incoming {
    pub async fn next(&mut self) -> Option<RfcommStream> {
        self.rx.recv().await
    }
}

impl Rfcomm {
    /// Registers the service `uuid` under `name` (shown in its SDP record).
    pub async fn register(adapter: &Adapter, uuid: &str, name: &str) -> Result<(Rfcomm, Incoming)> {
        static NEXT_ID: AtomicU32 = AtomicU32::new(0);

        let conn = adapter.conn.clone();
        let path = OwnedObjectPath::try_from(format!(
            "/org/ldrop/bt/profile{}_{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ))?;
        let routes = Arc::new(Mutex::new(Routes::default()));
        let (tx, rx) = mpsc::channel(INCOMING_QUEUE);
        conn.object_server()
            .at(&path, ProfileIface { uuid: uuid.to_owned(), routes: routes.clone(), incoming: tx })
            .await?;

        let options = HashMap::from([
            ("Name", Variant::from(name)),
            ("Channel", Variant::from(0u16)),
            ("RequireAuthentication", Variant::from(false)),
            ("RequireAuthorization", Variant::from(false)),
            ("AutoConnect", Variant::from(false)),
        ]);
        let registered = async { ProfileManagerProxy::new(&conn).await?.register_profile(&path, uuid, options).await };
        if let Err(e) = registered.await {
            let _ = conn.object_server().remove::<ProfileIface, _>(&path).await;
            return Err(e.into());
        }
        tracing::debug!("Bluetooth: RFCOMM service {uuid} registered");
        let registration = Arc::new(Registration { conn, path });
        let rfcomm =
            Rfcomm { adapter: adapter.clone(), uuid: uuid.to_owned(), routes, _registration: registration.clone() };
        Ok((rfcomm, Incoming { rx, _registration: registration }))
    }

    /// Connects to the service on `address`. The device must be known to BlueZ, which it is
    /// while discovery reports it.
    pub async fn connect(&self, address: &str) -> Result<RfcommStream> {
        self.adapter.ensure_device(address).await?;
        let (path, device) = self.adapter.device(address).await?;
        let (tx, rx) = oneshot::channel();
        let routes = self.routes.clone();
        routes.lock().unwrap_or_else(|e| e.into_inner()).outgoing.insert(path.clone(), tx);
        let _waiting = scopeguard(move || {
            routes.lock().unwrap_or_else(|e| e.into_inner()).outgoing.remove(&path);
        });

        match tokio::time::timeout(CONNECT_TIMEOUT, device.connect_profile(&self.uuid)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                let _ = device.disconnect_profile(&self.uuid).await;
                return Err(Error::Failed(format!("{address} did not answer")));
            }
        }
        match tokio::time::timeout(HANDOVER_TIMEOUT, rx).await {
            Ok(Ok(stream)) => Ok(RfcommStream { address: address.to_owned(), stream }),
            _ => Err(Error::Failed("BlueZ connected but handed over no socket".into())),
        }
    }
}

/// Runs `f` when dropped.
fn scopeguard(f: impl FnOnce()) -> impl Drop {
    struct Guard<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for Guard<F> {
        fn drop(&mut self) {
            if let Some(f) = self.0.take() {
                f();
            }
        }
    }
    Guard(Some(f))
}
