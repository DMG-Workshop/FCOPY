# Hosted FCOPY: Docker, Unraid and Proxmox LXC

The production `fcopy-web` binary serves **HTTPS on port 8443**. Its local file
browser and copy destinations are on the server, not the browser's computer.
Initial release images and native archives support **Linux x86-64**.

## Persistent state and first login

Keep `/config` and `/data` on different persistent volumes:

* `/config`: private TLS identity, salted account hashes, profiles, copy history,
  unfinished jobs and torrent state. Back this up; do not expose it as a share.
* `/data`: only the files FCOPY should be able to read and modify.

On the first start, an `admin` account gets a randomly generated password printed
**once** in the service/container log. Alternatively mount a private password
file and pass `--admin-password-file /run/bootstrap-password` (or
`FCOPY_ADMIN_PASSWORD_FILE`). Only the password hash is saved. The file is read
only when there are no accounts; changing it does not reset existing users.
Create a personal administrator in Settings, then remove the bootstrap account.
Protect initial-start logs and remove the bootstrap secret after setup.

Accounts, certificate identity and unfinished jobs survive restart. Jobs return
**paused**; use Recovery to Resume, Retry or Discard. Remote passwords must be
entered again and are never saved. Browser sessions and API keys are in memory;
log in again after a restart. One server process may own a config directory.
For upgrades, back up config, stop the old server, replace it, then restart.

The default certificate is self-signed: verify the startup fingerprint before
accepting the browser warning. For a trusted hostname, mount a PEM certificate
chain and unencrypted private key **outside `/data`**, then set
`FCOPY_TLS_CERT` and `FCOPY_TLS_KEY`. Renew and restart the service after replacing
them. A reverse proxy must use an HTTPS upstream; do not disable TLS on the
console. No public certificate or DNS name is provisioned automatically.
Keep this service on a trusted LAN or VPN; do not forward its port publicly.

Torrents use a separate lane. DHT and tracker announces default off; explicit
startup opt-ins are `--torrent-dht` and `--torrent-trackers`.
`--no-torrents` disables the lane. Remote PC discovery uses multicast, which
may not cross Docker bridges or LXC networks; manually enter PC addresses.

## Docker / Compose

Published image: `ghcr.io/dmg-workshop/fcopy-web:0.1.1` (`latest` is also tagged).
Release CI builds and tests the image, including an authenticated restart and
paused recovery, before pushing that exact image to GHCR. A newly created GHCR
package must be made public by its owner before anonymous pulls can work.

Use the repository's `packaging/docker/compose.yml`, or:

```sh
mkdir -p config data
# Choose the intended service UID/GID; the image defaults to 1000:1000.
sudo chown 1000:1000 config data
docker run -d --name fcopy-web --restart unless-stopped \
  --user 1000:1000 --cap-drop=ALL --security-opt=no-new-privileges \
  --stop-timeout 30 -p 8443:8443 \
  -v "$PWD/config:/config" -v "$PWD/data:/data" \
  ghcr.io/dmg-workshop/fcopy-web:0.1.1
docker logs fcopy-web
```

Open `https://SERVER:8443`. Compose allows `FCOPY_UID` / `FCOPY_GID` overrides.
Pre-create writable bind directories; Docker-created root-owned directories
will correctly produce an explicit startup error, not be recursively chowned.
Do not give the container the Docker socket, privileged mode, host root mounts
or all host shares. Running as root is unnecessary.

Build locally with `docker build -f packaging/docker/Dockerfile -t fcopy-web:test .`;
test with `bash packaging/docker/test.sh` (Docker, curl, jq and cmp required).

## Unraid

Use the release asset `fcopy-unraid.xml` as a user Docker template (copy it to
`/boot/config/plugins/dockerMan/templates-user/my-fcopy.xml` on the Unraid host),
then select it in **Docker > Add Container > User templates**.
This is a user template, not a claim of Community Applications store inclusion.

Defaults:

* HTTPS host/container port 8443.
* `/mnt/user/appdata/fcopy` mapped to `/config`.
* `/mnt/user/fcopy` mapped to `/data` (change this to a **specific** intended share).
* Unraid's `nobody:users`, numeric **99:100**, nonprivileged with dropped caps.

Create these directories with the selected user's write permission before
starting. Adjust the template's `--user` setting if your share uses another
owner; there is no PUID/PGID startup chown shim. Read the first-start container
log for login and fingerprint. Preserve appdata when updating/removing the
container. Optional secret/certificate mounts must be read-only and outside
`/data`; add the corresponding environment variables in the template editor.

## Proxmox: native unprivileged Debian LXC

The release is a **native service archive plus installer**, not a prebuilt LXC
root filesystem or a privileged nested Docker appliance. Use an x86-64 Debian
12 or 13 guest. No nesting, privileged container or Docker daemon is required.

On the Proxmox host, choose a free CT ID, available Debian template, appropriate
storage and network bridge. For example (replace the template with one actually
listed by your host):

```sh
pveam update
pveam available --section system | grep debian
# Download your selected template with: pveam download local <TEMPLATE-NAME>
pct create 120 local:vztmpl/<TEMPLATE-NAME>.tar.zst \
  --hostname fcopy --unprivileged 1 --cores 2 --memory 2048 --swap 512 \
  --rootfs local-lvm:8 --net0 name=eth0,bridge=vmbr0,ip=dhcp --onboot 1
pct start 120
```

Download `fcopy-web-0.1.1-linux-x86_64.tar.gz` and `SHA256SUMS` from the same
GitHub release. Check its checksum on the download machine, then:

```sh
sha256sum --ignore-missing -c SHA256SUMS
pct push 120 fcopy-web-0.1.1-linux-x86_64.tar.gz /tmp/fcopy-web.tar.gz
pct exec 120 -- tar -xzf /tmp/fcopy-web.tar.gz -C /tmp
pct exec 120 -- bash /tmp/fcopy-web-0.1.1/install-lxc.sh
pct exec 120 -- journalctl -u fcopy-web --no-pager
```

The installer creates an unprivileged `fcopy-web` service user, private state
at `/var/lib/fcopy-web`, data at `/srv/fcopy`, and a sandboxed systemd unit.
The guest needs Internet access for Debian runtime dependencies. Open
`https://<GUEST-IP>:8443`, allowing TCP 8443 in your Proxmox/LAN firewall.
Service management: `systemctl status|restart|stop fcopy-web`.
To stage files without starting, pass `--no-start`.

Optional host bind mounts: stop the service first and mount only an intended
directory at `/srv/fcopy` using `pct set 120 -mp0 /HOST/DIRECTORY,mp=/srv/fcopy`.
With the default unprivileged mapping, guest UID/GID N map to host 100000+N;
custom mappings differ. Check `id fcopy-web` inside the guest and the CT's
actual idmap, then grant narrowly scoped host ACLs for that mapped identity.
Do not recursively chown existing shared storage or make the guest privileged
to bypass ownership. The installer does not change an existing data directory's
ownership. Back up bind-mounted data separately; it is not included in normal
LXC root-disk backups.
