# fcopy handoff

Last updated 2026-10-30. Branch `web-dashboard`. BitTorrent UI review and
JavaScript syntax check are complete. Waiting magnets can now be paused and
resumed before metadata arrives; loopback coverage passes. The working tree
contains uncommitted service logging, TLS certificate loading, torrent
discovery options, and documentation changes.

## Where things stand

Local commits on top of `1f13ddc`, none pushed:

| Commit | What |
|---|---|
| `1cd4848` | fc-web: BitTorrent tab with magnet and .torrent add |
| `5b084b9` | fc-torrent: waiting magnets and restart persistence |
| `d21c007` | fc-gui: BitTorrent tab; per-transfer save folder |

Git remote `origin` is set to `https://github.com/DMG-Workshop/FCOPY`.
The push was rejected: GitHub's "Block command line pushes that expose my
email" setting refuses the author address `gallagher.dennis@gmail.com`.
Either turn that setting off, or rewrite the author on these commits to a
GitHub noreply address and push again. Rewriting changes history, so ask first.

## What works

- **Copy engine, CLI, remote transfer, agent, web console and desktop copy
  queue**: as in CLAUDE.md. Unchanged this round.
- **BitTorrent engine** (`crates/fc-torrent`, librqbit 9):
  - Magnet links are listed at once as "Waiting for peers", keep their id
    when the file list arrives, and can be paused, resumed, or removed while
    waiting. Pause stops metadata discovery but preserves the row/id; resume
    starts discovery again.
  - A saved state directory keeps finished transfers (librqbit persistence,
    including paused state) and waiting magnets (`pending-magnets.json`).
  - Downloads can go to a chosen folder per transfer.
  - Peers can be given for a magnet or a .torrent file, since trackers and
    DHT are off by default.
- **Web console** (`crates/fc-web`): admin-only BitTorrent tab with add by
  magnet or file, pause, resume, remove, delete data. Rows arrive in telemetry
  frames. Checked in a browser: a waiting magnet survived a server restart.
- **Desktop window** (`crates/fc-gui/src/torrent.rs`): BitTorrent tab on its own
  runtime, with list, add form (magnet or .torrent path, peers, save folder),
  pause, resume, remove, delete data.

## Tests and checks (2026-10-30)

- `cargo test --workspace`: all test binaries passed, including 9/9 torrent
  loopback tests, GUI (13), remote loopback/reconnect tests, and web torrent
  routes. Run with Cargo's rustup directory first on PATH.
- `cargo clippy --workspace --all-targets`: clean.
- Torrent loopback coverage includes pausing a waiting magnet, verifying its
  paused/pending state and stable id, resuming discovery, and removing it.

## Not done or not verified

- **Windows service log assertion** still needs an elevated run of
  `powershell -NoProfile -ExecutionPolicy Bypass -File .\packaging\windows\test-service.ps1`.
  The script now uses unique temporary agent state so it will not modify the
  existing `C:\ProgramData\fcopy-agent` configuration.
- **Windows service log assertion** is the only immediate machine action still
  awaiting an elevated run. The sandbox blocks local peer sockets and some
  temporary-file operations, so run torrent loopback tests outside it if
  investigating failures.
- **Public certificate deployment** still requires the operator to supply a
  certificate for the chosen DNS name. The code now accepts a PEM chain and
  unencrypted key through `--tls-cert` and `--tls-key` in the serve example.
- **Small-file optimization experiment:** a 20k × 128 B Windows before/after
  comparison was noisy and the single-buffer change did not improve the
  median, so the change was removed. The broader performance work remains open.
- **Torrent tests need loopback networking and temporary filesystem access.**
  Run outside a restricted sandbox if peer connections or persistence fail.
- Desktop "Torrent feeds" and "Settings" entries are still disabled. They
  are not BitTorrent.
- Other open items from CLAUDE.md: macOS has not been compiled, no pull
  (download) direction is planned, and larger copy performance work remains.

## Things to know

- Windows, no `python`. Use PowerShell or `sed`/Bash for scripted edits.
- Put `%USERPROFILE%\.cargo\bin` first on PATH, or cargo picks up the
  Chocolatey Rust (see the memory note on dlltool).
- Commits use `gallagher.dennis <gallagher.dennis@gmail.com>` (the author of
  earlier commits here) plus the Co-Authored-By line for Claude.
- Test console: `cargo run -p fc-web --example serve -- <share-root> 8080 --http`.
  It prints a throwaway admin password. Downloads go to `<share>/BitTorrent`,
  and state goes to the agent folder, so restarts keep the same transfers.
- Desktop app state: `FCOPY_CONFIG_DIR` overrides the client folder, which
  holds `torrents/` state. Downloads default to the user's Downloads folder.
- A Firefox window on another account was left on a sign-in page during the
  desktop check. Nothing was typed into it.

## Next steps, in order

1. Decide on the push: change the GitHub email setting, or rewrite the author
   and push `web-dashboard` to `origin`.
2. Run the elevated Windows service test above to verify `service.log` end to end.
3. For a deployment, provide the TLS PEM chain/key and the matching DNS name.
4. Continue the remaining scoped copy performance work; do not reintroduce the
   single-buffer experiment without a reproducible win.
