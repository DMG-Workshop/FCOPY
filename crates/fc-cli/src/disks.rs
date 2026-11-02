//! `fcopy disks`: read-only inspection of attached disks. It never writes to a
//! device. Serial numbers are not printed, because they identify the hardware.

use std::process::ExitCode;

use fc_storage::health::{Health, HealthSource};

/// A size in binary units, such as `1.5 GiB`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// One line that says what the drive reported. Missing readings are left out,
/// and a drive with no readings says so, rather than printing zeros.
pub fn summarize_health(h: &Health) -> String {
    if h.source == HealthSource::Unavailable {
        return "unavailable (this enclosure does not report SMART)".to_owned();
    }
    let mut parts = Vec::new();
    match h.passed {
        Some(true) => parts.push("SMART passed".to_owned()),
        Some(false) => parts.push("SMART FAILED".to_owned()),
        None => {}
    }
    if let Some(t) = h.temperature_c {
        parts.push(format!("{t} °C"));
    }
    if let Some(hours) = h.power_on_hours {
        parts.push(format!("{hours} h powered on"));
    }
    if let Some(n) = h.reallocated_sectors {
        parts.push(format!("{n} reallocated sectors"));
    }
    if let Some(p) = h.percent_used {
        parts.push(format!("{p}% of rated life used"));
    }
    if parts.is_empty() {
        "unavailable (no readable SMART fields)".to_owned()
    } else {
        parts.join(", ")
    }
}

#[cfg(target_os = "linux")]
pub fn run(health: bool) -> ExitCode {
    use fc_storage::linux::LinuxSysfs;
    use fc_storage::platform::Platform;

    let host = LinuxSysfs::system();
    let disks = match host.enumerate() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("cannot list disks: {e}");
            return ExitCode::from(1);
        }
    };
    let system = host.system_disks();

    let header = format!(
        "{:<14} {:<24} {:>10}  {:<12} {:<9} {:<8} {}",
        "DEVICE", "MODEL", "SIZE", "BUS", "REMOVABLE", "SYSTEM", "MOUNTS"
    );
    println!("{header}");
    for d in &disks {
        let is_system = match &system {
            Some(set) if set.contains(&d.id) => "yes",
            Some(_) => "no",
            None => "unknown",
        };
        let mounts = if d.mounts.is_empty() {
            "-".to_owned()
        } else {
            d.mounts
                .iter()
                .map(|m| m.point.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!(
            "{:<14} {:<24} {:>10}  {:<12} {:<9} {:<8} {}",
            d.os_path,
            d.identity.model,
            human_size(d.identity.size_bytes),
            format!("{:?}", d.transport),
            if d.removable { "yes" } else { "no" },
            is_system,
            mounts
        );
        if health {
            match host.health(d) {
                Ok(h) => println!("    health: {}", summarize_health(&h)),
                Err(e) => println!("    health: error: {e}"),
            }
        }
    }
    ExitCode::SUCCESS
}

#[cfg(not(target_os = "linux"))]
pub fn run(_health: bool) -> ExitCode {
    eprintln!("disk inspection is available on Linux in this build");
    ExitCode::from(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024 / 2), "1.5 GiB");
    }

    #[test]
    fn missing_readings_are_not_printed_as_zero() {
        let none = Health {
            source: HealthSource::Unavailable,
            passed: None,
            temperature_c: None,
            power_on_hours: None,
            reallocated_sectors: None,
            percent_used: None,
        };
        assert!(summarize_health(&none).starts_with("unavailable"));
    }

    #[test]
    fn a_failing_drive_is_stated_plainly() {
        let failing = Health {
            source: HealthSource::Smartctl,
            passed: Some(false),
            temperature_c: Some(41),
            power_on_hours: None,
            reallocated_sectors: Some(12),
            percent_used: None,
        };
        assert_eq!(
            summarize_health(&failing),
            "SMART FAILED, 41 °C, 12 reallocated sectors"
        );
    }
}
