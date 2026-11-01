//! SMART report parsing. Pure: the input is `smartctl --json` output.
//!
//! A missing reading stays `None`. It is never reported as zero, because many
//! USB enclosures hide SMART data entirely.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthSource {
    Smartctl,
    /// The report contained no usable fields (for example, a hidden USB bridge).
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    pub source: HealthSource,
    pub passed: Option<bool>,
    pub temperature_c: Option<i64>,
    pub power_on_hours: Option<u64>,
    /// ATA attribute 5, Reallocated_Sector_Ct, raw value.
    pub reallocated_sectors: Option<u64>,
    /// NVMe `percentage_used`.
    pub percent_used: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HealthError {
    #[error("smartctl output is not valid JSON")]
    InvalidJson,
}

pub fn parse_smartctl(json: &str) -> Result<Health, HealthError> {
    let v: Value = serde_json::from_str(json).map_err(|_| HealthError::InvalidJson)?;

    let passed = v.pointer("/smart_status/passed").and_then(Value::as_bool);
    let temperature_c = v.pointer("/temperature/current").and_then(Value::as_i64);
    let power_on_hours = v.pointer("/power_on_time/hours").and_then(Value::as_u64);
    let percent_used = v
        .pointer("/nvme_smart_health_information_log/percentage_used")
        .and_then(Value::as_u64);
    let reallocated_sectors = v
        .pointer("/ata_smart_attributes/table")
        .and_then(Value::as_array)
        .and_then(|table| {
            table
                .iter()
                .find(|a| a.get("id").and_then(Value::as_u64) == Some(5))
                .and_then(|a| a.pointer("/raw/value"))
                .and_then(Value::as_u64)
        });

    let any_reading = passed.is_some()
        || temperature_c.is_some()
        || power_on_hours.is_some()
        || percent_used.is_some()
        || reallocated_sectors.is_some();

    Ok(Health {
        source: if any_reading {
            HealthSource::Smartctl
        } else {
            HealthSource::Unavailable
        },
        passed,
        temperature_c,
        power_on_hours,
        reallocated_sectors,
        percent_used,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ATA_HEALTHY: &str = r#"{
        "smart_status": { "passed": true },
        "temperature": { "current": 34 },
        "power_on_time": { "hours": 1234 },
        "ata_smart_attributes": { "table": [
            { "id": 9, "name": "Power_On_Hours", "raw": { "value": 1234 } },
            { "id": 5, "name": "Reallocated_Sector_Ct", "raw": { "value": 0 } }
        ] }
    }"#;

    const ATA_FAILING: &str = r#"{
        "smart_status": { "passed": false },
        "ata_smart_attributes": { "table": [
            { "id": 5, "name": "Reallocated_Sector_Ct", "raw": { "value": 12 } }
        ] }
    }"#;

    const NVME: &str = r#"{
        "smart_status": { "passed": true },
        "temperature": { "current": 41 },
        "nvme_smart_health_information_log": { "percentage_used": 3 }
    }"#;

    #[test]
    fn healthy_ata_drive() {
        let h = parse_smartctl(ATA_HEALTHY).unwrap();
        assert_eq!(h.source, HealthSource::Smartctl);
        assert_eq!(h.passed, Some(true));
        assert_eq!(h.temperature_c, Some(34));
        assert_eq!(h.power_on_hours, Some(1234));
        assert_eq!(h.reallocated_sectors, Some(0));
        assert_eq!(h.percent_used, None);
    }

    #[test]
    fn reallocated_sectors_are_reported_as_is() {
        let h = parse_smartctl(ATA_FAILING).unwrap();
        assert_eq!(h.passed, Some(false));
        assert_eq!(h.reallocated_sectors, Some(12));
    }

    #[test]
    fn nvme_wear_is_read_from_the_health_log() {
        let h = parse_smartctl(NVME).unwrap();
        assert_eq!(h.percent_used, Some(3));
        assert_eq!(h.reallocated_sectors, None);
    }

    #[test]
    fn hidden_smart_is_unavailable_not_zero() {
        let h = parse_smartctl(r#"{ "model_name": "USB Bridge" }"#).unwrap();
        assert_eq!(h.source, HealthSource::Unavailable);
        assert_eq!(h.passed, None);
        assert_eq!(h.temperature_c, None);
        assert_eq!(h.reallocated_sectors, None);
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert_eq!(parse_smartctl("not json"), Err(HealthError::InvalidJson));
    }
}
