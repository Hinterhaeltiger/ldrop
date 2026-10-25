// SPDX-License-Identifier: GPL-3.0-or-later
//! Fitting into the desktop (SPEC §11.6): whether a native widget (the Plasma applet or the
//! GNOME Shell extension) stands in for the tray icon, and the per-user setup that
//! `make install` cannot do because it runs once for the whole system.
//!
//! The setup runs once per step and user: each finished step is recorded in
//! `$XDG_STATE_HOME/ldrop/gui-setup`, so undoing one by hand (disabling the extension,
//! `ldrop integrate thunar --remove`) sticks.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ldrop_core::integrate::{self, Change};
use ldrop_core::paths::Paths;
use zbus::OwnedVariant;
use zbus::{Connection, proxy};

/// Plasma applet ID, `packaging/plasma/org.ldrop.visibility`.
const PLASMA_APPLET: &str = "org.ldrop.visibility";
/// GNOME Shell extension UUID, `integrations/gnome-shell/ldrop@ldrop.org`.
const GNOME_EXTENSION: &str = "ldrop@ldrop.org";

#[proxy(
    interface = "org.gnome.Shell.Extensions",
    default_service = "org.gnome.Shell.Extensions",
    default_path = "/org/gnome/Shell/Extensions"
)]
trait ShellExtensions {
    fn get_extension_info(&self, uuid: &str) -> zbus::Result<HashMap<String, OwnedVariant>>;
    fn enable_extension(&self, uuid: &str) -> zbus::Result<bool>;
}

pub const GNOME_WIDGET: &str = "the ldrop GNOME Shell extension";
/// The setup step that enables it, see [`first_run`].
pub const GNOME_EXTENSION_STEP: &str = "gnome-extension";

/// GNOME Shell's extension states; `ACTIVE` was called `ENABLED` before GNOME 46.
const EXTENSION_ACTIVE: f64 = 1.0;

fn desktop_is(name: &str) -> bool {
    std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.split(':').any(|d| d.eq_ignore_ascii_case(name)))
}

/// The GNOME Shell extension's state, `None` when GNOME does not know it (not installed, or
/// installed during this session: GNOME finds new extensions at login).
async fn gnome_extension_state(conn: &Connection) -> Option<f64> {
    let shell = ShellExtensionsProxy::new(conn).await.ok()?;
    let info = shell.get_extension_info(GNOME_EXTENSION).await.ok()?;
    info.get("state").and_then(|s| f64::try_from(s).ok())
}

/// The native widget that shows visibility in the panel instead of our tray icon, if any.
pub async fn native_widget(conn: &Connection) -> Option<&'static str> {
    if desktop_is("KDE")
        && data_dirs()
            .iter()
            .any(|dir| dir.join("plasma/plasmoids").join(PLASMA_APPLET).join("metadata.json").is_file())
    {
        return Some("the ldrop Plasma applet");
    }
    if desktop_is("GNOME") && gnome_extension_state(conn).await == Some(EXTENSION_ACTIVE) {
        return Some(GNOME_WIDGET);
    }
    None
}

/// Runs the setup steps that are still due. Returns the finished ones' names, for the log.
pub async fn first_run(conn: &Connection) -> Vec<&'static str> {
    let record = Paths::from_env().state_dir.join("gui-setup");
    let mut done: Vec<String> =
        std::fs::read_to_string(&record).unwrap_or_default().lines().map(str::to_owned).collect();
    let mut finished = Vec::new();
    for step in [Step::GnomeExtension, Step::Thunar, Step::NautilusScript] {
        if done.iter().any(|d| d == step.name()) {
            continue;
        }
        match step.run(conn).await {
            Outcome::Done => finished.push(step.name()),
            Outcome::NotNeeded => {}
            // Try again next time, e.g. after the next login.
            Outcome::NotYet => continue,
        }
        done.push(step.name().to_owned());
    }
    if !finished.is_empty() || !record.exists() {
        save(&record, &done);
    }
    finished
}

fn save(record: &Path, done: &[String]) {
    let write = || -> std::io::Result<()> {
        if let Some(dir) = record.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(record, done.iter().map(|d| format!("{d}\n")).collect::<String>())
    };
    if let Err(e) = write() {
        tracing::warn!("cannot write {}: {e}", record.display());
    }
}

#[derive(Clone, Copy)]
enum Step {
    /// Enable the GNOME Shell extension; installing it system-wide leaves it off.
    GnomeExtension,
    /// Add "Send with ldrop" to Thunar's per-user custom actions.
    Thunar,
    /// "Send with ldrop" under Scripts for Nautilus without nautilus-python.
    NautilusScript,
}

enum Outcome {
    Done,
    /// Nothing to do here, now or later.
    NotNeeded,
    NotYet,
}

impl Step {
    fn name(self) -> &'static str {
        match self {
            Step::GnomeExtension => GNOME_EXTENSION_STEP,
            Step::Thunar => "thunar",
            Step::NautilusScript => "nautilus-script",
        }
    }

    async fn run(self, conn: &Connection) -> Outcome {
        match self {
            Step::GnomeExtension => {
                if !desktop_is("GNOME") {
                    return Outcome::NotYet;
                }
                match gnome_extension_state(conn).await {
                    None => Outcome::NotYet,
                    Some(EXTENSION_ACTIVE) => Outcome::NotNeeded,
                    Some(_) => {
                        let enabled = match ShellExtensionsProxy::new(conn).await {
                            Ok(shell) => shell.enable_extension(GNOME_EXTENSION).await,
                            Err(e) => Err(e),
                        };
                        match enabled {
                            Ok(true) => Outcome::Done,
                            Ok(false) => Outcome::NotYet,
                            Err(e) => {
                                tracing::warn!("cannot enable the GNOME Shell extension: {e}");
                                Outcome::NotYet
                            }
                        }
                    }
                }
            }
            Step::Thunar => {
                if !integrate::on_path("thunar") {
                    return Outcome::NotYet;
                }
                change(integrate::thunar(false), &integrate::thunar_file())
            }
            Step::NautilusScript => {
                if !integrate::on_path("nautilus") {
                    return Outcome::NotYet;
                }
                // nautilus-python loads integrations/nautilus/ldrop.py instead.
                let extension = data_dirs().iter().any(|d| d.join("nautilus-python/extensions/ldrop.py").is_file());
                if extension && integrate::nautilus_python_installed() {
                    return Outcome::NotNeeded;
                }
                change(integrate::nautilus_script(false), &integrate::nautilus_script_file())
            }
        }
    }
}

fn change(result: std::io::Result<Change>, path: &Path) -> Outcome {
    match result {
        Ok(Change::Done) => Outcome::Done,
        Ok(Change::Unchanged) => Outcome::NotNeeded,
        Err(e) => {
            tracing::warn!("cannot add \"Send with ldrop\" to {}: {e}", path.display());
            Outcome::NotYet
        }
    }
}

/// `$XDG_DATA_HOME` and `$XDG_DATA_DIRS`, with their defaults.
fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.into_iter().chain(system.split(':').map(PathBuf::from).filter(|p| p.is_absolute())).collect()
}
