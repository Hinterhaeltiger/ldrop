// SPDX-License-Identifier: GPL-3.0-or-later
//! The tray icon (SPEC §11.4): a StatusNotifierItem with a `com.canonical.dbusmenu` menu,
//! served over zbus [K8]. The tray host (Plasma, Waybar, GNOME's AppIndicator extension, …)
//! draws both, so they follow the desktop theme without a toolkit on our side.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use futures_util::StreamExt;
use ldrop_core::Visibility;
use serde::Serialize;
use tokio::sync::mpsc;
use zbus::fdo::DBusProxy;
use zbus::object_server::SignalEmitter;
use zbus::wire::StructureBuilder;
use zbus::{Connection, ObjectPath, OwnedVariant, Type, Variant, interface, proxy};

const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";

/// What the tray shows; built from the app state by `app::view`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct View {
    /// Freedesktop icon name.
    pub icon: &'static str,
    /// Tooltip text and first (inactive) menu entry, e.g. "Visible to everyone · 7 min left".
    pub status: String,
    /// `None` while ldropd is unreachable: the visibility entries are disabled then.
    pub visibility: Option<Visibility>,
    /// A request waits for an answer.
    pub attention: bool,
}

/// A menu entry or click the app acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    SetVisibility(Visibility),
    /// Middle click on the icon.
    ToggleVisibility,
    OpenDownloads,
    Quit,
}

#[proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher"
)]
trait Watcher {
    fn register_status_notifier_item(&self, service: &str) -> zbus::Result<()>;
}

type Shared = Arc<Mutex<View>>;

fn lock(view: &Shared) -> MutexGuard<'_, View> {
    view.lock().unwrap_or_else(|e| e.into_inner())
}

/// `a(iiay)`: icons as ARGB32 pixmaps. We only use theme icon names.
type Pixmaps = Vec<(i32, i32, Vec<u8>)>;

struct ItemIface {
    view: Shared,
    actions: mpsc::UnboundedSender<Action>,
}

#[interface(name = "org.kde.StatusNotifierItem")]
impl ItemIface {
    #[zbus(property)]
    fn category(&self) -> &str {
        "Communications"
    }

    #[zbus(property)]
    fn id(&self) -> &str {
        "ldrop"
    }

    #[zbus(property)]
    fn title(&self) -> &str {
        "ldrop"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        if lock(&self.view).attention { "NeedsAttention" } else { "Active" }
    }

    #[zbus(property)]
    fn window_id(&self) -> i32 {
        0
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn menu(&self) -> ObjectPath<'static> {
        ObjectPath::from_static_str_unchecked(MENU_PATH)
    }

    /// There is no window to show, so a click opens the menu.
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn icon_name(&self) -> String {
        lock(&self.view).icon.to_owned()
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Pixmaps {
        Vec::new()
    }

    #[zbus(property)]
    fn overlay_icon_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn overlay_icon_pixmap(&self) -> Pixmaps {
        Vec::new()
    }

    #[zbus(property)]
    fn attention_icon_name(&self) -> String {
        lock(&self.view).icon.to_owned()
    }

    #[zbus(property)]
    fn attention_icon_pixmap(&self) -> Pixmaps {
        Vec::new()
    }

    #[zbus(property)]
    fn attention_movie_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn tool_tip(&self) -> (String, Pixmaps, String, String) {
        let view = lock(&self.view);
        (view.icon.to_owned(), Vec::new(), "ldrop".to_owned(), view.status.clone())
    }

    fn provide_xdg_activation_token(&self, _token: String) {}

    /// Hosts show [`MenuIface`] themselves.
    fn context_menu(&self, _x: i32, _y: i32) {}

    fn activate(&self, _x: i32, _y: i32) {}

    fn secondary_activate(&self, _x: i32, _y: i32) {
        let _ = self.actions.send(Action::ToggleVisibility);
    }

    fn scroll(&self, _delta: i32, _orientation: String) {}

    #[zbus(signal)]
    async fn new_icon(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_attention_icon(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_tool_tip(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_status(emitter: &SignalEmitter<'_>, status: &str) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------------------------
// Menu

const ROOT: i32 = 0;
const STATUS: i32 = 1;
const HIDDEN: i32 = 3;
const EVERYONE: i32 = 4;
const DOWNLOADS: i32 = 6;
const QUIT: i32 = 8;

#[derive(Debug, Clone, PartialEq)]
enum Entry {
    Separator,
    Label(String),
    Radio { label: &'static str, checked: bool, enabled: bool },
    Button(&'static str),
}

/// The menu below the root, as `(id, entry)`.
fn entries(view: &View) -> Vec<(i32, Entry)> {
    let radio = |label, mode| Entry::Radio {
        label,
        checked: view.visibility == Some(mode),
        enabled: view.visibility.is_some(),
    };
    vec![
        (STATUS, Entry::Label(view.status.clone())),
        (2, Entry::Separator),
        (HIDDEN, radio("_Hidden", Visibility::Hidden)),
        (EVERYONE, radio("Visible to _everyone", Visibility::Everyone)),
        (5, Entry::Separator),
        (DOWNLOADS, Entry::Button("Open _download folder")),
        (7, Entry::Separator),
        (QUIT, Entry::Button("_Quit")),
    ]
}

fn properties(entry: Option<&Entry>, wanted: &[String]) -> HashMap<String, OwnedVariant> {
    let mut props: Vec<(&str, Variant<'static>)> = match entry {
        None => vec![("children-display", Variant::from("submenu"))],
        Some(Entry::Separator) => vec![("type", Variant::from("separator"))],
        // dbusmenu labels use `_` for mnemonics.
        Some(Entry::Label(text)) => {
            vec![("label", Variant::from(text.replace('_', "__"))), ("enabled", Variant::from(false))]
        }
        Some(Entry::Radio { label, checked, enabled }) => vec![
            ("label", Variant::from(*label)),
            ("toggle-type", Variant::from("radio")),
            ("toggle-state", Variant::from(i32::from(*checked))),
            ("enabled", Variant::from(*enabled)),
        ],
        Some(Entry::Button(label)) => vec![("label", Variant::from(*label))],
    };
    if !wanted.is_empty() {
        props.retain(|(name, _)| wanted.iter().any(|w| w == name));
    }
    props
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.try_into_owned().expect("no file descriptors")))
        .collect()
}

/// `(ia{sv}av)`: an entry and its children, each child wrapped in a variant.
#[derive(Debug, Serialize, Type)]
struct Layout {
    id: i32,
    properties: HashMap<String, OwnedVariant>,
    children: Vec<OwnedVariant>,
}

fn layout(view: &View, parent: i32, depth: i32, wanted: &[String]) -> Option<Layout> {
    let entries = entries(view);
    if parent != ROOT {
        let entry = entries.iter().find(|(id, _)| *id == parent).map(|(_, e)| e)?;
        return Some(Layout { id: parent, properties: properties(Some(entry), wanted), children: Vec::new() });
    }
    let children = if depth == 0 {
        Vec::new()
    } else {
        entries
            .iter()
            .map(|(id, entry)| {
                let structure = StructureBuilder::new()
                    .add_field(*id)
                    .add_field(properties(Some(entry), wanted))
                    .add_field(Vec::<OwnedVariant>::new())
                    .build()
                    .expect("non-empty structure");
                Variant::from(structure).try_into_owned().expect("no file descriptors")
            })
            .collect()
    };
    Some(Layout { id: ROOT, properties: properties(None, wanted), children })
}

struct MenuIface {
    view: Shared,
    revision: u32,
    actions: mpsc::UnboundedSender<Action>,
}

impl MenuIface {
    fn clicked(&self, id: i32) {
        let action = match id {
            HIDDEN => Action::SetVisibility(Visibility::Hidden),
            EVERYONE => Action::SetVisibility(Visibility::Everyone),
            DOWNLOADS => Action::OpenDownloads,
            QUIT => Action::Quit,
            _ => return,
        };
        let _ = self.actions.send(action);
    }
}

#[interface(name = "com.canonical.dbusmenu")]
impl MenuIface {
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, Layout)> {
        let view = lock(&self.view).clone();
        let layout = layout(&view, parent_id, recursion_depth, &property_names)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("no menu entry {parent_id}")))?;
        Ok((self.revision, layout))
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        property_names: Vec<String>,
    ) -> Vec<(i32, HashMap<String, OwnedVariant>)> {
        let view = lock(&self.view).clone();
        let entries = entries(&view);
        let mut out = Vec::new();
        if ids.is_empty() || ids.contains(&ROOT) {
            out.push((ROOT, properties(None, &property_names)));
        }
        for (id, entry) in &entries {
            if ids.is_empty() || ids.contains(id) {
                out.push((*id, properties(Some(entry), &property_names)));
            }
        }
        out
    }

    fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedVariant> {
        let view = lock(&self.view).clone();
        let entries = entries(&view);
        let entry = match id {
            ROOT => None,
            _ => Some(
                entries
                    .iter()
                    .find(|(i, _)| *i == id)
                    .map(|(_, e)| e)
                    .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("no menu entry {id}")))?,
            ),
        };
        properties(entry, std::slice::from_ref(&name))
            .remove(&name)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("no property {name}")))
    }

    fn event(&self, id: i32, event_id: String, _data: OwnedVariant, _timestamp: u32) {
        if event_id == "clicked" {
            self.clicked(id);
        }
    }

    fn event_group(&self, events: Vec<(i32, String, OwnedVariant, u32)>) -> Vec<i32> {
        for (id, event_id, _, _) in &events {
            if event_id == "clicked" {
                self.clicked(*id);
            }
        }
        Vec::new()
    }

    fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (Vec::new(), Vec::new())
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(signal)]
    async fn layout_updated(emitter: &SignalEmitter<'_>, revision: u32, parent: i32) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------------------------

/// The registered tray icon. Gone when the connection closes.
pub struct Tray {
    conn: Connection,
    view: Shared,
}

impl Tray {
    /// Serves the icon and menu and registers with the tray host, again whenever a host starts.
    pub async fn start(conn: &Connection, view: View, actions: mpsc::UnboundedSender<Action>) -> zbus::Result<Tray> {
        let view = Arc::new(Mutex::new(view));
        let server = conn.object_server();
        server.at(ITEM_PATH, ItemIface { view: view.clone(), actions: actions.clone() }).await?;
        server.at(MENU_PATH, MenuIface { view: view.clone(), revision: 1, actions }).await?;
        let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
        conn.request_name(name.as_str()).await?;
        tokio::spawn(keep_registered(conn.clone(), name));
        Ok(Tray { conn: conn.clone(), view })
    }

    /// Shows `new`, telling the host what changed.
    pub async fn update(&self, new: View) {
        let old = std::mem::replace(&mut *lock(&self.view), new.clone());
        if old == new {
            return;
        }
        let server = self.conn.object_server();
        if let Ok(item) = server.interface::<_, ItemIface>(ITEM_PATH).await {
            let em = item.signal_emitter();
            if old.icon != new.icon {
                let _ = ItemIface::new_icon(em).await;
                let _ = ItemIface::new_attention_icon(em).await;
            }
            if old.attention != new.attention {
                let _ = ItemIface::new_status(em, if new.attention { "NeedsAttention" } else { "Active" }).await;
            }
            let _ = ItemIface::new_tool_tip(em).await;
        }
        if entries(&old) != entries(&new)
            && let Ok(menu) = server.interface::<_, MenuIface>(MENU_PATH).await
        {
            let revision = {
                let mut menu = menu.get_mut().await;
                menu.revision += 1;
                menu.revision
            };
            let _ = MenuIface::layout_updated(menu.signal_emitter(), revision, ROOT).await;
        }
    }
}

async fn keep_registered(conn: Connection, name: String) {
    let owners = async {
        let dbus = DBusProxy::new(&conn).await?;
        dbus.receive_name_owner_changed_with_args(&[(0, WATCHER)]).await
    };
    let mut owners = match owners.await {
        Ok(owners) => Some(owners),
        Err(e) => {
            tracing::warn!("cannot watch for tray hosts: {e}");
            None
        }
    };
    register(&conn, &name).await;
    let Some(owners) = owners.as_mut() else { return };
    while let Some(change) = owners.next().await {
        if change.args().is_ok_and(|a| a.new_owner().is_some()) {
            register(&conn, &name).await;
        }
    }
}

async fn register(conn: &Connection, name: &str) {
    let registered = async { WatcherProxy::new(conn).await?.register_status_notifier_item(name).await };
    match registered.await {
        Ok(()) => tracing::debug!("tray icon registered as {name}"),
        Err(e) => tracing::info!("no tray host yet ({e}); the icon appears once one starts"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(visibility: Option<Visibility>) -> View {
        View { icon: "view-hidden", status: "Hidden".into(), visibility, attention: false }
    }

    #[test]
    fn radio_entries_follow_visibility() {
        let state = |v: &View, id: i32| {
            let props = properties(entries(v).iter().find(|(i, _)| *i == id).map(|(_, e)| e), &[]);
            (i32::try_from(&props["toggle-state"]).unwrap(), bool::try_from(&props["enabled"]).unwrap())
        };
        let everyone = view(Some(Visibility::Everyone));
        assert_eq!(state(&everyone, HIDDEN), (0, true));
        assert_eq!(state(&everyone, EVERYONE), (1, true));
        let offline = view(None);
        assert_eq!(state(&offline, HIDDEN), (0, false));
    }

    #[test]
    fn layouts_hold_every_entry() {
        let v = view(Some(Visibility::Hidden));
        let root = layout(&v, ROOT, -1, &[]).unwrap();
        assert_eq!(root.children.len(), entries(&v).len());
        assert_eq!(root.properties.len(), 1, "children-display");
        let shallow = layout(&v, ROOT, 0, &[]).unwrap();
        assert!(shallow.children.is_empty());
        let leaf = layout(&v, QUIT, -1, &["label".into()]).unwrap();
        assert_eq!(leaf.properties.keys().collect::<Vec<_>>(), ["label"]);
        assert!(layout(&v, 99, -1, &[]).is_none());
    }

    #[test]
    fn status_labels_escape_mnemonics() {
        let mut v = view(Some(Visibility::Hidden));
        v.status = "my_laptop".into();
        let props = properties(Some(&Entry::Label(v.status.clone())), &[]);
        assert_eq!(<&str>::try_from(&props["label"]).unwrap(), "my__laptop");
    }

    #[test]
    fn clicks_become_actions() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let menu = MenuIface { view: Arc::new(Mutex::new(View::default())), revision: 1, actions: tx };
        for id in [STATUS, HIDDEN, EVERYONE, DOWNLOADS, QUIT] {
            menu.clicked(id);
        }
        let got: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(
            got,
            [
                Action::SetVisibility(Visibility::Hidden),
                Action::SetVisibility(Visibility::Everyone),
                Action::OpenDownloads,
                Action::Quit
            ]
        );
    }
}
