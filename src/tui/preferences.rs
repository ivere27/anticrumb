use super::editor::Draft;
use crate::{domain::*, store::Store};
use std::path::Path;

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelPreference {
    pub model: Option<String>,
    pub thinking: Option<String>,
}

pub struct Preferences {
    store: Store,
}
impl Preferences {
    pub fn open(root: &Path) -> Result<Self> {
        Ok(Self {
            store: Store::open(root)?,
        })
    }
    fn key(account: &str, workspace: &Path) -> String {
        digest(
            serde_json::json!([account, workspace])
                .to_string()
                .as_bytes(),
        )
    }
    pub fn history(&self, account: &str, workspace: &Path) -> Result<Vec<Draft>> {
        match self
            .store
            .get::<Vec<Draft>>("ui", "history", &Self::key(account, workspace))
        {
            Ok(history) => Ok(history),
            Err(error) if error.code == ErrorCode::NotFound => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }
    pub fn record(&self, account: &str, workspace: &Path, input: &Draft) -> Result<Vec<Draft>> {
        let key = Self::key(account, workspace);
        self.store
            .update_immediate("ui", "history", &key, |history| append(history, input))
    }
    pub fn theme(&self) -> String {
        self.store
            .get::<String>("ui", "settings", "theme")
            .unwrap_or_else(|_| "terminal".into())
    }
    pub fn set_theme(&self, theme: &str) -> Result<()> {
        self.store.put("ui", "settings", "theme", &theme)
    }
    pub fn debug_level(&self) -> u8 {
        self.store
            .get::<u8>("ui", "settings", "debug_level")
            .unwrap_or(0)
            .min(2)
    }
    pub fn set_debug_level(&self, level: u8) -> Result<()> {
        self.store
            .put("ui", "settings", "debug_level", &level.min(2))
    }
    pub fn model_selection(&self, account: &str) -> ModelPreference {
        self.store
            .get::<ModelPreference>("ui", "model_selection", account)
            .unwrap_or_default()
    }
    pub fn set_model_selection(
        &self,
        account: &str,
        model: Option<&str>,
        thinking: Option<&str>,
    ) -> Result<()> {
        self.store.put(
            "ui",
            "model_selection",
            account,
            &ModelPreference {
                model: model.map(str::to_owned),
                thinking: thinking.map(str::to_owned),
            },
        )
    }
}
pub fn append(history: &mut Vec<Draft>, input: &Draft) {
    if input.text.is_empty() || history.last().is_some_and(|s| s == input) {
        return;
    }
    history.push(input.clone());
    while history.len() > 200
        || history.iter().map(|draft| draft.text.len()).sum::<usize>() > 128 * 1024
    {
        history.remove(0);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_and_theme_survive_restart_without_crossing_workspaces_or_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let a = Preferences::open(dir.path()).unwrap();
        let b = Preferences::open(dir.path()).unwrap();
        a.record("a", Path::new("/one"), &Draft::plain("first".into()))
            .unwrap();
        b.record("a", Path::new("/one"), &Draft::plain("/model".into()))
            .unwrap();
        a.record("a", Path::new("/one"), &Draft::plain("/model".into()))
            .unwrap();
        assert_eq!(
            a.history("a", Path::new("/one")).unwrap(),
            vec![Draft::plain("first".into()), Draft::plain("/model".into())]
        );
        assert!(a.history("b", Path::new("/one")).unwrap().is_empty());
        assert!(a.history("a", Path::new("/two")).unwrap().is_empty());
        a.set_theme("nord").unwrap();
        assert_eq!(b.theme(), "nord");
        assert_eq!(a.debug_level(), 0);
        a.set_debug_level(2).unwrap();
        assert_eq!(b.debug_level(), 2);
        a.set_debug_level(9).unwrap();
        assert_eq!(b.debug_level(), 2);
        a.set_debug_level(0).unwrap();
        assert_eq!(b.debug_level(), 0);
        a.set_model_selection("a", Some("gemini-pro"), Some("high"))
            .unwrap();
        assert_eq!(
            b.model_selection("a"),
            ModelPreference {
                model: Some("gemini-pro".into()),
                thinking: Some("high".into()),
            }
        );
        assert_eq!(b.model_selection("other"), ModelPreference::default());
        a.set_model_selection("a", None, None).unwrap();
        assert_eq!(b.model_selection("a"), ModelPreference::default());
        let mut editor = super::super::editor::Editor::default();
        editor.paste(&"@literal ".repeat(130));
        let pasted = editor.draft();
        a.record("a", Path::new("/one"), &pasted).unwrap();
        assert_eq!(
            Preferences::open(dir.path())
                .unwrap()
                .history("a", Path::new("/one"))
                .unwrap()
                .last(),
            Some(&pasted)
        );
    }
}
