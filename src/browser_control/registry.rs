use super::{page::ControlledTab, validate_request};
use crate::{
    agent::CancelToken,
    browser_host as browser,
    domain::*,
    rpc::pb::{self, browser_control_request::Command},
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, MutexGuard, TryLockError},
    time::{Duration, Instant},
};

const IDLE_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_SESSIONS: usize = 8;
const MAX_TABS: usize = 8;

type Sessions = HashMap<(String, String), Arc<Mutex<Session>>>;
#[derive(Default)]
pub struct BrowserControlRegistry {
    sessions: Mutex<Sessions>,
}

struct Session {
    // Tabs must close before their context and outbound proxy are disposed.
    tabs: BTreeMap<String, ControlledTab>,
    context: Option<browser::BrowserContextGuard>,
    port: u16,
    active: String,
    last_used: Instant,
}
impl Default for Session {
    fn default() -> Self {
        Self {
            tabs: BTreeMap::new(),
            context: None,
            port: 0,
            active: String::new(),
            last_used: Instant::now(),
        }
    }
}
impl Session {
    fn refresh_tabs(&mut self) -> Result<()> {
        let Some(context) = &self.context else {
            return Ok(());
        };
        let targets = context.targets().map_err(error)?;
        self.tabs
            .retain(|_, tab| targets.iter().any(|t| t.id == tab.target_id()));
        for target in targets {
            if self.tabs.values().any(|tab| tab.target_id() == target.id) {
                continue;
            }
            if self.tabs.len() >= MAX_TABS {
                // Bound unsolicited popups as well as explicit open commands.
                let _ = browser::close_tab(self.port, &target.id);
                continue;
            }
            let id = format!("tab_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
            self.tabs.insert(id, ControlledTab::new(self.port, target));
        }
        if !self.tabs.contains_key(&self.active) {
            self.active = self.tabs.keys().next().cloned().unwrap_or_default();
        }
        Ok(())
    }
    fn response(&self, id: &str) -> pb::BrowserControlResponse {
        pb::BrowserControlResponse {
            session_id: id.into(),
            active_tab_id: self.active.clone(),
            tabs: self
                .tabs
                .iter()
                .map(|(id, tab)| pb::BrowserTab {
                    tab_id: id.clone(),
                    title: tab.title().into(),
                    url: tab.url().into(),
                })
                .collect(),
            page: None,
            message: String::new(),
        }
    }
}
fn error(message: impl ToString) -> ServiceError {
    let mut error = ServiceError::invalid(message.to_string());
    error.stage = "browser_control".into();
    error
}
fn lock_session<'a>(
    session: &'a Mutex<Session>,
    cancel: &CancelToken,
    deadline: Instant,
) -> Result<MutexGuard<'a, Session>> {
    loop {
        cancel.check().map_err(error)?;
        if Instant::now() >= deadline {
            return Err(error(
                "timed out waiting for this browser session; no action was executed",
            ));
        }
        match session.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(_)) => return Err(error("browser session lock poisoned")),
            Err(TryLockError::WouldBlock) => {
                cancel.sleep_or_cancelled(Duration::from_millis(20));
            }
        }
    }
}
impl BrowserControlRegistry {
    pub fn close_all(&self) {
        let removed = self
            .sessions
            .lock()
            .map(|mut map| std::mem::take(&mut *map));
        drop(removed); // No Chrome I/O while holding the registry lock.
    }

    fn session(
        &self,
        account: &str,
        id: &str,
        create: bool,
    ) -> Result<Option<Arc<Mutex<Session>>>> {
        let mut removed = Vec::new();
        let result = (|| {
            let mut map = self.sessions.lock().map_err(error)?;
            let expired = map
                .iter()
                .filter(|(_, session)| {
                    // A waiting/in-flight operation pins the session. Never evict live state.
                    Arc::strong_count(session) == 1
                        && session
                            .try_lock()
                            .is_ok_and(|s| s.last_used.elapsed() > IDLE_TTL)
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            for key in expired {
                if let Some(session) = map.remove(&key) {
                    removed.push(session);
                }
            }
            let key = (account.to_owned(), id.to_owned());
            if let Some(session) = map.get(&key) {
                return Ok(Some(session.clone()));
            }
            if !create {
                return Ok(None);
            }
            if map.len() >= MAX_SESSIONS {
                return Err(error(
                    "browser session limit (8) reached; reset an unused session first",
                ));
            }
            let session = Arc::new(Mutex::new(Session::default()));
            map.insert(key, session.clone());
            Ok(Some(session))
        })();
        drop(removed);
        result
    }

    pub fn execute(
        &self,
        browser_profile: &BrowserProfile,
        account: &Account,
        req: &pb::BrowserControlRequest,
        cancel: CancelToken,
    ) -> Result<pb::BrowserControlResponse> {
        validate_request(req).map_err(error)?;
        if req.account_id != account.id {
            return Err(error("browser account does not match request"));
        }
        let command = req.command.as_ref().unwrap();
        if let Command::Open(p) | Command::Navigate(p) = command {
            crate::web::validate_http_url(&p.url, crate::web::allow_private_env())
                .map_err(error)?;
        }
        let deadline = Instant::now() + Duration::from_millis(req.timeout_ms.into());
        cancel.check().map_err(error)?;
        let handle = self.session(
            &account.id,
            &req.session_id,
            matches!(command, Command::Open(_)),
        )?;
        let Some(handle) = handle else {
            if matches!(command, Command::Tabs(_) | Command::Reset(_)) {
                return Ok(pb::BrowserControlResponse {
                    session_id: req.session_id.clone(),
                    message: "No open tabs in this session.".into(),
                    ..Default::default()
                });
            }
            return Err(error(
                "browser session is missing or expired; open a page first",
            ));
        };
        let mut session = lock_session(&handle, &cancel, deadline)?;
        session.last_used = Instant::now();
        let result = (|| {
            // Reset can always dispose a crashed browser without first reconnecting.
            if !matches!(command, Command::Reset(_)) {
                session.refresh_tabs()?;
            }
            match command {
                Command::Tabs(_) => Ok(session.response(&req.session_id)),
                Command::Reset(_) => {
                    *session = Session::default();
                    let mut response = session.response(&req.session_id);
                    response.message =
                        "Session reset; all tabs and session cookies were cleared.".into();
                    Ok(response)
                }
                Command::Open(p) => {
                    if session.tabs.len() >= MAX_TABS {
                        return Err(error("tab limit (8) reached; close an unused tab first"));
                    }
                    let target = if session.port == 0 {
                        let (port, target, context) =
                            crate::account::browser_tool_tab(browser_profile)?;
                        session.port = port;
                        session.context = context;
                        target
                    } else if let Some(context) = &session.context {
                        context.create_tab().map_err(error)?
                    } else {
                        browser::create_tab(session.port, "about:blank").map_err(error)?
                    };
                    let id = format!("tab_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
                    let port = session.port;
                    session
                        .tabs
                        .insert(id.clone(), ControlledTab::new(port, target));
                    session.active = id.clone();
                    let page = match session.tabs.get_mut(&id).unwrap().navigate(
                        &p.url,
                        req.max_chars,
                        &cancel,
                        deadline,
                    ) {
                        Ok(page) => page,
                        Err(message) => {
                            let _ = session.refresh_tabs();
                            let recovery = if session.tabs.contains_key(&id) {
                                format!("tab {id} remains selected; use screenshot to inspect it or navigate to another URL")
                            } else {
                                format!(
                                    "tab {id} closed; use tabs or open before another page command"
                                )
                            };
                            return Err(error(format!("{message}; {recovery}")));
                        }
                    };
                    session.refresh_tabs()?;
                    let mut response = session.response(&req.session_id);
                    response.page = Some(pb::BrowserSnapshot { tab_id: id, ..page });
                    Ok(response)
                }
                _ => {
                    let id = if req.tab_id.is_empty() {
                        session.active.clone()
                    } else {
                        req.tab_id.clone()
                    };
                    if id.is_empty() {
                        return Err(error("no open tabs in this session; open a page first"));
                    }
                    if !session.tabs.contains_key(&id) {
                        return Err(error(format!(
                            "unknown or closed tab '{id}' in this session; use tabs or open"
                        )));
                    }
                    if matches!(command, Command::Close(_)) {
                        session.tabs.remove(&id);
                        if session.active == id {
                            session.active =
                                session.tabs.keys().next().cloned().unwrap_or_default();
                        }
                        let mut response = session.response(&req.session_id);
                        response.message = format!("Closed {id}.");
                        return Ok(response);
                    }
                    if matches!(command, Command::UseTab(_)) {
                        session.active = id.clone();
                    }
                    let tab = session.tabs.get_mut(&id).unwrap();
                    let page = tab
                        .execute(command, req.max_chars, req.wait_ms, &cancel, deadline)
                        .map_err(|message| {
                            let mut error = ServiceError::backend(message, false);
                            error.stage = "browser_control".into();
                            error
                        })?;
                    session.refresh_tabs()?;
                    let mut response = session.response(&req.session_id);
                    response.page = Some(pb::BrowserSnapshot { tab_id: id, ..page });
                    Ok(response)
                }
            }
        })();
        session.last_used = Instant::now();
        // Reset and failures before browser creation release scarce session slots. A queued operation
        // keeps the same mutex and cannot race with a replacement session.
        let empty = session.port == 0 || matches!(command, Command::Reset(_));
        if empty {
            if let Ok(mut map) = self.sessions.lock() {
                if Arc::strong_count(&handle) == 2 {
                    map.remove(&(account.id.clone(), req.session_id.clone()));
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registry_isolates_accounts_bounds_sessions_and_does_not_evict_pinned_work() {
        let registry = BrowserControlRegistry::default();
        let pinned = registry.session("a", "same", true).unwrap().unwrap();
        assert!(registry.session("b", "same", false).unwrap().is_none());
        pinned.lock().unwrap().last_used = Instant::now() - IDLE_TTL - Duration::from_secs(1);
        for n in 0..MAX_SESSIONS - 1 {
            registry.session("a", &format!("other-{n}"), true).unwrap();
        }
        assert!(registry.session("a", "overflow", true).is_err());
        assert!(Arc::ptr_eq(
            &pinned,
            &registry.session("a", "same", false).unwrap().unwrap()
        ));
        drop(pinned);
        assert!(registry.session("a", "same", false).unwrap().is_none());
        assert!(registry.session("a", "overflow", true).is_ok());
        registry.close_all();
        assert!(registry.sessions.lock().unwrap().is_empty());
    }
    #[test]
    fn waiting_for_session_is_bounded_and_cancellable() {
        let session = Mutex::new(Session::default());
        let _guard = session.lock().unwrap();
        let deadline = Instant::now() + Duration::from_millis(25);
        assert!(lock_session(&session, &CancelToken::new(), deadline)
            .err()
            .unwrap()
            .to_string()
            .contains("no action was executed"));
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(lock_session(&session, &cancel, Instant::now() + Duration::from_secs(10)).is_err());
    }
}
