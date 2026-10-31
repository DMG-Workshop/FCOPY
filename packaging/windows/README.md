# Windows downloads

Release assets for Windows x86-64:

* `FCOPY-VERSION-windows-x86_64-unsigned-setup.exe`: **per-user** installer.
* `FCOPY-VERSION-windows-x86_64-unsigned-portable.zip`: desktop app and CLI,
  without an installer.

Both contain `fcopy-gui.exe` and `fcopy.exe`; no background agent, web server,
Windows service, PATH modification or automatic startup is installed.
Windows 10 (1809+) / Windows 11 x64 are the intended desktop platforms.
CI uses Windows Server 2022; interactive desktop behavior also has a separate
local Windows regression script.

Starting with v0.1.2, both executables statically link the Visual C++ runtime:
no separate Visual C++ Redistributable installation is needed. v0.1.1 instead
requires the Microsoft Visual C++ x64 runtime to already be installed.

## Unsigned builds

**The installer and executables are not Authenticode signed.** Expect Windows
SmartScreen / Unknown publisher warnings until code signing is available.
Download from the project's GitHub Releases and verify SHA256SUMS:

```powershell
Get-FileHash .\FCOPY-VERSION-windows-x86_64-unsigned-setup.exe -Algorithm SHA256
```

Compare the complete hash with the matching line in the release's SHA256SUMS.
Only approve a specific download if you trust its source; do not disable
SmartScreen or antivirus globally. `UNSIGNED.txt` is shipped in both packages
and displayed by the installer before installation.

## Install / uninstall

The default location is `%LOCALAPPDATA%\Programs\FCOPY`. No administrator prompt
is needed. The Start menu shortcut opens the desktop app. Desktop shortcut and
Explorer **Copy with FCOPY** integration are optional and unchecked by default.
On Windows 11, the classic Explorer verb is under **Show more options**.
Selections are sent to the existing FCOPY window using `--sources`; they do not
silently pick a destination or start a competing queue.

Close FCOPY before upgrading or uninstalling, allowing active jobs to be saved
for recovery. Remove it through Windows Installed apps or the installation's
`unins000.exe`. The uninstaller removes installed program files, shortcuts and
Explorer verbs pointing to that installation. It preserves settings, profiles,
history, recovery state and every transfer destination/partial file.
If another FCOPY copy has since registered Explorer integration, uninstalling
the older installation does not remove that newer registration.

Configuration normally lives in `%APPDATA%\fcopy`, outside the installation.
`FCOPY_CONFIG_DIR` overrides this location. Do not choose a program installation
directory as a data/configuration directory; only program files belong there.
Reinstalling reuses existing user configuration and recovers unfinished jobs
paused. Remote passwords are prompted for again.

Unattended installation (no Explorer integration unless explicitly selected):

```powershell
.\FCOPY-VERSION-windows-x86_64-unsigned-setup.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /TASKS=""
# Opt in: /TASKS="explorer" (or "explorer,desktopicon")
```

## Portable ZIP

Extract the ZIP to a persistent folder and run `fcopy-gui.exe`, or:

```powershell
.\fcopy.exe copy SOURCE DEST --verify readback
```

"Portable" means no installer is required; configuration still uses the normal
user profile unless `FCOPY_CONFIG_DIR` is explicitly set. Extraction and launch
do not register Explorer verbs. To opt in, run:

```powershell
Start-Process .\fcopy-gui.exe -ArgumentList --install-integration -Wait
# Before moving/deleting this folder, undo it:
Start-Process .\fcopy-gui.exe -ArgumentList --uninstall-integration -Wait
```

Keep the executable at that path while its menu is installed. Delete the
extracted folder to remove the program; user configuration and transferred
files remain untouched.

## Build and validate

Install Rust's MSVC toolchain, Visual Studio C++ Build Tools, and Inno Setup 6.
From the repository root:

```powershell
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo build --locked --release -p fc-cli -p fc-gui
.\packaging\windows\build.ps1
.\packaging\windows\test-packages.ps1
```

The repository's `.cargo/config.toml` enables static runtime linking on MSVC
targets only. Do not override it with dynamic-CRT `RUSTFLAGS`. Both the package
builder and smoke test inspect DLL imports with MSVC `dumpbin.exe` and reject
executables that require redistributable Visual C++ runtime DLLs, even when
those DLLs happen to be installed on the build/test machine.

Pass `-Compiler C:\Path\ISCC.exe` if the compiler is not in its standard location.
Packages go to `dist`. The package smoke test uses isolated paths, checks
portable and installed CLI read-back copies, optional Explorer integration,
upgrade behavior, removal and preserved data. It refuses to run if FCOPY's
installer or Explorer verbs are already registered for the current user, so it
cannot overwrite your existing installation/integration. Use a disposable user
or CI runner in that case.

For focus, minimized-window restoration and forwarded GUI copies, run
`packaging\windows\test-single-instance.ps1 -Exe .\target\release\fcopy-gui.exe`
in an interactive desktop. This is separate from headless package validation.
