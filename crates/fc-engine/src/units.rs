//! Human-readable sizes, rates and durations, plus parsing of `--limit 10MB`.
//! Shared by the CLI and GUI so both speak the same units.

use std::time::Duration;

/// `1536` → `"1.5 KiB"`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[i]) }
}

/// Bytes per second → `"123.4 MiB/s"`.
pub fn human_rate(bps: f64) -> String {
    format!("{}/s", human_bytes(bps.max(0.0) as u64))
}

/// `"42s"`, `"3m 07s"`, `"1h 02m"`.
pub fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// Parse a size or rate such as `500k`, `10MB`, `10MiB`, `1.5G`, `10MB/s`.
///
/// Suffixes without `i` are decimal (SI, like network speeds and drive
/// labels): `10M` = 10 000 000. With `i` they are binary: `10Mi` = 10 485 760.
/// A bare number is bytes.
pub fn parse_size(input: &str) -> Result<u64, String> {
    let s = input.trim().trim_end_matches("/s").trim_end_matches("ps").trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let value: f64 = num.parse().map_err(|_| format!("invalid size '{input}'"))?;
    let unit = unit.trim().to_ascii_lowercase();
    let unit = unit.strip_suffix('b').unwrap_or(&unit);
    let mult: f64 = match unit {
        "" => 1.0,
        "k" => 1e3,
        "m" => 1e6,
        "g" => 1e9,
        "t" => 1e12,
        "ki" => 1024.0,
        "mi" => 1024.0 * 1024.0,
        "gi" => 1024.0 * 1024.0 * 1024.0,
        "ti" => 1024f64.powi(4),
        _ => return Err(format!("unknown unit in '{input}' (use k, M, G, Ki, Mi, Gi)")),
    };
    let bytes = value * mult;
    if !bytes.is_finite() || bytes < 0.0 || bytes > u64::MAX as f64 {
        return Err(format!("size out of range: '{input}'"));
    }
    Ok(bytes.round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        assert_eq!(parse_size("10MB"), Ok(10_000_000));
        assert_eq!(parse_size("10 MB/s"), Ok(10_000_000));
        assert_eq!(parse_size("10MiB"), Ok(10_485_760));
        assert_eq!(parse_size("1.5G"), Ok(1_500_000_000));
        assert_eq!(parse_size("500k"), Ok(500_000));
        assert_eq!(parse_size("4096"), Ok(4096));
        assert_eq!(parse_size("2Mbps"), Ok(2_000_000)); // bytes, despite the "b"
        assert!(parse_size("ten").is_err());
        assert!(parse_size("5X").is_err());
    }

    #[test]
    fn format() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
        assert_eq!(human_duration(Duration::from_secs(42)), "42s");
        assert_eq!(human_duration(Duration::from_secs(187)), "3m 07s");
        assert_eq!(human_duration(Duration::from_secs(3720)), "1h 02m");
    }
}
