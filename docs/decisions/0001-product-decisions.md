# 0001: Product decisions from the unified technical proposal

**Status:** Accepted (owner sign-off recorded in conversation)
**Context:** The unified technical proposal asked six decisions before Phase 3 work.
This record captures each answer so the implementation follows it.

## Decisions

| # | Question | Decision | Consequence |
|---|---|---|---|
| 1 | Internal disk wiping | Excluded until Phase 5, and then only behind a policy switch. | `fc-storage::safety::Policy::allow_internal` defaults to `false`. No UI toggle in Phases 3–4. |
| 2 | Destructive operations in the web dashboard | Never. Desktop app only. | `fc-web` gets read-only storage types only. The crate graph enforces it. |
| 3 | Windows privilege model | Separate `fcopy-storage-svc` service with a named pipe, not one UAC prompt per operation. | Windows storage work is Phase 4. `fc-agent` stays the network-facing service and is not reused. |
| 4 | Target customer (enterprise IT or prosumer) | **Open.** The proposal gave no recommendation here. | Phase 5 priorities cannot be fixed until an owner chooses. |
| 5 | Brand name | Deferred until clearance checks are complete. | Packaging and binary renames wait for Phase 4. Working name remains FCOPY. |
| 6 | SSD purge in the enterprise release | **Open.** The proposal gave no recommendation here. | Until decided, the product makes no purge claims for SSDs. |

Decisions 4 and 6 were confirmed as a set, but the proposal did not attach a
recommendation to either. They need a named owner's explicit answer before Phase 5
planning starts.

## Status: on hold

### macOS compile and test (Phase 1 exit criterion)

- **What:** `cargo test --workspace` and clippy on macOS. The macOS branches in
  `fc-engine` (`platform/meta.rs`, `cache.rs`), `fc-gui` (`integration.rs`), and
  `fc-remote` (`config.rs`) have never been compiled.
- **Why on hold:** requires a Mac, which is not available in this environment.
- **Effect:** Phase 1 cannot be formally closed. Phase 4 macOS work is blocked behind it.
- **Resume when:** a Mac is available. Run `cargo test --workspace` first.
