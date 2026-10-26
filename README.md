# fcopy

Fast, verified file copying for Windows, macOS and Linux: a TeraCopy-style
engine with a CLI and an egui GUI. Remote PC-to-PC transfer over QUIC is in progress.

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

Developer context and roadmap: [CLAUDE.md](CLAUDE.md).
