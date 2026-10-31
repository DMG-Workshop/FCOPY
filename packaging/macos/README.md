# macOS releases

Release CI builds separate **Apple Silicon (`arm64`)** and **Intel (`x86_64`)**
DMGs on native Macs. Drag `fcopy.app` to Applications, eject the disk image,
then launch it. Both the desktop app and CLI are included:

```sh
/Applications/fcopy.app/Contents/MacOS/fcopy copy SOURCE DEST --verify readback
```

The deployment target is macOS 11 or later; native CI runs on macOS 15.
CI tests platform code, strict Clippy, bundle signatures, a real CLI read-back
copy and native GUI startup from the built DMG. Older OS versions still require
real-machine validation.

## Signing and Gatekeeper

These are **ad-hoc signed**, not Apple Developer ID signed or notarized.
Gatekeeper can block downloaded applications; after verifying the release
SHA256SUMS, use macOS's explicit per-app approval in **System Settings > Privacy
& Security > Open Anyway** if you trust this release. Do not disable Gatekeeper
globally. An organization requiring notarized software must sign/notarize its
own build or wait for a future Developer ID release.

## Build on a Mac

Install Rust, Xcode Command Line Tools and Homebrew `librsvg`, then:

```sh
export MACOSX_DEPLOYMENT_TARGET=11.0
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo build --locked --release -p fc-cli -p fc-gui
bash packaging/macos/build.sh
bash packaging/macos/smoke-test.sh dist/*.dmg
```

The script generates the icon, checks for unexpected non-system dylibs,
ad-hoc signs each binary and the bundle, and creates the architecture-specific
DMG in `dist`. Run it separately on each architecture; this is not a universal
binary or a cross-compilation recipe.

## Files and Finder

Drag files onto the **open application window**, or run
`fcopy-gui --sources PATH...`. A second launch forwards to the existing queue.
Dock-icon drops and Finder "Open With" use Apple Events that are not forwarded
by the current window toolkit, so the bundle does not register those handlers.

A packaged Finder Quick Action is not included; `--install-integration` on
macOS requires a separately supplied Automator workflow. To create one manually,
make an Automator Quick Action receiving files/folders in Finder and add a Run
Shell Script action with input passed **as arguments**:

```sh
"/Applications/fcopy.app/Contents/MacOS/fcopy-gui" --sources "$@" &
```

Save it as "Copy with fcopy". macOS installs a user-created action in Services.
Access to protected folders may require user consent or Full Disk Access in
System Settings > Privacy & Security; the app cannot grant itself access.

Downloads permit macOS's standard `/var`, `/tmp` and `/etc` aliases only when
they point to their corresponding `/private` directories. User-created links
or junctions in a download destination are still refused.
