use serde_json::Value;

pub fn name(model: &Value) -> &str {
    model["sublabel"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| model["label"].as_str())
        .unwrap_or("Unknown model")
}
pub fn title(model: &Value) -> String {
    let name = name(model);
    let detail = model["label"].as_str().unwrap_or("");
    if detail.is_empty() || detail == name {
        name.into()
    } else {
        format!("{name} · {detail}")
    }
}
pub struct Level {
    pub id: Option<String>,
    pub label: String,
}
pub struct ModelChoice {
    pub id: Option<String>,
    pub levels: Vec<Level>,
    pub selected: usize,
}
impl ModelChoice {
    pub fn new(id: Option<String>, values: &Value, current: Option<&str>) -> Self {
        let mut levels = Vec::new();
        let mut matched_current = None;
        let mut matched_web_selected = None;
        if let Some(values) = values.as_array() {
            for value in values.iter().filter(|v| v["locked"] != true) {
                let Some(id) = value["level"].as_str() else {
                    continue;
                };
                let label = value["label"].as_str().unwrap_or(id);
                // Positional IDs can shift when Google inserts a level. Match its observed label instead.
                let selection_id = if id.starts_with("option:") { label } else { id };
                if current == Some(selection_id) && matched_current.is_none() {
                    matched_current = Some(levels.len());
                }
                if value["selected"] == true && matched_web_selected.is_none() {
                    matched_web_selected = Some(levels.len());
                }
                levels.push(Level {
                    id: Some(selection_id.into()),
                    label: label.into(),
                });
            }
        }
        if levels.is_empty() {
            levels.push(Level {
                id: None,
                label: "Unavailable".into(),
            });
        }
        let selected = matched_current.or(matched_web_selected).unwrap_or(0);
        Self {
            id,
            levels,
            selected,
        }
    }
    pub fn shift(&mut self, right: bool) {
        if self.levels.len() <= 1 {
            return;
        }
        self.selected = if right {
            (self.selected + 1).min(self.levels.len() - 1)
        } else {
            self.selected.saturating_sub(1)
        };
    }
    pub fn level(&self) -> &Level {
        &self.levels[self.selected]
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn model_names_and_thinking_are_data_driven() {
        let m = json!({"mode_id":"argon","label":"Code & analysis","sublabel":"4 Argon"});
        assert_eq!(name(&m), "4 Argon");
        let mut choice = ModelChoice::new(
            Some("argon".into()),
            &json!([
                {"level":"low","label":"Low","selected":true},
                {"level":"medium","label":"Medium"},
                {"level":"high","label":"High"},
                {"level":"locked","label":"Locked","locked":true}
            ]),
            None,
        );
        assert_eq!(choice.level().id.as_deref(), Some("low"));
        assert!(choice
            .levels
            .iter()
            .all(|l| l.id.is_some() && l.label != "Web default"));
        choice.shift(false);
        assert_eq!(choice.level().id.as_deref(), Some("low"));
        choice.shift(true);
        assert_eq!(choice.level().id.as_deref(), Some("medium"));
        choice.shift(true);
        assert_eq!(choice.level().id.as_deref(), Some("high"));
        choice.shift(true);
        assert_eq!(choice.level().id.as_deref(), Some("high"));
        choice.shift(false);
        assert_eq!(choice.level().id.as_deref(), Some("medium"));
        choice.shift(false);
        assert_eq!(choice.level().id.as_deref(), Some("low"));
        let fallback_to_web_selected = ModelChoice::new(
            Some("argon".into()),
            &json!([
                {"level":"low","label":"Low"},
                {"level":"medium","label":"Medium","selected":true},
                {"level":"high","label":"High"}
            ]),
            Some("extra"),
        );
        assert_eq!(
            fallback_to_web_selected.level().id.as_deref(),
            Some("medium")
        );
        let mut unavailable = ModelChoice::new(Some("fast".into()), &json!([]), Some("high"));
        unavailable.shift(true);
        assert!(unavailable.level().id.is_none());
        assert_eq!(unavailable.level().label, "Unavailable");
    }
}

#[cfg(test)]
mod extensibility {
    use super::*;
    use serde_json::json;
    #[test]
    fn fourth_level_and_new_model_require_no_code_change() {
        let model =
            json!({"mode_id":"future-hydrogen","sublabel":"4.0 Hydrogen","label":"Everyday help"});
        assert_eq!(name(&model), "4.0 Hydrogen");
        let levels = json!([
            {"level":"option:0","label":"Low","selected":true},
            {"level":"option:1","label":"Medium"},
            {"level":"option:2","label":"High"},
            {"level":"option:3","label":"Ultra"}
        ]);
        let mut choice = ModelChoice::new(Some("future-hydrogen".into()), &levels, None);
        for _ in 0..3 {
            choice.shift(true);
        }
        assert_eq!(choice.level().id.as_deref(), Some("Ultra"));
        let reordered =
            json!([{"level":"option:0","label":"Ultra"},{"level":"option:1","label":"Low"}]);
        let after = ModelChoice::new(Some("future-hydrogen".into()), &reordered, Some("Ultra"));
        assert_eq!(after.level().label, "Ultra");
    }
}
