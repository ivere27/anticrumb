use serde_json::Value;

fn field(value: &Value, name: &str) -> Option<String> {
    value[name]
        .as_str()
        .map(|s| {
            super::markdown::strip_controls(s, false)
                .take(512)
                .collect::<String>()
        })
        .filter(|s| !s.trim().is_empty())
}

fn used_percent(limit: &Value) -> Option<f64> {
    limit["used_percent"]
        .as_f64()
        .filter(|used| used.is_finite() && (0.0..=100.0).contains(used))
}

fn percent(value: f64) -> String {
    let number = format!("{value:.1}");
    format!("{}%", number.trim_end_matches('0').trim_end_matches('.'))
}

fn remaining_percent(limit: &Value) -> Option<String> {
    used_percent(limit).map(|used| format!("≈{}", percent(100.0 - used)))
}

fn remaining(limit: &Value) -> Option<String> {
    field(limit, "remaining").or_else(|| remaining_percent(limit))
}

fn compact_reset(reset: &str) -> String {
    // Shorten the English display labels without inferring a missing date.
    // Other languages and unfamiliar formats retain the provider's wording.
    let reset = reset.strip_prefix("Resets ").unwrap_or(reset);
    reset
        .strip_prefix("at ")
        .unwrap_or(reset)
        .replace(" at ", " ")
        .replace(" AM", "am")
        .replace(" PM", "pm")
}

/// A compact view of the same observation printed by /usage. Keep unfamiliar
/// labels instead of guessing their quota windows from their position.
pub(super) fn footer(value: &Value) -> String {
    let limits = value["limits"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if limits.is_empty() {
        return "Left:unavailable".into();
    }
    limits
        .iter()
        .take(2)
        .map(|limit| {
            let label = field(limit, "label").unwrap_or_else(|| "Left".into());
            let short = match label.as_str() {
                "Current usage" => "Left".into(),
                "Weekly limit" => "Wk".into(),
                _ => super::clip(&label, 8),
            };
            let balance = remaining_percent(limit)
                .or_else(|| remaining(limit))
                .unwrap_or_else(|| "?".into());
            let reset = field(limit, "reset")
                .map(|reset| format!(" @{}", super::clip(&compact_reset(&reset), 24)))
                .unwrap_or_default();
            format!("{short}:{}{reset}", super::clip(&balance, 12))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn report(value: &Value) -> String {
    let mut lines = vec![format!(
        "**Web account usage** · {}{}",
        field(value, "account_id").unwrap_or_default(),
        field(value, "plan")
            .map(|plan| format!(" · **{plan}**"))
            .unwrap_or_default()
    )];
    if let Some(limits) = value["limits"].as_array() {
        for limit in limits {
            let balance = remaining(limit)
                .map(|r| format!("**{r} remaining**"))
                .unwrap_or_else(|| "Remaining: not reported".into());
            let reported = field(limit, "used");
            let used = match (used_percent(limit), reported) {
                (Some(used), Some(text)) if !text.contains(['%', '٪']) => {
                    Some(format!("{} used ({text})", percent(used)))
                }
                (Some(used), None) => Some(format!("{} used", percent(used))),
                (_, text) => text,
            }
            .map(|u| format!(" · {u}"))
            .unwrap_or_default();
            let reset = field(limit, "reset")
                .map(|reset| format!(" · **{reset}**"))
                .unwrap_or_default();
            lines.push(format!(
                "**{}**: {balance}{used}{reset}",
                field(limit, "label").unwrap_or_else(|| "Usage limit".into())
            ));
            if let Some(total) = field(limit, "limit") {
                lines.push(format!("  Limit: {total}"));
            }
            if limit["status"] == "exhausted" {
                lines.push("  **Limit reached**".into());
            }
            if let Some(detail) = field(limit, "detail") {
                lines.push(format!("  {detail}"));
            }
        }
        if limits.is_empty() {
            lines.push("Usage limits are not reported by this account's web page.".into());
        }
    }
    if let Some(updated) = field(value, "updated_label") {
        lines.push(updated);
    }
    if let Some(timezone) = field(value, "timezone") {
        lines.push(format!("Reset times: {timezone} (Chrome host)"));
    }
    lines.push("≈ remaining is calculated from the website's displayed percentage.".into());
    if let Some(notice) = field(value, "notice") {
        lines.push(notice);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn percentages_keep_zero_and_distinguish_unknown_counts_and_reset_timezone() {
        let result = report(
            &json!({"account_id":"default","plan":"PRO","timezone":"Asia/Seoul","limits":[
                {"label":"Current usage","used":"1% used","used_percent":1.0,"reset":"Resets at 5:41 AM"},
                {"label":"Weekly limit","used":"0% used","used_percent":0.0},
                {"label":"Other","used":"<1% used"},
                {"label":"Full","used":"100% used","used_percent":100.0,"status":"exhausted"}
            ]}),
        );
        assert!(result.contains("**≈99% remaining** · 1% used"));
        assert!(result.contains("**≈100% remaining** · 0% used"));
        assert!(result.contains("Remaining: not reported · <1% used"));
        assert!(result.contains("**≈0% remaining**"));
        assert!(result.contains("**Limit reached**"));
        assert!(result.contains("Resets at 5:41 AM"));
        assert!(result.contains("Asia/Seoul (Chrome host)"));
    }

    #[test]
    fn footer_uses_reported_remaining_and_does_not_invent_unknown_quota_windows() {
        let value = json!({"limits":[
            {"label":"Current usage","used_percent":25.0},
            {"label":"Weekly limit","used_percent":0.0}
        ]});
        assert_eq!(footer(&value), "Left:≈75% Wk:≈100%");
        assert_eq!(
            footer(&json!({"limits":[{"label":"Images","remaining":"12 images"}]})),
            "Images:12 images"
        );
        assert_eq!(
            footer(&json!({"limits":[{"label":"Current usage","used":"<1% used"}]})),
            "Left:?"
        );
    }

    #[test]
    fn reset_dates_stay_next_to_percentages_in_report_and_footer() {
        let value = json!({"limits":[
            {"label":"Current usage","used_percent":25.0,"reset":"Resets at 5:41 AM"},
            {"label":"Weekly limit","used_percent":0.0,"reset":"Resets Nov 20 at 3:41 PM"}
        ]});
        assert_eq!(footer(&value), "Left:≈75% @5:41am Wk:≈100% @Nov 20 3:41pm");
        let report = report(&value);
        assert!(report
            .lines()
            .any(|line| line.contains("25% used") && line.contains("Resets at 5:41 AM")));
        assert!(report
            .lines()
            .any(|line| line.contains("0% used") && line.contains("Resets Nov 20 at 3:41 PM")));
        assert_eq!(
            footer(
                &json!({"limits":[{"label":"Weekly limit","used":"<1% used","reset":"2026-11-20 15:41"}]})
            ),
            "Wk:? @2026-11-20 15:41"
        );
        assert_eq!(
            compact_reset("Réinitialisation le 20 novembre"),
            "Réinitialisation le 20 novembre"
        );
        let counted = json!({"limits":[{"label":"Images","used":"3 images","remaining":"9 images","used_percent":25.0,"reset":"2026-11-20"}]});
        assert_eq!(footer(&counted), "Images:≈75% @2026-11-20");
        assert!(super::report(&counted)
            .contains("9 images remaining** · 25% used (3 images) · **2026-11-20**"));
    }
}
