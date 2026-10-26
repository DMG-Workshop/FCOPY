# Platform quirks: Windows, macOS, Linux

How fcopy deals with the parts of file copying that differ between operating
systems, and where each piece lives in the code.

> **Verification status.** Linux code paths are compiled and tested in CI (ext4).
> Windows and macOS `cfg` branches were written against the exact
> `windows-sys 0.61` / `winreg 0.56` / `std` signatures, but **have not been
> compiled on those OSes yet**. Build `cargo test --workspace` on a Windows and a
> macOS host before the first release. The affected code is small on purpose:
> `crates/fc-engine/src/platform/meta.rs`, `cache.rs`,
> `crates/fc-gui/src/integration.rs`.

---

## 1. `cfg` conventions

Keep portable logic in pure functions (testable everywhere) and make `cfg`
blocks thin adapters to system calls:

```rust
// Pure: unit-tested on Linux, used on Windows.
pub fn extend_str(abs: &str) -> Option<String> { /* \\?\ transform */ }

// Thin adapter: the only Windows-only part.
pub fn to_extended(path: &Path) -> Cow<'_, Path> {
    #[cfg(windows)]
    { /* absolute + extend_str */ }
    #[cfg(not(windows))]
    { Cow::Borrowed(path) }
}
```

| Attribute | Use for |
|-----------|---------|
| `#[cfg(unix)]` | POSIX mode bits, xattrs, `fsync` of directories |
| `#[cfg(windows)]` | Win32 attributes, security descriptors, registry, `\\?\` |
| `#[cfg(target_os = "linux")]` | `posix_fadvise`, `fallocate` specifics |
| `#[cfg(target_os = "macos")]` | `F_NOCACHE`, birth time, Finder integration |
| `#[cfg(all(unix, not(target_os = "macos")))]` | freedesktop (Linux + BSD) desktop integration |
| `#[cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]` | no console window for the release GUI |
| `[target.'cfg(windows)'.dependencies]` | `windows-sys`, `winreg`: never downloaded into Linux builds' dependency graph |

Every OS-specific function has a fallback for "other" targets so the crate
still compiles (e.g. WASI, BSDs).

---

## 2. Metadata and permissions

`crates/fc-engine/src/platform/meta.rs`. **Policy: metadata never fails a
copy.** Destination filesystems routinely can't store what the source has, so
every failure becomes a warning in `CopyReport::warnings` (shown in the GUI's log
drawer and printed by the CLI). The data itself is never at risk.

| | Linux | macOS | Windows |
|---|---|---|---|
| mtime / atime | `File::set_times` | `File::set_times` | `File::set_times` |
| creation time | not settable on Linux | `std::os::macos::fs::FileTimesExt::set_created` | `std::os::windows::fs::FileTimesExt::set_created` |
| permissions | `rwx` bits only (`& 0o777`) | same | read-only, hidden, system, archive, not-indexed (`SetFileAttributesW`) |
| xattrs | `user.*`, `trusted.*`, `system.posix_acl_*`. `security.*` (SELinux labels, capabilities) skipped. | all `com.apple.*` (resource fork, Finder info, quarantine) | n/a (alternate data streams: future work) |
| ACLs | POSIX ACLs travel as `system.posix_acl_access` xattrs | native ACLs are **not** xattrs: use `copyfile(COPYFILE_METADATA)` | DACL + protection flag via `Get/SetNamedSecurityInfoW` (`--acls`, off by default) |

Ordering inside the engine matters:

1. Write the data to `name.fcpart`, `fsync`, and do the read-back verify.
2. **Then** set timestamps, because the verify read could bump atime under `relatime`.
3. Then xattrs and the ACL.
4. **Last**, permissions and attributes: a read-only temp file can't be cleaned up on
   Windows if something later fails.
5. Atomic rename to the real name.

Deliberate choices:

* **setuid/setgid/sticky are dropped.** A copy must never gain privilege
  (`cp -p` behaves the same unless run as root).
* **Ownership (uid/gid, Windows owner) is not copied.** It needs root,
  `CAP_CHOWN` or `SeRestorePrivilege`. Add it behind a flag only for the
  "backup as admin" use case.
* **Windows ACLs are opt-in.** Explorer's default is for copies to inherit the
  destination folder's permissions, and most users expect that.
* **xattr "not supported" is reported once per file**, not once per attribute
  (`ENOTSUP`/`EOPNOTSUPP` stops the loop). That is what happens when copying to
  FAT32/exFAT or some network shares.

### Filesystem capability notes

| Destination | Watch out for |
|---|---|
| FAT32 | 4 GiB max file size, 2-second mtime resolution, no permissions or xattrs |
| exFAT | no permissions or xattrs; case-insensitive |
| NTFS (from Linux via ntfs3) | POSIX modes not stored by default |
| SMB / NFS | `fallocate` unsupported (engine ignores it), xattrs depend on server config, `fsync` latency |
| btrfs / XFS / APFS / ReFS | reflink clone is possible (instant copy): a future fast path in the engine |

---

## 3. Long paths on Windows

`crates/fc-engine/src/platform/longpath.rs`, plus `packaging/windows/fcopy.exe.manifest`.

**What Rust already does:** since 1.58, `std::fs` on Windows converts long
absolute paths to `\\?\` form internally, so `File::open`, `rename`, `read_dir`,
`create_dir_all` and friends work beyond 260 characters out of the box.

**What still needs handling (and is handled):**

1. **Raw Win32 / FFI calls** (`SetFileAttributesW`, `Get/SetNamedSecurityInfoW`,
   future `CopyFileExW`) don't get std's conversion. `platform::meta::win::wide()`
   runs every path through `longpath::to_extended()` first.
2. **`\\?\` paths are verbatim**: no `.`/`..` resolution, no `/` → `\`
   conversion. `extend_str()` makes the path absolute, normalises separators,
   resolves `.`/`..` (never above the root) and handles UNC
   (`\\server\share\x` → `\\?\UNC\server\share\x`).
3. **Verbatim paths bypass Win32 name rules.** A file named `report. ` can be
   created through `\\?\` and then can't be deleted from Explorer. So names are
   sanitized at planning time (`platform::names`, `PlanOptions::windows_safe_names`,
   on by default on Windows, `--windows-safe-names` elsewhere for exFAT sticks):
   * illegal characters `<>:"/\|?*` and control characters → `_`
   * trailing dots/spaces → `_`
   * reserved device names `CON`, `NUL`, `COM1`… (with any extension) → `CON_`
   * case-only collisions (`README`/`readme`) → warning before the copy starts
4. **The app manifest** declares `longPathAware`, so even third-party code
   using plain Win32 paths works when the system policy
   `HKLM\SYSTEM\CurrentControlSet\Control\FileSystem\LongPathsEnabled = 1` is set.
   It is embedded by `crates/fc-gui/build.rs` (via `embed-resource`) only when
   the target OS is Windows.

```rust
use fc_engine::platform::longpath::{extend_str, strip_extended};
assert_eq!(extend_str(r"C:\a\.\b\..\c").as_deref(), Some(r"\\?\C:\a\c"));
assert_eq!(extend_str(r"\\nas\share\x").as_deref(), Some(r"\\?\UNC\nas\share\x"));
assert_eq!(strip_extended(r"\\?\C:\x"), r"C:\x"); // for display
```

---

## 4. Desktop integration

One contract on every OS: **the file manager runs `fcopy-gui --sources <paths…>`**,
which opens the window with those paths pre-filled; the user picks the
destination. `--sources` matters: without it, the last selected file would be
taken as the destination.

Per-user install, no admin needed: `fcopy-gui --install-integration`
(`--uninstall-integration` to undo). Code: `crates/fc-gui/src/integration.rs`.
System packages install the same files system-wide from `packaging/`.

### Windows

* **Classic verbs** in `HKCU\Software\Classes\{*,Directory}\shell\fcopy` with
  `MultiSelectModel=Player`, so one process receives up to ~100 selected items
  (`packaging/windows/context-menu.reg` is the static equivalent). On Windows 11
  these appear under *Show more options*.
* **Top-level Windows 11 menu** requires an `IExplorerCommand` COM handler
  (a small DLL, e.g. with the `windows` crate's `implement` macro) registered
  through a **sparse MSIX package** for package identity. Do it as a separate
  `fc-shellext` crate once the app is stable.
* **Taking over Explorer's own copy/paste** (what TeraCopy does) means
  replacing the copy hook / `IFileOperation` path via a shell extension. It is
  invasive and frequently broken by Windows updates. Recommended: context-menu
  verbs plus drag-and-drop onto the window.
* Single-instance forwarding (second launch hands its paths to the running
  window instead of opening another) needs a named pipe. That is planned
  together with the remote-agent IPC.

### Linux (Debian, Fedora, Arch)

| File | Installed to | Purpose |
|------|--------------|---------|
| `io.github.fcopy.desktop` | `/usr/share/applications/` | launcher, *Open With* for folders/files |
| `fcopy-servicemenu.desktop` | `/usr/share/kio/servicemenus/` (mode 755) | KDE Dolphin *Copy with fcopy…* |
| `fcopy.nemo_action` | `/usr/share/nemo/actions/` | Cinnamon Nemo |
| Nautilus script | `~/.local/share/nautilus/scripts/` (per-user only) | GNOME Files *Scripts* submenu |
| `io.github.fcopy.metainfo.xml` | `/usr/share/metainfo/` | GNOME Software / Discover listing |
| `io.github.fcopy.svg` | `/usr/share/icons/hicolor/scalable/apps/` | icon |

* **MIME association:** the `.desktop` file declares `inode/directory` and
  `application/octet-stream`, which gets fcopy into *Open With*. There is **no
  freedesktop mechanism to become the default copy handler** of a file manager,
  so context-menu entries are the integration point. Never make fcopy the
  *default* app for those types (`xdg-mime default`): it would hijack
  double-click on folders.
* The window's Wayland `app_id` / X11 `WM_CLASS` is `io.github.fcopy`, matching
  the `.desktop` file name, so docks group the window under the right icon.
* Packaging:
  * Debian/Ubuntu: `cargo deb -p fc-gui` (metadata in `crates/fc-gui/Cargo.toml`)
  * Fedora/openSUSE: `cargo generate-rpm -p crates/fc-gui`
  * Arch: `packaging/arch/PKGBUILD`
  * Flatpak would need `--filesystem=host` to be useful for a copier, which
    Flathub reviews strictly. Native packages are the better first target.

### macOS

* Bundle layout, `Info.plist`, signing and notarization: `packaging/macos/`.
* `CFBundleDocumentTypes` with `LSHandlerRank=Alternate` for `public.item`
  puts fcopy in *Open With* without ever becoming a default handler.
* **Drops on the window** work today (winit → `RawInput::dropped_files`).
* **Drops on the Dock icon / Open With** arrive as an Apple Event, which winit
  doesn't forward. Use the shipped **Finder Quick Action** (installed into
  `~/Library/Services` by `--install-integration`), or add an
  `application:openURLs:` delegate via `objc2-app-kit`.
* Copying protected locations triggers TCC. Document *Full Disk Access* for
  users; it can't be requested programmatically.
