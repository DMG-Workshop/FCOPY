//! # fc-storage: hardware-free core of the storage toolkit
//!
//! Everything in this crate is pure logic or operates on the [`wipe::RawIo`]
//! trait, so it builds and tests on every platform without touching a disk.
//! OS adapters (sysfs, IOCTL, DiskArbitration) and the privileged worker are
//! separate layers that depend on this crate, never the other way round.
//!
//! | Concern | Module |
//! |---------|--------|
//! | Device model and stable disk identity | [`model`] |
//! | Device-name parsing (partition to parent disk) | [`devname`] |
//! | Guardrails for destructive operations | [`safety`] |
//! | Partition-table and label validation | [`plan`] |
//! | Overwrite engine with verification | [`wipe`] |
//! | SMART report parsing | [`health`] |

pub mod devname;
pub mod health;
pub mod model;
pub mod plan;
pub mod safety;
pub mod wipe;
