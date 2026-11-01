//! # fc-storage: core of the storage toolkit
//!
//! Almost everything here is pure logic, or works on the [`wipe::RawIo`] trait,
//! so it builds and tests on every platform without touching a disk.
//!
//! The read-only half is always compiled: inspection, safety rules, plan
//! validation, health parsing, the platform traits and the Linux reader.
//! The write half sits behind the `storage-destructive` feature, which is off
//! by default. A normal build cannot write to a disk.
//!
//! | Concern | Module | Feature |
//! |---------|--------|---------|
//! | Device model and stable disk identity | [`model`] | always |
//! | Device-name parsing (partition to parent disk) | [`devname`] | always |
//! | Guardrails for destructive operations | [`safety`] | always |
//! | Partition-table and label validation | [`plan`] | always |
//! | Overwrite engine with verification | [`wipe`] | always |
//! | SMART report parsing | [`health`] | always |
//! | Traits every OS adapter implements | [`platform`] | always |
//! | Read-only Linux adapter (sysfs, mountinfo) | [`linux`] | always |
//! | In-memory platform for tests | [`fake`] | always |
//! | Guarded execution pipeline and audit trail | [`orchestrate`] | `storage-destructive` |
//! | Signed plan envelope and worker events | [`protocol`] | `storage-destructive` |
//! | Linux block-device writer (`O_DIRECT`, `O_EXCL`) | [`linux_block`] | `storage-destructive` |

pub mod devname;
pub mod fake;
pub mod health;
pub mod linux;
pub mod model;
pub mod plan;
pub mod platform;
pub mod safety;
pub mod wipe;

#[cfg(all(feature = "storage-destructive", target_os = "linux"))]
pub mod linux_block;
#[cfg(feature = "storage-destructive")]
pub mod orchestrate;
#[cfg(feature = "storage-destructive")]
pub mod protocol;
