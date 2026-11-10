# Manual test: disk erase authorization on Ubuntu 26.04 Desktop

This checklist tests the polkit gate in front of `fcopy-storage-worker`, the GNOME
password prompt, and one erase of a spare USB stick through the desktop app's
Disks tab. It covers what automated tests cannot: a real administrator prompt in an
active desktop session.

Run it on a test machine or VM, never on a machine whose data you need.

> **The erase step destroys all data on the USB stick you choose.** Pick the stick
> before you start, unplug other removable disks, and check the model, size and
> serial at each stage.

Polkit shows the password prompt only to an **active local session**. The policy sets
`allow_inactive` to `no`, so SSH and other remote sessions are refused without a
prompt. Run the positive tests from the desktop itself.

## What was already verified elsewhere

In a container with polkit but no login session (2026-11-03):

- A non-root user with no authentication agent is refused: `Not authorized`, exit 127, the worker never runs.
- An administrator with a terminal but no active session is refused before any prompt.
  A control action whose policy allows inactive sessions (`org.freedesktop.packagekit.package-install`)
  asked for authentication under the same conditions, which confirms the cause.
- Root through `pkexec` reaches the worker over the app's client path, and the worker's
  refusal event comes back.
- The worker with empty stdin prints `{"Finished":{"Failed":"malformed input: the key is not 32 bytes of hex"}}` and exits 1.

The password prompt itself has not been seen. This checklist is what closes that gap.

## 0. Prerequisites

- [ ] Ubuntu 26.04 LTS Desktop, logged in locally as a user in the `sudo` group.
- [ ] This repository checked out at `~/FCOPY`.
- [ ] Rust installed for your user (`rustup`).
- [ ] A spare USB stick, at least 1 GB, with nothing on it you need.

```bash
sudo apt-get update
sudo apt-get install -y policykit-1 build-essential pkg-config libxkbcommon-dev libwayland-dev
```

System library names for the GUI build can differ on 26.04. If the build stops on a
missing library, install it and build again.

## 1. Build the worker and the desktop app

```bash
cd ~/FCOPY
cargo build -p fc-storage --features storage-destructive --bin fcopy-storage-worker --release
cargo build -p fc-gui --features storage-destructive --release
```

- [ ] `target/release/fcopy-storage-worker` exists.
- [ ] `target/release/fcopy-gui` exists.

## 2. Install the worker and the polkit policy

```bash
sudo install -d /usr/libexec/fcopy
sudo install -m 0755 target/release/fcopy-storage-worker /usr/libexec/fcopy/fcopy-storage-worker
sudo install -m 0644 packaging/linux/org.fcopy.storage.policy /usr/share/polkit-1/actions/
pkaction --action-id org.fcopy.storage.destroy --verbose
```

- [ ] The output shows `implicit active: auth_admin` and `implicit inactive: no`.
- [ ] The `org.freedesktop.policykit.exec.path` annotation points to `/usr/libexec/fcopy/fcopy-storage-worker`.

## 3. Confirm the desktop session is active

In a terminal on the desktop:

```bash
loginctl show-session "$XDG_SESSION_ID" -p Active -p Remote -p Type
```

- [ ] `Active=yes`, `Remote=no`, and `Type=wayland` or `Type=x11`.
- [ ] If `Active=no`, stop. The prompt cannot appear until the session is active.

## 4. Negative test: a remote session is refused

From another machine, SSH in as the same user:

```bash
pkexec /usr/libexec/fcopy/fcopy-storage-worker </dev/null; echo "exit=$?"
```

- [ ] No password prompt appears.
- [ ] Output shows `Not authorized`, and `exit=127`.

## 5. Positive test: the GNOME password prompt

In a terminal on the desktop:

```bash
pkexec /usr/libexec/fcopy/fcopy-storage-worker </dev/null; echo "exit=$?"
```

- [ ] A GNOME dialog asks for a password and shows "Authentication is required to erase, partition or format this disk".
- [ ] **Cancel** the dialog. Expected: `exit=127`, no worker output.
- [ ] Run it again and enter a **wrong** password. Expected: the dialog reports a failure; after it closes, `exit=127` and no worker output.
- [ ] Run it again and enter the **correct** password. Expected:
  `{"Finished":{"Failed":"malformed input: the key is not 32 bytes of hex"}}` and `exit=1`.
  That message means the worker ran as root and refused the empty input, which is correct.

## 6. Erase a spare USB stick through the Disks tab

- [ ] Plug in the spare USB stick. Note its model and size.
- [ ] Start the desktop app:

```bash
cd ~/FCOPY
./target/release/fcopy-gui
```

- [ ] Open the **Disks** tab. Find the stick by model and size.
- [ ] Select the system disk once. Expected: a red reason, "This disk hosts the running system.", and no erase controls.
- [ ] Select the stick. Expected: no red reason, a method selector, and an instruction line that shows its serial number.
- [ ] Leave **Clear** selected. Type the serial with one wrong character. Expected: **Erase disk** stays disabled.
- [ ] Correct the serial. Expected: **Erase disk** becomes enabled.
- [ ] Click **Erase disk**. Expected: the status line says it is waiting for the administrator, and the GNOME password dialog appears.
- [ ] Enter your password. Expected: the status line changes to "Erase completed and verified.", and the disk list refreshes.
  If it says refused or failed, copy the exact message.
- [ ] Check that the stick carries no filesystem. Replace `sdX` with the device shown in the Disks tab:

```bash
sudo blkid /dev/sdX; echo "blkid exit=$?"
```

- [ ] Expected: no output and `blkid exit=2`, because Clear writes zeros over the whole device.
- [ ] Check the audit record. The worker writes it before the first write to the disk, and creates `/var/lib/fcopy` itself if it is missing:

```bash
sudo tail -5 /var/lib/fcopy/storage-audit.jsonl
```

- [ ] Expected: a `begin` record with the disk id, `"method":"Clear"` and `"scope":"WholeDevice"`,
  then an `end` record with `"outcome":"Completed"`. The typed serial does not appear.

## 7. Check the system log

```bash
journalctl --since "-60min" | grep -iE "polkit|pkexec|fcopy" | tail -40
```

- [ ] One record for each cancelled or failed authentication, and one for each successful one.

## 8. Clean up

```bash
sudo rm -rf /usr/libexec/fcopy
sudo rm -f /usr/share/polkit-1/actions/org.fcopy.storage.policy
pkaction --action-id org.fcopy.storage.destroy
```

- [ ] `pkaction` now reports that the action does not exist.
- [ ] Keep `/var/lib/fcopy/storage-audit.jsonl` if you want a record of the test; otherwise remove it.

## Results to report

| Step | Record |
|------|--------|
| 3 | `Active`, `Remote`, `Type` values |
| 4 | Message and exit code |
| 5 | Whether the dialog appeared; result of cancel, wrong password, correct password |
| 6 | System-disk refusal text; status message after the erase; `blkid` output and exit code; audit record |
| 7 | Any unexpected log entries |
