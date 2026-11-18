//! Asynchronous workspace `@` mention search, fuzzy ranking, and popup state.
use super::editor::Editor;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::Instant,
};

pub(super) const VISIBLE_MATCH_ROWS: usize = 5;
pub(super) const POPUP_ROWS: usize = 1 + VISIBLE_MATCH_ROWS + 1 + 1;
pub(super) const MAX_STORED_MATCHES: usize = 100;
const MAX_WALK_ENTRIES: usize = 25_000;
const MAX_WALK_DEPTH: usize = 16;
const HOME_WALK_DEPTH: usize = 2;

const IGNORED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "__pycache__",
    ".next",
    ".venv",
    "venv",
    "dist",
    ".cache",
    ".hg",
    ".svn",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MentionKind {
    File,
    Dir,
    Git,
}

impl MentionKind {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::File => "File",
            Self::Dir => "Dir",
            Self::Git => "Git",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MentionMatch {
    /// Path relative to workspace (or `~/...` or `git:diff`), with trailing `/` for dirs.
    pub display: String,
    pub kind: MentionKind,
    /// Absolute disk path or short description displayed on the right.
    pub detail: String,
    pub score: i64,
}

impl MentionMatch {
    pub(super) fn format_insertion(&self, force_quote: bool) -> String {
        let needs_quote = force_quote || self.display.chars().any(char::is_whitespace);
        match self.kind {
            MentionKind::Dir => {
                if needs_quote {
                    format!("@\"{}", self.display)
                } else {
                    format!("@{}", self.display)
                }
            }
            MentionKind::File | MentionKind::Git => {
                if needs_quote {
                    format!("@\"{}\" ", self.display)
                } else {
                    format!("@{} ", self.display)
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MentionQuery {
    /// Byte offset of `@` in `editor.text`.
    pub start: usize,
    /// Byte offset of cursor in `editor.text`.
    pub end: usize,
    /// Normalized query after `@` (stripped of leading opening quote).
    pub query: String,
    /// Whether the token started with `"` or `'`.
    pub quoted: bool,
}

#[derive(Clone, Debug)]
pub(super) struct MentionPopup {
    pub start: usize,
    pub end: usize,
    pub query: String,
    pub quoted: bool,
    pub request_id: u64,
    pub loading: bool,
    pub matches: Vec<MentionMatch>,
    pub total: usize,
    pub selected: usize,
    pub scroll: usize,
    pub started_at: Instant,
}

impl MentionPopup {
    pub(super) fn new(parsed: &MentionQuery, request_id: u64, prev: Option<&Self>) -> Self {
        let keep_previous = prev.is_some();
        let previous_matches = prev
            .map(|p| {
                p.matches
                    .iter()
                    .filter(|m| {
                        parsed.query.is_empty() || fuzzy_score(&parsed.query, &m.display).is_some()
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let total = if keep_previous {
            previous_matches.len()
        } else {
            0
        };
        Self {
            start: parsed.start,
            end: parsed.end,
            query: parsed.query.clone(),
            quoted: parsed.quoted,
            request_id,
            loading: true,
            matches: previous_matches,
            total,
            selected: 0,
            scroll: 0,
            started_at: prev.map_or_else(Instant::now, |p| p.started_at),
        }
    }

    pub(super) fn apply_results(
        &mut self,
        request_id: u64,
        query: &str,
        matches: Vec<MentionMatch>,
        total: usize,
    ) -> bool {
        if request_id != self.request_id || query != self.query {
            return false;
        }
        self.loading = false;
        self.matches = matches;
        self.total = total;
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
        self.clamp_scroll();
        true
    }

    pub(super) fn move_selection(&mut self, up: bool) {
        if self.matches.is_empty() {
            return;
        }
        if up {
            self.selected = self.selected.saturating_sub(1);
        } else {
            self.selected = (self.selected + 1).min(self.matches.len().saturating_sub(1));
        }
        self.clamp_scroll();
    }

    pub(super) fn clamp_scroll(&mut self) {
        if self.matches.len() <= VISIBLE_MATCH_ROWS {
            self.scroll = 0;
            return;
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + VISIBLE_MATCH_ROWS {
            self.scroll = self.selected + 1 - VISIBLE_MATCH_ROWS;
        }
        let max_scroll = self.matches.len().saturating_sub(VISIBLE_MATCH_ROWS);
        self.scroll = self.scroll.min(max_scroll);
    }

    pub(super) fn visible_slice(&self) -> &[MentionMatch] {
        let start = self.scroll.min(self.matches.len());
        let end = (start + VISIBLE_MATCH_ROWS).min(self.matches.len());
        &self.matches[start..end]
    }

    pub(super) fn hidden_above(&self) -> usize {
        self.scroll
    }

    pub(super) fn hidden_below(&self) -> usize {
        let shown_end = (self.scroll + self.visible_slice().len()).min(self.total);
        self.total.saturating_sub(shown_end)
    }

    pub(super) fn has_overflow_row(&self) -> bool {
        self.hidden_above() > 0 || self.hidden_below() > 0
    }

    pub(super) fn visible_window(&self, total_rows: usize) -> (&[MentionMatch], usize) {
        if total_rows <= 1 || self.matches.is_empty() {
            return (&[], 0);
        }
        let has_hint = total_rows >= 3;
        let avail = total_rows - 1 - usize::from(has_hint);
        let visible_all = self.visible_slice();
        let has_overflow = self.has_overflow_row() && avail >= 2;
        let item_slots = if has_overflow {
            avail.saturating_sub(1)
        } else {
            avail
        }
        .min(VISIBLE_MATCH_ROWS);
        let local_offset = if visible_all.len() <= item_slots {
            0
        } else {
            let rel_sel = self.selected.saturating_sub(self.scroll);
            rel_sel.saturating_sub(item_slots.saturating_sub(1))
        };
        let end = visible_all.len().min(local_offset + item_slots);
        (&visible_all[local_offset..end], local_offset)
    }

    /// Fixed terminal rows reserved for this popup (divider + 5 item slots + overflow slot + hint)
    /// so the composer prompt row above never shifts as match counts change.
    pub(super) fn desired_rows(&self) -> usize {
        POPUP_ROWS
    }
}

pub(super) fn parse_at_cursor(editor: &Editor) -> Option<MentionQuery> {
    if editor.is_literal_before_cursor() {
        return None;
    }
    let prefix = &editor.text[..editor.cursor];
    // Slash commands at index 0 use inline command completion instead of the mention panel.
    if prefix.starts_with('/') && !prefix.contains(char::is_whitespace) {
        return None;
    }
    let start = prefix
        .char_indices()
        .rev()
        .find(|(i, c)| {
            *c == '@' && crate::input::is_mention_boundary(prefix[..*i].chars().next_back())
        })?
        .0;
    let raw = &prefix[start + 1..];
    let quoted = raw.starts_with('"') || raw.starts_with('\'');
    if !quoted && raw.contains(char::is_whitespace) {
        return None;
    }
    let query = raw.trim_start_matches(['"', '\'']).to_owned();
    if query.contains(['"', '\'', '\n', '\r']) {
        return None;
    }
    Some(MentionQuery {
        start,
        end: editor.cursor,
        query,
        quoted,
    })
}

pub(super) fn fuzzy_score(query: &str, candidate: &str) -> Option<i64> {
    if query.is_empty() {
        let depth = candidate.matches('/').count() as i64;
        return Some(1_000 - depth * 12 - candidate.chars().count() as i64);
    }
    let q_lower: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if q_lower.is_empty() {
        return Some(0);
    }
    let c_orig: Vec<char> = candidate.chars().collect();
    let c_lower: Vec<char> = c_orig.iter().flat_map(|ch| ch.to_lowercase()).collect();
    if c_lower.len() < q_lower.len() {
        return None;
    }
    let basename_start = candidate
        .trim_end_matches('/')
        .rfind('/')
        .map_or(0, |byte_idx| candidate[..=byte_idx].chars().count());

    let mut qi = 0usize;
    let mut score: i64 = 0;
    let mut prev_match: Option<usize> = None;
    let mut all_in_basename = true;

    for (ci, &ch) in c_lower.iter().enumerate() {
        if qi < q_lower.len() && ch == q_lower[qi] {
            if ci < basename_start {
                all_in_basename = false;
            }
            score += 10;
            if ci >= basename_start {
                score += 8;
            }
            let is_boundary = ci == 0
                || matches!(c_orig[ci - 1], '/' | '\\' | '_' | '-' | '.' | ' ' | ':')
                || (c_orig[ci - 1].is_lowercase() && c_orig[ci].is_uppercase());
            if is_boundary {
                score += 14;
            }
            if let Some(prev) = prev_match {
                if ci == prev + 1 {
                    score += 18;
                } else {
                    score -= ((ci - prev - 1) as i64).min(8);
                }
            }
            prev_match = Some(ci);
            qi += 1;
        }
    }

    if qi != q_lower.len() {
        return None;
    }

    let q_str: String = q_lower.iter().collect();
    let c_str: String = c_lower.iter().collect();
    let base_str: String = c_lower[basename_start..].iter().collect();

    if c_str == q_str {
        score += 140;
    } else if base_str.trim_end_matches('/') == q_str {
        score += 110;
    } else if base_str.starts_with(&q_str) {
        score += 80;
    } else if base_str.contains(&q_str) {
        score += 45;
    } else if c_str.starts_with(&q_str) {
        score += 55;
    } else if c_str.contains(&q_str) {
        score += 25;
    }

    if all_in_basename {
        score += 15;
    }

    let depth = candidate.trim_end_matches('/').matches('/').count() as i64;
    score -= depth * 3 + (c_orig.len() as i64);
    Some(score)
}

pub(super) fn search_mentions(
    workspace: &Path,
    query: &str,
    cancel_counter: Option<&AtomicU64>,
    request_id: u64,
) -> Option<(Vec<MentionMatch>, usize)> {
    let is_cancelled =
        || cancel_counter.is_some_and(|counter| counter.load(Ordering::Relaxed) != request_id);
    if is_cancelled() {
        return None;
    }

    let include_hidden = query.starts_with('.') || query.contains("/.");
    let mut results: Vec<MentionMatch> = Vec::new();

    if !query.starts_with("~/") {
        for (name, desc) in [
            ("git:diff", "Unstaged git diff"),
            ("git:staged", "Staged git diff"),
        ] {
            if let Some(score) = fuzzy_score(query, name) {
                results.push(MentionMatch {
                    display: name.to_owned(),
                    kind: MentionKind::Git,
                    detail: desc.to_owned(),
                    score: score - if query.is_empty() { 40 } else { 0 },
                });
            }
        }
    }

    let (walk_root, prefix, max_depth, is_home_mode) = if let Some(tail) = query.strip_prefix("~/")
    {
        let home = crate::input::home_dir()?;
        let (dir_tail, _) = tail.rsplit_once('/').unwrap_or(("", tail));
        let root = if dir_tail.is_empty() {
            home
        } else {
            home.join(dir_tail)
        };
        let pfx = if dir_tail.is_empty() {
            "~/".to_owned()
        } else {
            format!("~/{dir_tail}/")
        };
        (root, pfx, HOME_WALK_DEPTH, true)
    } else if let Some((dir_part, _)) = query.rsplit_once('/') {
        let candidate_dir = workspace.join(dir_part);
        if !dir_part.is_empty() && candidate_dir.is_dir() {
            (candidate_dir, format!("{dir_part}/"), MAX_WALK_DEPTH, false)
        } else {
            (
                workspace.to_path_buf(),
                String::new(),
                MAX_WALK_DEPTH,
                false,
            )
        }
    } else {
        (
            workspace.to_path_buf(),
            String::new(),
            MAX_WALK_DEPTH,
            false,
        )
    };

    let mut queue: VecDeque<(PathBuf, String, usize)> = VecDeque::new();
    queue.push_back((walk_root, prefix, 0));
    let mut visited = 0usize;

    while let Some((dir, rel_prefix, depth)) = queue.pop_front() {
        if is_cancelled() {
            return None;
        }
        if visited >= MAX_WALK_ENTRIES || depth >= max_depth {
            continue;
        }
        let Ok(read_dir) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry_res in read_dir {
            visited += 1;
            if visited.is_multiple_of(128) && is_cancelled() {
                return None;
            }
            if visited > MAX_WALK_ENTRIES {
                break;
            }
            let Ok(entry) = entry_res else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.contains(['"', '\'', '\n', '\r']) {
                continue;
            }
            if !include_hidden && name.starts_with('.') {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let is_dir = file_type.is_dir();
            if is_dir
                && !is_home_mode
                && rel_prefix.is_empty()
                && IGNORED_DIRS.contains(&name.as_str())
            {
                // Allow explicit subdirectory traversal if user typed e.g. `target/`, skip otherwise.
                if !query.starts_with(&name) {
                    continue;
                }
            } else if is_dir && IGNORED_DIRS.contains(&name.as_str()) {
                continue;
            }

            let display = if is_dir {
                format!("{rel_prefix}{name}/")
            } else {
                format!("{rel_prefix}{name}")
            };

            if let Some(score) = fuzzy_score(query, &display) {
                let abs_path = entry.path();
                results.push(MentionMatch {
                    display: display.clone(),
                    kind: if is_dir {
                        MentionKind::Dir
                    } else {
                        MentionKind::File
                    },
                    detail: abs_path.display().to_string(),
                    score: if is_dir && !query.ends_with('/') {
                        score - 4
                    } else {
                        score
                    },
                });
            }

            if is_dir && depth + 1 < max_depth {
                queue.push_back((entry.path(), format!("{rel_prefix}{name}/"), depth + 1));
            }
        }
    }

    if is_cancelled() {
        return None;
    }

    results.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.display.len().cmp(&b.display.len()))
            .then_with(|| a.display.cmp(&b.display))
    });
    let total = results.len();
    results.truncate(MAX_STORED_MATCHES);
    Some((results, total))
}

#[derive(Clone, Debug)]
struct SearchJob {
    id: u64,
    workspace: PathBuf,
    query: String,
}

/// Coalesced background `@` mention searcher so stale queries abort immediately
/// and never stall UI rendering or RPC calls.
pub(super) struct MentionSearchHandle {
    pending: Arc<(Mutex<Option<SearchJob>>, Condvar)>,
    latest_id: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MentionSearchHandle {
    pub(super) fn spawn(updates: mpsc::SyncSender<super::worker::Update>) -> Self {
        let pending = Arc::new((Mutex::new(None::<SearchJob>), Condvar::new()));
        let latest_id = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));

        let pending_worker = Arc::clone(&pending);
        let latest_worker = Arc::clone(&latest_id);
        let shutdown_worker = Arc::clone(&shutdown);

        let thread = thread::spawn(move || loop {
            let job = {
                let (lock, cv) = &*pending_worker;
                let mut guard = match lock.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                while guard.is_none() && !shutdown_worker.load(Ordering::Relaxed) {
                    guard = match cv.wait(guard) {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                }
                if shutdown_worker.load(Ordering::Relaxed) {
                    break;
                }
                guard.take()
            };
            let Some(job) = job else {
                continue;
            };
            if latest_worker.load(Ordering::Relaxed) != job.id {
                continue;
            }
            if let Some((matches, total)) =
                search_mentions(&job.workspace, &job.query, Some(&latest_worker), job.id)
            {
                if latest_worker.load(Ordering::Relaxed) == job.id {
                    let _ = updates.try_send(super::worker::Update::MentionMatches {
                        id: job.id,
                        query: job.query,
                        matches,
                        total,
                    });
                }
            }
        });

        Self {
            pending,
            latest_id,
            shutdown,
            thread: Some(thread),
        }
    }

    pub(super) fn schedule(&self, id: u64, workspace: PathBuf, query: String) {
        self.latest_id.store(id, Ordering::Relaxed);
        let (lock, cv) = &*self.pending;
        if let Ok(mut guard) = lock.lock() {
            *guard = Some(SearchJob {
                id,
                workspace,
                query,
            });
            cv.notify_one();
        }
    }
}

impl Drop for MentionSearchHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.latest_id.store(u64::MAX, Ordering::Relaxed);
        let (lock, cv) = &*self.pending;
        if let Ok(mut guard) = lock.lock() {
            *guard = None;
            cv.notify_all();
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    fn temp_workspace(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "anticrumb-mentions-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parse_at_cursor_recognizes_mentions_and_ignores_emails_and_commands() {
        let mut editor = Editor::default();
        editor.set("@dd".into());
        assert_eq!(
            parse_at_cursor(&editor),
            Some(MentionQuery {
                start: 0,
                end: 3,
                query: "dd".into(),
                quoted: false,
            })
        );

        editor.set("read @src/main".into());
        assert_eq!(
            parse_at_cursor(&editor),
            Some(MentionQuery {
                start: 5,
                end: 14,
                query: "src/main".into(),
                quoted: false,
            })
        );

        editor.set("email a@b.com".into());
        assert_eq!(parse_at_cursor(&editor), None);

        editor.set("/status @file".into());
        assert!(parse_at_cursor(&editor).is_some());
        editor.set("/help".into());
        assert_eq!(parse_at_cursor(&editor), None);

        editor.set("check @\"a b".into());
        assert_eq!(
            parse_at_cursor(&editor),
            Some(MentionQuery {
                start: 6,
                end: 11,
                query: "a b".into(),
                quoted: true,
            })
        );

        editor.set("check @file done".into());
        assert_eq!(parse_at_cursor(&editor), None);
    }

    #[test]
    fn fuzzy_search_ranks_short_and_boundary_matches_and_skips_ignored_dirs() {
        let root = temp_workspace("fuzzy");
        fs::write(root.join("README.md"), "# root").unwrap();
        fs::create_dir_all(root.join("providers")).unwrap();
        fs::write(root.join("providers/README.md"), "# providers").unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("docs/building.md"), "# docs").unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("target/debug/dd_ignored.md"), "ignored").unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/HEAD"), "ref").unwrap();

        let (matches, total) = search_mentions(&root, "dd", None, 1).unwrap();
        assert!(total >= 3);
        assert_eq!(matches[0].display, "README.md");
        assert!(matches.iter().any(|m| m.display == "providers/README.md"));
        assert!(matches.iter().any(|m| m.display == "docs/building.md"));
        assert!(!matches.iter().any(|m| m.display.contains("target/debug")));
        assert!(!matches.iter().any(|m| m.display.contains(".git")));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn search_abort_returns_none_when_generation_advances() {
        let root = temp_workspace("cancel");
        fs::write(root.join("alpha.txt"), "a").unwrap();
        let latest = AtomicU64::new(2);
        assert!(search_mentions(&root, "alpha", Some(&latest), 1).is_none());
        assert!(search_mentions(&root, "alpha", Some(&latest), 2).is_some());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn async_mention_handle_delivers_latest_results() {
        let root = temp_workspace("async");
        fs::write(root.join("alpha_file.rs"), "").unwrap();
        fs::write(root.join("beta_file.rs"), "").unwrap();
        let (tx, rx) = mpsc::sync_channel(16);
        let handle = MentionSearchHandle::spawn(tx);
        handle.schedule(1, root.clone(), "beta".into());

        let update = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("expected async mention matches");
        match update {
            super::super::worker::Update::MentionMatches {
                id,
                query,
                matches,
                total,
            } => {
                assert_eq!(id, 1);
                assert_eq!(query, "beta");
                assert_eq!(total, 1);
                assert_eq!(matches[0].display, "beta_file.rs");
            }
            _ => panic!("unexpected update variant"),
        }
        drop(handle);
        fs::remove_dir_all(&root).unwrap();
    }
}
