//! Browser tab pool for Gemini Web anti-bot tab reuse and multi-agent isolation.
//!
//! Each account uses a 2-tier tab pool in Chrome:
//! - `Master` tab(s) (1 dedicated tab per active project workspace TUI, unlimited workspaces by default):
//!   persistent project conversation tabs (`SessionMode::Chat`). Reused in-place via SPA
//!   `New Chat` reset or hydrated from saved `session.remote_url` (`https://gemini.google.com/app/<id>`).
//! - `Worker` tab pool (default 2, configurable via `--workers N`):
//!   shared by concurrent multi-agent `TempChat` runs (`SessionMode::Temporary`) and high-priority
//!   system tasks (`/model refresh`, `/usage`, web history enumeration, and conversation deletion).
//!   - System tasks preempt any `Idle` `Worker` tab in-place (zero tab creation).
//!   - If all `Worker` tabs are actively running multi-step `TempChat` turns, a system task
//!     opens 1 temporary overflow tab and closes it immediately upon completion.
//!
//! External web fetches (`browser_fetch`) are intentionally excluded from this pool and run
//! in their own isolated ephemeral tabs without holding the Gemini account gate.

use crate::{
    browser_host::{self as browser, TargetInfo},
    domain::{now_ms, Account, Result, ServiceError, Session, SessionMode, SessionState},
    gemini,
    provider::AccountRoute,
    store::Store,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
};

/// Total live page tabs allowed across all accounts sharing a single provider Chrome instance
/// before evicting the globally oldest idle `Worker` tab from another account.
pub const MAX_SHARED_BROWSER_TABS: usize = 12;

static WORKER_TABS_OVERRIDE: AtomicUsize = AtomicUsize::new(0);
static MASTER_TABS_OVERRIDE: AtomicUsize = AtomicUsize::new(0);

fn pool_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn active_system_tabs() -> &'static Mutex<HashSet<String>> {
    static BUSY: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    BUSY.get_or_init(|| Mutex::new(HashSet::new()))
}

fn busy_system_tab_ids() -> HashSet<String> {
    active_system_tabs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

pub(crate) struct SystemTabLease {
    target_id: String,
}

impl SystemTabLease {
    pub(crate) fn acquire(target_id: impl Into<String>) -> Self {
        let target_id = target_id.into();
        active_system_tabs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(target_id.clone());
        Self { target_id }
    }
}

impl Drop for SystemTabLease {
    fn drop(&mut self) {
        active_system_tabs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.target_id);
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TabRole {
    Master,
    #[serde(alias = "system")]
    Worker,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TabPoolConfig {
    /// Optional cap on Master tabs (`None` by default = 1 dedicated tab per active workspace, unlimited).
    pub master_tabs: Option<usize>,
    /// Number of reusable Worker (TempChat) tabs in the pool (default 2, configurable via `--workers N`).
    pub worker_tabs: usize,
}

impl Default for TabPoolConfig {
    fn default() -> Self {
        Self {
            master_tabs: None,
            worker_tabs: 2,
        }
    }
}

pub fn set_worker_tabs(store: Option<&Store>, worker_tabs: usize) {
    let clamped = worker_tabs.clamp(1, 16);
    WORKER_TABS_OVERRIDE.store(clamped, Ordering::SeqCst);
    if let Some(store) = store {
        let mut cfg = store
            .get::<Value>("local", "runtime", "tab_pool_config")
            .unwrap_or_else(|_| json!({}));
        cfg["worker_tabs"] = json!(clamped);
        let _ = store.put("local", "runtime", "tab_pool_config", &cfg);
    }
}
pub(crate) fn worker_override() -> Option<u32> {
    let value = WORKER_TABS_OVERRIDE.load(Ordering::SeqCst);
    (value > 0).then_some(value as u32)
}

pub fn set_master_tabs(store: Option<&Store>, master_tabs: usize) {
    let clamped = master_tabs.clamp(1, 64);
    MASTER_TABS_OVERRIDE.store(clamped, Ordering::SeqCst);
    if let Some(store) = store {
        let mut cfg = store
            .get::<Value>("local", "runtime", "tab_pool_config")
            .unwrap_or_else(|_| json!({}));
        cfg["master_tabs"] = json!(clamped);
        let _ = store.put("local", "runtime", "tab_pool_config", &cfg);
    }
}

impl TabPoolConfig {
    pub fn load(store: Option<&Store>) -> Self {
        let stored = store.and_then(|s| s.get::<Value>("local", "runtime", "tab_pool_config").ok());
        let stored_workers = stored
            .as_ref()
            .and_then(|v| v["worker_tabs"].as_u64())
            .map(|n| (n as usize).clamp(1, 16));
        let stored_masters = stored
            .as_ref()
            .and_then(|v| v["master_tabs"].as_u64())
            .map(|n| (n as usize).clamp(1, 64));

        let worker_override = WORKER_TABS_OVERRIDE.load(Ordering::SeqCst);
        let master_override = MASTER_TABS_OVERRIDE.load(Ordering::SeqCst);

        let worker_tabs = if worker_override > 0 {
            worker_override
        } else {
            stored_workers.unwrap_or(2)
        };

        let master_tabs = if master_override > 0 {
            Some(master_override)
        } else {
            stored_masters
        };
        Self {
            master_tabs,
            worker_tabs,
        }
    }

    pub fn max_tabs(&self) -> usize {
        match self.master_tabs {
            Some(masters) => masters + self.worker_tabs,
            None => usize::MAX,
        }
    }

    pub fn capacity(&self, role: TabRole) -> usize {
        match role {
            TabRole::Master => self.master_tabs.unwrap_or(usize::MAX),
            TabRole::Worker => self.worker_tabs,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TabSlot {
    pub target_id: String,
    pub role: TabRole,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub last_used: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TabPoolState {
    #[serde(default)]
    pub slots: Vec<TabSlot>,
}

pub(crate) fn system_tab_id(store: &Store, account_id: &str) -> Option<String> {
    if let Some(id) = store
        .get::<Value>(account_id, "runtime", "system_tab")
        .ok()
        .and_then(|v| v["target_id"].as_str().map(str::to_owned))
        .filter(|id| !id.is_empty())
    {
        return Some(id);
    }
    load_state(store, account_id)
        .slots
        .into_iter()
        .find(|s| s.role == TabRole::Worker && s.session_id.is_none() && !s.target_id.is_empty())
        .map(|s| s.target_id)
}

pub(crate) fn all_worker_tab_ids(store: &Store, extra_account: Option<&str>) -> HashSet<String> {
    let mut ids = HashSet::new();
    let mut account_ids: Vec<String> = store
        .list::<Account>("local", "account")
        .unwrap_or_default()
        .into_iter()
        .map(|a| a.id)
        .collect();
    if let Some(extra) = extra_account {
        if !extra.is_empty() && !account_ids.iter().any(|id| id == extra) {
            account_ids.push(extra.to_owned());
        }
    }
    for account_id in account_ids {
        if let Some(id) = system_tab_id(store, &account_id) {
            ids.insert(id);
        }
        for slot in load_state(store, &account_id).slots {
            if slot.role == TabRole::Worker {
                ids.insert(slot.target_id);
            }
        }
    }
    ids
}

/// Collect every Master, Worker, and system tab ID belonging to OTHER accounts so one account's
/// TabPool never adopts or steals a warm tab owned by a sibling user sharing the same Chrome.
pub(crate) fn all_other_account_pool_tab_ids(store: &Store, account_id: &str) -> HashSet<String> {
    let mut ids = HashSet::new();
    for acct in store
        .list::<Account>("local", "account")
        .unwrap_or_default()
    {
        if acct.id == account_id {
            continue;
        }
        if let Some(id) = system_tab_id(store, &acct.id) {
            ids.insert(id);
        }
        for slot in load_state(store, &acct.id).slots {
            if !slot.target_id.is_empty() {
                ids.insert(slot.target_id);
            }
        }
    }
    ids
}

/// Enroll or update a tab used by a system task (`/usage`, `/model refresh`, `/delete`) in the
/// `Worker` pool. Returns `true` if the tab belongs to the persistent `Worker` pool (and must NOT
/// be closed on drop), or `false` if all `Worker` slots are currently busy with active multi-agent
/// runs so this tab is an overflow temporary tab that should be closed immediately on completion.
pub(crate) fn record_system_tab_id(
    store: &Store,
    account_id: &str,
    port: u16,
    target_id: &str,
) -> bool {
    let _pool_guard = pool_lock().lock().unwrap_or_else(|e| e.into_inner());
    let config = TabPoolConfig::load(Some(store));
    let effective_port = if port > 0 {
        port
    } else {
        store
            .get::<Account>("local", "account", account_id)
            .ok()
            .and_then(|a| store.browser_for_account(&a).ok())
            .map(|b| b.port)
            .unwrap_or(0)
    };
    let mut state = if effective_port > 0 {
        match browser::list_targets(effective_port) {
            Ok(targets) => reconcile(store, account_id, &targets),
            Err(_) => load_state(store, account_id),
        }
    } else {
        load_state(store, account_id)
    };

    if let Some(slot) = state.slots.iter_mut().find(|s| s.target_id == target_id) {
        slot.role = TabRole::Worker;
        slot.last_used = now_ms();
        let _ = store.put(
            account_id,
            "runtime",
            "system_tab",
            &json!({"target_id": target_id}),
        );
        let _ = save_state(store, account_id, &state);
        return true;
    }

    let worker_count = state
        .slots
        .iter()
        .filter(|s| s.role == TabRole::Worker)
        .count();
    if worker_count < config.worker_tabs {
        state.slots.push(TabSlot {
            target_id: target_id.to_owned(),
            role: TabRole::Worker,
            session_id: None,
            last_used: now_ms(),
        });
        let _ = store.put(
            account_id,
            "runtime",
            "system_tab",
            &json!({"target_id": target_id}),
        );
        let _ = save_state(store, account_id, &state);
        true
    } else {
        // All Worker pool slots are currently busy: treat this tab as a temporary overflow tab.
        false
    }
}

pub(crate) fn clear_system_tab_id(store: &Store, account_id: &str) {
    let _ = store.put(
        account_id,
        "runtime",
        "system_tab",
        &json!({"target_id": null}),
    );
}

pub(crate) fn load_state(store: &Store, account_id: &str) -> TabPoolState {
    store
        .get::<TabPoolState>(account_id, "runtime", "tab_pool")
        .unwrap_or_default()
}

pub(crate) fn save_state(store: &Store, account_id: &str, state: &TabPoolState) -> Result<()> {
    store.put(account_id, "runtime", "tab_pool", state)
}

fn active_sessions(store: &Store) -> Vec<Session> {
    store
        .list::<Session>("local", "session")
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !s.is_inactive())
        .collect()
}

pub(crate) fn active_session_targets(store: &Store) -> Option<HashSet<String>> {
    Some(
        store
            .list::<Session>("local", "session")
            .ok()?
            .into_iter()
            .filter(|s| !s.is_inactive())
            .filter_map(|s| s.target_id)
            .collect(),
    )
}

/// Media recovery may reuse only a live target still owned by this conversation.
/// Closed sessions retain target IDs for history, but their tabs can be recycled.
pub(crate) fn owned_media_target(
    store: &Store,
    session: &Session,
    targets: Vec<TargetInfo>,
) -> Result<Option<TargetInfo>> {
    if session.is_inactive() {
        return Ok(None);
    }
    let Some(id) = session.target_id.as_deref() else {
        return Ok(None);
    };
    if store
        .list::<Session>("local", "session")?
        .iter()
        .any(|other| {
            other.id != session.id && !other.is_inactive() && other.target_id.as_deref() == Some(id)
        })
    {
        return Ok(None);
    }
    let Some(expected) = session
        .remote_url
        .as_deref()
        .and_then(|url| crate::url_lite::Url::parse(url).ok())
    else {
        return Ok(None);
    };
    Ok(targets.into_iter().find(|target| {
        target.id == id
            && target.target_type == "page"
            && crate::url_lite::Url::parse(&target.url).is_ok_and(|actual| {
                actual.origin_ascii() == expected.origin_ascii()
                    && actual.path() == expected.path()
                    && actual.query() == expected.query()
            })
    }))
}

/// Reconcile persisted pool slots with live Chrome targets and active sessions.
pub(crate) fn reconcile(store: &Store, account_id: &str, targets: &[TargetInfo]) -> TabPoolState {
    let live_ids: HashSet<&str> = targets
        .iter()
        .filter(|t| t.target_type == "page")
        .map(|t| t.id.as_str())
        .collect();
    let sessions = active_sessions(store);
    let mut state = load_state(store, account_id);

    // Drop slots whose Chrome tab no longer exists.
    state
        .slots
        .retain(|s| live_ids.contains(s.target_id.as_str()));

    // Ensure recorded system_tab is reflected as an idle TabRole::Worker slot if alive and unowned.
    let config = TabPoolConfig::load(Some(store));
    let owned_targets: HashSet<&str> = sessions
        .iter()
        .filter_map(|s| s.target_id.as_deref())
        .collect();
    if let Some(sys_id) = store
        .get::<Value>(account_id, "runtime", "system_tab")
        .ok()
        .and_then(|v| v["target_id"].as_str().map(str::to_owned))
        .filter(|id| !id.is_empty())
    {
        if live_ids.contains(sys_id.as_str()) && !owned_targets.contains(sys_id.as_str()) {
            let worker_count = state
                .slots
                .iter()
                .filter(|s| s.role == TabRole::Worker)
                .count();
            if let Some(slot) = state.slots.iter_mut().find(|s| s.target_id == sys_id) {
                slot.role = TabRole::Worker;
            } else if worker_count < config.worker_tabs {
                state.slots.push(TabSlot {
                    target_id: sys_id,
                    role: TabRole::Worker,
                    session_id: None,
                    last_used: now_ms(),
                });
            }
        }
    }

    // Release session bindings for sessions that are closed/deleted/invalidated/outcome_unknown,
    // keeping the underlying tab warm in its role (Master or Worker) for in-place reuse.
    let active_by_id: std::collections::HashMap<&str, &Session> =
        sessions.iter().map(|s| (s.id.as_str(), s)).collect();
    for slot in &mut state.slots {
        if let Some(sid) = slot.session_id.as_deref() {
            if let Some(sess) = active_by_id.get(sid) {
                // Acquisition reserves a slot before the provider saves target_id.
                // Keep that reservation while preparation is still running.
                if sess.target_id.as_deref() != Some(slot.target_id.as_str())
                    && sess.state != SessionState::Running
                {
                    slot.session_id = None;
                }
            } else {
                slot.session_id = None;
            }
        }
    }

    // Ensure every active session for this account with a live target_id is represented in slots.
    for sess in sessions.iter().filter(|s| s.account_id == account_id) {
        let Some(tid) = sess.target_id.as_deref() else {
            continue;
        };
        if !live_ids.contains(tid) {
            continue;
        }
        let role = match sess.mode {
            SessionMode::Chat => TabRole::Master,
            SessionMode::Temporary => TabRole::Worker,
        };
        if let Some(slot) = state.slots.iter_mut().find(|s| s.target_id == tid) {
            slot.role = role;
            slot.session_id = Some(sess.id.clone());
            slot.last_used = slot.last_used.max(sess.last_used);
        } else {
            state.slots.push(TabSlot {
                target_id: tid.to_owned(),
                role,
                session_id: Some(sess.id.clone()),
                last_used: sess.last_used.max(now_ms()),
            });
        }
    }

    let _ = save_state(store, account_id, &state);
    state
}

pub(crate) struct TabPoolMatcher<'a> {
    pub entry_url: &'a str,
    pub is_reusable_url: &'a dyn Fn(&str) -> bool,
    pub is_chat_url: &'a dyn Fn(&str) -> bool,
    pub is_system_page_url: &'a dyn Fn(&str) -> bool,
}

pub(crate) fn gemini_matcher_for<R>(
    route: &AccountRoute,
    f: impl FnOnce(&TabPoolMatcher<'_>) -> R,
) -> R {
    let scope = route.scope.as_deref();
    let is_reusable = |url: &str| gemini::is_reusable_gemini_url_for(url, scope);
    let is_chat = |url: &str| gemini::url_has_chat_id(url);
    let is_system = |url: &str| gemini::is_gemini_usage_url_for(url, scope);
    let matcher = TabPoolMatcher {
        entry_url: &route.entry_url,
        is_reusable_url: &is_reusable,
        is_chat_url: &is_chat,
        is_system_page_url: &is_system,
    };
    f(&matcher)
}

#[cfg(test)]
fn default_gemini_route() -> AccountRoute {
    AccountRoute {
        entry_url: gemini::gemini_app_url(Some("0")),
        usage_url: Some(gemini::gemini_usage_url(Some("0"))),
        scope: Some("0".into()),
    }
}

/// Find a reusable non-worker `Master` tab for persistent chat sessions matching a provider matcher.
pub(crate) fn find_reusable_target_for(
    store: &Store,
    account_id: &str,
    port: u16,
    matcher: &TabPoolMatcher<'_>,
) -> Option<TargetInfo> {
    let owned = active_session_targets(store)?;
    let busy_system = busy_system_tab_ids();
    let worker_tabs = all_worker_tab_ids(store, Some(account_id));
    let other_pool_tabs = all_other_account_pool_tab_ids(store, account_id);
    browser::list_targets(port).ok()?.into_iter().find(|t| {
        t.target_type == "page"
            && !owned.contains(&t.id)
            && !busy_system.contains(&t.id)
            && !worker_tabs.contains(&t.id)
            && !other_pool_tabs.contains(&t.id)
            && (matcher.is_reusable_url)(&t.url)
    })
}

/// Find a reusable non-worker `Master` Gemini tab for persistent chat sessions.
#[cfg(test)]
pub(crate) fn find_reusable_gemini_target(
    store: &Store,
    account_id: &str,
    port: u16,
) -> Option<TargetInfo> {
    gemini_matcher_for(&default_gemini_route(), |matcher| {
        find_reusable_target_for(store, account_id, port, matcher)
    })
}

/// Preempt an `Idle` `Worker` tab from the pool for high-priority system operations
/// (`/usage`, history, catalog discovery, chat deletion).
/// - Never hijacks a `Master` conversation tab or an active `Worker` tab
///   that is currently mid-run (`owned`).
/// - Returns `None` when all `Worker` slots are currently busy with active multi-agent runs,
///   signaling the caller to open a temporary overflow tab and close it after the system task.
pub(crate) fn find_system_target_for(
    store: &Store,
    account_id: &str,
    port: u16,
    matcher: &TabPoolMatcher<'_>,
) -> Option<TargetInfo> {
    let _pool_guard = pool_lock().lock().unwrap_or_else(|e| e.into_inner());
    find_system_target_for_locked(store, account_id, port, matcher)
}

pub(crate) fn reserve_system_target_for(
    store: &Store,
    account_id: &str,
    port: u16,
    matcher: &TabPoolMatcher<'_>,
) -> Option<(TargetInfo, SystemTabLease)> {
    let _pool_guard = pool_lock().lock().unwrap_or_else(|e| e.into_inner());
    let target = find_system_target_for_locked(store, account_id, port, matcher)?;
    let lease = SystemTabLease::acquire(target.id.clone());
    Some((target, lease))
}

fn find_system_target_for_locked(
    store: &Store,
    account_id: &str,
    port: u16,
    matcher: &TabPoolMatcher<'_>,
) -> Option<TargetInfo> {
    let config = TabPoolConfig::load(Some(store));
    let mut owned = active_session_targets(store)?;
    owned.extend(busy_system_tab_ids());
    let other_pool_tabs = all_other_account_pool_tab_ids(store, account_id);
    let targets = browser::list_targets(port).ok()?;
    let state = reconcile(store, account_id, &targets);

    // 1. Prefer the last-used idle Worker tab (or any idle Worker slot in the pool).
    if let Some(saved_id) = system_tab_id(store, account_id) {
        let is_idle_worker = state.slots.iter().any(|s| {
            s.target_id == saved_id && s.role == TabRole::Worker && s.session_id.is_none()
        });
        if is_idle_worker && !owned.contains(&saved_id) && !other_pool_tabs.contains(&saved_id) {
            if let Some(found) = targets
                .iter()
                .find(|t| t.target_type == "page" && t.id == saved_id)
            {
                return Some(found.clone());
            }
        }
    }
    if let Some(slot) = state
        .slots
        .iter()
        .filter(|s| {
            s.role == TabRole::Worker
                && s.session_id.is_none()
                && !owned.contains(&s.target_id)
                && !other_pool_tabs.contains(&s.target_id)
        })
        .min_by_key(|s| s.last_used)
    {
        if let Some(found) = targets
            .iter()
            .find(|t| t.target_type == "page" && t.id == slot.target_id)
        {
            return Some(found.clone());
        }
    }

    // 2. If all configured Worker slots are already allocated (and busy), return None so the
    //    system task opens a temporary overflow tab instead of disturbing active tabs.
    let worker_count = state
        .slots
        .iter()
        .filter(|s| s.role == TabRole::Worker)
        .count();
    if worker_count >= config.worker_tabs {
        return None;
    }

    // 3. Otherwise, adopt an untracked non-conversation page matching this account's route into
    //    the Worker pool without ever taking tabs pooled by sibling accounts.
    let tracked_ids: HashSet<&str> = state.slots.iter().map(|s| s.target_id.as_str()).collect();
    targets.into_iter().find(|t| {
        t.target_type == "page"
            && !tracked_ids.contains(t.id.as_str())
            && !owned.contains(&t.id)
            && !other_pool_tabs.contains(&t.id)
            && !(matcher.is_chat_url)(&t.url)
            && ((matcher.is_reusable_url)(&t.url) || (matcher.is_system_page_url)(&t.url))
    })
}

pub(crate) fn find_system_gemini_target_for_route(
    store: &Store,
    account_id: &str,
    port: u16,
    route: &AccountRoute,
) -> Option<TargetInfo> {
    gemini_matcher_for(route, |matcher| {
        find_system_target_for(store, account_id, port, matcher)
    })
}

pub(crate) fn reserve_system_gemini_target_for_route(
    store: &Store,
    account_id: &str,
    port: u16,
    route: &AccountRoute,
) -> Option<(TargetInfo, SystemTabLease)> {
    gemini_matcher_for(route, |matcher| {
        reserve_system_target_for(store, account_id, port, matcher)
    })
}

#[cfg(test)]
pub(crate) fn find_system_gemini_target(
    store: &Store,
    account_id: &str,
    port: u16,
) -> Option<TargetInfo> {
    find_system_gemini_target_for_route(store, account_id, port, &default_gemini_route())
}

#[cfg(test)]
pub(crate) fn reserve_system_gemini_target(
    store: &Store,
    account_id: &str,
    port: u16,
) -> Option<(TargetInfo, SystemTabLease)> {
    reserve_system_gemini_target_for_route(store, account_id, port, &default_gemini_route())
}

/// Evict the globally least-recently-used idle Worker tab from another account sharing this
/// browser whenever live page tabs reach `MAX_SHARED_BROWSER_TABS`.
/// Master tabs, active sessions, and busy system tabs are strictly protected.
fn evict_shared_browser_tab_if_needed(
    store: &Store,
    current_account_id: &str,
    port: u16,
    targets: &[TargetInfo],
    owned: &HashSet<String>,
) {
    let live_page_ids: HashSet<&str> = targets
        .iter()
        .filter(|t| t.target_type == "page")
        .map(|t| t.id.as_str())
        .collect();
    if live_page_ids.len() < MAX_SHARED_BROWSER_TABS {
        return;
    }
    let current_provider = store
        .get::<Account>("local", "account", current_account_id)
        .ok()
        .map(|a| a.provider);
    let running_session_ids: HashSet<String> = store
        .list::<Session>("local", "session")
        .unwrap_or_default()
        .into_iter()
        .filter(|s| matches!(s.state, SessionState::Running | SessionState::AwaitingHuman))
        .map(|s| s.id)
        .collect();

    let mut victim: Option<(String, String, i64)> = None;
    for acct in store
        .list::<Account>("local", "account")
        .unwrap_or_default()
    {
        if acct.id == current_account_id {
            continue;
        }
        if current_provider
            .as_deref()
            .is_some_and(|p| p != acct.provider)
        {
            continue;
        }
        let state = load_state(store, &acct.id);
        for slot in state.slots {
            if slot.role != TabRole::Worker {
                continue;
            }
            if !live_page_ids.contains(slot.target_id.as_str())
                || owned.contains(&slot.target_id)
                || slot
                    .session_id
                    .as_deref()
                    .is_some_and(|sid| running_session_ids.contains(sid))
            {
                continue;
            }
            if victim
                .as_ref()
                .is_none_or(|(_, _, oldest)| slot.last_used < *oldest)
            {
                victim = Some((acct.id.clone(), slot.target_id.clone(), slot.last_used));
            }
        }
    }

    if let Some((victim_account, victim_target, _)) = victim {
        let mut state = load_state(store, &victim_account);
        if let Some(removed) = state
            .slots
            .iter()
            .find(|s| s.target_id == victim_target)
            .cloned()
        {
            if let Some(old_sid) = removed.session_id {
                if let Ok(mut old_session) = store.get::<Session>("local", "session", &old_sid) {
                    if old_session.target_id.as_deref() == Some(victim_target.as_str()) {
                        old_session.target_id = None;
                        if old_session.mode == SessionMode::Temporary
                            && old_session.state == SessionState::Ready
                        {
                            old_session.state = SessionState::Invalidated;
                        }
                        let _ = store.put("local", "session", &old_session.id, &old_session);
                    }
                }
            }
        }
        state.slots.retain(|s| s.target_id != victim_target);
        let _ = save_state(store, &victim_account, &state);
        if system_tab_id(store, &victim_account).as_deref() == Some(victim_target.as_str()) {
            clear_system_tab_id(store, &victim_account);
        }
        let _ = browser::close_tab(port, &victim_target);
    }
}

/// Acquire or allocate a `Master` (`SessionMode::Chat`) or `Worker` (`SessionMode::Temporary`)
/// tab from the account's pool. Returns `(TargetInfo, is_reused_existing_tab)`.
pub(crate) fn acquire_session_tab_for(
    store: &Store,
    account_id: &str,
    port: u16,
    session_id: &str,
    mode: &SessionMode,
    matcher: &TabPoolMatcher<'_>,
) -> Result<(TargetInfo, bool)> {
    let _pool_guard = pool_lock().lock().unwrap_or_else(|e| e.into_inner());
    let role = match mode {
        SessionMode::Chat => TabRole::Master,
        SessionMode::Temporary => TabRole::Worker,
    };
    let config = TabPoolConfig::load(Some(store));
    let targets = browser::list_targets(port).map_err(|e| ServiceError::backend(e, false))?;
    let mut state = reconcile(store, account_id, &targets);
    let mut owned = active_session_targets(store).unwrap_or_default();
    owned.extend(busy_system_tab_ids());
    let worker_tabs = all_worker_tab_ids(store, Some(account_id));
    let other_pool_tabs = all_other_account_pool_tab_ids(store, account_id);

    let find_reusable_target = |id: &str| -> Option<TargetInfo> {
        targets
            .iter()
            .find(|t| {
                t.target_type == "page"
                    && t.id == id
                    && !other_pool_tabs.contains(&t.id)
                    && (role == TabRole::Worker || !worker_tabs.contains(&t.id))
                    && ((matcher.is_reusable_url)(&t.url)
                        || (role == TabRole::Worker && (matcher.is_system_page_url)(&t.url)))
            })
            .cloned()
    };

    // 1. Prefer an idle pooled slot of the exact requested role (Master or Worker).
    //    For Master, prefer the most-recently-released slot so a workspace starting `/new`
    //    reclaims its own warm Master tab; for Worker, rotate across idle worker tabs.
    let idle_candidates = state.slots.iter().enumerate().filter(|(_, s)| {
        s.role == role
            && s.session_id.is_none()
            && !owned.contains(&s.target_id)
            && find_reusable_target(&s.target_id).is_some()
    });
    let chosen_idle = match role {
        TabRole::Master => idle_candidates.max_by_key(|(_, s)| s.last_used),
        TabRole::Worker => idle_candidates.min_by_key(|(_, s)| s.last_used),
    };
    if let Some((slot_idx, _)) = chosen_idle {
        let target = find_reusable_target(&state.slots[slot_idx].target_id).unwrap();
        state.slots[slot_idx].session_id = Some(session_id.to_owned());
        state.slots[slot_idx].last_used = now_ms();
        let _ = save_state(store, account_id, &state);
        return Ok((target, true));
    }

    // 2. Adopt an untracked reusable provider page in Chrome matching this account's route
    //    before opening new tabs (never taking tabs pooled by sibling accounts).
    let tracked_ids: HashSet<&str> = state.slots.iter().map(|s| s.target_id.as_str()).collect();
    if let Some(untracked) = targets.iter().find(|t| {
        t.target_type == "page"
            && !tracked_ids.contains(t.id.as_str())
            && !owned.contains(&t.id)
            && !other_pool_tabs.contains(&t.id)
            && (role == TabRole::Worker || !worker_tabs.contains(&t.id))
            && (matcher.is_reusable_url)(&t.url)
    }) {
        state.slots.push(TabSlot {
            target_id: untracked.id.clone(),
            role,
            session_id: Some(session_id.to_owned()),
            last_used: now_ms(),
        });
        let _ = save_state(store, account_id, &state);
        return Ok((untracked.clone(), true));
    }

    // 3. If current tabs for this role are below role capacity, create a new tab in Chrome.
    let role_count = state.slots.iter().filter(|s| s.role == role).count();
    if role_count < config.capacity(role) {
        evict_shared_browser_tab_if_needed(store, account_id, port, &targets, &owned);
        let created = browser::create_tab(port, matcher.entry_url)
            .map_err(|e| ServiceError::backend(e, false))?;
        state.slots.push(TabSlot {
            target_id: created.id.clone(),
            role,
            session_id: Some(session_id.to_owned()),
            last_used: now_ms(),
        });
        let _ = save_state(store, account_id, &state);
        return Ok((created, false));
    }

    // 4. Role is at capacity: evict the least-recently-used non-running session slot of this role
    //    (detaching the old session's target_id while preserving its remote_url for later hydration).
    let running_session_ids: HashSet<String> = store
        .list::<Session>("local", "session")
        .unwrap_or_default()
        .into_iter()
        .filter(|s| matches!(s.state, SessionState::Running | SessionState::AwaitingHuman))
        .map(|s| s.id)
        .collect();

    if let Some(evict_idx) = state
        .slots
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            s.role == role
                && !owned.contains(&s.target_id)
                && s.session_id
                    .as_deref()
                    .is_none_or(|sid| !running_session_ids.contains(sid))
                && find_reusable_target(&s.target_id).is_some()
        })
        .min_by_key(|(_, s)| s.last_used)
        .map(|(idx, _)| idx)
    {
        if let Some(old_sid) = state.slots[evict_idx].session_id.take() {
            if let Ok(mut old_session) = store.get::<Session>("local", "session", &old_sid) {
                if old_session.target_id.as_deref() == Some(&state.slots[evict_idx].target_id) {
                    old_session.target_id = None;
                    if old_session.mode == SessionMode::Temporary
                        && old_session.state == SessionState::Ready
                    {
                        old_session.state = SessionState::Invalidated;
                    }
                    let _ = store.put("local", "session", &old_session.id, &old_session);
                }
            }
        }
        let target = find_reusable_target(&state.slots[evict_idx].target_id).unwrap();
        state.slots[evict_idx].session_id = Some(session_id.to_owned());
        state.slots[evict_idx].last_used = now_ms();
        let _ = save_state(store, account_id, &state);
        return Ok((target, true));
    }

    // 5. Fallback: if any idle non-system tab for this account's route exists, reuse it; otherwise create a tab.
    if let Some(existing) = find_reusable_target_for(store, account_id, port, matcher) {
        if let Some(slot) = state.slots.iter_mut().find(|s| s.target_id == existing.id) {
            slot.role = role;
            slot.session_id = Some(session_id.to_owned());
            slot.last_used = now_ms();
        } else {
            state.slots.push(TabSlot {
                target_id: existing.id.clone(),
                role,
                session_id: Some(session_id.to_owned()),
                last_used: now_ms(),
            });
        }
        let _ = save_state(store, account_id, &state);
        return Ok((existing, true));
    }

    evict_shared_browser_tab_if_needed(store, account_id, port, &targets, &owned);
    let created = browser::create_tab(port, matcher.entry_url)
        .map_err(|e| ServiceError::backend(e, false))?;
    state.slots.push(TabSlot {
        target_id: created.id.clone(),
        role,
        session_id: Some(session_id.to_owned()),
        last_used: now_ms(),
    });
    let _ = save_state(store, account_id, &state);
    Ok((created, false))
}

pub(crate) fn acquire_session_tab_for_route(
    store: &Store,
    account_id: &str,
    port: u16,
    session_id: &str,
    mode: &SessionMode,
    route: &AccountRoute,
) -> Result<(TargetInfo, bool)> {
    gemini_matcher_for(route, |matcher| {
        acquire_session_tab_for(store, account_id, port, session_id, mode, matcher)
    })
}

#[cfg(test)]
pub(crate) fn acquire_session_tab(
    store: &Store,
    account_id: &str,
    port: u16,
    session_id: &str,
    mode: &SessionMode,
) -> Result<(TargetInfo, bool)> {
    acquire_session_tab_for_route(
        store,
        account_id,
        port,
        session_id,
        mode,
        &default_gemini_route(),
    )
}

/// Release a session's tab back to the pool as `Idle` without closing the Chrome tab
/// unless Chrome currently has more provider tabs than the configured pool maximum.
pub(crate) fn release_session_tab_for(
    store: &Store,
    account_id: &str,
    port: u16,
    session: &Session,
    matcher: &TabPoolMatcher<'_>,
) -> Result<()> {
    let Some(target_id) = session.target_id.as_deref() else {
        return Ok(());
    };
    let _pool_guard = pool_lock().lock().unwrap_or_else(|e| e.into_inner());
    let config = TabPoolConfig::load(Some(store));
    let targets = browser::list_targets(port).unwrap_or_default();
    let mut state = reconcile(store, account_id, &targets);
    let role = match session.mode {
        SessionMode::Chat => TabRole::Master,
        SessionMode::Temporary => TabRole::Worker,
    };

    if let Some(slot) = state.slots.iter_mut().find(|s| s.target_id == target_id) {
        slot.role = role;
        slot.session_id = None;
        slot.last_used = now_ms();
    } else if targets
        .iter()
        .any(|t| t.target_type == "page" && t.id == target_id)
    {
        state.slots.push(TabSlot {
            target_id: target_id.to_owned(),
            role,
            session_id: None,
            last_used: now_ms(),
        });
    }

    // Only physically close the tab if Chrome has more pooled tabs for this role than configured
    // AND more total provider pages than max_tabs.
    let role_tabs_count = state.slots.iter().filter(|s| s.role == role).count();
    let provider_pages_count = targets
        .iter()
        .filter(|t| t.target_type == "page" && (matcher.is_reusable_url)(&t.url))
        .count();

    if role_tabs_count > config.capacity(role) && provider_pages_count > config.max_tabs() {
        state.slots.retain(|s| s.target_id != target_id);
        let _ = save_state(store, account_id, &state);
        let _ = browser::close_tab(port, target_id);
        return Ok(());
    }

    let _ = save_state(store, account_id, &state);
    Ok(())
}

pub(crate) fn release_session_tab_for_route(
    store: &Store,
    account_id: &str,
    port: u16,
    session: &Session,
    route: &AccountRoute,
) -> Result<()> {
    gemini_matcher_for(route, |matcher| {
        release_session_tab_for(store, account_id, port, session, matcher)
    })
}

#[cfg(test)]
pub(crate) fn release_session_tab(
    store: &Store,
    account: &Account,
    port: u16,
    session: &Session,
) -> Result<()> {
    release_session_tab_for_route(store, &account.id, port, session, &default_gemini_route())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{default_provider, BrowserProfile};
    use std::sync::{Arc, Mutex};

    fn media_session() -> Session {
        Session {
            id: "a".into(),
            provider: crate::domain::default_provider(),
            account_id: "default".into(),
            mode: SessionMode::Chat,
            target_id: Some("reused".into()),
            remote_url: Some("https://gemini.google.com/app/chat-a".into()),
            remote_session_id: None,
            history: vec![],
            state: SessionState::Ready,
            model: None,
            thinking: None,
            tool_digest: String::new(),
            remote_turns: 1,
            last_used: now_ms(),
            reuse_scope: None,
        }
    }

    #[test]
    fn media_recovery_rejects_recycled_and_navigated_targets() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut session = media_session();
        let mut target = TargetInfo {
            id: "reused".into(),
            target_type: "page".into(),
            title: "fixture".into(),
            url: session.remote_url.clone().unwrap(),
            ws_url: "ws://127.0.0.1:1/reused".into(),
        };
        store
            .put("local", "session", &session.id, &session)
            .unwrap();
        assert!(owned_media_target(&store, &session, vec![target.clone()])
            .unwrap()
            .is_some());
        target.url = "https://gemini.google.com/app/chat-b".into();
        assert!(owned_media_target(&store, &session, vec![target.clone()])
            .unwrap()
            .is_none());
        target.url = session.remote_url.clone().unwrap();
        let mut other = session.clone();
        other.id = "b".into();
        store.put("local", "session", &other.id, &other).unwrap();
        assert!(owned_media_target(&store, &session, vec![target.clone()])
            .unwrap()
            .is_none());
        other.state = SessionState::Closed;
        store.put("local", "session", &other.id, &other).unwrap();
        session.state = SessionState::Closed;
        assert!(owned_media_target(&store, &session, vec![target])
            .unwrap()
            .is_none());
    }

    #[test]
    fn reconciliation_preserves_a_running_sessions_pending_tab_reservation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut session = media_session();
        session.mode = SessionMode::Temporary;
        session.state = SessionState::Running;
        session.target_id = None;
        store
            .put("local", "session", &session.id, &session)
            .unwrap();
        let state = TabPoolState {
            slots: vec![TabSlot {
                target_id: "reserved".into(),
                role: TabRole::Worker,
                session_id: Some(session.id.clone()),
                last_used: now_ms(),
            }],
        };
        save_state(&store, "default", &state).unwrap();
        let target = TargetInfo {
            id: "reserved".into(),
            target_type: "page".into(),
            title: String::new(),
            url: browser::GEMINI_APP_URL.into(),
            ws_url: "ws://127.0.0.1:1/reserved".into(),
        };
        let state = reconcile(&store, "default", std::slice::from_ref(&target));
        assert_eq!(state.slots[0].session_id.as_deref(), Some("a"));
        // A resumed conversation may still hold its previous target ID while
        // its replacement tab is being acquired.
        session.target_id = Some("gone-old-target".into());
        store
            .put("local", "session", &session.id, &session)
            .unwrap();
        let state = reconcile(&store, "default", &[target]);
        assert_eq!(state.slots[0].session_id.as_deref(), Some("a"));
    }

    #[test]
    fn two_tier_tab_pool_shares_worker_tabs_for_system_and_overflows_only_when_all_workers_busy() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let pages: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let created_count = Arc::new(Mutex::new(0usize));
        let closed_ids = Arc::new(Mutex::new(Vec::<String>::new()));

        let pages_srv = pages.clone();
        let created_srv = created_count.clone();
        let closed_srv = closed_ids.clone();
        let server = browser::MockHttpServer::spawn(move |stream, req_str, port| {
            let first_line = req_str.lines().next().unwrap_or("");
            if first_line.contains("/devtools/browser/") {
                let mut ws = browser::MockHttpServer::accept_ws(stream, req_str);
                if let Ok(msg) = ws.read() {
                    if let Ok(txt) = msg.to_text() {
                        let v: Value = serde_json::from_str(txt).unwrap();
                        let mut c = created_srv.lock().unwrap();
                        *c += 1;
                        let new_id = format!("tab-created-{}", *c);
                        let entry = json!({
                            "id": new_id,
                            "type": "page",
                            "title": "Gemini",
                            "url": "https://gemini.google.com/app",
                            "webSocketDebuggerUrl": format!("ws://127.0.0.1:{port}/devtools/page/{new_id}")
                        });
                        pages_srv.lock().unwrap().push(entry);
                        let reply = json!({
                            "id": v["id"],
                            "result": { "targetId": new_id }
                        });
                        let _ = ws.send(tokio_tungstenite::tungstenite::Message::Text(
                            reply.to_string().into(),
                        ));
                    }
                }
                return;
            }
            let body = if first_line.contains("/json/version") {
                json!({
                    "Browser": "Chrome/fixture",
                    "webSocketDebuggerUrl": format!("ws://127.0.0.1:{port}/devtools/browser/fixture")
                })
                .to_string()
            } else if let Some(rest) = first_line.split("/json/close/").nth(1) {
                let id = rest.split_whitespace().next().unwrap_or("").to_owned();
                closed_srv.lock().unwrap().push(id.clone());
                pages_srv.lock().unwrap().retain(|p| p["id"] != id);
                "\"Target is closing\"".to_owned()
            } else {
                serde_json::to_string(&*pages_srv.lock().unwrap()).unwrap()
            };
            browser::MockHttpServer::write_json(stream, &body);
        });
        let port = server.port;
        pages.lock().unwrap().push(json!({
            "id": "tab-sys",
            "type": "page",
            "title": "Gemini",
            "url": "https://gemini.google.com/app",
            "webSocketDebuggerUrl": format!("ws://127.0.0.1:{port}/devtools/page/tab-sys")
        }));
        store
            .put_browser_profile(&BrowserProfile {
                provider_id: crate::domain::default_provider(),
                profile: dir.path().join("profile"),
                subprofile: None,
                port,
                headless: true,
            })
            .unwrap();
        let account = Account {
            id: "default".into(),
            provider: crate::domain::default_provider(),
            state: "ready".into(),
            identity: None,
            label: None,
            slot: None,
        };
        store.put("local", "account", "default", &account).unwrap();

        set_worker_tabs(Some(&store), 2);

        // 1. System task acquires an idle Worker tab -> claims tab-sys into the Worker pool
        let sys = find_system_gemini_target(&store, "default", port).unwrap();
        assert_eq!(sys.id, "tab-sys");
        assert!(record_system_tab_id(&store, "default", port, &sys.id));

        // 2. Allocate Master session 1 (Workspace A) -> creates dedicated tab-created-1 (never steals Worker tab-sys)
        let (master1, reused) =
            acquire_session_tab(&store, "default", port, "sess-master-1", &SessionMode::Chat)
                .unwrap();
        assert!(!reused);
        assert_eq!(master1.id, "tab-created-1");
        let s_master1 = Session {
            id: "sess-master-1".into(),
            provider: default_provider(),
            account_id: "default".into(),
            mode: SessionMode::Chat,
            target_id: Some(master1.id.clone()),
            remote_url: Some("https://gemini.google.com/app/chat_one".into()),
            remote_session_id: None,
            history: vec![],
            state: SessionState::Ready,
            model: None,
            thinking: None,
            tool_digest: String::new(),
            remote_turns: 1,
            last_used: 10,
            reuse_scope: None,
        };
        store
            .put("local", "session", &s_master1.id, &s_master1)
            .unwrap();

        // 3. Allocate Master session 2 (Workspace B) -> creates dedicated tab-created-2
        let (master2, reused) =
            acquire_session_tab(&store, "default", port, "sess-master-2", &SessionMode::Chat)
                .unwrap();
        assert!(!reused);
        assert_eq!(master2.id, "tab-created-2");
        let s_master2 = Session {
            id: "sess-master-2".into(),
            provider: default_provider(),
            account_id: "default".into(),
            mode: SessionMode::Chat,
            target_id: Some(master2.id.clone()),
            remote_url: Some("https://gemini.google.com/app/chat_two".into()),
            remote_session_id: None,
            history: vec![],
            state: SessionState::Ready,
            model: None,
            thinking: None,
            tool_digest: String::new(),
            remote_turns: 1,
            last_used: 15,
            reuse_scope: None,
        };
        store
            .put("local", "session", &s_master2.id, &s_master2)
            .unwrap();

        // 4. Worker session 1 reuses idle Worker tab `tab-sys` in-place (zero new tab creation!)
        let (worker1, reused) = acquire_session_tab(
            &store,
            "default",
            port,
            "sess-worker-1",
            &SessionMode::Temporary,
        )
        .unwrap();
        assert!(reused);
        assert_eq!(worker1.id, "tab-sys");
        let mut s_worker1 = Session {
            id: "sess-worker-1".into(),
            provider: default_provider(),
            account_id: "default".into(),
            mode: SessionMode::Temporary,
            target_id: Some(worker1.id.clone()),
            remote_url: Some("https://gemini.google.com/app".into()),
            remote_session_id: None,
            history: vec![],
            state: SessionState::Ready,
            model: None,
            thinking: None,
            tool_digest: String::new(),
            remote_turns: 2,
            last_used: 20,
            reuse_scope: None,
        };
        store
            .put("local", "session", &s_worker1.id, &s_worker1)
            .unwrap();

        // 5. While Worker 1 is active, a System task still has 1 free Worker slot -> creates tab-created-3 and keeps it in the Worker pool!
        assert!(find_system_gemini_target(&store, "default", port).is_none());
        let sys2 = gemini::SystemGeminiTab::acquire_persistent(
            port,
            None,
            "https://gemini.google.com/usage",
        )
        .unwrap();
        assert_eq!(sys2.target.id, "tab-created-3");
        assert!(record_system_tab_id(
            &store,
            "default",
            port,
            &sys2.target.id
        ));
        drop(sys2);
        assert!(closed_ids.lock().unwrap().is_empty());

        // 6. Worker session 2 reuses the second pooled Worker tab `tab-created-3` in-place!
        let (worker2, reused) = acquire_session_tab(
            &store,
            "default",
            port,
            "sess-worker-2",
            &SessionMode::Temporary,
        )
        .unwrap();
        assert!(reused);
        assert_eq!(worker2.id, "tab-created-3");
        let mut s_worker2 = Session {
            id: "sess-worker-2".into(),
            provider: default_provider(),
            account_id: "default".into(),
            mode: SessionMode::Temporary,
            target_id: Some(worker2.id.clone()),
            remote_url: Some("https://gemini.google.com/app".into()),
            remote_session_id: None,
            history: vec![],
            state: SessionState::Ready,
            model: None,
            thinking: None,
            tool_digest: String::new(),
            remote_turns: 1,
            last_used: 25,
            reuse_scope: None,
        };
        store
            .put("local", "session", &s_worker2.id, &s_worker2)
            .unwrap();

        // 7. Now ALL 2 Worker tabs (`tab-sys` and `tab-created-3`) are busy!
        //    A System task gets None from find_system_gemini_target, opens overflow `tab-created-4`,
        //    record_system_tab_id returns false, and dropping SystemGeminiTab immediately closes `tab-created-4`!
        assert!(find_system_gemini_target(&store, "default", port).is_none());
        let mut overflow = gemini::SystemGeminiTab::acquire_persistent(
            port,
            None,
            "https://gemini.google.com/usage",
        )
        .unwrap();
        assert_eq!(overflow.target.id, "tab-created-4");
        overflow.keep_persistent =
            record_system_tab_id(&store, "default", port, &overflow.target.id);
        assert!(!overflow.keep_persistent);
        drop(overflow);
        assert_eq!(
            *closed_ids.lock().unwrap(),
            vec!["tab-created-4".to_string()]
        );

        // 8. Release both Worker sessions -> keeps both Worker tabs in pool WITHOUT closing Chrome tabs,
        //    and the next System task immediately preempts idle Worker `tab-sys`!
        for w in [&mut s_worker1, &mut s_worker2] {
            w.state = SessionState::Closed;
            store.put("local", "session", &w.id, w).unwrap();
            release_session_tab(&store, &account, port, w).unwrap();
        }
        let preempted = find_system_gemini_target(&store, "default", port)
            .map(|t| t.id)
            .unwrap();
        assert!(matches!(preempted.as_str(), "tab-sys" | "tab-created-3"));

        // 9. Holding a SystemTabLease (`reserve_system_gemini_target`) prevents a concurrent
        //    Worker turn (`acquire_session_tab`) from stealing the actively scraped `/usage` tab.
        let (reserved, lease) = reserve_system_gemini_target(&store, "default", port).unwrap();
        let (worker3, reused3) = acquire_session_tab(
            &store,
            "default",
            port,
            "sess-worker-3",
            &SessionMode::Temporary,
        )
        .unwrap();
        assert!(reused3);
        assert_ne!(
            worker3.id, reserved.id,
            "active /usage Worker tab must not be acquired by a concurrent Worker turn"
        );
        drop(lease);
    }
}
