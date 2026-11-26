//! Desktop integration on Linux. Wayland compositors (KDE, GNOME) find a window's icon
//! through the desktop entry named after its app id (`crack.desktop`), not through
//! anything the window sets. A binary run straight from a download or a build has no
//! such entry, so the window would show a generic icon. At startup crack installs its
//! icon and, if no other `crack.desktop` exists, its own entry under `~/.local/share`.
//! Turn it off with `desktop_integration = false`.

use std::path::{Path, PathBuf};

const ICON_512: &[u8] = include_bytes!("../assets/icon-512.png");
const ICON_64: &[u8] = include_bytes!("../assets/icon-64.png");
/// Marks an entry crack wrote, so it's kept up to date (and others are left alone).
const MARKER: &str = "X-Crack-Generated=true";

/// Installs the icon and desktop entry if they're missing or out of date. Never fails
/// the launch: problems are only logged.
pub fn integrate() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let Some(data) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))
    else {
        return;
    };
    let exe = std::env::current_exe().ok();
    if let Err(error) = install(&data, exe.as_deref(), &system_entry_exists()) {
        tracing::warn!("desktop integration: {error:#}");
    }
}

fn install(data: &Path, exe: Option<&Path>, system_entry: &dyn Fn() -> bool) -> anyhow::Result<()> {
    let icons = data.join("icons/hicolor");
    write_if_changed(&icons.join("512x512/apps/crack.png"), ICON_512)?;
    write_if_changed(&icons.join("64x64/apps/crack.png"), ICON_64)?;

    let entry = data.join("applications/crack.desktop");
    let ours = std::fs::read_to_string(&entry).map(|t| t.contains(MARKER));
    match ours {
        // Someone else's entry (a package, or the user's own): leave it.
        Ok(false) => return Ok(()),
        Err(_) if system_entry() => return Ok(()),
        _ => {}
    }
    let Some(exe) = exe else { return Ok(()) };
    write_if_changed(&entry, desktop_entry(exe).as_bytes())
}

fn desktop_entry(exe: &Path) -> String {
    // Exec quoting per the desktop entry spec: quote the path, escape what needs it.
    let path = exe
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$");
    format!(
        "[Desktop Entry]\nType=Application\nName=crack\nGenericName=Coding agent\n\
         Comment=Coding agents, certified cracked.\nExec=\"{path}\"\nIcon=crack\nTerminal=false\n\
         Categories=Development;IDE;\nStartupWMClass=crack\n{MARKER}\n"
    )
}

/// A `crack.desktop` installed system-wide or by a package manager.
fn system_entry_exists() -> impl Fn() -> bool {
    || {
        let dirs =
            std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
        let mut candidates: Vec<PathBuf> = dirs.split(':').map(PathBuf::from).collect();
        if let Some(home) = dirs::home_dir() {
            candidates.push(home.join(".nix-profile/share"));
        }
        candidates
            .iter()
            .any(|d| d.join("applications/crack.desktop").is_file())
    }
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if std::fs::read(path).is_ok_and(|old| old == bytes) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staged = path.with_extension("crack-tmp");
    std::fs::write(&staged, bytes)?;
    std::fs::rename(&staged, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_icon_and_entry_and_respects_others() {
        let root = std::env::temp_dir().join(format!("crack-desktop-{}", uuid::Uuid::new_v4()));
        let data = root.join("a");
        let data = data.as_path();
        let exe = Path::new("/opt/my apps/crack");
        install(data, Some(exe), &|| false).unwrap();
        assert_eq!(
            std::fs::read(data.join("icons/hicolor/512x512/apps/crack.png")).unwrap(),
            ICON_512
        );
        let entry = std::fs::read_to_string(data.join("applications/crack.desktop")).unwrap();
        assert!(entry.contains("Exec=\"/opt/my apps/crack\""));
        assert!(entry.contains("Icon=crack"));

        // Ours is updated when the binary moves.
        install(data, Some(Path::new("/usr/bin/crack")), &|| false).unwrap();
        let entry = std::fs::read_to_string(data.join("applications/crack.desktop")).unwrap();
        assert!(entry.contains("Exec=\"/usr/bin/crack\""));

        // Someone else's entry is left alone.
        std::fs::write(
            data.join("applications/crack.desktop"),
            "[Desktop Entry]\nExec=mine\n",
        )
        .unwrap();
        install(data, Some(exe), &|| false).unwrap();
        let entry = std::fs::read_to_string(data.join("applications/crack.desktop")).unwrap();
        assert_eq!(entry, "[Desktop Entry]\nExec=mine\n");

        // A packaged entry elsewhere: no entry of our own, but the icon is still there.
        let other = root.join("b");
        install(&other, Some(exe), &|| true).unwrap();
        assert!(!other.join("applications/crack.desktop").exists());
        assert!(other.join("icons/hicolor/64x64/apps/crack.png").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
