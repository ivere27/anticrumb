//! Build metadata and release-channel display formatting.
//!
//! `package_version()` stays a pure `X.Y.Z` SemVer string from `Cargo.toml` for
//! daemon protocol compatibility, release tag validation, and SemVer comparisons,
//! while `display_version()` appends `-nightly` or `-dev` for non-stable builds.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildChannel {
    Stable,
    Nightly,
    Dev,
}

impl BuildChannel {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "dev" => Some(Self::Dev),
            "nightly" => Some(Self::Nightly),
            "stable" => Some(Self::Stable),
            _ => None,
        }
    }

    pub fn current() -> Self {
        option_env!("ANTICRUMB_BUILD_CHANNEL")
            .and_then(Self::parse)
            .unwrap_or(Self::Dev)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Nightly => "nightly",
            Self::Dev => "dev",
        }
    }

    pub const fn version_suffix(self) -> &'static str {
        match self {
            Self::Stable => "",
            Self::Nightly => "-nightly",
            Self::Dev => "-dev",
        }
    }
}

pub const fn package_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn format_display_version(base_version: &str, channel: BuildChannel) -> String {
    format!("{base_version}{}", channel.version_suffix())
}

pub fn display_version() -> String {
    format_display_version(package_version(), BuildChannel::current())
}

pub const fn build_target() -> &'static str {
    env!("ANTICRUMB_BUILD_TARGET")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DateTimeParts {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
}

fn utc_datetime_parts(epoch_secs: u64) -> DateTimeParts {
    let secs_of_day = epoch_secs % 86400;
    let hour = (secs_of_day / 3600) as u32;
    let minute = ((secs_of_day % 3600) / 60) as u32;
    let second = (secs_of_day % 60) as u32;

    let z = (epoch_secs / 86400) as i64 + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = if month <= 2 { y + 1 } else { y };

    DateTimeParts {
        year,
        month,
        day,
        hour,
        minute,
        second,
    }
}

pub(crate) fn utc_ymd(epoch_secs: u64) -> Option<(i32, u32, u32)> {
    let parts = utc_datetime_parts(epoch_secs);
    let year = i32::try_from(parts.year).ok()?;
    Some((year, parts.month, parts.day))
}

pub(crate) fn is_valid_civil_date(year: i32, month: u32, day: u32) -> bool {
    if !(1..=12).contains(&month) || day == 0 {
        return false;
    }
    let y = i64::from(year);
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => return false,
    };
    day <= max_day
}

pub(crate) fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m_adj = if month > 2 { month - 3 } else { month + 9 } as u64;
    let doy = (153 * m_adj + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

fn civil_to_epoch_secs(parts: DateTimeParts) -> Option<i64> {
    let days = days_from_civil(parts.year, parts.month, parts.day);
    let day_secs = days.checked_mul(86400)?;
    let time_secs =
        i64::from(parts.hour) * 3600 + i64::from(parts.minute) * 60 + i64::from(parts.second);
    day_secs.checked_add(time_secs)
}

#[cfg(unix)]
fn local_datetime_with_offset(epoch_secs: u64) -> Option<(DateTimeParts, i32)> {
    extern "C" {
        fn tzset();
    }
    static TZ_INIT: std::sync::Once = std::sync::Once::new();
    TZ_INIT.call_once(|| unsafe {
        tzset();
    });

    let raw_time = libc::time_t::try_from(epoch_secs).ok()?;
    let utc_secs = i64::try_from(epoch_secs).ok()?;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::zeroed();
    let ptr = unsafe { libc::localtime_r(&raw_time, tm.as_mut_ptr()) };
    if ptr.is_null() {
        return None;
    }
    let tm = unsafe { tm.assume_init() };
    if !(0..=11).contains(&tm.tm_mon)
        || !(1..=31).contains(&tm.tm_mday)
        || !(0..=23).contains(&tm.tm_hour)
        || !(0..=59).contains(&tm.tm_min)
        || !(0..=60).contains(&tm.tm_sec)
    {
        return None;
    }
    let parts = DateTimeParts {
        year: i64::from(tm.tm_year) + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        second: tm.tm_sec.min(59) as u32,
    };
    let local_secs = civil_to_epoch_secs(parts)?;
    let offset_secs = i32::try_from(local_secs.checked_sub(utc_secs)?).ok()?;
    if offset_secs.abs() >= 86400 {
        return None;
    }
    Some((parts, offset_secs))
}

#[cfg(windows)]
fn local_datetime_with_offset(epoch_secs: u64) -> Option<(DateTimeParts, i32)> {
    use windows_sys::Win32::{
        Foundation::SYSTEMTIME, System::Time::SystemTimeToTzSpecificLocalTime,
    };

    let utc_secs = i64::try_from(epoch_secs).ok()?;
    let utc = utc_datetime_parts(epoch_secs);
    if !(1601..=30827).contains(&utc.year) {
        return None;
    }
    let utc_st = SYSTEMTIME {
        wYear: utc.year as u16,
        wMonth: utc.month as u16,
        wDayOfWeek: 0,
        wDay: utc.day as u16,
        wHour: utc.hour as u16,
        wMinute: utc.minute as u16,
        wSecond: utc.second as u16,
        wMilliseconds: 0,
    };
    let mut local_st = std::mem::MaybeUninit::<SYSTEMTIME>::zeroed();
    let ok = unsafe {
        SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc_st, local_st.as_mut_ptr())
    };
    if ok == 0 {
        return None;
    }
    let local = unsafe { local_st.assume_init() };
    if !(1..=12).contains(&local.wMonth)
        || !(1..=31).contains(&local.wDay)
        || local.wHour > 23
        || local.wMinute > 59
        || local.wSecond > 59
    {
        return None;
    }
    let parts = DateTimeParts {
        year: i64::from(local.wYear),
        month: u32::from(local.wMonth),
        day: u32::from(local.wDay),
        hour: u32::from(local.wHour),
        minute: u32::from(local.wMinute),
        second: u32::from(local.wSecond),
    };
    let local_secs = civil_to_epoch_secs(parts)?;
    let offset_secs = i32::try_from(local_secs.checked_sub(utc_secs)?).ok()?;
    if offset_secs.abs() >= 86400 {
        return None;
    }
    Some((parts, offset_secs))
}

#[cfg(not(any(unix, windows)))]
fn local_datetime_with_offset(_epoch_secs: u64) -> Option<(DateTimeParts, i32)> {
    None
}

fn format_datetime_with_offset(parts: DateTimeParts, offset_secs: i32) -> String {
    let DateTimeParts {
        year,
        month,
        day,
        hour,
        minute,
        second,
    } = parts;
    if offset_secs == 0 {
        return format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC");
    }
    let sign = if offset_secs >= 0 { '+' } else { '-' };
    let abs = offset_secs.unsigned_abs();
    let off_hours = abs / 3600;
    let off_mins = (abs % 3600) / 60;
    format!(
        "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} {sign}{off_hours:02}:{off_mins:02}"
    )
}

fn format_compact_log_datetime(parts: DateTimeParts, utc_fallback: bool) -> String {
    let DateTimeParts {
        year,
        month,
        day,
        hour,
        minute,
        second,
    } = parts;
    if utc_fallback {
        format!("[{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC]")
    } else {
        format!("[{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}]")
    }
}

pub fn format_local_timestamp(epoch_secs: u64) -> String {
    match local_datetime_with_offset(epoch_secs) {
        Some((parts, offset_secs)) => format_datetime_with_offset(parts, offset_secs),
        None => format_datetime_with_offset(utc_datetime_parts(epoch_secs), 0),
    }
}

pub(crate) fn format_local_log_timestamp(epoch_secs: u64) -> String {
    match local_datetime_with_offset(epoch_secs) {
        Some((parts, _offset_secs)) => format_compact_log_datetime(parts, false),
        None => format_compact_log_datetime(utc_datetime_parts(epoch_secs), true),
    }
}

pub const fn build_time() -> &'static str {
    match option_env!("ANTICRUMB_BUILD_TIME") {
        Some(time) => time,
        None => "unknown",
    }
}

pub fn build_epoch() -> Option<u64> {
    option_env!("ANTICRUMB_BUILD_EPOCH").and_then(|raw| raw.trim().parse::<u64>().ok())
}

pub fn display_build_time() -> String {
    match build_epoch() {
        Some(epoch) => format_local_timestamp(epoch),
        None => build_time().to_string(),
    }
}

pub(crate) fn normalize_commit_head(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "unknown" {
        return None;
    }
    let (hash, dirty) = match trimmed.strip_suffix("-dirty") {
        Some(prefix) => (prefix, true),
        None => (trimmed, false),
    };
    if hash.len() < 7 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let short: String = hash.chars().take(7).collect();
    if dirty {
        Some(format!("{short}-dirty"))
    } else {
        Some(short)
    }
}

pub fn commit_head() -> Option<String> {
    if let Some(normalized) = option_env!("ANTICRUMB_GIT_HEAD").and_then(normalize_commit_head) {
        return Some(normalized);
    }
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dot_git = manifest_dir.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else if dot_git.is_file() {
        let content = std::fs::read_to_string(&dot_git).ok()?;
        let raw = content.trim().strip_prefix("gitdir:")?.trim();
        let path = Path::new(raw);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            manifest_dir.join(path)
        }
    } else {
        return None;
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let trimmed = head.trim();
    let full_sha = if let Some(reference) = trimmed.strip_prefix("ref: ") {
        let reference = reference.trim();
        let ref_path = git_dir.join(reference);
        if let Ok(sha) = std::fs::read_to_string(ref_path) {
            sha.trim().to_string()
        } else {
            let packed = std::fs::read_to_string(git_dir.join("packed-refs")).ok()?;
            packed
                .lines()
                .filter(|line| !line.starts_with('#') && !line.starts_with('^'))
                .find_map(|line| {
                    let (sha, name) = line.split_once(' ')?;
                    (name.trim() == reference).then(|| sha.trim().to_string())
                })?
        }
    } else {
        trimmed.to_string()
    };
    let short: String = full_sha.chars().take(7).collect();
    (short.len() == 7 && short.chars().all(|c| c.is_ascii_hexdigit())).then_some(short)
}

pub fn format_help_banner(version: &str, commit: Option<&str>, built: &str) -> String {
    let commit = commit.unwrap_or("unknown");
    format!("anticrumb {version} (commit: {commit}, built: {built})")
}

pub fn help_banner() -> String {
    format_help_banner(
        &display_version(),
        commit_head().as_deref(),
        &display_build_time(),
    )
}

pub fn format_cli_version(version: &str, commit: Option<&str>, target: &str) -> String {
    match commit {
        Some(head) => format!("anticrumb {version} ({head}, {target})"),
        None => format!("anticrumb {version} ({target})"),
    }
}

pub fn cli_version() -> String {
    format_cli_version(&display_version(), commit_head().as_deref(), build_target())
}

pub fn format_client_version(version: &str, commit: Option<&str>) -> String {
    let commit = commit.unwrap_or("unknown");
    format!("anticrumb {version} ({commit})")
}

pub fn client_version() -> String {
    format_client_version(&display_version(), commit_head().as_deref())
}

pub fn format_tui_title(version: &str, commit: Option<&str>) -> String {
    match commit {
        Some(head) => format!("anticrumb {version} ({head})"),
        None => format!("anticrumb {version}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_channel_parses_known_values_and_rejects_unknown() {
        assert_eq!(BuildChannel::parse(""), Some(BuildChannel::Dev));
        assert_eq!(BuildChannel::parse("  DEV \n"), Some(BuildChannel::Dev));
        assert_eq!(BuildChannel::parse("Nightly"), Some(BuildChannel::Nightly));
        assert_eq!(BuildChannel::parse("stable"), Some(BuildChannel::Stable));
        assert_eq!(BuildChannel::parse("beta"), None);
        assert_eq!(BuildChannel::parse("release"), None);
    }

    #[test]
    fn display_version_formats_each_channel_consistently() {
        assert_eq!(
            format_display_version("0.8.3", BuildChannel::Stable),
            "0.8.3"
        );
        assert_eq!(
            format_display_version("0.8.3", BuildChannel::Nightly),
            "0.8.3-nightly"
        );
        assert_eq!(
            format_display_version("0.8.3", BuildChannel::Dev),
            "0.8.3-dev"
        );
        assert_eq!(
            display_version(),
            format_display_version(package_version(), BuildChannel::current())
        );
    }

    #[test]
    fn banners_and_version_strings_reflect_channel_version_commit_and_build_time() {
        assert_eq!(
            format_help_banner("0.8.3-nightly", Some("3d9d844"), "2026-11-19 04:54:37 UTC"),
            "anticrumb 0.8.3-nightly (commit: 3d9d844, built: 2026-11-19 04:54:37 UTC)"
        );
        assert_eq!(
            format_cli_version(
                "0.8.3-dev",
                Some("3d9d844-dirty"),
                "x86_64-unknown-linux-gnu"
            ),
            "anticrumb 0.8.3-dev (3d9d844-dirty, x86_64-unknown-linux-gnu)"
        );
        assert_eq!(
            format_cli_version("0.8.3", None, "x86_64-unknown-linux-gnu"),
            "anticrumb 0.8.3 (x86_64-unknown-linux-gnu)"
        );
        assert_eq!(
            format_client_version("0.8.3-nightly", Some("3d9d844")),
            "anticrumb 0.8.3-nightly (3d9d844)"
        );
        assert_eq!(
            format_tui_title("0.8.3-nightly", Some("3d9d844")),
            "anticrumb 0.8.3-nightly (3d9d844)"
        );
        assert_eq!(format_tui_title("0.8.3", None), "anticrumb 0.8.3");

        let banner = help_banner();
        assert!(banner.starts_with(&format!("anticrumb {} (commit: ", display_version())));
        assert!(banner.contains("built: "));
        assert!(cli_version().starts_with(&format!("anticrumb {} ", display_version())));
        assert!(client_version().starts_with(&format!("anticrumb {} ", display_version())));
    }

    #[test]
    fn normalize_commit_head_preserves_dirty_suffix_and_rejects_invalid() {
        assert_eq!(normalize_commit_head("abc1234"), Some("abc1234".into()));
        assert_eq!(
            normalize_commit_head("abc1234def5678"),
            Some("abc1234".into())
        );
        assert_eq!(
            normalize_commit_head(" abc1234-dirty \n"),
            Some("abc1234-dirty".into())
        );
        assert_eq!(
            normalize_commit_head("abc1234def5678-dirty"),
            Some("abc1234-dirty".into())
        );
        assert_eq!(normalize_commit_head("unknown"), None);
        assert_eq!(normalize_commit_head(""), None);
        assert_eq!(normalize_commit_head("-dirty"), None);
        assert_eq!(normalize_commit_head("123456-dirty"), None);
        assert_eq!(normalize_commit_head("not_hex-dirty"), None);
    }

    #[test]
    fn utc_civil_and_epoch_round_trip_across_leap_years_and_boundaries() {
        let cases = [
            (0u64, 1970, 1, 1, 0, 0, 0),
            (951_868_799, 2000, 2, 29, 23, 59, 59),
            (1_709_210_096, 2024, 2, 29, 12, 34, 56),
            (1_795_064_077, 2026, 11, 19, 4, 54, 37),
            (4_102_444_799, 2099, 12, 31, 23, 59, 59),
        ];
        for (epoch, year, month, day, hour, minute, second) in cases {
            let expected = DateTimeParts {
                year,
                month,
                day,
                hour,
                minute,
                second,
            };
            assert_eq!(utc_datetime_parts(epoch), expected);
            assert_eq!(utc_ymd(epoch), Some((year as i32, month, day)));
            assert_eq!(civil_to_epoch_secs(expected), Some(epoch as i64));
        }
        assert_eq!(utc_ymd(u64::MAX), None);
        assert!(is_valid_civil_date(2000, 2, 29));
        assert!(is_valid_civil_date(2028, 2, 29));
        assert!(!is_valid_civil_date(1900, 2, 29));
        assert!(!is_valid_civil_date(2026, 2, 29));
        assert!(!is_valid_civil_date(2026, 4, 31));
        assert!(!is_valid_civil_date(2026, 13, 1));
    }

    #[test]
    fn format_datetime_with_offset_renders_utc_and_signed_offsets() {
        assert_eq!(
            format_datetime_with_offset(
                DateTimeParts {
                    year: 2026,
                    month: 11,
                    day: 19,
                    hour: 4,
                    minute: 54,
                    second: 37,
                },
                0,
            ),
            "2026-11-19 04:54:37 UTC"
        );
        assert_eq!(
            format_datetime_with_offset(
                DateTimeParts {
                    year: 2026,
                    month: 11,
                    day: 19,
                    hour: 13,
                    minute: 54,
                    second: 37,
                },
                9 * 3600,
            ),
            "2026-11-19 13:54:37 +09:00"
        );
        assert_eq!(
            format_datetime_with_offset(
                DateTimeParts {
                    year: 2026,
                    month: 11,
                    day: 18,
                    hour: 23,
                    minute: 54,
                    second: 37,
                },
                -5 * 3600,
            ),
            "2026-11-18 23:54:37 -05:00"
        );
        assert_eq!(
            format_datetime_with_offset(
                DateTimeParts {
                    year: 2026,
                    month: 11,
                    day: 19,
                    hour: 10,
                    minute: 24,
                    second: 37,
                },
                5 * 3600 + 30 * 60,
            ),
            "2026-11-19 10:24:37 +05:30"
        );
        assert_eq!(
            format_datetime_with_offset(
                DateTimeParts {
                    year: 2026,
                    month: 11,
                    day: 19,
                    hour: 10,
                    minute: 39,
                    second: 37,
                },
                5 * 3600 + 45 * 60,
            ),
            "2026-11-19 10:39:37 +05:45"
        );
    }

    #[test]
    fn format_compact_log_datetime_omits_offset_except_on_utc_fallback() {
        let parts = DateTimeParts {
            year: 2026,
            month: 11,
            day: 19,
            hour: 13,
            minute: 54,
            second: 37,
        };
        assert_eq!(
            format_compact_log_datetime(parts, false),
            "[2026-11-19 13:54:37]"
        );
        assert_eq!(
            format_compact_log_datetime(parts, true),
            "[2026-11-19 13:54:37 UTC]"
        );
    }

    #[test]
    fn runtime_build_time_and_log_timestamp_format_cleanly() {
        if let Some(epoch) = build_epoch() {
            let built = display_build_time();
            assert!(
                built.ends_with(" UTC")
                    || (built.len() == 26
                        && matches!(built.as_bytes()[20], b'+' | b'-')
                        && built.as_bytes()[23] == b':'),
                "unexpected display_build_time format: {built}"
            );
            let log_ts = format_local_log_timestamp(epoch);
            assert!(
                log_ts.starts_with('[') && log_ts.ends_with(']') && log_ts.len() == 21,
                "expected compact 21-char local log timestamp, got: {log_ts}"
            );
        }
    }
}
