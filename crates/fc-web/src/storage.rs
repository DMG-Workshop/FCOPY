//! Read-only disk inspection for the dashboard. Administrators only, because mounts
//! and hardware details are sensitive. Serial numbers are never returned. Nothing
//! here writes to a device.

use std::collections::HashSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use fc_storage::health::{Health, HealthSource};
use fc_storage::model::{Disk, DiskId};
use serde::{Deserialize, Serialize};

use crate::auth::Principal;
use crate::{ApiError, AppState};

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct DiskDto {
    pub os_path: String,
    pub model: String,
    pub size_bytes: u64,
    pub transport: String,
    pub removable: bool,
    /// `None` when the system set could not be determined.
    pub system: Option<bool>,
    pub mounts: Vec<String>,
}

impl DiskDto {
    /// Converts a disk, leaving out the serial number, which identifies the hardware.
    pub fn from_disk(d: &Disk, system: Option<&HashSet<DiskId>>) -> Self {
        Self {
            os_path: d.os_path.clone(),
            model: d.identity.model.clone(),
            size_bytes: d.identity.size_bytes,
            transport: format!("{:?}", d.transport),
            removable: d.removable,
            system: system.map(|set| set.contains(&d.id)),
            mounts: d
                .mounts
                .iter()
                .map(|m| m.point.display().to_string())
                .collect(),
        }
    }
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct HealthDto {
    pub source: &'static str,
    pub passed: Option<bool>,
    pub temperature_c: Option<i64>,
    pub power_on_hours: Option<u64>,
    pub reallocated_sectors: Option<u64>,
    pub percent_used: Option<u64>,
}

impl From<Health> for HealthDto {
    fn from(h: Health) -> Self {
        Self {
            source: match h.source {
                HealthSource::Smartctl => "smartctl",
                HealthSource::Unavailable => "unavailable",
            },
            passed: h.passed,
            temperature_c: h.temperature_c,
            power_on_hours: h.power_on_hours,
            reallocated_sectors: h.reallocated_sectors,
            percent_used: h.percent_used,
        }
    }
}

#[derive(Deserialize)]
pub struct HealthQuery {
    pub device: String,
}

fn require_admin(p: &Principal) -> Result<(), ApiError> {
    if p.is_admin() {
        Ok(())
    } else {
        Err(ApiError::Forbidden("administrators only"))
    }
}

/// The disk with this OS path, if the enumeration found it. Health is only ever read
/// for a disk the enumeration returned, so a client cannot name an arbitrary path.
pub fn find_enumerated<'a>(disks: &'a [Disk], device: &str) -> Option<&'a Disk> {
    disks.iter().find(|d| d.os_path == device)
}

#[cfg(not(target_os = "linux"))]
const NOT_LINUX: &str = "disk inspection is available on Linux in this build";

#[cfg(target_os = "linux")]
fn scan() -> Result<(Vec<Disk>, Option<HashSet<DiskId>>), ApiError> {
    use fc_storage::linux::LinuxSysfs;
    use fc_storage::platform::Platform;
    let host = LinuxSysfs::system();
    let disks = host
        .enumerate()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok((disks, host.system_disks()))
}

#[cfg(not(target_os = "linux"))]
fn scan() -> Result<(Vec<Disk>, Option<HashSet<DiskId>>), ApiError> {
    Err(ApiError::NotAvailable(NOT_LINUX.to_owned()))
}

#[cfg(target_os = "linux")]
fn read_health(device: &str) -> Result<Option<Health>, ApiError> {
    use fc_storage::linux::LinuxSysfs;
    use fc_storage::platform::Platform;
    let host = LinuxSysfs::system();
    let disks = host
        .enumerate()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    match find_enumerated(&disks, device) {
        Some(d) => host
            .health(d)
            .map(Some)
            .map_err(|e| ApiError::Internal(e.to_string())),
        None => Ok(None),
    }
}

#[cfg(not(target_os = "linux"))]
fn read_health(_device: &str) -> Result<Option<Health>, ApiError> {
    Err(ApiError::NotAvailable(NOT_LINUX.to_owned()))
}

/// `GET /api/storage/disks`: attached disks, without serial numbers.
pub async fn list_disks(
    State(_s): State<Arc<AppState>>,
    p: Principal,
) -> Result<Json<Vec<DiskDto>>, ApiError> {
    require_admin(&p)?;
    let (disks, system) = tokio::task::spawn_blocking(scan)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))??;
    Ok(Json(
        disks
            .iter()
            .map(|d| DiskDto::from_disk(d, system.as_ref()))
            .collect(),
    ))
}

/// `GET /api/storage/health?device=/dev/sdb`: read-only SMART health for a listed disk.
pub async fn health(
    State(_s): State<Arc<AppState>>,
    p: Principal,
    Query(q): Query<HealthQuery>,
) -> Result<Json<HealthDto>, ApiError> {
    require_admin(&p)?;
    let device = q.device;
    let found = tokio::task::spawn_blocking(move || read_health(&device))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))??;
    found
        .map(|h| Json(HealthDto::from(h)))
        .ok_or(ApiError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fc_storage::model::{DiskIdentity, Mount, Transport};
    use std::path::PathBuf;

    fn disk(serial: &str) -> Disk {
        Disk::new(
            "/dev/sdb",
            Transport::Usb,
            true,
            DiskIdentity {
                serial: Some(serial.into()),
                model: "Cruzer".into(),
                size_bytes: 1 << 30,
            },
            512,
            vec![Mount {
                point: PathBuf::from("/media/usb"),
                label: None,
            }],
        )
    }

    #[test]
    fn the_dto_never_carries_the_serial_number() {
        let d = disk("SECRET-SERIAL-123");
        let dto = DiskDto::from_disk(&d, None);
        let json = serde_json::to_string(&dto).unwrap();
        assert!(!json.contains("SECRET-SERIAL-123"), "serial leaked: {json}");
        assert_eq!(
            dto.system, None,
            "an unknown system set is reported as unknown"
        );
    }

    #[test]
    fn health_is_only_read_for_an_enumerated_disk() {
        let disks = vec![disk("A")];
        assert!(find_enumerated(&disks, "/dev/sdb").is_some());
        assert!(find_enumerated(&disks, "/etc/passwd").is_none());
        assert!(find_enumerated(&disks, "/dev/sdb --option").is_none());
    }

    #[test]
    fn only_administrators_may_inspect_disks() {
        let user = Principal {
            name: "bob".into(),
            role: crate::auth::Role::User,
        };
        assert!(matches!(require_admin(&user), Err(ApiError::Forbidden(_))));
        let admin = Principal {
            name: "alice".into(),
            role: crate::auth::Role::Admin,
        };
        assert!(require_admin(&admin).is_ok());
    }
}
