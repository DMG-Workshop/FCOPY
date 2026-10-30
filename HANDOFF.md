# fcopy handoff

Updated 2026-10-30. Repository: `C:\Users\Neraxis\Desktop\Projects\filecopy`.
Branch: `web-dashboard`. Latest local commit: `feaa010` (`Complete torrent
controls and service diagnostics`). The working tree was clean after that commit.
No changes were pushed.

## Project status

fcopy is a Rust workspace for local file copy and verification, authenticated
QUIC transfers between paired PCs, an agent service, a web management console,
an egui desktop GUI, and a separate BitTorrent lane.

### BitTorrent

- Engine uses librqbit 9 in `crates/fc-torrent`; torrents stay outside the
  local copy queue.
- Web console has an admin-only BitTorrent tab. It accepts magnet links and
  `.torrent` files, supports optional peer addresses and per-transfer save
  folders, and offers pause, resume, remove, and delete-data actions.
- Desktop GUI has a separate BitTorrent tab/runtime with equivalent add and
  transfer controls.
- Magnets appear immediately as “Waiting for peers”. They retain the same row
  and ID when metadata arrives, and can be paused, resumed, or removed while
  waiting. Pausing stops metadata discovery; resuming restarts it.
- Torrent state persists when a state directory is configured, including
  paused/finished transfers and unresolved magnets.
- DHT and tracker announces are off by default. Explicit launch flags are
  `--torrent-dht` and `--torrent-trackers` for the desktop app and web serve
  example. Direct peer addresses work without enabling either.

### Remote transfer and web security

- `fc-remote` provides QUIC/TLS transfers, authentication, certificate pinning,
  reconnect/resume, and LAN discovery. `fc-agent` receives transfers and has a
  Windows service implementation.
- The web console uses HTTPS by default. `fc-web` can load a PEM certificate
  chain and unencrypted private key; the serve example accepts `--tls-cert`,
  `--tls-key`, and `--bind`. Plain HTTP is loopback-only.
- A real deployment still needs a certificate and DNS name supplied by its
  operator.

## Verification

On 2026-10-30, these checks passed on Windows:

- `cargo test --workspace` — all workspace test binaries passed, including all
  9 torrent loopback tests, remote reconnect tests, web routes, and GUI tests.
- `cargo clippy --workspace --all-targets` — clean.
- `rustfmt --edition 2024 --check` on the touched Rust files.
- PowerShell parser check for `packaging/windows/test-service.ps1`.

Put Cargo's rustup directory first on PATH before building. In PowerShell:

```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
cargo test --workspace
cargo clippy --workspace --all-targets
```

Torrent loopback tests need local peer networking and filesystem access. If
they fail inside a restricted sandbox, run them outside it.

## Remaining work and limitations

1. **Windows service log end-to-end check:** run from Administrator PowerShell
   at the repository root:

   ```powershell
   powershell -NoProfile -ExecutionPolicy Bypass -File .\packaging\windows\test-service.ps1
   ```

   The script checks elevation and uses a unique temporary agent state; it
   does not modify the existing `C:\ProgramData\fcopy-agent` configuration.
   This check cannot be completed from a non-elevated session.
2. macOS has not been compiled or tested here.
3. The broader small-file copy performance work remains open. A Windows
   20,000 × 128-byte-file benchmark did not show a reproducible gain from the
   single-buffer experiment, so that code was removed.
4. Remote pull/download direction is not implemented or currently planned.
5. Desktop “Torrent feeds” and “Settings” entries are separate disabled
   placeholders; they are not part of the BitTorrent feature.

## Useful commands

- Run the web console locally with HTTP for development only:
  `cargo run -p fc-web --example serve -- <share-root> 8080 --http`
- Agent service test: see the elevated PowerShell command above.
- Desktop torrent state uses `FCOPY_CONFIG_DIR` when set; downloads default to
  the user's Downloads directory.
