use super::{
    validate_provider_id, GeminiWebProvider, ManifestWebProvider, Provider, ProviderInfo,
    ProviderManifest,
};
use crate::{
    domain::{ErrorCode, Result, ServiceError},
    store::Store,
};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{Arc, RwLock},
};

pub const PROVIDER_STORE_KIND: &str = "provider_manifest";

pub struct ProviderRegistry {
    providers: RwLock<BTreeMap<String, Arc<dyn Provider>>>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self {
            providers: RwLock::new(BTreeMap::new()),
        }
    }
}

impl ProviderRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn with_builtins() -> Arc<Self> {
        let registry = Self::new();
        for id in super::bundled::IDS {
            let manifest: ProviderManifest =
                serde_json::from_str(super::bundled::manifest(id).unwrap())
                    .expect("bundled provider manifest must be valid JSON");
            manifest
                .validate()
                .expect("bundled provider manifest must be valid");
            let provider: Arc<dyn Provider> = if *id == super::DEFAULT_PROVIDER_ID {
                Arc::new(GeminiWebProvider {
                    inline_prompt_chars: manifest.resolved_inline_prompt_chars(),
                    source: "builtin",
                })
            } else {
                Arc::new(ManifestWebProvider::bundled(manifest).expect("valid bundled provider"))
            };
            registry
                .register(provider)
                .expect("bundled providers must have unique IDs");
        }
        registry
    }

    pub fn register(&self, provider: Arc<dyn Provider>) -> Result<ProviderInfo> {
        self.install(provider, false)
    }

    fn install(&self, provider: Arc<dyn Provider>, replace_builtin: bool) -> Result<ProviderInfo> {
        let info = provider.info();
        validate_provider_id(&info.id)?;
        let mut map = self.providers.write().map_err(ServiceError::storage)?;
        if map
            .get(&info.id)
            .is_some_and(|current| !replace_builtin || current.info().source != "builtin")
        {
            return Err(ServiceError::invalid(format!(
                "provider '{}' is already registered; unregister it before replacing",
                info.id
            )));
        }
        provider.on_register()?;
        let previous = map.insert(info.id.clone(), provider);
        drop(map);
        if let Some(previous) = previous {
            previous.on_unregister();
        }
        Ok(info)
    }

    fn load_manifest_override(&self, manifest: ProviderManifest) -> Result<ProviderInfo> {
        let provider: Arc<dyn Provider> = if manifest.id == super::DEFAULT_PROVIDER_ID {
            // Gemini keeps its native engine; its DOM files still use the same
            // external-before-bundled lookup as the scripted providers.
            manifest.validate()?;
            Arc::new(GeminiWebProvider {
                inline_prompt_chars: manifest.resolved_inline_prompt_chars(),
                source: "runtime",
            })
        } else {
            Arc::new(ManifestWebProvider::new(manifest)?)
        };
        self.install(provider, true)
    }

    pub fn register_manifest(&self, manifest: ProviderManifest) -> Result<ProviderInfo> {
        let provider = Arc::new(ManifestWebProvider::new(manifest)?);
        self.register(provider)
    }

    pub fn unregister(&self, id: &str) -> Result<ProviderInfo> {
        validate_provider_id(id)?;
        let removed = {
            let mut map = self.providers.write().map_err(ServiceError::storage)?;
            map.remove(id).ok_or_else(|| {
                ServiceError::new(
                    ErrorCode::NotFound,
                    "provider",
                    format!("provider '{id}' is not registered"),
                )
            })?
        };
        let info = removed.info();
        removed.on_unregister();
        Ok(info)
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.providers
            .read()
            .ok()
            .and_then(|map| map.get(id).cloned())
    }

    pub fn resolve(&self, id: &str) -> Result<Arc<dyn Provider>> {
        let key = if id.is_empty() {
            super::DEFAULT_PROVIDER_ID
        } else {
            id
        };
        self.providers
            .read()
            .map_err(ServiceError::storage)?
            .get(key)
            .cloned()
            .ok_or_else(|| {
                ServiceError::new(
                    ErrorCode::NotFound,
                    "provider",
                    format!("provider '{key}' is not registered"),
                )
            })
    }

    pub fn list(&self) -> Vec<ProviderInfo> {
        self.providers
            .read()
            .map(|map| map.values().map(|provider| provider.info()).collect())
            .unwrap_or_default()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.providers
            .read()
            .map(|map| map.contains_key(id))
            .unwrap_or(false)
    }

    pub fn load_providers_dir(&self, dir: &Path) -> Result<Vec<ProviderInfo>> {
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut loaded = Vec::new();
        let entries = fs::read_dir(dir).map_err(ServiceError::storage)?;
        let mut paths: Vec<_> = entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.is_dir())
            .collect();
        paths.sort();
        for provider_dir in paths {
            let manifest_path = provider_dir.join("provider.json");
            if !manifest_path.is_file() {
                continue;
            }
            let mut manifest = ProviderManifest::read_file(&manifest_path)?;
            if manifest.scripts_dir.is_none() {
                let scripts = provider_dir.join("scripts");
                manifest.scripts_dir = Some(if scripts.is_dir() {
                    scripts
                } else {
                    provider_dir
                });
            }
            loaded.push(self.load_manifest_override(manifest)?);
        }
        Ok(loaded)
    }

    pub fn load_persisted_manifests(&self, store: &Store) -> Result<Vec<ProviderInfo>> {
        let mut loaded = Vec::new();
        for manifest in store.list::<ProviderManifest>("local", PROVIDER_STORE_KIND)? {
            if self
                .get(&manifest.id)
                .is_some_and(|current| current.info().source != "builtin")
            {
                continue;
            }
            loaded.push(self.load_manifest_override(manifest)?);
        }
        Ok(loaded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::CancelToken,
        domain::{Account, BrowserProfile, ModelCatalogRecord, Session, TurnRequest, TurnResult},
        provider::{ProviderCapabilities, ProviderContext},
        service::Emit,
    };

    struct TestProvider {
        id: &'static str,
        unregistered: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Provider for TestProvider {
        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                id: self.id.into(),
                name: "Test Provider".into(),
                version: "v1".into(),
                kind: "test".into(),
                entry_url: Some("https://example.invalid/app".into()),
                default_model: "test-default".into(),
                source: "runtime".into(),
                capabilities: ProviderCapabilities::default(),
                inline_prompt_chars: super::super::DEFAULT_INLINE_PROMPT_CHARS,
            }
        }

        fn discover_models(
            &self,
            _: &ProviderContext<'_>,
            _: &BrowserProfile,
            _: &Account,
            _: CancelToken,
        ) -> Result<ModelCatalogRecord> {
            Ok(ModelCatalogRecord::default())
        }

        fn generate(
            &self,
            _: &ProviderContext<'_>,
            _: &BrowserProfile,
            _: &Account,
            session: &mut Session,
            _: &TurnRequest,
            _: CancelToken,
            _: Emit<'_>,
        ) -> Result<TurnResult> {
            Ok(TurnResult {
                session_id: session.id.clone(),
                text: format!("handled by {}", self.id),
                tool_calls: vec![],
                artifacts: vec![],
                model: None,
                elapsed_ms: 1,
            })
        }

        fn close_session(
            &self,
            _: &ProviderContext<'_>,
            _: &BrowserProfile,
            _: &Account,
            _: &Session,
        ) -> Result<()> {
            Ok(())
        }

        fn on_unregister(&self) {
            self.unregistered
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn runtime_register_and_unregister_updates_registry_and_triggers_lifecycle_hooks() {
        let registry = ProviderRegistry::with_builtins();
        assert!(registry.contains("gemini-web"));

        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let info = registry
            .register(Arc::new(TestProvider {
                id: "test-web",
                unregistered: flag.clone(),
            }))
            .unwrap();
        assert_eq!(info.id, "test-web");
        assert!(registry.contains("test-web"));
        assert_eq!(
            registry
                .list()
                .into_iter()
                .map(|p| p.id)
                .collect::<Vec<_>>(),
            vec!["chatgpt-web", "claude-web", "gemini-web", "test-web"]
        );

        let dup = registry.register(Arc::new(TestProvider {
            id: "test-web",
            unregistered: flag.clone(),
        }));
        assert_eq!(dup.unwrap_err().code, ErrorCode::InvalidRequest);

        let removed = registry.unregister("test-web").unwrap();
        assert_eq!(removed.id, "test-web");
        assert!(flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!registry.contains("test-web"));
        assert_eq!(
            registry.resolve("test-web").err().unwrap().code,
            ErrorCode::NotFound
        );
    }

    #[test]
    fn all_bundled_providers_work_without_external_files_and_overrides_take_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state")).unwrap();
        let registry = ProviderRegistry::with_builtins();
        assert_eq!(registry.list().len(), 3);
        for id in super::super::bundled::IDS {
            let manifest: ProviderManifest =
                serde_json::from_str(super::super::bundled::manifest(id).unwrap()).unwrap();
            manifest.validate().unwrap();
            let info = registry.resolve(id).unwrap().info();
            assert_eq!(info.id, *id);
            assert_eq!(info.source, "builtin");
            assert_eq!(
                info.inline_prompt_chars,
                super::super::DEFAULT_INLINE_PROMPT_CHARS
            );
            assert!(info.capabilities.file_uploads);
        }
        let mut persisted: ProviderManifest =
            serde_json::from_str(super::super::bundled::manifest("claude-web").unwrap()).unwrap();
        persisted.name = "Persisted Claude".into();
        persisted.inline_prompt_chars = Some(900);
        store
            .put("local", PROVIDER_STORE_KIND, &persisted.id, &persisted)
            .unwrap();
        registry.load_persisted_manifests(&store).unwrap();
        assert_eq!(
            registry.resolve("claude-web").unwrap().info().name,
            "Persisted Claude"
        );

        let provider_dir = dir.path().join("providers/claude-web");
        fs::create_dir_all(&provider_dir).unwrap();
        let mut external = persisted;
        external.name = "External Claude".into();
        external.inline_prompt_chars = Some(1200);
        fs::write(
            provider_dir.join("provider.json"),
            serde_json::to_vec(&external).unwrap(),
        )
        .unwrap();
        let restarted = ProviderRegistry::with_builtins();
        restarted
            .load_providers_dir(&dir.path().join("providers"))
            .unwrap();
        restarted.load_persisted_manifests(&store).unwrap();
        let info = restarted.resolve("claude-web").unwrap().info();
        assert_eq!(info.name, "External Claude");
        assert_eq!(info.inline_prompt_chars, 1200);
        assert_eq!(info.source, "runtime");
        assert_eq!(
            restarted.resolve("chatgpt-web").unwrap().info().source,
            "builtin"
        );
    }

    #[test]
    fn gemini_settings_file_changes_transport_without_replacing_native_provider() {
        let dir = tempfile::tempdir().unwrap();
        let provider_dir = dir.path().join("gemini-web");
        fs::create_dir_all(&provider_dir).unwrap();
        for setting in [None, Some(1000), Some(3000), Some(0)] {
            let mut settings = serde_json::json!({"id":"gemini-web", "name":"Gemini Web"});
            if let Some(limit) = setting {
                settings["inline_prompt_chars"] = serde_json::json!(limit);
            }
            fs::write(provider_dir.join("provider.json"), settings.to_string()).unwrap();
            let registry = ProviderRegistry::with_builtins();
            let loaded = registry.load_providers_dir(dir.path()).unwrap();
            assert_eq!(loaded.len(), 1);
            let provider = registry.resolve("gemini-web").unwrap();
            let info = provider.info();
            assert_eq!(
                info.inline_prompt_chars,
                setting.unwrap_or(super::super::DEFAULT_INLINE_PROMPT_CHARS)
            );
            assert_eq!(info.kind, "builtin_web");
            assert!(info.capabilities.file_uploads);
            assert_eq!(
                provider.default_login_url(),
                Some(crate::browser_host::GEMINI_APP_URL)
            );
        }
        fs::write(
            provider_dir.join("provider.json"),
            r#"{"id":"gemini-web","name":"Gemini Web","inline_prompt_chars":-1}"#,
        )
        .unwrap();
        assert!(ProviderRegistry::with_builtins()
            .load_providers_dir(dir.path())
            .is_err());
    }

    #[test]
    fn directory_provider_manifests_can_be_loaded_and_removed_at_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let provider_dir = dir.path().join("chatgpt-web");
        std::fs::create_dir_all(provider_dir.join("scripts")).unwrap();
        std::fs::write(
            provider_dir.join("provider.json"),
            serde_json::to_string_pretty(&ProviderManifest {
                id: "chatgpt-web".into(),
                name: "ChatGPT Web".into(),
                version: "v1".into(),
                kind: "web_provider".into(),
                entry_url: Some("https://chatgpt.com/".into()),
                allowed_origins: vec!["https://chatgpt.com".into()],
                default_model: Some("chatgpt-web-default".into()),
                scripts_dir: None,
                scripts: BTreeMap::new(),
                capabilities: ProviderCapabilities {
                    browser_auth: true,
                    temporary_sessions: true,
                    ..Default::default()
                },
                inline_prompt_chars: Some(1234),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();

        let registry = ProviderRegistry::with_builtins();
        let loaded = registry.load_providers_dir(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "chatgpt-web");
        assert_eq!(loaded[0].inline_prompt_chars, 1234);
        assert_eq!(
            registry.resolve("chatgpt-web").unwrap().default_login_url(),
            Some("https://chatgpt.com/")
        );

        registry.unregister("chatgpt-web").unwrap();
        assert!(!registry.contains("chatgpt-web"));
    }

    #[test]
    fn directory_provider_resolves_explicit_relative_scripts_dir() {
        let dir = tempfile::tempdir().unwrap();
        let provider_dir = dir.path().join("relative-test-web");
        fs::create_dir_all(provider_dir.join("scripts")).unwrap();
        fs::write(provider_dir.join("scripts/driver.js"), "() => ({})").unwrap();
        fs::write(
            provider_dir.join("provider.json"),
            r#"{
                "id": "relative-test-web",
                "name": "Relative Scripts Test",
                "entry_url": "https://example.invalid/",
                "scripts_dir": "scripts",
                "capabilities": {"browser_auth": true}
            }"#,
        )
        .unwrap();
        assert_ne!(std::env::current_dir().unwrap(), provider_dir);

        let registry = ProviderRegistry::new();
        registry.load_providers_dir(dir.path()).unwrap();
        let provider = registry.resolve("relative-test-web").unwrap();
        let browser = BrowserProfile {
            provider_id: "relative-test-web".into(),
            profile: dir.path().join("profile"),
            subprofile: None,
            port: 0,
            headless: true,
        };
        let account = Account {
            id: "relative-test".into(),
            provider: "relative-test-web".into(),
            state: "ready".into(),
            identity: None,
            label: None,
            slot: None,
        };
        let cancel = CancelToken::new();
        cancel.cancel();

        // Missing driver.js returns a static catalog. Reaching cancellation proves
        // the registered provider found its script, without accessing a browser.
        let error = provider
            .discover_models(&ProviderContext::stateless(), &browser, &account, cancel)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Cancelled);
    }
}
