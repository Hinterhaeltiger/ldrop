// SPDX-License-Identifier: GPL-3.0-or-later
//! Desktop notifications (`org.freedesktop.Notifications`) with their actions routed back to
//! whoever posted them, plus KDE's job progress view (`org.kde.JobViewServer`), which Plasma
//! shows as a notification with a progress bar.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use futures_util::StreamExt;
use tokio::sync::mpsc;
use zbus::names::BusName;
use zbus::{Connection, OwnedObjectPath, Variant, proxy};

pub const DESKTOP_ENTRY: &str = "org.ldrop.Ldrop";
const APP_NAME: &str = "ldrop";
const JOB_VIEW_SERVER: &str = "org.kde.JobViewServer";
/// `KJob::Killable`: the view gets a Cancel button.
const KILLABLE: i32 = 1;

#[proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Variant<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;

    fn get_capabilities(&self) -> zbus::Result<Vec<String>>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.kde.JobViewServerV2",
    default_service = "org.kde.JobViewServer",
    default_path = "/JobViewServer"
)]
trait JobViewServer {
    // KDE's job interfaces use camelCase names.
    #[zbus(name = "requestView")]
    fn request_view(
        &self,
        desktop_entry: &str,
        capabilities: i32,
        hints: HashMap<&str, Variant<'_>>,
    ) -> zbus::Result<OwnedObjectPath>;
}

#[proxy(interface = "org.kde.JobViewV3", default_service = "org.kde.JobViewServer")]
pub trait JobView {
    #[zbus(name = "update")]
    fn update(&self, properties: HashMap<&str, Variant<'_>>) -> zbus::Result<()>;
    #[zbus(name = "terminate")]
    fn terminate(&self, error_code: u32, error_message: &str, hints: HashMap<&str, Variant<'_>>) -> zbus::Result<()>;

    #[zbus(signal, name = "cancelRequested")]
    fn cancel_requested(&self) -> zbus::Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Normal = 1,
    Critical = 2,
}

/// One notification to post.
#[derive(Debug, Clone)]
pub struct Notification {
    pub summary: String,
    pub body: String,
    pub icon: &'static str,
    /// `(key, label)`; the key `default` is the click on the notification itself.
    pub actions: Vec<(&'static str, &'static str)>,
    pub urgency: Urgency,
    /// Freedesktop category: `transfer`, `transfer.complete`, `transfer.error`.
    pub category: &'static str,
    /// Progress in percent, for servers that draw a bar (the `value` hint).
    pub progress: Option<u8>,
    /// Milliseconds; 0 stays until dismissed, -1 is the server's default.
    pub timeout_ms: i32,
    /// A picture to show, such as the preview a sender attached. A server that cannot read it
    /// shows the icon.
    pub image: Option<std::path::PathBuf>,
}

impl Notification {
    pub fn new(summary: impl Into<String>, body: impl Into<String>) -> Self {
        Notification {
            summary: summary.into(),
            body: body.into(),
            icon: "document-share",
            actions: Vec::new(),
            urgency: Urgency::Normal,
            category: "transfer",
            progress: None,
            timeout_ms: -1,
            image: None,
        }
    }
}

/// Text as body markup shows it: peer and file names must not become tags or links.
fn escape_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// What happened to a posted notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Action(String),
    /// Closed with the spec's reason: 1 expired, 2 dismissed by the user, 3 closed by us.
    Closed(u32),
}

pub const DISMISSED: u32 = 2;

type Routes = Arc<Mutex<HashMap<u32, mpsc::UnboundedSender<Reply>>>>;

fn lock(routes: &Routes) -> MutexGuard<'_, HashMap<u32, mpsc::UnboundedSender<Reply>>> {
    routes.lock().unwrap_or_else(|e| e.into_inner())
}

/// Posts notifications and routes their actions back.
#[derive(Clone)]
pub struct Notifier {
    conn: Connection,
    proxy: NotificationsProxy<'static>,
    routes: Routes,
    actions: bool,
    /// The server reads the body as markup, so text from peers must be escaped.
    markup: bool,
}

impl Notifier {
    /// Fails when there is no notification server.
    pub async fn new(conn: &Connection) -> zbus::Result<Notifier> {
        let proxy = NotificationsProxy::new(conn).await?;
        let capabilities = proxy.get_capabilities().await?;
        let mut invoked = proxy.receive_action_invoked().await?;
        let mut closed = proxy.receive_notification_closed().await?;
        let routes = Routes::default();
        let dispatch = routes.clone();
        tokio::spawn(async move {
            loop {
                let (id, reply) = tokio::select! {
                    Some(signal) = invoked.next() => {
                        let Ok(args) = signal.args() else { continue };
                        (*args.id(), Reply::Action(args.action_key().clone()))
                    }
                    Some(signal) = closed.next() => {
                        let Ok(args) = signal.args() else { continue };
                        (*args.id(), Reply::Closed(*args.reason()))
                    }
                    else => return,
                };
                let mut routes = lock(&dispatch);
                let closing = matches!(reply, Reply::Closed(_));
                if let Some(tx) = routes.get(&id)
                    && (tx.send(reply).is_err() || closing)
                {
                    routes.remove(&id);
                }
            }
        });
        let has = |name: &str| capabilities.iter().any(|c| c == name);
        Ok(Notifier { conn: conn.clone(), proxy, routes, actions: has("actions"), markup: has("body-markup") })
    }

    /// Whether the server shows action buttons.
    pub fn has_actions(&self) -> bool {
        self.actions
    }

    /// Posts `n`, replacing notification `replaces` (0 for a new one). Its actions and its
    /// closing go to `replies`, until it closes.
    pub async fn show(
        &self,
        n: &Notification,
        replaces: u32,
        replies: mpsc::UnboundedSender<Reply>,
    ) -> zbus::Result<u32> {
        let actions: Vec<&str> = n.actions.iter().flat_map(|(key, label)| [*key, *label]).collect();
        let mut hints = HashMap::from([
            ("urgency", Variant::from(n.urgency as u8)),
            ("category", Variant::from(n.category)),
            ("desktop-entry", Variant::from(DESKTOP_ENTRY)),
        ]);
        if let Some(value) = n.progress {
            hints.insert("value", Variant::from(i32::from(value.min(100))));
        }
        if let Some(image) = &n.image {
            hints.insert("image-path", Variant::from(ldrop_core::uri::path_to_file_uri(image)));
        }
        let body = if self.markup { escape_markup(&n.body) } else { n.body.clone() };
        let id =
            self.proxy.notify(APP_NAME, replaces, n.icon, &n.summary, &body, &actions, hints, n.timeout_ms).await?;
        lock(&self.routes).insert(id, replies);
        Ok(id)
    }

    pub async fn close(&self, id: u32) {
        lock(&self.routes).remove(&id);
        let _ = self.proxy.close_notification(id).await;
    }

    /// A Plasma progress view, `None` where there is no job view server.
    pub async fn job_view(&self, hints: HashMap<&str, Variant<'_>>) -> Option<JobViewProxy<'static>> {
        let dbus = zbus::fdo::DBusProxy::new(&self.conn).await.ok()?;
        let name = BusName::from_static_str(JOB_VIEW_SERVER).ok()?;
        if !dbus.name_has_owner(name).await.ok()? {
            return None;
        }
        let path = JobViewServerProxy::new(&self.conn).await.ok()?.request_view(DESKTOP_ENTRY, KILLABLE, hints).await;
        let path = match path {
            Ok(path) => path,
            Err(e) => {
                tracing::debug!("no job view: {e}");
                return None;
            }
        };
        JobViewProxy::builder(&self.conn).path(path).build().await.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_is_escaped() {
        assert_eq!(
            escape_markup("<b>Tom & Jerry's</b> \"x\""),
            "&lt;b&gt;Tom &amp; Jerry&apos;s&lt;/b&gt; &quot;x&quot;"
        );
        assert_eq!(escape_markup("plain"), "plain");
    }
}
