# fcopy: project context for Claude Code

A TeraCopy-style file copy and verify tool in Rust for Windows, macOS and Linux.
It started in a claude.ai conversation. This file carries that context over:
what exists, the decisions behind it, and the exact plan for the unfinished
remote-transfer work.

## Workspace

```
crates/
  fc-engine/   core library: no UI, no async. Copy pipeline, checksums, space check,
               Retry/Skip/Cancel recovery, pause/resume/skip, rate limiter, queue,
               telemetry, platform layer (long paths, names, metadata, ACLs)
  fc-cli/      `fcopy` binary (clap): copy / hash / check
  fc-gui/      `fcopy-gui` binary (egui 0.32): dashboard, queue, log drawer, decisions,
               desktop integration (--install-integration)
  fc-remote/   IN PROGRESS: QUIC (quinn) remote transfer, SCRAM-Argon2 auth
  fc-torrent/  BitTorrent engine over librqbit; its own lane beside the copy queue
packaging/     .desktop/servicemenu/nemo, PKGBUILD, deb/rpm metadata (in fc-gui/Cargo.toml),
               Windows manifest/.rc/.reg, macOS Info.plist + bundle README, icon
docs/          PLATFORM.md (OS quirks guide), gui-screenshot.png
```

Dependency direction: `fc-cli`, `fc-gui` → `fc-remote` → `fc-engine`. Nothing
depends upward; `fc-engine` must stay UI-free and sync.

## Commands

```sh
cargo test --workspace              # 127 on Linux, 117 on Windows; all passing
cargo clippy --workspace --all-targets   # zero warnings expected
cargo run -p fc-cli -- copy SRC... DEST [--verify readback] [--limit 10MB] [--on-error ask|auto|skip|cancel]
cargo run -p fc-gui -- [--limit 20MB] [--verify] SRC... DEST
```

Headless GUI check (Linux): `Xvfb :99 &` then `DISPLAY=:99 LIBGL_ALWAYS_SOFTWARE=1 fcopy-gui …`
and take screenshots with `import -window root`.

## Conventions and decisions (keep these)

- MSRV 1.85, edition 2024. egui pinned to 0.32 (0.37 needs Rust 1.95).
- Prefer complete-file edits; the owner reviews whole files.
- Platform code: pure functions (tested everywhere) plus thin `#[cfg]` adapters.
  **Windows (MSVC, Win 11) builds and passes** `cargo test --workspace` + clippy
  (117 tests; the rest are `cfg(unix)`). That first Windows run found read-back
  verify breaking on the `FILE_FLAG_NO_BUFFERING` handle (unaligned tail read),
  now fixed. **macOS branches have still never been compiled**: first thing on a
  Mac, `cargo test --workspace`. Affected: `fc-engine/src/platform/meta.rs`,
  `cache.rs`, `fc-gui/src/integration.rs`, `fc-remote/src/config.rs` (dir helpers).
  On Windows the GUI's runtime behavior (context menu, `.rc` embedding) is built but not exercised.
- Copies write `name.fcpart`, then fsync, then read-back verify (cache dropped),
  then metadata, then permissions **last** (Windows read-only cleanup), then an
  atomic rename.
- A failed fsync is never retried on the same fd (fsyncgate): the whole file is re-copied.
- Metadata never fails a copy; it produces warnings instead.
- Queue: commands go through `Mutex<QState>` + `Condvar`; events go out on an `mpsc`
  channel; telemetry is throttled at the source (~10 Hz). The GUI drains with
  `try_recv` and repaints via the `on_event` → `request_repaint` hook.
- Desktop integration always launches `fcopy-gui --sources <paths…>`; without
  `--sources` the last selected file would be taken as the destination.

## Benchmarks (cloud VM, ext4, cold cache, timings include `sync`)

| Case | cp | fcopy |
|---|---|---|
| 2 GiB single file | 1.94 s best | 1.67 s (default, 1 MiB × 4) |
| 2 GiB + verify | cp + drop cache + cmp: 3.42 s | readback xxh3: 2.23 s; sha256: 4.31 s; md5: 8.35 s |
| 20k small files | 1.27 s | 1.76 s (**slower**: per-file thread hand-off + metadata) |
| 20k small files + readback | n/a | 7.1 s (**fsync per file dominates**) |

Known performance to-dos: a small-file fast path (batch, no per-file fsync for
readback, use `syncfs`/batched fsync), parallel small files, kernel fast paths
(reflink/`copy_file_range`/`CopyFileExW`/`clonefile`).

## fc-remote status

Done and tested (26 unit tests):
- `protocol.rs`: framing `[u32 len][kind][payload]`; MSG = postcard `Message`, DATA = `u64 offset ‖ bytes`; ALPN `fcopy/1`, port 47800
- `crypto.rs`: SCRAM with Argon2id KDF + TLS-exporter channel binding, decoy verifiers, fingerprints
- `tls.rs`: self-signed identity (rcgen), `PinnedVerifier` (TOFU / pin / hard fail on change), QUIC transport config
- `config.rs`: `AgentConfig` (shares, accounts interactive/unattended, LAN-only default allowlist, limits), 0600 writes, `KnownHosts`
- `access.rs`: per-IP + per-account login lockout, single-use session tokens
- `server.rs`: accept loop (allowlist + lockout refusal before TLS), auth, control
  stream (pause/resume both ways; the server enforces pause via flow control), per-file
  streams with contiguous-offset checks, `O_NOFOLLOW` temp files, symlink-escape
  check, resume from `.fcpart` + `.fcpart.id` (64 KiB aligned), server-side re-read
  verify, `TestHooks::drop_after_bytes` for dropout tests

Engine hooks already added for remote: `TransferBackend` trait + `Target::Backend`
(queue plans with *relative* dst paths and calls `backend.run`), public `Progress`
per-file hooks, `Progress::count_file` (for concurrent small files),
`Progress::set_link(LinkStats)` → `Snapshot.link`, `RateLimiter::reserve` (async-friendly),
`Digest::new`, `CopyError::Backend`.

### Remaining work, in order

1. **`client.rs`**
   - `RemoteSpec::parse("[user@]host[:port]:/share/path")`, including `[ipv6]`. Reject a one-letter head (a `C:/` drive).
   - `Session::open(connector, Option<token>)`: `Endpoint::client`, `PinnedVerifier`. If there is no pin, call the trust callback with the fingerprint **before** auth.
   - SCRAM: `Hello`, then `AuthStart`, `AuthChallenge` (sanity-bound the KDF: m ≤ 1 GiB, t ≤ 10, p ≤ 8), then `ClientKeys::derive` in `spawn_blocking`, then `AuthProof`, then `AuthOk`, then `verify_server`. Token path: `AuthResume`.
   - Control stream: a reader task sets `remote_paused`; a writer gets an mpsc and sends `Pause`/`Resume` when the local `JobControl` pause state changes.
   - `push_file`:
     - `source_id = sha256(path|size|mtime)[..16]`.
     - Open a bi stream and send `FileOffer`; expect `FileAccept{resume_from}`.
     - Hash the local prefix, then stream `DATA` frames.
     - Run send and ack-read concurrently (`try_join!`); update progress on ack.
     - Send `FileEnd{digest}`, expect `FileDone` and compare digests.
     - On skip: `send.reset()`.
   - Gate before each chunk: cancel → `Cancelled`; `take_skip` → `Skipped`; local or remote pause → sleep 50 ms; `limiter.reserve(n)` → `tokio::time::sleep`. Never call the blocking `acquire` inside async code.
   - Errors: `Connection` (retry after reconnect), `Rejected`, `Local`, `Mismatch` (retry once with a fresh `source_id`), `Cancelled`, `Skipped`.
2. **`backend.rs`**: `RemoteBackend` implements `TransferBackend`.
   - Owns a multi-thread tokio `Runtime` and uses `block_on`.
   - `set_job_totals(files, bytes, false)`.
   - Order: dirs (`EntryKind::Dir`), then small files (< 4 MiB) with `buffer_unordered(8)` using `count_file`, then large files sequentially with `begin_file`/`finish_file`.
   - Reconnect loop: token first, then the password; backoff 0.5 s → 10 s, up to 120 s total; stop on cancel. Count reconnects into `LinkStats`.
   - Update `LinkStats` from `conn.stats()` (`path.rtt`, `cwnd`, `lost_packets`).
   - Remote path = `spec.path` + rel components joined with `/`.
   - Algorithm: MD5 → XXH3 (MD5 isn't offered remotely).
   - Needs `futures-util`.
3. **Integration tests** over loopback QUIC (use `crypto::TEST_KDF`-style cheap params in the config):
   - auth ok, wrong password, unknown user (same error), lockout;
   - push a tree byte-identically;
   - `drop_after_bytes`, then auto-reconnect resumes with `resume_from > 0` and the result is identical;
   - path traversal rejected; allowlist refusal; pin mismatch fails; `pause_all` stalls the client.
4. **`fcopy-agent` binary** (clap):
   - `init`, `serve`, `user add|del|list` (password prompt via `rpassword`), `unattended enable` (prints a `generate_access_key()` key once), `share add|del|list`, `allow add|del`, `fingerprint`.
   - Service files: systemd unit (`StateDirectory=fcopy-agent`, `ProtectSystem=strict`, `ReadWritePaths=` for the shares, `NoNewPrivileges`), launchd plist, Windows service via the `windows-service` crate (cfg(windows), unverified).
5. **`discovery.rs`** (mdns-sd 0.21): advertise `_fcopy._udp.local.` with TXT `fp=`, `ver=1`; `browse(timeout) -> Vec<Peer>`. Check the `ServiceEvent` API in the vendored source.
6. **CLI**:
   - `fcopy copy SRC… user@host:/share/dir` with `--password` (warn that it's visible in `ps`), `--password-file`, `FCOPY_PASSWORD`, or a prompt.
   - `--accept-fingerprint SHA256:…`; known_hosts in `config::default_client_dir()`.
   - `fcopy peers`.
7. **GUI**:
   - Remote tab: discovered peers, saved profiles (host, port, user, pinned fingerprint; store no raw passwords; use the OS keyring or prompt).
   - Copy-to selector: Local path / Remote PC.
   - Dashboard link panel when `Snapshot.link` is present: RTT, acked vs sent, reconnects, "paused by remote".

Not planned yet: pull (download) direction (add a `FileRequest` message mirroring
`FileOffer`), single-instance IPC for the GUI, the Windows 11 modern context menu
(IExplorerCommand + sparse MSIX).

## Status: remote agent work (updated)

Done since the handoff, each with tests:
- `RemoteBackend` (fc-remote/backend.rs): queue jobs to a paired PC. Reconnect with token then password, resume, identity refusal.
- `fcopy-agent` (fc-agent): init, serve, fingerprint, share, user, unattended, allow, service install/uninstall/run (Windows service verified on this PC). systemd and launchd files in packaging/.
- LAN discovery (fc-remote/discovery.rs): advertise and browse `_fcopy._udp.local.`. The agent announces itself when `discovery = true`.
- `fcopy copy SRC user@host:/share/dir` with password from file, env or prompt. `--accept-fingerprint`, trust saved after confirmation. `fcopy peers`.
- Web console: Remote node mode in Add task; HTTPS only, with the agent's certificate.
- Desktop GUI: Copy to a PC on the network; redesigned window (tab bar, sidebar, queue table, status bar).

Still open:
- Windows service: verified on this PC (elevated): install, start, listening on UDP 47800, stop, uninstall. It writes no logs yet. End to end (packaging/windows/test-service.ps1): a file copied over QUIC into the running service matched byte for byte.
- Pull (download) direction, not planned yet.
- BitTorrent (fc-torrent, librqbit 9): own lane, not the copy queue, since a torrent
  holds the one-job worker until its download finishes. Web console: admin-only tab
  with magnet and .torrent add (optional peer addresses), pause, resume, remove,
  delete data; rows arrive in telemetry frames. Desktop tab still disabled.
  Gaps: no persistence across restarts; a magnet has no row until its file list
  arrives from peers; trackers and DHT off by default.
- Public certificate for the web console; the self-signed one needs trusting once.
