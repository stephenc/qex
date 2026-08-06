//! This module reads and writes two types of value: byte sizes and durations.
//!
//! The parsers accept the formats that a person or an agent writes without
//! instruction. Each error message shows the permitted formats.

use std::fmt;

/// Reads a byte size. Examples: `8GB`, `8G`, `8gb`, `8192MB`, `512K`, `1073741824`.
///
/// One unit step is 1024. `GB` has the same value as `GiB`.
/// The tools that an agent compares against, such as `free`, `htop` and the
/// cgroup files, all use steps of 1024. A claim of `8GB` that gives 7.45 GiB is
/// an incorrect claim.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("empty size; expected e.g. `8GB`, `512MB`, `2G`".into());
    }

    let digits_end = t
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(digits_end);
    let num: f64 = num
        .parse()
        .map_err(|_| format!("invalid size `{s}`; expected e.g. `8GB`, `512MB`, `2G`"))?;
    if num < 0.0 {
        return Err(format!("size `{s}` cannot be negative"));
    }

    let unit = unit.trim().to_ascii_lowercase();
    let mult: u64 = match unit.as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1u64 << 40,
        other => {
            return Err(format!(
                "unknown size unit `{other}` in `{s}`; use B, KB, MB, GB or TB"
            ))
        }
    };

    let bytes = num * mult as f64;
    if !bytes.is_finite() || bytes > u64::MAX as f64 {
        return Err(format!("size `{s}` is too large"));
    }
    Ok(bytes as u64)
}

/// Writes a byte count in the format that a person writes.
///
/// [`parse_size`] reads back each value that this function writes. The status
/// output and the error messages use this function.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1u64 << 40, "TB"),
        (1 << 30, "GB"),
        (1 << 20, "MB"),
        (1 << 10, "KB"),
    ];
    for (mult, suffix) in UNITS {
        if bytes >= mult {
            let v = bytes as f64 / mult as f64;
            // A whole number is easier to read without the `.0` at the end.
            return if (v.round() - v).abs() < 0.05 {
                format!("{}{}", v.round() as u64, suffix)
            } else {
                format!("{v:.1}{suffix}")
            };
        }
    }
    format!("{bytes}B")
}

/// Reads a duration. Examples: `30s`, `5m`, `4h`, `2d`, `90`.
///
/// A number without a unit is a number of seconds. The values `0` and `none`
/// mean that there is no limit. For these two values, the result is `None`.
pub fn parse_duration(s: &str) -> Result<Option<std::time::Duration>, String> {
    let t = s.trim().to_ascii_lowercase();
    if t.is_empty() {
        return Err("empty duration; expected e.g. `30s`, `5m`, `4h`, or `0` for none".into());
    }
    if t == "0" || t == "none" || t == "never" || t == "unlimited" {
        return Ok(None);
    }

    let digits_end = t
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(digits_end);
    let num: f64 = num
        .parse()
        .map_err(|_| format!("invalid duration `{s}`; expected e.g. `30s`, `5m`, `4h`"))?;
    if num < 0.0 {
        return Err(format!("duration `{s}` cannot be negative"));
    }

    let secs: f64 = match unit.trim() {
        "" | "s" | "sec" | "secs" | "second" | "seconds" => num,
        "m" | "min" | "mins" | "minute" | "minutes" => num * 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => num * 3600.0,
        "d" | "day" | "days" => num * 86400.0,
        other => {
            return Err(format!(
                "unknown duration unit `{other}` in `{s}`; use s, m, h or d"
            ))
        }
    };
    if secs == 0.0 {
        return Ok(None);
    }
    Ok(Some(std::time::Duration::from_secs_f64(secs)))
}

/// Writes a count and the word for the thing that it counts.
///
/// The word takes an `s` when the count is not one. A message that says
/// `1 lines` looks like a fault of qex, and the reader then doubts the number
/// as well as the words.
///
/// Give the word in the singular: `count_of(3, "job")` gives `3 jobs`.
pub fn count_of(count: usize, word: &str) -> String {
    if count == 1 {
        format!("1 {word}")
    } else {
        format!("{count} {word}s")
    }
}

/// Writes a duration in a short form for the status output.
///
/// Examples: `1h5m`, `45s`, `2d3h`.
pub fn format_duration(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        return format!("{s}s");
    }
    if s < 3600 {
        return Compound(s / 60, "m", s % 60, "s").to_string();
    }
    if s < 86400 {
        return Compound(s / 3600, "h", (s % 3600) / 60, "m").to_string();
    }
    Compound(s / 86400, "d", (s % 86400) / 3600, "h").to_string()
}

/// Writes two units together, for example `5m30s`.
///
/// If the second value is zero, this type writes the first unit only: `5m`.
struct Compound(u64, &'static str, u64, &'static str);

impl fmt::Display for Compound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.0, self.1)?;
        if self.2 > 0 {
            write!(f, "{}{}", self.2, self.3)?;
        }
        Ok(())
    }
}

/// Reads a budget value. Examples: `75%`, `12`, `20GB`.
///
/// A budget is an absolute value, or a percentage of the machine capacity.
/// Give the machine capacity in `total`. Use the same unit as the result.
pub fn parse_budget(s: &str, total: u64, is_size: bool) -> Result<u64, String> {
    let t = s.trim();
    if let Some(pct) = t.strip_suffix('%') {
        let pct: f64 = pct
            .trim()
            .parse()
            .map_err(|_| format!("invalid percentage `{s}`; expected e.g. `75%`"))?;
        if !(0.0..=100.0).contains(&pct) {
            return Err(format!("percentage `{s}` must be between 0% and 100%"));
        }
        return Ok((total as f64 * pct / 100.0) as u64);
    }
    if is_size {
        parse_size(t)
    } else {
        t.parse::<u64>()
            .map_err(|_| format!("invalid core count `{s}`; expected an integer or a percentage"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_the_forms_agents_actually_write() {
        assert_eq!(parse_size("8GB").unwrap(), 8 << 30);
        assert_eq!(parse_size("8G").unwrap(), 8 << 30);
        assert_eq!(parse_size("8gb").unwrap(), 8 << 30);
        assert_eq!(parse_size("8GiB").unwrap(), 8 << 30);
        assert_eq!(parse_size(" 8 GB ").unwrap(), 8 << 30);
        assert_eq!(parse_size("512MB").unwrap(), 512 << 20);
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("1.5GB").unwrap(), 1610612736);
    }

    #[test]
    fn size_errors_name_the_accepted_forms() {
        for bad in ["", "GB", "8XB", "-1GB", "abc"] {
            let err = parse_size(bad).unwrap_err();
            assert!(!err.is_empty(), "no error message for `{bad}`");
        }
        assert!(parse_size("8XB").unwrap_err().contains("KB, MB, GB"));
    }

    #[test]
    fn sizes_round_trip_through_formatting() {
        for bytes in [1024u64, 8 << 30, 512 << 20, 1u64 << 40] {
            let rendered = format_size(bytes);
            assert_eq!(
                parse_size(&rendered).unwrap(),
                bytes,
                "round trip {rendered}"
            );
        }
        assert_eq!(format_size(8 << 30), "8GB");
        assert_eq!(format_size(1536 << 20), "1.5GB");
        assert_eq!(format_size(512), "512B");
    }

    #[test]
    fn durations_parse_and_zero_means_unlimited() {
        use std::time::Duration;
        assert_eq!(
            parse_duration("30s").unwrap(),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_duration("5m").unwrap(),
            Some(Duration::from_secs(300))
        );
        assert_eq!(
            parse_duration("4h").unwrap(),
            Some(Duration::from_secs(14400))
        );
        assert_eq!(
            parse_duration("2d").unwrap(),
            Some(Duration::from_secs(172800))
        );
        assert_eq!(parse_duration("90").unwrap(), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("0").unwrap(), None);
        assert_eq!(parse_duration("none").unwrap(), None);
        assert!(parse_duration("5w").is_err());
    }

    #[test]
    fn durations_format_compactly() {
        use std::time::Duration;
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(Duration::from_secs(330)), "5m30s");
        assert_eq!(format_duration(Duration::from_secs(300)), "5m");
        assert_eq!(format_duration(Duration::from_secs(3900)), "1h5m");
    }

    #[test]
    fn budgets_accept_percentages_and_absolutes() {
        assert_eq!(parse_budget("75%", 16, false).unwrap(), 12);
        assert_eq!(parse_budget("12", 16, false).unwrap(), 12);
        assert_eq!(parse_budget("100%", 16, false).unwrap(), 16);
        assert_eq!(parse_budget("50%", 8 << 30, true).unwrap(), 4 << 30);
        assert_eq!(parse_budget("20GB", 0, true).unwrap(), 20 << 30);
        assert!(parse_budget("150%", 16, false).is_err());
    }
}
