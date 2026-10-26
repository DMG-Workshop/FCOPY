//! Per-user desktop integration: "Copy with fcopy…" in the file manager.
//!
//! `fcopy-gui --install-integration` / `--uninstall-integration`
//!
//! Everything is installed **per user** (no admin/root), and every OS gets the
//! same contract: the file manager launches `fcopy-gui <selected paths>`, which
//! opens the window with those paths pre-filled as sources.
//!
//! | OS | Mechanism | Where |
//! |----|-----------|-------|
//! | Windows | classic shell verbs in `HKCU\Software\Classes` | `*`, `Directory` (right-click → *Show more options* on Win 11) |
//! | Linux | `.desktop` launcher + Dolphin service menu + Nautilus script + Nemo action | `$XDG_DATA_HOME` (`~/.local/share`) |
//! | macOS | Finder Quick Action (Services) wrapping the bundle binary | `~/Library/Services` |
//!
//! System packages (deb/rpm/PKGBUILD/MSI) install the same files system-wide;
//! see `packaging/`.
//!
//! **Only the Linux branch is compiled and tested in this repo's CI.**

use std::io;
use std::path::{Path, PathBuf};

pub const APP_ID: &str = "io.github.fcopy";

pub fn install() -> io::Result<Vec<PathBuf>> {
    let exe = std::env::current_exe()?;
    imp::install(&exe)
}

pub fn uninstall() -> io::Result<Vec<PathBuf>> {
    imp::uninstall()
}

/// Quote an argument for a `.desktop` `Exec=` line (freedesktop spec: reserved
/// characters inside double quotes must be backslash-escaped).
pub fn desktop_exec_quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::from("\"");
    for c in s.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// Linux / BSD (freedesktop)
// ---------------------------------------------------------------------------

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use super::*;
    use std::fs;

    fn data_home() -> io::Result<PathBuf> {
        if let Some(d) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
            return Ok(PathBuf::from(d));
        }
        let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME not set"))?;
        Ok(PathBuf::from(home).join(".local/share"))
    }

    pub(super) struct Item {
        pub rel: &'static str,
        pub body: String,
        pub executable: bool,
    }

    pub(super) fn items(exe: &Path) -> Vec<Item> {
        let exec = desktop_exec_quote(exe);
        vec![
            // App launcher. %F = list of local files; lets "Open With → fcopy" work too.
            Item {
                rel: "applications/io.github.fcopy.desktop",
                body: format!(
                    "[Desktop Entry]\nType=Application\nName=fcopy\nGenericName=File Copier\n\
                     Comment=Fast, verified file copying\nExec={exec} --sources %F\nIcon=io.github.fcopy\n\
                     Terminal=false\nCategories=Utility;FileTools;System;\n\
                     MimeType=inode/directory;application/octet-stream;\n\
                     StartupWMClass=io.github.fcopy\nKeywords=copy;teracopy;verify;checksum;\n"
                ),
                executable: false,
            },
            // KDE Dolphin (KF6 path; KF5 also reads it).
            Item {
                rel: "kio/servicemenus/fcopy.desktop",
                body: format!(
                    "[Desktop Entry]\nType=Service\nMimeType=all/all;\nActions=fcopyCopy;\n\
                     X-KDE-Priority=TopLevel\n\n[Desktop Action fcopyCopy]\nName=Copy with fcopy…\n\
                     Icon=io.github.fcopy\nExec={exec} --sources %F\n"
                ),
                executable: true, // KF6 requires service menus to be executable
            },
            // GNOME Files: Scripts submenu.
            Item {
                rel: "nautilus/scripts/Copy with fcopy…",
                body: format!(
                    "#!/bin/sh\n# Nautilus passes selected paths as arguments.\nexec {exec} --sources \"$@\"\n"
                ),
                executable: true,
            },
            // Cinnamon Nemo.
            Item {
                rel: "nemo/actions/fcopy.nemo_action",
                body: format!(
                    "[Nemo Action]\nName=Copy with fcopy…\nComment=Copy the selection with fcopy\n\
                     Exec={exec} --sources %F\nIcon-Name=io.github.fcopy\nSelection=notnone\nExtensions=any;\n"
                ),
                executable: false,
            },
        ]
    }

    pub fn install(exe: &Path) -> io::Result<Vec<PathBuf>> {
        let base = data_home()?;
        let mut written = vec![];
        for item in items(exe) {
            let path = base.join(item.rel);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(&path, item.body)?;
            if item.executable {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
            }
            written.push(path);
        }
        // Refresh the MIME/desktop cache if the tool exists; harmless if not.
        let _ = std::process::Command::new("update-desktop-database").arg(base.join("applications")).status();
        Ok(written)
    }

    pub fn uninstall() -> io::Result<Vec<PathBuf>> {
        let base = data_home()?;
        let mut removed = vec![];
        for item in items(Path::new("fcopy-gui")) {
            let path = base.join(item.rel);
            if fs::remove_file(&path).is_ok() {
                removed.push(path);
            }
        }
        Ok(removed)
    }
}

// ---------------------------------------------------------------------------
// Windows (unverified here: built on Windows CI only)
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::*;
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    /// Verb locations: any file, folders, and the folder background (paste-into).
    const ROOTS: [&str; 2] = [r"Software\Classes\*\shell\fcopy", r"Software\Classes\Directory\shell\fcopy"];

    pub fn install(exe: &Path) -> io::Result<Vec<PathBuf>> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let exe = exe.display().to_string();
        let mut written = vec![];
        for root in ROOTS {
            let (key, _) = hkcu.create_subkey(root)?;
            key.set_value("", &"Copy with fcopy…")?;
            key.set_value("Icon", &format!("\"{exe}\",0"))?;
            // Multi-select: Explorer starts one process per item unless the verb
            // uses MultiSelectModel=Player, which passes up to ~100 items in %*
            // to a single process. (For unlimited selections, implement
            // IExplorerCommand in a COM DLL: the Windows 11 modern menu needs that
            // plus a sparse MSIX package anyway.)
            key.set_value("MultiSelectModel", &"Player")?;
            let (cmd, _) = key.create_subkey("command")?;
            cmd.set_value("", &format!("\"{exe}\" --sources %*"))?;
            written.push(PathBuf::from(format!(r"HKCU\{root}")));
        }
        Ok(written)
    }

    pub fn uninstall() -> io::Result<Vec<PathBuf>> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let mut removed = vec![];
        for root in ROOTS {
            if hkcu.delete_subkey_all(root).is_ok() {
                removed.push(PathBuf::from(format!(r"HKCU\{root}")));
            }
        }
        Ok(removed)
    }
}

// ---------------------------------------------------------------------------
// macOS (unverified here: built on macOS CI only)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    /// A Quick Action is an Automator workflow bundle. The essential part is a
    /// "Run Shell Script" action with input "as arguments"; writing the full
    /// `document.wflow` plist by hand is brittle, so the recommended path is to
    /// ship a pre-built `Copy with fcopy.workflow` inside the .app's Resources
    /// (see packaging/macos/README) and copy it into ~/Library/Services here.
    pub fn install(exe: &Path) -> io::Result<Vec<PathBuf>> {
        let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| io::Error::other("HOME not set"))?;
        // exe = fcopy.app/Contents/MacOS/fcopy-gui → Resources is ../Resources
        let bundled = exe
            .parent()
            .and_then(Path::parent)
            .map(|c| c.join("Resources/Copy with fcopy.workflow"))
            .filter(|p| p.exists())
            .ok_or_else(|| io::Error::other("Quick Action not found in app bundle Resources"))?;
        let dest = home.join("Library/Services/Copy with fcopy.workflow");
        copy_dir(&bundled, &dest)?;
        Ok(vec![dest])
    }

    pub fn uninstall() -> io::Result<Vec<PathBuf>> {
        let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| io::Error::other("HOME not set"))?;
        let dest = home.join("Library/Services/Copy with fcopy.workflow");
        std::fs::remove_dir_all(&dest)?;
        Ok(vec![dest])
    }

    fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            let target = to.join(e.file_name());
            if e.file_type()?.is_dir() { copy_dir(&e.path(), &target)? } else { std::fs::copy(e.path(), target).map(|_| ())? }
        }
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use super::*;
    pub fn install(_: &Path) -> io::Result<Vec<PathBuf>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no desktop integration on this platform"))
    }
    pub fn uninstall() -> io::Result<Vec<PathBuf>> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_quoting_follows_the_desktop_spec() {
        assert_eq!(desktop_exec_quote(Path::new("/opt/fcopy/fcopy-gui")), "\"/opt/fcopy/fcopy-gui\"");
        assert_eq!(desktop_exec_quote(Path::new("/home/a b/$x\"y")), "\"/home/a b/\\$x\\\"y\"");
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_install_and_uninstall_into_xdg_data_home() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded test section; no other test reads XDG_DATA_HOME.
        unsafe { std::env::set_var("XDG_DATA_HOME", dir.path()) };
        let written = imp::install(Path::new("/opt/fcopy/fcopy-gui")).unwrap();
        assert_eq!(written.len(), 4);
        let desktop = std::fs::read_to_string(dir.path().join("applications/io.github.fcopy.desktop")).unwrap();
        assert!(desktop.contains("Exec=\"/opt/fcopy/fcopy-gui\" --sources %F"));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("kio/servicemenus/fcopy.desktop")).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "Dolphin service menus must be executable");
        assert_eq!(imp::uninstall().unwrap().len(), 4);
        assert!(!dir.path().join("applications/io.github.fcopy.desktop").exists());
    }
}
