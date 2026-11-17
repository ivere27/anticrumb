//! Provider script resolution with compiled fallbacks for all bundled providers.
//!
//! Search order for a provider's script `<name>`:
//! 1. `<ANTICRUMB_HOME>/providers/<provider>/scripts/<name>`
//! 2. `<ANTICRUMB_HOME>/providers/<provider>/<name>`
//! 3. Built-in compiled scripts for the requested provider.
use std::{borrow::Cow, path::Path};

const MAX_SCRIPT_BYTES: usize = 128 * 1024;

fn validate_script_name(name: &str) -> Result<(), String> {
    if name.is_empty() || matches!(name, "." | "..") || name.contains(['/', '\\', '\0']) {
        return Err("invalid adapter script name".into());
    }
    Ok(())
}

fn read_override(path: &Path, name: &str) -> Result<Cow<'static, str>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("adapter {name}: {e}"))?;
    if text.len() > MAX_SCRIPT_BYTES {
        return Err("adapter script exceeds 128 KiB".into());
    }
    Ok(Cow::Owned(text))
}

pub fn script(name: &str) -> Result<Cow<'static, str>, String> {
    script_for("gemini-web", name)
}

pub fn script_for(provider_id: &str, name: &str) -> Result<Cow<'static, str>, String> {
    script_for_in_home(&crate::browser_host::anticrumb_home(), provider_id, name)
}

fn script_for_in_home(
    home: &Path,
    provider_id: &str,
    name: &str,
) -> Result<Cow<'static, str>, String> {
    validate_script_name(name)?;
    let provider_dir = home.join("providers").join(provider_id);
    for dir in [provider_dir.join("scripts"), provider_dir] {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return read_override(&candidate, name);
        }
    }
    load_for(provider_id, name, None)
}

pub fn load(name: &str, dir: Option<&Path>) -> Result<Cow<'static, str>, String> {
    load_for("gemini-web", name, dir)
}

pub fn load_for(
    provider_id: &str,
    name: &str,
    dir: Option<&Path>,
) -> Result<Cow<'static, str>, String> {
    validate_script_name(name)?;
    if let Some(dir) = dir {
        return read_override(&dir.join(name), name);
    }
    if let Some(bundled) = crate::provider::bundled::script(provider_id, name) {
        return Ok(Cow::Borrowed(bundled));
    }
    Err(format!("unknown script {name} for provider {provider_id}"))
}

#[cfg(test)]
mod tests {
    use super::script_for_in_home;
    use tempfile::tempdir;

    #[test]
    fn script_search_order_uses_external_files_then_each_bundled_provider() {
        let dir = tempdir().unwrap();
        let home = dir.path();

        // Legacy `<ANTICRUMB_HOME>/adapters/` directories are ignored.
        let legacy_provider = home.join("adapters").join("gemini-web");
        let legacy_root = home.join("adapters");
        std::fs::create_dir_all(&legacy_provider).unwrap();
        std::fs::write(legacy_provider.join("driver.js"), "// legacy provider").unwrap();
        std::fs::write(legacy_root.join("driver.js"), "// legacy root").unwrap();

        let builtin = script_for_in_home(home, "gemini-web", "driver.js").unwrap();
        assert!(!builtin.starts_with("// legacy"));
        assert!(script_for_in_home(home, "unknown-web", "driver.js").is_err());

        for provider in crate::provider::bundled::IDS {
            for name in ["driver.js", "observer.js", "extract-response.js"] {
                let script = script_for_in_home(home, provider, name).unwrap();
                assert!(matches!(script, std::borrow::Cow::Borrowed(_)));
                assert!(!script.is_empty());
            }
        }
        assert!(script_for_in_home(home, "claude-web", "usage.js").is_ok());
        assert!(script_for_in_home(home, "chatgpt-web", "usage.js").is_err());

        // `<ANTICRUMB_HOME>/providers/<provider>/<name>` overrides built-in.
        let provider_dir = home.join("providers").join("gemini-web");
        std::fs::create_dir_all(&provider_dir).unwrap();
        std::fs::write(provider_dir.join("driver.js"), "// provider root").unwrap();
        assert_eq!(
            script_for_in_home(home, "gemini-web", "driver.js")
                .unwrap()
                .as_ref(),
            "// provider root"
        );

        // `<ANTICRUMB_HOME>/providers/<provider>/scripts/<name>` has highest priority.
        let scripts_dir = provider_dir.join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        std::fs::write(scripts_dir.join("driver.js"), "// provider scripts").unwrap();
        assert_eq!(
            script_for_in_home(home, "gemini-web", "driver.js")
                .unwrap()
                .as_ref(),
            "// provider scripts"
        );

        for provider in ["claude-web", "chatgpt-web"] {
            let directory = home.join("providers").join(provider).join("scripts");
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join("driver.js");
            std::fs::write(&path, "// external DOM v1").unwrap();
            assert_eq!(
                script_for_in_home(home, provider, "driver.js").unwrap(),
                "// external DOM v1"
            );
            std::fs::write(&path, "// external DOM v2").unwrap();
            assert_eq!(
                script_for_in_home(home, provider, "driver.js").unwrap(),
                "// external DOM v2"
            );
            // A partial override still has access to all other compiled scripts.
            assert!(matches!(
                script_for_in_home(home, provider, "observer.js").unwrap(),
                std::borrow::Cow::Borrowed(_)
            ));
            std::fs::remove_file(path).unwrap();
            assert!(matches!(
                script_for_in_home(home, provider, "driver.js").unwrap(),
                std::borrow::Cow::Borrowed(_)
            ));
        }
    }
}
