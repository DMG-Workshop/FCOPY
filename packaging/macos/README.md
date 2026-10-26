# macOS bundle

```
fcopy.app/
└── Contents/
    ├── Info.plist                 ← fcopy.app-template/Contents/Info.plist
    ├── MacOS/
    │   ├── fcopy-gui              ← cargo build --release -p fc-gui (lipo x86_64 + arm64 for universal)
    │   └── fcopy                  ← CLI, optional; symlink into /usr/local/bin on install
    └── Resources/
        ├── fcopy.icns             ← iconutil -c icns fcopy.iconset (from packaging/icons/*.svg)
        └── Copy with fcopy.workflow/   ← Finder Quick Action (see below)
```

Build and sign:

```sh
cargo build --release -p fc-gui --target aarch64-apple-darwin
cargo build --release -p fc-gui --target x86_64-apple-darwin
lipo -create -output fcopy.app/Contents/MacOS/fcopy-gui \
  target/aarch64-apple-darwin/release/fcopy-gui target/x86_64-apple-darwin/release/fcopy-gui
codesign --force --deep --options runtime --sign "Developer ID Application: …" fcopy.app
xcrun notarytool submit fcopy.zip --keychain-profile notary --wait && xcrun stapler staple fcopy.app
```

(`cargo-bundle` or `cargo-packager` can generate the same layout from Cargo metadata.)

## Files dropped on the window

Handled in-app: winit delivers drops to egui as `RawInput::dropped_files`
(`crates/fc-gui/src/app.rs → handle_drops`). Works on all three OSes.

## Files dropped on the Dock icon / "Open With"

macOS delivers these as an Apple Event (`application:openURLs:`), **not** as
`argv`. winit (and therefore eframe) does not currently forward that event to
the app. Options, in order of effort:

1. **Finder Quick Action** (recommended, no native code): in Automator create a
   *Quick Action* receiving "files or folders" in Finder, add *Run Shell Script*
   with "Pass input: as arguments":
   `"/Applications/fcopy.app/Contents/MacOS/fcopy-gui" --sources "$@" >/dev/null 2>&1 &`
   Save as `Copy with fcopy.workflow`, ship it in `Resources/`;
   `fcopy-gui --install-integration` copies it to `~/Library/Services`.
2. Install an `NSApplicationDelegate` method for `application:openURLs:` via
   `objc2-app-kit` before eframe starts, forwarding URLs into the queue.

## Full-disk access

Copying from `~/Library/Mail`, `~/Desktop` on managed Macs, other users' homes,
or external volumes may trigger TCC prompts. Users grant "Full Disk Access" in
System Settings → Privacy & Security; the app cannot request it programmatically.
