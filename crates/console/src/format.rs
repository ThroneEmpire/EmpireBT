//! Human-readable formatting for sizes, rates, and durations.

const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

pub fn bytes(value: u64) -> String {
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// A transfer rate, or an empty string when nothing is moving.
pub fn rate(bytes_per_second: f64) -> String {
    if bytes_per_second < 1.0 {
        return String::new();
    }
    format!("{}/s", bytes(bytes_per_second as u64))
}

pub fn eta(remaining: u64, bytes_per_second: f64) -> String {
    if remaining == 0 || bytes_per_second < 1.0 {
        return String::new();
    }
    duration((remaining as f64 / bytes_per_second).ceil() as u64)
}

/// A compact duration such as `45s`, `2m 05s`, or `1h 02m`.
pub fn duration(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        3600..86_400 => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
        _ => format!("{}d {:02}h", seconds / 86_400, seconds % 86_400 / 3600),
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_binary_units() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }

    #[test]
    fn hides_idle_rates_and_etas() {
        assert_eq!(rate(0.0), "");
        assert_eq!(rate(2048.0), "2.0 KiB/s");
        assert_eq!(eta(100, 0.0), "");
        assert_eq!(eta(0, 100.0), "");
    }

    #[test]
    fn formats_eta_ranges() {
        assert_eq!(eta(30, 1.0), "30s");
        assert_eq!(eta(125, 1.0), "2m 05s");
        assert_eq!(eta(3_720, 1.0), "1h 02m");
        assert_eq!(eta(90_000, 1.0), "1d 01h");
    }
}
