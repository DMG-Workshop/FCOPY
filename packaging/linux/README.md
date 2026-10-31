# Linux packages and releases

The packages contain **fcopy (CLI) and fcopy-gui**, desktop/application metadata,
the icon, Dolphin and Nemo context-menu entries, and the README. They do not
install or enable `fcopy-agent`, a web server, or any background service.
GNOME Files users can install the per-user Scripts entry with
`fcopy-gui --install-integration`.

## Downloads and installation

Tagged builds publish the following to
[GitHub Releases](https://github.com/DMG-Workshop/FCOPY/releases) **only after all
Linux build and installation tests pass**:

| Format | Release asset | Target |
|---|---|---|
| Debian package | `fcopy_X.Y.Z_amd64.deb` | Ubuntu 22.04 and 24.04 |
| RPM | `fcopy-X.Y.Z-1.x86_64.rpm` | Fedora 44 |
| Arch package | `fcopy-X.Y.Z-1-x86_64.pkg.tar.zst` | Current Arch Linux |
| AppImage | `FCOPY-X.Y.Z-x86_64.AppImage` | x86-64 glibc Linux, baseline Ubuntu 22.04 |

`SHA256SUMS` covers the four downloads. Verify from the download directory:

```sh
sha256sum --check --ignore-missing SHA256SUMS
sudo apt install ./fcopy_X.Y.Z_amd64.deb
# Or, on Fedora:
sudo dnf install ./fcopy-X.Y.Z-1.x86_64.rpm
# Or, on Arch:
sudo pacman -U ./fcopy-X.Y.Z-1-x86_64.pkg.tar.zst
```

These are upstream release assets, not an Ubuntu/Fedora repository or an AUR
submission. Native dependencies are installed by the distro package manager.
There are no automatic package updates or package signatures yet; checksums
detect damaged downloads but are not an independent authenticity signature.

For AppImage:

```sh
chmod +x FCOPY-X.Y.Z-x86_64.AppImage
./FCOPY-X.Y.Z-x86_64.AppImage
# Without FUSE:
APPIMAGE_EXTRACT_AND_RUN=1 ./FCOPY-X.Y.Z-x86_64.AppImage
# Optional file-manager entries (put the image in its permanent location first):
./FCOPY-X.Y.Z-x86_64.AppImage --install-integration
```

The AppImage bundles application/window-system libraries, not glibc, graphics
drivers, or a display server. It still needs a working desktop, OpenGL/EGL
drivers and trusted system CA certificates. It is not a sandbox and does not
provide a fully portable home/configuration directory. Configuration and the
single-instance window use the normal client directory; `FCOPY_CONFIG_DIR`
overrides that directory. Moving the AppImage requires reinstalling integration.

## Building DEB, RPM and AppImage

Use Ubuntu 22.04 x86-64 for the release baseline. A newer build host can introduce
glibc/OpenSSL requirements that older distros cannot satisfy. The CI install
matrix is the supported compatibility check, not a claim that every Linux distro
or CPU architecture has been tested. The Arch recipe can also build from source
on its declared architectures; only x86-64 is built and published by CI.

```sh
sudo apt-get update
sudo apt-get install -y build-essential cmake pkg-config libssl-dev \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-dev \
  libxcursor-dev libxi-dev libxrandr-dev libgl1-mesa-dev libegl1-mesa-dev \
  dpkg-dev rpm patchelf desktop-file-utils appstream curl file xz-utils

cargo build --locked --release -p fc-cli -p fc-gui
bash packaging/linux/check-version.sh
bash packaging/linux/build.sh all
# Or select one format: deb, rpm, appimage.
```

Outputs go to `dist/`; scratch staging is under `target/` and is cleaned on exit.
DEB dependencies are generated with `dpkg-shlibdeps`; RPM uses `rpmbuild`'s ELF
dependency generator. Both explicitly include libraries that winit loads with
`dlopen`, which binary dependency scanners cannot discover.

The AppImage builder downloads checksum-pinned linuxdeploy
`1-alpha-20251107-1` and type-2 runtime `20251108`. Update both the URLs and their
SHA-256 hashes when upgrading these tools. Its output plugin receives an explicit
runtime path and output name; it does not fetch a mutable runtime implicitly.
Pinned tools and `Cargo.lock` control inputs, but byte-for-byte reproducible builds
are not claimed.

The native recipes are [debian.control.in](debian.control.in) and
[fcopy.spec](fcopy.spec), sharing [install.sh](install.sh). These replace the old
Cargo DEB/RPM metadata; `cargo deb` and `cargo generate-rpm` are no longer the
packaging entry points.

## Building Arch from a release tag

Copy [PKGBUILD](../arch/PKGBUILD) to a build directory, then run as a normal user:

```sh
makepkg --syncdeps --cleanbuild
sudo pacman -U ./fcopy-*.pkg.tar.zst
```

The recipe fetches the real upstream Git repository at `v$pkgver`, builds using
`Cargo.lock`, and runs engine/GUI tests. `SKIP` applies to the Git source, not a
downloaded release archive. No live `main` branch is used for a versioned package.
CI uses a private Git mirror to run this same recipe against its exact checkout;
branch and pull-request builds do not need an already-published release tag.
The optional split-debug archive produced by makepkg is not a release download;
CI uploads the main `fcopy` package only.

## CI and publishing

[linux-release.yml](../../.github/workflows/linux-release.yml) runs for pushes to
`main`, pull requests, manual runs, and `v*` tags.

1. Validate packaging, test and lint the workspace on Ubuntu 22.04.
2. Build the release binaries, DEB, RPM and AppImage.
3. Install/uninstall the DEB on Ubuntu 22.04 and 24.04 and the RPM on Fedora 44.
4. Build the Arch package as an unprivileged user, then install/uninstall it.
5. Test actual CLI read-back copies, per-user desktop integration and GUI startup
   under Xvfb/Mesa. The AppImage is exercised without FUSE on Ubuntu and Fedora.
6. Also test native macOS DMGs, unsigned Windows installer/portable packages,
   the Docker image, and the hosted Debian service.
   On a version-tag push only, publish the tested GHCR image and all desktop,
   hosted and Unraid assets with checksums to a GitHub
   release. Rerunning the workflow updates assets on an existing release.

Branch/PR/manual runs retain downloadable package artifacts for 14 days and the
intermediate tested container image for 3 days; they never publish a release.
Publication jobs alone have `contents: write` or `packages: write`; build and PR
jobs have read-only repository permissions. No personal access token is needed.

To release, update `[workspace.package].version` in the root Cargo manifest and
`pkgver` in the Arch recipe together, regenerate `Cargo.lock`, and commit. The
tag must match both versions exactly:

```sh
bash packaging/linux/check-version.sh
git tag vX.Y.Z
git push origin vX.Y.Z
```

The repository must allow GitHub Actions and the workflow's release job to write
repository contents. A failed build, test, install or version check blocks
publishing; do not announce downloads until the tagged workflow succeeds.
