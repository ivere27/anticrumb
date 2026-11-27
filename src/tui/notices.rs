//! Bundled, zero-network startup tips and author project notices for the interactive TUI.
//!
//! Entries are statically embedded from the repository root `notices.json` at compile time.
//! Set `ANTICRUMB_NO_NOTICE=1` (or `true`) to disable startup notices.
use super::markdown::Face;
use std::time::{SystemTime, UNIX_EPOCH};
use unicode_width::UnicodeWidthStr;

const RAW_NOTICES: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/notices.json"));
const ENV_NO_NOTICE: &str = "ANTICRUMB_NO_NOTICE";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct UtcDate {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

impl UtcDate {
    pub(super) fn parse(raw: &str) -> Option<Self> {
        let bytes = raw.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return None;
        }
        if !bytes[0..4].iter().all(u8::is_ascii_digit)
            || !bytes[5..7].iter().all(u8::is_ascii_digit)
            || !bytes[8..10].iter().all(u8::is_ascii_digit)
        {
            return None;
        }
        let year = raw[0..4].parse::<i32>().ok()?;
        let month = raw[5..7].parse::<u32>().ok()?;
        let day = raw[8..10].parse::<u32>().ok()?;
        if !crate::build_info::is_valid_civil_date(year, month, day) {
            return None;
        }
        Some(Self { year, month, day })
    }

    pub(super) fn from_epoch_secs(epoch_secs: u64) -> Option<Self> {
        let (year, month, day) = crate::build_info::utc_ymd(epoch_secs)?;
        Some(Self { year, month, day })
    }

    #[cfg(test)]
    fn to_civil_days(self) -> i64 {
        crate::build_info::days_from_civil(i64::from(self.year), self.month, self.day)
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NoticeCatalog {
    #[serde(rename = "_comment", default)]
    _comment: Option<String>,
    entries: Vec<NoticeEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum NoticeKind {
    Tip,
    Author,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NoticeEntry {
    #[allow(dead_code)]
    pub id: String,
    pub kind: NoticeKind,
    #[serde(default)]
    pub title: Option<String>,
    pub text: String,
    #[serde(default)]
    pub url: Option<String>,
    pub probability: f64,
    #[serde(default)]
    pub starts_at: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

impl NoticeEntry {
    fn is_active_on(&self, today: UtcDate) -> bool {
        if let Some(raw_start) = self.starts_at.as_deref() {
            let Some(start) = UtcDate::parse(raw_start) else {
                return false;
            };
            if today < start {
                return false;
            }
        }
        if let Some(raw_end) = self.expires_at.as_deref() {
            let Some(end) = UtcDate::parse(raw_end) else {
                return false;
            };
            if today > end {
                return false;
            }
        }
        true
    }
}

pub(super) struct NoticeLine {
    pub plain: String,
    pub spans: Vec<(String, Face)>,
}

pub(super) fn is_opt_out_value(raw: Option<&str>) -> bool {
    raw.is_some_and(|v| {
        let trimmed = v.trim();
        trimmed == "1" || trimmed.eq_ignore_ascii_case("true")
    })
}

fn is_opted_out() -> bool {
    is_opt_out_value(std::env::var(ENV_NO_NOTICE).ok().as_deref())
}

fn random_unit_f64() -> f64 {
    // Bytes 9..16 of an RFC 4122/9562 UUID v4 carry 56 pure random bits (no version/variant bits).
    let b = uuid::Uuid::new_v4().into_bytes();
    let raw = u64::from_be_bytes([0, b[9], b[10], b[11], b[12], b[13], b[14], b[15]]);
    (raw >> 3) as f64 / ((1u64 << 53) as f64)
}

pub(super) fn select_notice_at(
    entries: &[NoticeEntry],
    today: UtcDate,
    roll: f64,
) -> Option<NoticeEntry> {
    if !roll.is_finite() || !(0.0..1.0).contains(&roll) {
        return None;
    }
    let mut acc = 0.0_f64;
    for entry in entries {
        if !entry.probability.is_finite() || entry.probability <= 0.0 {
            continue;
        }
        if !entry.is_active_on(today) {
            continue;
        }
        acc += entry.probability;
        if roll < acc {
            return Some(entry.clone());
        }
    }
    None
}

pub(super) fn pick_startup_notice() -> Option<NoticeEntry> {
    if is_opted_out() {
        return None;
    }
    let catalog: NoticeCatalog = serde_json::from_str(RAW_NOTICES).ok()?;
    let epoch_secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let today = UtcDate::from_epoch_secs(epoch_secs)?;
    let roll = random_unit_f64();
    select_notice_at(&catalog.entries, today, roll)
}

pub(super) fn format_notice_lines(entry: &NoticeEntry) -> Vec<NoticeLine> {
    match entry.kind {
        NoticeKind::Tip => {
            let prefix = "  Tip · ";
            let plain = format!("{prefix}{}", entry.text);
            vec![NoticeLine {
                plain,
                spans: vec![
                    (prefix.into(), Face::ACCENT),
                    (entry.text.clone(), Face::MUTED),
                ],
            }]
        }
        NoticeKind::Author => {
            let prefix = "  Also by author · ";
            let mut lines = Vec::with_capacity(2);
            if let Some(title) = entry.title.as_deref().filter(|t| !t.trim().is_empty()) {
                let plain = format!("{prefix}{title} — {}", entry.text);
                lines.push(NoticeLine {
                    plain,
                    spans: vec![
                        (prefix.into(), Face::MUTED),
                        (title.to_owned(), Face::TEXT),
                        (" — ".into(), Face::MUTED),
                        (entry.text.clone(), Face::MUTED),
                    ],
                });
            } else {
                let plain = format!("{prefix}{}", entry.text);
                lines.push(NoticeLine {
                    plain,
                    spans: vec![
                        (prefix.into(), Face::MUTED),
                        (entry.text.clone(), Face::MUTED),
                    ],
                });
            }
            if let Some(url) = entry.url.as_deref().filter(|u| !u.trim().is_empty()) {
                let indent = " ".repeat(UnicodeWidthStr::width(prefix));
                let line = format!("{indent}{url}");
                lines.push(NoticeLine {
                    plain: line.clone(),
                    spans: vec![(line, Face::MUTED)],
                });
            }
            lines
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashSet};

    const MAX_NOTICE_LINE_WIDTH: usize = 78;

    fn is_clean_single_line(s: &str) -> bool {
        !s.trim().is_empty() && !s.chars().any(char::is_control)
    }

    fn is_valid_https_url(url: &str) -> bool {
        let Some(rest) = url.strip_prefix("https://") else {
            return false;
        };
        if rest.is_empty() || rest.starts_with('/') {
            return false;
        }
        !url.chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '[' || c == ']')
    }

    fn validate_catalog(catalog: &NoticeCatalog) -> Result<(), String> {
        let mut seen_ids = HashSet::new();
        let mut windows: Vec<(i64, i64, f64)> = Vec::with_capacity(catalog.entries.len());
        let mut checkpoints = BTreeSet::new();
        checkpoints.insert(i64::MIN);

        for (idx, entry) in catalog.entries.iter().enumerate() {
            if !is_clean_single_line(&entry.id) {
                return Err(format!("entry[{idx}] has empty or control-char id"));
            }
            if !seen_ids.insert(entry.id.as_str()) {
                return Err(format!("duplicate notice id {:?}", entry.id));
            }
            if !is_clean_single_line(&entry.text) {
                return Err(format!(
                    "entry {:?} has empty or multi-line/control text",
                    entry.id
                ));
            }
            match entry.kind {
                NoticeKind::Author => {
                    let Some(title) = entry.title.as_deref() else {
                        return Err(format!("author entry {:?} is missing title", entry.id));
                    };
                    if !is_clean_single_line(title) {
                        return Err(format!("author entry {:?} has invalid title", entry.id));
                    }
                }
                NoticeKind::Tip => {
                    if let Some(title) = entry.title.as_deref() {
                        if !is_clean_single_line(title) {
                            return Err(format!("tip entry {:?} has invalid title", entry.id));
                        }
                    }
                }
            }
            if let Some(url) = entry.url.as_deref() {
                if !is_valid_https_url(url) {
                    return Err(format!("entry {:?} has invalid url {:?}", entry.id, url));
                }
            }
            if !entry.probability.is_finite() || entry.probability <= 0.0 || entry.probability > 1.0
            {
                return Err(format!(
                    "entry {:?} probability {} must be in (0.0, 1.0]",
                    entry.id, entry.probability
                ));
            }

            let start_date = match entry.starts_at.as_deref() {
                Some(raw) => Some(UtcDate::parse(raw).ok_or_else(|| {
                    format!("entry {:?} has invalid starts_at {:?}", entry.id, raw)
                })?),
                None => None,
            };
            let end_date = match entry.expires_at.as_deref() {
                Some(raw) => Some(UtcDate::parse(raw).ok_or_else(|| {
                    format!("entry {:?} has invalid expires_at {:?}", entry.id, raw)
                })?),
                None => None,
            };
            if let (Some(start), Some(end)) = (start_date, end_date) {
                if start > end {
                    return Err(format!(
                        "entry {:?} starts_at {:?} > expires_at {:?}",
                        entry.id, entry.starts_at, entry.expires_at
                    ));
                }
            }

            let start_day = start_date.map_or(i64::MIN, UtcDate::to_civil_days);
            let end_day = end_date.map_or(i64::MAX, UtcDate::to_civil_days);
            checkpoints.insert(start_day);
            if end_day < i64::MAX {
                checkpoints.insert(end_day + 1);
            }
            windows.push((start_day, end_day, entry.probability));

            let rendered = format_notice_lines(entry);
            if rendered.is_empty() {
                return Err(format!("entry {:?} rendered zero lines", entry.id));
            }
            for (line_idx, line) in rendered.iter().enumerate() {
                let joined: String = line.spans.iter().map(|(s, _)| s.as_str()).collect();
                if joined != line.plain {
                    return Err(format!(
                        "entry {:?} line {line_idx} plain text does not match spans",
                        entry.id
                    ));
                }
                if line.spans.iter().any(|(_, face)| *face == Face::SUBTLE) {
                    return Err(format!(
                        "entry {:?} line {line_idx} must not use Face::SUBTLE",
                        entry.id
                    ));
                }
                let width = UnicodeWidthStr::width(line.plain.as_str());
                if width > MAX_NOTICE_LINE_WIDTH {
                    return Err(format!(
                        "entry {:?} line {line_idx} width {width} exceeds {MAX_NOTICE_LINE_WIDTH}: {:?}",
                        entry.id, line.plain
                    ));
                }
            }
        }

        for day in checkpoints {
            let active_sum: f64 = windows
                .iter()
                .filter(|&&(start, end, _)| start <= day && day <= end)
                .map(|&(_, _, prob)| prob)
                .sum();
            if active_sum > 1.0 + 1e-9 {
                return Err(format!(
                    "concurrent active probability sum {active_sum} exceeds 1.0 at civil day {day}"
                ));
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn make_entry(
        id: &str,
        kind: NoticeKind,
        title: Option<&str>,
        text: &str,
        url: Option<&str>,
        probability: f64,
        starts_at: Option<&str>,
        expires_at: Option<&str>,
    ) -> NoticeEntry {
        NoticeEntry {
            id: id.into(),
            kind,
            title: title.map(str::to_owned),
            text: text.into(),
            url: url.map(str::to_owned),
            probability,
            starts_at: starts_at.map(str::to_owned),
            expires_at: expires_at.map(str::to_owned),
        }
    }

    #[test]
    fn bundled_notices_json_is_valid() {
        let catalog: NoticeCatalog =
            serde_json::from_str(RAW_NOTICES).expect("bundled notices.json must deserialize");
        assert!(
            catalog
                ._comment
                .as_deref()
                .is_some_and(|c| !c.trim().is_empty()),
            "bundled notices.json should document its static/no-telemetry purpose in _comment"
        );
        assert!(
            !catalog.entries.is_empty(),
            "bundled notices.json must contain entries"
        );
        validate_catalog(&catalog).expect("bundled notices.json must satisfy all invariants");

        // Unknown fields must fail deserialization (`#[serde(deny_unknown_fields)]`).
        let bad_root = r#"{"entries": [], "unexpected": 1}"#;
        assert!(serde_json::from_str::<NoticeCatalog>(bad_root).is_err());
        let bad_entry =
            r#"{"entries": [{"id":"x","kind":"tip","text":"hi","probability":0.1,"typo":true}]}"#;
        assert!(serde_json::from_str::<NoticeCatalog>(bad_entry).is_err());
    }

    #[test]
    fn opt_out_env_parsing() {
        for truthy in ["1", "true", "TRUE", "True", " true ", "\t1\n"] {
            assert!(
                is_opt_out_value(Some(truthy)),
                "expected {truthy:?} to opt out"
            );
        }
        for falsy in [
            None,
            Some(""),
            Some("   "),
            Some("0"),
            Some("false"),
            Some("FALSE"),
            Some("yes"),
        ] {
            assert!(
                !is_opt_out_value(falsy),
                "expected {falsy:?} not to opt out"
            );
        }
    }

    #[test]
    fn date_window_filtering_and_calendar_validation() {
        assert_eq!(
            UtcDate::parse("2028-02-29"),
            Some(UtcDate {
                year: 2028,
                month: 2,
                day: 29,
            })
        );
        assert_eq!(
            UtcDate::parse("2000-02-29"),
            Some(UtcDate {
                year: 2000,
                month: 2,
                day: 29,
            })
        );
        for invalid in [
            "2026-02-29",
            "2100-02-29",
            "2026-02-30",
            "2026-13-01",
            "2026-00-10",
            "2026-04-31",
            "2026-01-00",
            "2026-1-01",
            "2026-01-1",
            "+026-01-01",
            "-026-01-01",
            "2026/01/01",
            "",
        ] {
            assert_eq!(UtcDate::parse(invalid), None, "must reject {invalid:?}");
        }

        let windowed = make_entry(
            "author-windowed",
            NoticeKind::Author,
            Some("Synurang"),
            "Bridge",
            None,
            0.5,
            Some("2026-11-26"),
            Some("2027-06-30"),
        );
        assert!(!windowed.is_active_on(UtcDate::parse("2026-11-25").unwrap()));
        assert!(windowed.is_active_on(UtcDate::parse("2026-11-26").unwrap()));
        assert!(windowed.is_active_on(UtcDate::parse("2027-06-30").unwrap()));
        assert!(!windowed.is_active_on(UtcDate::parse("2027-07-01").unwrap()));

        let always = make_entry(
            "tip-always",
            NoticeKind::Tip,
            None,
            "Always on",
            None,
            0.5,
            None,
            None,
        );
        assert!(always.is_active_on(UtcDate::parse("1970-01-01").unwrap()));
        assert!(always.is_active_on(UtcDate::parse("2099-12-31").unwrap()));

        // Malformed date strings safely render an entry inactive at runtime rather than panicking.
        let malformed = make_entry(
            "bad-date",
            NoticeKind::Tip,
            None,
            "Broken start",
            None,
            0.5,
            Some("2026-02-30"),
            None,
        );
        assert!(!malformed.is_active_on(UtcDate::parse("2026-11-26").unwrap()));
    }

    #[test]
    fn probability_selection_boundaries() {
        let today = UtcDate::parse("2026-12-01").unwrap();
        let entries = vec![
            make_entry("tip-a", NoticeKind::Tip, None, "A", None, 0.05, None, None),
            make_entry("tip-b", NoticeKind::Tip, None, "B", None, 0.05, None, None),
            make_entry(
                "author-c",
                NoticeKind::Author,
                Some("Proj"),
                "C",
                None,
                0.01,
                Some("2026-11-26"),
                Some("2027-06-30"),
            ),
        ];

        assert_eq!(
            select_notice_at(&entries, today, 0.0).map(|e| e.id),
            Some("tip-a".into())
        );
        assert_eq!(
            select_notice_at(&entries, today, 0.049_999_999).map(|e| e.id),
            Some("tip-a".into())
        );
        assert_eq!(
            select_notice_at(&entries, today, 0.05).map(|e| e.id),
            Some("tip-b".into())
        );
        assert_eq!(
            select_notice_at(&entries, today, 0.099_999_999).map(|e| e.id),
            Some("tip-b".into())
        );
        assert_eq!(
            select_notice_at(&entries, today, 0.10).map(|e| e.id),
            Some("author-c".into())
        );
        assert_eq!(
            select_notice_at(&entries, today, 0.109_999_999).map(|e| e.id),
            Some("author-c".into())
        );
        assert!(select_notice_at(&entries, today, 0.11).is_none());
        assert!(select_notice_at(&entries, today, 0.999_999).is_none());

        // Out-of-range or NaN rolls safely return None.
        assert!(select_notice_at(&entries, today, -0.01).is_none());
        assert!(select_notice_at(&entries, today, 1.0).is_none());
        assert!(select_notice_at(&entries, today, f64::NAN).is_none());

        // After author-c expires, its 0.01 slot disappears without affecting earlier tips.
        let after_expiry = UtcDate::parse("2027-07-01").unwrap();
        assert!(select_notice_at(&entries, after_expiry, 0.10).is_none());
        assert_eq!(
            select_notice_at(&entries, after_expiry, 0.099).map(|e| e.id),
            Some("tip-b".into())
        );

        // Sweep-line allows total sum > 1.0 across non-overlapping date windows...
        let non_overlapping = NoticeCatalog {
            _comment: None,
            entries: vec![
                make_entry(
                    "base-tip",
                    NoticeKind::Tip,
                    None,
                    "Always",
                    None,
                    0.4,
                    None,
                    None,
                ),
                make_entry(
                    "camp-1",
                    NoticeKind::Author,
                    Some("One"),
                    "First campaign",
                    None,
                    0.6,
                    Some("2026-01-01"),
                    Some("2026-06-30"),
                ),
                make_entry(
                    "camp-2",
                    NoticeKind::Author,
                    Some("Two"),
                    "Second campaign",
                    None,
                    0.6,
                    Some("2026-07-01"),
                    Some("2026-12-31"),
                ),
            ],
        };
        assert!(validate_catalog(&non_overlapping).is_ok());

        // ...but rejects overlapping windows whose concurrent sum exceeds 1.0.
        let overlapping = NoticeCatalog {
            _comment: None,
            entries: vec![
                make_entry(
                    "base-tip",
                    NoticeKind::Tip,
                    None,
                    "Always",
                    None,
                    0.4,
                    None,
                    None,
                ),
                make_entry(
                    "camp-1",
                    NoticeKind::Author,
                    Some("One"),
                    "First campaign",
                    None,
                    0.6,
                    Some("2026-01-01"),
                    Some("2026-07-01"),
                ),
                make_entry(
                    "camp-2",
                    NoticeKind::Author,
                    Some("Two"),
                    "Second campaign",
                    None,
                    0.6,
                    Some("2026-07-01"),
                    Some("2026-12-31"),
                ),
            ],
        };
        assert!(validate_catalog(&overlapping).is_err());
    }

    #[test]
    fn format_notice_lines_renders_tip_and_author() {
        let tip = make_entry(
            "tip-1",
            NoticeKind::Tip,
            None,
            "Run /copy to copy the last response with original Markdown intact.",
            None,
            0.05,
            None,
            None,
        );
        let tip_lines = format_notice_lines(&tip);
        assert_eq!(tip_lines.len(), 1);
        assert_eq!(
            tip_lines[0].plain,
            "  Tip · Run /copy to copy the last response with original Markdown intact."
        );
        assert_eq!(
            tip_lines[0].spans,
            vec![
                ("  Tip · ".into(), Face::ACCENT),
                (
                    "Run /copy to copy the last response with original Markdown intact.".into(),
                    Face::MUTED
                ),
            ]
        );
        assert!(!tip_lines[0].spans.iter().any(|(_, f)| *f == Face::SUBTLE));

        let author_with_url = make_entry(
            "author-1",
            NoticeKind::Author,
            Some("Synurang"),
            "In-process gRPC/FFI bridge powering anticrumb",
            Some("https://github.com/ivere27/synurang"),
            0.01,
            None,
            None,
        );
        let author_lines = format_notice_lines(&author_with_url);
        assert_eq!(author_lines.len(), 2);
        assert_eq!(
            author_lines[0].plain,
            "  Also by author · Synurang — In-process gRPC/FFI bridge powering anticrumb"
        );
        assert_eq!(
            author_lines[0].spans,
            vec![
                ("  Also by author · ".into(), Face::MUTED),
                ("Synurang".into(), Face::TEXT),
                (" — ".into(), Face::MUTED),
                (
                    "In-process gRPC/FFI bridge powering anticrumb".into(),
                    Face::MUTED
                ),
            ]
        );
        assert_eq!(
            author_lines[1].plain,
            "                   https://github.com/ivere27/synurang"
        );
        assert_eq!(
            author_lines[1].spans,
            vec![(
                "                   https://github.com/ivere27/synurang".into(),
                Face::MUTED
            )]
        );
        let title_col = UnicodeWidthStr::width(
            &author_lines[0].plain[..author_lines[0].plain.find("Synurang").unwrap()],
        );
        let url_col = UnicodeWidthStr::width(
            &author_lines[1].plain[..author_lines[1].plain.find("https://").unwrap()],
        );
        assert_eq!(url_col, title_col);
        assert!(author_lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .all(|(_, f)| *f != Face::SUBTLE));

        let author_no_url = make_entry(
            "author-2",
            NoticeKind::Author,
            Some("Synurang"),
            "In-process gRPC/FFI bridge powering anticrumb",
            None,
            0.01,
            None,
            None,
        );
        let author_no_url_lines = format_notice_lines(&author_no_url);
        assert_eq!(author_no_url_lines.len(), 1);
        assert_eq!(
            author_no_url_lines[0].plain,
            "  Also by author · Synurang — In-process gRPC/FFI bridge powering anticrumb"
        );
    }
}
