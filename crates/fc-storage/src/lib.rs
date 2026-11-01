//! # fc-storage: core of the storage toolkit
//!
//! Almost everything here is pure logic, or works on the [`wipe::RawIo`] trait,
//! so it builds and tests on every platform without touching a disk. The one
//! OS adapter, [`linux`], is read-only. Destructive writes are reachable only
//! through [`orchestrate::execute`], which the privileged worker will call.
//!
//! | Concern | Module |
//! |---------|--------|
//! | Device model and stable disk identity | [`model`] |
//! | Device-name parsing (partition to parent disk) | [`devname`] |
//! | Guardrails for destructive operations | [`safety`] |
//! | Partition-table and label validation | [`plan`] |
//! | Overwrite engine with verification | [`wipe`] |
//! | SMART report parsing | [`health`] |
//! | Traits every OS adapter implements | [`platform`] |
//! | In-memory platform for tests | [`fake`] |
//! | Guarded execution pipeline and audit trail | [`orchestrate`] |
//! | Signed plan envelope and worker events | [`protocol`] |
//! | Read-only Linux adapter (sysfs, mountinfo) | [`linux`] |

pub mod devname;
pub mod fake;
pub mod health;
pub mod linux;
pub mod model;
pub mod orchestrate;
pub mod plan;
pub mod platform;
pub mod protocol;
pub mod safety;
pub mod wipe;
