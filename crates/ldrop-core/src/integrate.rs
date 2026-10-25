// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-user "Send with ldrop" entries for file managers that have no system-wide drop-in
//! directory (SPEC §11.6). `make install` puts the other entries in place; these two live in
//! the user's home, so `ldrop-gui` adds them on its first start and `ldrop integrate` can add
//! or remove them by hand.
//!
//! - Thunar reads custom actions only from `~/.config/Thunar/uca.xml`.
//! - Nautilus without nautilus-python still runs scripts from `~/.local/share/nautilus/scripts`
//!   (right-click → Scripts).

use std::io;
use std::path::{Path, PathBuf};

/// What an add or remove did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Done,
    /// The entry was already there (add) or already gone (remove).
    Unchanged,
}

/// Marks our Thunar action, so removing finds it again.
const UNIQUE_ID: &str = "ldrop-send-with-ldrop";

const ACTION: &str = "<action>
\t<icon>document-share</icon>
\t<name>Send with ldrop</name>
\t<submenu></submenu>
\t<unique-id>ldrop-send-with-ldrop</unique-id>
\t<command>ldrop-gui --share %F</command>
\t<description>Send to a nearby phone or computer</description>
\t<range></range>
\t<patterns>*</patterns>
\t<audio-files/>
\t<image-files/>
\t<other-files/>
\t<text-files/>
\t<video-files/>
</action>
";

const EMPTY: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n</actions>\n";

/// `$XDG_CONFIG_HOME/Thunar/uca.xml`.
pub fn thunar_file() -> PathBuf {
    config_home().join("Thunar/uca.xml")
}

/// Adds or removes the Thunar action, keeping a backup of the old file next to it.
pub fn thunar(remove: bool) -> io::Result<Change> {
    let path = thunar_file();
    let current = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => EMPTY.to_owned(),
        Err(e) => return Err(e),
    };
    let updated = if remove { remove_action(&current) } else { add_action(&current) };
    let updated = updated.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if updated == current {
        return Ok(Change::Unchanged);
    }
    if path.exists() {
        std::fs::copy(&path, path.with_extension("xml.ldrop-backup"))?;
    } else if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, &updated)?;
    Ok(Change::Done)
}

fn add_action(xml: &str) -> Result<String, &'static str> {
    if xml.contains(UNIQUE_ID) {
        return Ok(xml.to_owned());
    }
    let end = xml.rfind("</actions>").ok_or("not a Thunar actions file")?;
    Ok(format!("{}{ACTION}{}", &xml[..end], &xml[end..]))
}

fn remove_action(xml: &str) -> Result<String, &'static str> {
    let Some(id) = xml.find(UNIQUE_ID) else { return Ok(xml.to_owned()) };
    let start = xml[..id].rfind("<action>").ok_or("malformed action")?;
    let close = "</action>";
    let end = id + xml[id..].find(close).ok_or("malformed action")? + close.len();
    // Take the line break after it too.
    let end = if xml[end..].starts_with('\n') { end + 1 } else { end };
    Ok(format!("{}{}", &xml[..start], &xml[end..]))
}

/// Nautilus names the menu entry after the file.
const SCRIPT_NAME: &str = "Send with ldrop";

const SCRIPT: &str = "#!/bin/sh
# Added by ldrop: \"Send with ldrop\" in Nautilus (right-click → Scripts).
# Remove with: ldrop integrate nautilus-script --remove
# Nautilus passes the selection one path per line; directories are skipped.
set -f
IFS='
'
set --
for f in $NAUTILUS_SCRIPT_SELECTED_FILE_PATHS; do
    [ -f \"$f\" ] && set -- \"$@\" \"$f\"
done
[ $# -gt 0 ] && exec ldrop-gui --share \"$@\"
";

/// `$XDG_DATA_HOME/nautilus/scripts/Send with ldrop`.
pub fn nautilus_script_file() -> PathBuf {
    data_home().join("nautilus/scripts").join(SCRIPT_NAME)
}

/// Adds or removes the Nautilus script. A file of that name that ldrop did not write is left
/// alone.
pub fn nautilus_script(remove: bool) -> io::Result<Change> {
    let path = nautilus_script_file();
    let ours = std::fs::read_to_string(&path).is_ok_and(|s| s.contains("Added by ldrop"));
    if remove {
        if !ours {
            return Ok(Change::Unchanged);
        }
        std::fs::remove_file(&path)?;
        return Ok(Change::Done);
    }
    if path.exists() && (!ours || std::fs::read_to_string(&path).is_ok_and(|s| s == SCRIPT)) {
        return Ok(Change::Unchanged);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_executable(&path, SCRIPT)?;
    Ok(Change::Done)
}

fn write_executable(path: &Path, text: &str) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

/// Whether the nautilus-python extension is installed, which loads the richer entry from
/// `integrations/nautilus/ldrop.py` instead of the script.
pub fn nautilus_python_installed() -> bool {
    ["/usr/lib", "/usr/lib64", "/usr/local/lib", "/usr/lib/x86_64-linux-gnu", "/usr/lib/aarch64-linux-gnu"]
        .iter()
        .any(|lib| Path::new(lib).join("nautilus/extensions-4/libnautilus-python.so").exists())
}

/// Whether `program` is on `$PATH`.
pub fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

fn config_home() -> PathBuf {
    xdg_home("XDG_CONFIG_HOME", ".config")
}

fn data_home() -> PathBuf {
    xdg_home("XDG_DATA_HOME", ".local/share")
}

fn xdg_home(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(default)))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const THUNAR_DEFAULT: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<actions>
<action>
\t<icon>utilities-terminal</icon>
\t<name>Open Terminal Here</name>
\t<unique-id>1-1</unique-id>
\t<command>exo-open --working-directory %f --launch TerminalEmulator</command>
\t<patterns>*</patterns>
\t<directories/>
</action>
</actions>
";

    #[test]
    fn adds_once_and_removes_cleanly() {
        let added = add_action(THUNAR_DEFAULT).unwrap();
        assert!(added.contains("<command>ldrop-gui --share %F</command>"));
        assert!(added.contains("Open Terminal Here"));
        assert!(added.trim_end().ends_with("</actions>"));
        assert_eq!(add_action(&added).unwrap(), added);
        assert_eq!(remove_action(&added).unwrap(), THUNAR_DEFAULT);
        assert_eq!(remove_action(THUNAR_DEFAULT).unwrap(), THUNAR_DEFAULT);
    }

    #[test]
    fn starts_a_missing_file() {
        let added = add_action(EMPTY).unwrap();
        assert!(added.starts_with("<?xml") && added.contains(UNIQUE_ID));
        assert!(add_action("<html></html>").is_err());
    }

    #[test]
    fn script_passes_files_with_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("send");
        let log = dir.path().join("log");
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        write_executable(&script, SCRIPT).unwrap();
        write_executable(&bin.join("ldrop-gui"), &format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n", log.display()))
            .unwrap();
        let a = dir.path().join("a b.txt");
        std::fs::write(&a, "x").unwrap();
        let selection = format!("{}\n{}\n", a.display(), dir.path().display());
        let status = std::process::Command::new(&script)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("NAUTILUS_SCRIPT_SELECTED_FILE_PATHS", selection)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read_to_string(&log).unwrap(), format!("--share\n{}\n", a.display()));
    }
}
