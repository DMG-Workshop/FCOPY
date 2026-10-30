# fcopy

Fast, verified file copying for Windows, macOS and Linux: a TeraCopy-style
engine with a CLI, an egui GUI, and a web console. Upload and download files
between paired PCs over authenticated QUIC.

![GUI](docs/gui-screenshot.png)

```sh
cargo test --workspace
cargo run --release -p fc-cli -- copy ~/Photos /mnt/backup --verify readback --limit 50MB
cargo run --release -p fc-gui
```

* Pipelined copy with pre-allocated aligned buffers, atomic `.fcpart` + rename
* XXH3 / SHA-256 / MD5, read-back verification that bypasses the page cache
* Free-space pre-check, Retry / Skip / Cancel recovery, pause / resume / skip-file
* Live speed limit, throttled telemetry (speed, ETA, percent), job queue
* Metadata, long paths and Windows-safe names: see [docs/PLATFORM.md](docs/PLATFORM.md)

## Download from a PC

The remote PC must run a current `fcopy-agent` with an account and a share.
Confirm its fingerprint before connecting. Passwords/access keys are prompted
for or supplied per operation, not saved in profiles.

* **Desktop:** Add task → Download from PC. Enter the PC's address and credentials,
  browse its shares, add files or folders, choose a local destination folder,
  then start the copy.
* **Web console (admins):** Add task → Download from paired PC. Choose the paired
  PC, enter its credentials, browse shares, and queue the selection. The destination
  is on the **console host**, inside a configured local share—not on the browser's
  machine.
* **CLI:** `fcopy copy alice@nas:/backup/photos DEST` downloads a file or folder.

Desktop and web downloads use the normal queue: pause/resume, cancel, skip
(desktop), live bandwidth limits, progress, and per-file results. They reconnect
and resume interrupted files, re-read and compare downloaded bytes before committing,
and include empty directories. Read-only remote shares are supported. Existing
local links/junctions and colliding destination names are rejected rather than
followed or overwritten implicitly. Remote files are transferred sequentially.

Developer context and roadmap: [CLAUDE.md](CLAUDE.md).

## VPN settings and private PC links

**Desktop Settings / VPN** and **web Settings / VPN** link to official downloads
and help for NordVPN, ExpressVPN, Surfshark, Proton VPN, Mullvad, Private Internet
Access, CyberGhost, Windscribe, TunnelBear, and IPVanish. This is a representative
selection of ten services, not a ranking or endorsement.

The desktop can save an installed app's absolute executable path per provider
and open it on demand. On macOS an `.app` bundle is also supported. Use a path
without quotes or command-line arguments; an empty path removes the saved link.
App links and an optional remote target are saved in `vpn-links.toml` in the
client configuration directory. No VPN credentials are collected or saved.
Web links open on the **browser's device**; the console never launches programs
on its host. Install and connect the VPN on the **console host** for web transfers.

For private PC-to-PC transfers, select **Tailscale** or **ZeroTier** in VPN settings:

1. Follow the [Tailscale quickstart](https://tailscale.com/docs/how-to/quickstart)
   or [ZeroTier guide](https://docs.zerotier.com/start/) on both PCs. Join the same
   private network and authorize both devices.
2. Run `fcopy-agent` on the remote PC with an account and a share. Its listen
   address must include the VPN interface. Permit its UDP port (47800 by default)
   in the host firewall and mesh access rules. Check its configured allowlist;
   if necessary, allow only the connecting PC's VPN address, not the public internet.
3. Enter `alice@VPN-IP-OR-DNS:47800:/backup` in VPN settings (bracket IPv6 addresses).
   LAN discovery is not required and may not cross the VPN.
4. Desktop: choose upload or download and confirm the agent fingerprint in the
   transfer form. Web admins: **Pair PC over VPN**, confirm the fingerprint,
   authenticate, then use the saved PC in Add task. Normal queue controls apply.

These are setup links and existing QUIC transfers over an OS-managed network,
not an embedded VPN client. A consumer VPN subscription alone does not join your
PCs into a private network. FCOPY does not automatically connect tunnels, check
VPN connection status, bind transfers to a VPN interface, or implement a kill
switch. Sign in/connect in the provider's app first; check split tunneling and
other VPN clients if the mesh address is unreachable.
