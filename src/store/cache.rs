//! Cache retention never removes conversation records or browser profiles.
use super::Store;
use crate::domain::*;
use fs2::FileExt;
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const DEFAULT_DAYS: u32 = 7;
pub const INTERVAL: Duration = Duration::from_secs(6 * 3600);
pub const BLOB_LIMIT: u64 = 1024 * 1024 * 1024;
const TEMP_AGE_MS: i64 = 24 * 3600 * 1000;
pub const USAGE: &str = "cache [stats|prune [DAYS] [--dry-run]] (DAYS: 1..3650, default: 7)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    pub days: u32,
    pub prune: bool,
    pub dry_run: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            days: DEFAULT_DAYS,
            prune: false,
            dry_run: false,
        }
    }
}
impl Options {
    pub fn parse(args: &[&str]) -> Result<Self> {
        let mut options = Self::default();
        match args {
            [] | ["stats"] => return Ok(options),
            ["prune", ..] => options.prune = true,
            _ => return Err(ServiceError::invalid(USAGE)),
        }
        let mut days = false;
        for arg in &args[1..] {
            if *arg == "--dry-run" && !options.dry_run {
                options.dry_run = true;
            } else if !days && arg.bytes().all(|ch| ch.is_ascii_digit()) {
                options.days = arg.parse().map_err(|_| ServiceError::invalid(USAGE))?;
                days = true;
            } else {
                return Err(ServiceError::invalid(USAGE));
            }
        }
        options.validate()?;
        Ok(options)
    }
    pub fn validate(self) -> Result<()> {
        if !(1..=3650).contains(&self.days) || self.dry_run && !self.prune {
            return Err(ServiceError::invalid(USAGE));
        }
        Ok(())
    }
}

struct Removal {
    path: PathBuf,
    bytes: u64,
    directory: bool,
    // Hold an abandoned temporary directory's lease until deletion starts.
    lease: Option<fs::File>,
}

pub(super) fn owned_name(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|id| id.len() == 32 && id.bytes().all(|ch| ch.is_ascii_hexdigit()))
}

fn old(metadata: &fs::Metadata, cutoff: i64) -> bool {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .is_some_and(|age| age.as_millis() < cutoff.max(0) as u128)
}

fn is_transient_windows_io_error(error: &std::io::Error) -> bool {
    #[cfg(windows)]
    {
        // Concurrent directory cleanup on Windows briefly leaves deleted entries in
        // DELETE_PENDING (`ERROR_ACCESS_DENIED` = 5) or contended (`ERROR_SHARING_VIOLATION` = 32,
        // `ERROR_DIR_NOT_EMPTY` = 145) before the OS finishes closing handles.
        error.kind() == std::io::ErrorKind::PermissionDenied
            || matches!(error.raw_os_error(), Some(5 | 32 | 145))
    }
    #[cfg(not(windows))]
    {
        let _ = error;
        false
    }
}

pub(super) fn retry_transient<T>(mut op: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    #[cfg(windows)]
    const MAX_ATTEMPTS: usize = 50;
    #[cfg(not(windows))]
    const MAX_ATTEMPTS: usize = 1;

    let mut attempt = 0;
    loop {
        match op() {
            Err(error) if is_transient_windows_io_error(&error) && attempt + 1 < MAX_ATTEMPTS => {
                attempt += 1;
                if attempt <= 3 {
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(Duration::from_millis(if attempt <= 8 { 1 } else { 5 }));
                }
            }
            result => return result,
        }
    }
}

pub(super) enum TempLease {
    Acquired(fs::File),
    LockedOrInvalid,
    Gone,
    Missing,
}

pub(super) fn try_open_temp_lease(lock_path: &Path) -> Result<TempLease> {
    match metadata(lock_path)? {
        Some(meta) if meta.is_file() && !meta.file_type().is_symlink() => {
            let file = match retry_transient(|| {
                fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(lock_path)
            }) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(TempLease::Gone);
                }
                Err(error) => return Err(ServiceError::storage(error)),
            };
            if file.try_lock_exclusive().is_err() {
                return Ok(TempLease::LockedOrInvalid);
            }
            Ok(TempLease::Acquired(file))
        }
        Some(_) => Ok(TempLease::LockedOrInvalid),
        None => Ok(TempLease::Missing),
    }
}

fn entries_raw(path: &Path) -> std::io::Result<Option<Vec<fs::DirEntry>>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
            let dir = match fs::read_dir(path) {
                Ok(dir) => dir,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Some(Vec::new()));
                }
                Err(error) => return Err(error),
            };
            let mut items = Vec::new();
            for entry in dir {
                match entry {
                    Ok(entry) => items.push(entry),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(Some(items))
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Some(Vec::new())),
        Err(error) => Err(error),
    }
}

pub(super) fn entries(path: &Path) -> Result<Vec<fs::DirEntry>> {
    match retry_transient(|| entries_raw(path)).map_err(ServiceError::storage)? {
        Some(items) => Ok(items),
        None => Err(ServiceError::invalid(
            "cache directory must not be a symlink or a file",
        )),
    }
}

pub(super) fn metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    match retry_transient(|| fs::symlink_metadata(path)) {
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ServiceError::storage(error)),
    }
}

// Refuse to traverse links, including links inside an abandoned directory.
pub(super) fn directory_size(path: &Path, newest: &mut SystemTime) -> Result<Option<u64>> {
    let mut bytes = 0;
    for entry in entries(path)? {
        let Some(meta) = metadata(&entry.path())? else {
            continue;
        };
        if meta.file_type().is_symlink() {
            return Ok(None);
        }
        *newest = (*newest).max(meta.modified().map_err(ServiceError::storage)?);
        if meta.is_dir() {
            let Some(size) = directory_size(&entry.path(), newest)? else {
                return Ok(None);
            };
            bytes += size;
        } else if meta.is_file() {
            bytes += meta.len();
        } else {
            return Ok(None);
        }
    }
    Ok(Some(bytes))
}

fn clip_summary_str(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

fn compact_tool_args(args: &serde_json::Value) -> serde_json::Value {
    const SUMMARY_KEYS: &[&str] = &[
        "path",
        "command",
        "url",
        "pattern",
        "name",
        "label",
        "action",
        "id",
        "read_only",
        "replace_all",
        "query",
        "cwd",
        "glob",
        "symbol",
        "start_line",
        "end_line",
    ];
    let Some(map) = args.as_object() else {
        return serde_json::Value::Object(serde_json::Map::new());
    };
    let mut out = serde_json::Map::new();
    for &key in SUMMARY_KEYS {
        let Some(val) = map.get(key) else {
            continue;
        };
        let compacted = match val {
            serde_json::Value::String(s) => serde_json::Value::String(clip_summary_str(s, 256)),
            serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::Null => val.clone(),
            _ => continue,
        };
        out.insert(key.to_owned(), compacted);
    }
    if let Some(serde_json::Value::String(task)) = map.get("task") {
        if task.len() <= 256 {
            out.insert("task".to_owned(), serde_json::Value::String(task.clone()));
        }
    }
    serde_json::Value::Object(out)
}

fn compact_agent_record(record: &mut AgentRecord) -> bool {
    let Some(result) = record.result.as_mut() else {
        return false;
    };
    if result.inspector_pruned || result.steps.is_empty() {
        return false;
    }
    for step in &mut result.steps {
        step.model_response.clear();
        step.notes.clear();
        for tc in &mut step.tool_calls {
            tc.args = compact_tool_args(&tc.args);
        }
        for obs in &mut step.observations {
            obs.output.clear();
        }
    }
    result.inspector_pruned = true;
    true
}

impl Store {
    pub fn cache_report(&self, options: Options) -> Result<CacheStats> {
        Ok(self.cache_at(options, now_ms(), None)?.unwrap())
    }

    pub(crate) fn auto_clean_cache(&self) -> Result<()> {
        self.cache_at(
            Options {
                prune: true,
                ..Default::default()
            },
            now_ms(),
            Some(INTERVAL),
        )?;
        Ok(())
    }

    pub(super) fn clean_cache_under_pressure(&self) -> Result<()> {
        self.cache_at(
            Options {
                prune: true,
                ..Default::default()
            },
            now_ms(),
            Some(Duration::from_secs(60)),
        )?;
        Ok(())
    }

    fn cache_at(
        &self,
        options: Options,
        now: i64,
        minimum_interval: Option<Duration>,
    ) -> Result<Option<CacheStats>> {
        options.validate()?;
        let _lease = self.blob_lease()?;
        let mut conn = self.conn.lock().map_err(ServiceError::storage)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(ServiceError::storage)?;
        let last: i64 = tx.query_row(
            "SELECT json_extract(payload,'$.last_cleanup') FROM objects WHERE scope='local' AND kind='maintenance' AND id='cache'",
            [], |row| row.get(0),
        ).optional().map_err(ServiceError::storage)?.unwrap_or(0);
        if last > 0
            && minimum_interval
                .is_some_and(|interval| now.saturating_sub(last) < interval.as_millis() as i64)
        {
            return Ok(None);
        }
        let busy: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM objects WHERE kind IN ('run','agent') AND json_extract(payload,'$.state') IN ('queued','preparing','submitted','running'))",
            [], |row| row.get(0),
        ).map_err(ServiceError::storage)?;
        let cutoff = now - i64::from(options.days) * 86_400_000;
        let mut candidates = Vec::new();
        // Protect both saved records and in-flight tool observations. An active
        // worker can also hold a reference that has not reached its journal yet.
        if !busy {
            let mut query = tx.prepare(
                "SELECT a.scope,a.id FROM objects a WHERE a.kind='artifact' AND json_extract(a.payload,'$.created_at')<?1
                 AND NOT EXISTS(SELECT 1 FROM objects r WHERE r.kind!='artifact' AND instr(r.payload,a.id)>0)
                 AND NOT EXISTS(SELECT 1 FROM events e WHERE instr(e.data,a.id)>0)"
            ).map_err(ServiceError::storage)?;
            candidates = query
                .query_map([cutoff], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(ServiceError::storage)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(ServiceError::storage)?;
        }
        let candidate_ids: HashSet<_> = candidates.iter().cloned().collect();
        let mut all_hashes = HashSet::new();
        let mut retained_hashes = HashSet::new();
        {
            let mut query = tx.prepare("SELECT scope,id,json_extract(payload,'$.hash') FROM objects WHERE kind='artifact'")
                .map_err(ServiceError::storage)?;
            let rows = query
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(ServiceError::storage)?;
            for row in rows {
                let (scope, id, hash) = row.map_err(ServiceError::storage)?;
                all_hashes.insert(hash.clone());
                if !candidate_ids.contains(&(scope, id)) {
                    retained_hashes.insert(hash);
                }
            }
        }
        let pruned_events: u64 = tx
            .query_row(
                "SELECT count(*) FROM events e
                 WHERE e.at < ?1
                   AND e.kind NOT IN ('browser_turn','browser_session','subagent_browser_session')
                   AND NOT EXISTS(
                       SELECT 1 FROM agent_turns t
                       WHERE t.id = e.run_id
                         AND t.state IN ('queued','running')
                   )
                   AND NOT EXISTS(
                       SELECT 1 FROM run_stats r
                       WHERE r.id = e.run_id
                         AND r.state IN ('queued','preparing','submitted','running')
                   )",
                [cutoff],
                |row| row.get(0),
            )
            .map_err(ServiceError::storage)?;
        let mut compacted_payloads: Vec<(String, String)> = Vec::new();
        {
            let mut query = tx
                .prepare(
                    "SELECT o.id, o.payload
                     FROM agent_turns t
                     JOIN objects o ON o.scope='local' AND o.kind='agent' AND o.id=t.id
                     WHERE t.updated_at > 0
                       AND t.updated_at < ?1
                       AND t.state NOT IN ('queued','running')",
                )
                .map_err(ServiceError::storage)?;
            let rows = query
                .query_map([cutoff], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(ServiceError::storage)?;
            for row in rows {
                let (id, payload) = row.map_err(ServiceError::storage)?;
                let Ok(mut record) = serde_json::from_str::<AgentRecord>(&payload) else {
                    continue;
                };
                if compact_agent_record(&mut record) {
                    let serialized =
                        serde_json::to_string(&record).map_err(ServiceError::storage)?;
                    compacted_payloads.push((id, serialized));
                }
            }
        }
        let mut stats = CacheStats {
            retention_days: options.days,
            last_cleanup: last,
            busy,
            eligible_artifacts: candidates.len() as u64,
            expired_cache: tx
                .query_row(
                    "SELECT count(*) FROM cache WHERE expires<?1",
                    [now],
                    |row| row.get(0),
                )
                .map_err(ServiceError::storage)?,
            pruned_events,
            compacted_turns: compacted_payloads.len() as u64,
            ..Default::default()
        };
        let mut removals = Vec::new();
        for entry in entries(&self.root.join("blobs"))? {
            let Some(meta) = metadata(&entry.path())? else {
                continue;
            };
            if !meta.is_file() || meta.file_type().is_symlink() {
                continue;
            }
            stats.blob_bytes += meta.len();
            let name = entry.file_name().to_string_lossy().into_owned();
            let hash = name.len() == 64 && name.bytes().all(|ch| ch.is_ascii_hexdigit());
            let expired = hash && all_hashes.contains(&name) && !retained_hashes.contains(&name);
            let orphan = (hash && !all_hashes.contains(&name) || owned_name(&name, "tmp_"))
                && old(&meta, now - TEMP_AGE_MS);
            if !busy && (expired || orphan) {
                stats.orphan_blobs += u64::from(orphan);
                removals.push(Removal {
                    path: entry.path(),
                    bytes: meta.len(),
                    directory: false,
                    lease: None,
                });
            }
        }
        for (category, prefix) in [("uploads", "turn_"), ("media", "download_")] {
            for entry in entries(&self.root.join(category))? {
                let Some(meta) = metadata(&entry.path())? else {
                    continue;
                };
                if !meta.is_dir() || meta.file_type().is_symlink() {
                    continue;
                }
                let mut newest = meta.modified().map_err(ServiceError::storage)?;
                let Some(bytes) = directory_size(&entry.path(), &mut newest)? else {
                    continue;
                };
                stats.temporary_bytes += bytes;
                if !owned_name(&entry.file_name().to_string_lossy(), prefix)
                    || newest
                        .duration_since(UNIX_EPOCH)
                        .map_err(ServiceError::storage)?
                        .as_millis()
                        >= (now - TEMP_AGE_MS).max(0) as u128
                {
                    continue;
                }
                let lock = entry.path().join(".lease");
                let lease = match try_open_temp_lease(&lock)? {
                    TempLease::Acquired(file) => {
                        (options.prune && !options.dry_run).then_some(file)
                    }
                    TempLease::LockedOrInvalid | TempLease::Gone => continue,
                    TempLease::Missing if !busy => None,
                    TempLease::Missing => continue,
                };
                stats.temporary_entries += 1;
                removals.push(Removal {
                    path: entry.path(),
                    bytes,
                    directory: true,
                    lease,
                });
            }
        }
        // Staging entries under `blobs/.tmp` always hold a `.lease` before `blob_lease`
        // is dropped, so any unlocked entry is an abandoned crash leftover and can be
        // reclaimed immediately regardless of `busy` or age.
        // Consistent with `uploads/` and `media/`, `temporary_bytes` tracks total staging
        // usage (including active writers), while `temporary_entries` counts reclaimable
        // entries queued in `removals`.
        for entry in entries(&self.root.join("blobs").join(".tmp"))? {
            let Some(meta) = metadata(&entry.path())? else {
                continue;
            };
            if !meta.is_dir() || meta.file_type().is_symlink() {
                continue;
            }
            let mut newest = meta.modified().map_err(ServiceError::storage)?;
            let Some(bytes) = directory_size(&entry.path(), &mut newest)? else {
                continue;
            };
            stats.temporary_bytes += bytes;
            if !owned_name(&entry.file_name().to_string_lossy(), "tmp_") {
                continue;
            }
            let lock = entry.path().join(".lease");
            let lease = match try_open_temp_lease(&lock)? {
                TempLease::Acquired(file) => (options.prune && !options.dry_run).then_some(file),
                TempLease::LockedOrInvalid => continue,
                TempLease::Gone => {
                    if metadata(&entry.path())?.is_none() {
                        stats.temporary_bytes = stats.temporary_bytes.saturating_sub(bytes);
                    }
                    continue;
                }
                TempLease::Missing => {
                    if metadata(&entry.path())?.is_none() {
                        stats.temporary_bytes = stats.temporary_bytes.saturating_sub(bytes);
                        continue;
                    }
                    None
                }
            };
            stats.temporary_entries += 1;
            removals.push(Removal {
                path: entry.path(),
                bytes,
                directory: true,
                lease,
            });
        }
        stats.reclaimable_bytes = removals.iter().map(|file| file.bytes).sum();
        if options.prune && !options.dry_run {
            tx.execute("DELETE FROM cache WHERE expires<?1", [now])
                .map_err(ServiceError::storage)?;
            if stats.pruned_events > 0 {
                tx.execute(
                    "DELETE FROM events
                     WHERE at < ?1
                       AND kind NOT IN ('browser_turn','browser_session','subagent_browser_session')
                       AND NOT EXISTS(
                           SELECT 1 FROM agent_turns t
                           WHERE t.id = events.run_id
                             AND t.state IN ('queued','running')
                       )
                       AND NOT EXISTS(
                           SELECT 1 FROM run_stats r
                           WHERE r.id = events.run_id
                             AND r.state IN ('queued','preparing','submitted','running')
                       )",
                    [cutoff],
                )
                .map_err(ServiceError::storage)?;
            }
            for (id, payload) in &compacted_payloads {
                tx.execute(
                    "UPDATE objects SET payload=?2 WHERE scope='local' AND kind='agent' AND id=?1",
                    params![id, payload],
                )
                .map_err(ServiceError::storage)?;
            }
            for (scope, id) in &candidates {
                tx.execute(
                    "DELETE FROM objects WHERE scope=?1 AND kind='artifact' AND id=?2",
                    params![scope, id],
                )
                .map_err(ServiceError::storage)?;
            }
            tx.commit().map_err(ServiceError::storage)?;
            for file in removals {
                drop(file.lease);
                let removed = if file.directory {
                    retry_transient(|| fs::remove_dir_all(&file.path))
                } else {
                    retry_transient(|| fs::remove_file(&file.path))
                };
                if let Err(error) = removed {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        continue;
                    }
                    return Err(ServiceError::storage(error));
                }
                stats.reclaimed_bytes += file.bytes;
                if file.directory {
                    stats.temporary_bytes -= file.bytes;
                } else {
                    stats.blob_bytes -= file.bytes;
                }
            }
            stats.pruned = true;
            stats.last_cleanup = now;
            conn.execute("INSERT INTO objects VALUES('local','maintenance','cache',?1) ON CONFLICT(scope,kind,id) DO UPDATE SET payload=excluded.payload",
                [serde_json::json!({"last_cleanup":now}).to_string()]).map_err(ServiceError::storage)?;
            if stats.pruned_events > 0
                || stats.compacted_turns > 0
                || stats.expired_cache > 0
                || !candidates.is_empty()
            {
                let _ = conn.execute_batch("PRAGMA incremental_vacuum(256); PRAGMA optimize;");
            }
        }
        Ok(Some(stats))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn artifact(store: &Store, name: &str, data: &[u8], created_at: i64) -> Artifact {
        let mut value = store
            .artifact("default", name, "image/png", data, "browser")
            .unwrap();
        value.created_at = created_at;
        store.put("default", "artifact", &value.id, &value).unwrap();
        value
    }

    #[test]
    fn cache_retention_protects_records_events_shared_blobs_and_preview() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let now = now_ms();
        let old = now - 8 * 86_400_000;
        let unused = artifact(&store, "unused.png", b"unused", old);
        let recent = artifact(&store, "recent.png", b"recent", now - 6 * 86_400_000);
        let saved = artifact(&store, "saved.png", b"saved", old);
        let alias = artifact(&store, "alias.png", b"saved", old);
        store
            .put(
                "local",
                "agent",
                "saved",
                &json!({"state":"completed","request":{"artifacts":[saved.id]}}),
            )
            .unwrap();
        let from_event = artifact(&store, "event.png", b"event", old);
        store
            .event("saved", "tool_result", json!({"artifacts":[from_event.id]}))
            .unwrap();
        for kind in ["run", "session"] {
            let image = artifact(&store, kind, kind.as_bytes(), old);
            store
                .put(
                    "local",
                    kind,
                    kind,
                    &json!({"provider":"gemini-web","account_id":"default","mode":"chat","state":"closed","artifacts":[image.id],"request":{"account_id":"default","mode":"chat","messages":[]}}),
                )
                .unwrap();
        }
        // A broken referenced blob is kept for recovery, even after retention expires.
        let (_, saved_path) = store.artifact_path("default", &saved.id).unwrap();
        fs::write(&saved_path, b"corrupt").unwrap();
        store
            .cache_put("default", "expired", &json!(true), 0)
            .unwrap();
        let now = now_ms();
        let before = store.total_changes();
        let preview = store
            .cache_at(
                Options {
                    prune: true,
                    dry_run: true,
                    ..Default::default()
                },
                now + 1,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(preview.eligible_artifacts, 2);
        assert_eq!(preview.reclaimable_bytes, 6);
        assert!(!preview.pruned);
        assert_eq!(preview.last_cleanup, 0);
        assert_eq!(store.total_changes(), before);
        assert_eq!(
            store.artifact_bytes("default", &unused.id).unwrap().1,
            b"unused"
        );
        let cleaned = store
            .cache_at(
                Options {
                    prune: true,
                    ..Default::default()
                },
                now + 1,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(cleaned.reclaimed_bytes, preview.reclaimable_bytes);
        assert_eq!(cleaned.expired_cache, 1);
        assert!(cleaned.pruned);
        assert!(store.artifact_path("default", &unused.id).is_err());
        assert!(store.artifact_path("default", &alias.id).is_err());
        assert_eq!(
            store.artifact_bytes("default", &recent.id).unwrap().1,
            b"recent"
        );
        assert!(store.artifact_path("default", &saved.id).is_ok());
        assert_eq!(fs::read(saved_path).unwrap(), b"corrupt");
        assert!(store.artifact_bytes("default", &from_event.id).is_ok());
        for kind in ["agent", "run", "session"] {
            assert_eq!(
                store
                    .list::<serde_json::Value>("local", kind)
                    .unwrap()
                    .len(),
                1
            );
        }
        assert_eq!(store.events("saved", 0).unwrap().len(), 1);
    }

    #[test]
    fn cache_cleanup_respects_temp_leases_active_work_grace_and_owned_paths() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let now = now_ms() + 2 * TEMP_AGE_MS;
        let active_path = dir.path().join("uploads").join(id("turn"));
        let guard = crate::service::TempDirGuard::new(active_path.clone()).unwrap();
        fs::write(active_path.join("input.png"), b"active pixels").unwrap();
        let abandoned = dir.path().join("media").join(id("download"));
        fs::create_dir_all(&abandoned).unwrap();
        fs::write(abandoned.join(".lease"), b"").unwrap();
        fs::write(abandoned.join("output.png"), b"abandoned").unwrap();
        let legacy = dir.path().join("uploads").join(id("turn"));
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("input"), b"legacy").unwrap();
        let fresh = dir.path().join("media").join(id("download"));
        fs::create_dir_all(&fresh).unwrap();
        let fresh_file = fs::File::create(fresh.join("new.png")).unwrap();
        fresh_file
            .set_times(
                fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_millis(now as u64)),
            )
            .unwrap();
        let orphan = dir.path().join("blobs").join(digest(b"orphan"));
        fs::write(&orphan, b"orphan").unwrap();
        let partial = dir.path().join("blobs").join(id("tmp"));
        fs::write(&partial, b"partial").unwrap();
        let profile = dir.path().join("profile");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("Cookies"), b"keep").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&profile, dir.path().join("uploads").join(id("turn"))).unwrap();
        store
            .put("local", "agent", "active", &json!({"state":"running"}))
            .unwrap();
        let options = Options {
            prune: true,
            ..Default::default()
        };
        let busy = store.cache_at(options, now, None).unwrap().unwrap();
        assert!(busy.busy);
        assert!(!abandoned.exists());
        assert!(active_path.exists() && legacy.exists() && orphan.exists() && partial.exists());
        store
            .put("local", "agent", "active", &json!({"state":"completed"}))
            .unwrap();
        let idle = store.cache_at(options, now, None).unwrap().unwrap();
        assert!(!idle.busy);
        assert_eq!(idle.orphan_blobs, 2);
        assert!(!legacy.exists() && !orphan.exists() && !partial.exists());
        assert!(active_path.exists() && fresh.exists());
        assert_eq!(fs::read(profile.join("Cookies")).unwrap(), b"keep");
        drop(guard);
        assert!(!active_path.exists());
    }

    #[test]
    fn cache_reclaims_abandoned_blob_staging_under_busy_while_protecting_live_uploads() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .put("local", "agent", "active", &json!({"state":"running"}))
            .unwrap();

        let mut live = store
            .artifact_writer(
                "default",
                "live.bin",
                "application/octet-stream",
                Some(10),
                None,
                "input",
            )
            .unwrap();
        live.write_chunk(b"12345").unwrap();

        let crashed = dir.path().join("blobs").join(".tmp").join(id("tmp"));
        fs::create_dir_all(&crashed).unwrap();
        fs::write(crashed.join(".lease"), b"").unwrap();
        fs::write(crashed.join("data"), b"crashed upload bytes").unwrap();

        let stats = store
            .cache_at(
                Options {
                    prune: true,
                    ..Default::default()
                },
                now_ms(),
                None,
            )
            .unwrap()
            .unwrap();
        assert!(stats.busy);
        assert!(!crashed.exists());
        assert_eq!(stats.temporary_entries, 1);
        assert_eq!(stats.reclaimed_bytes, b"crashed upload bytes".len() as u64);

        live.write_chunk(b"67890").unwrap();
        let art = live.finish().unwrap();
        assert_eq!(
            store.artifact_bytes("default", &art.id).unwrap().1,
            b"1234567890"
        );
    }

    #[test]
    fn cache_automatic_interval_and_pressure_reclaim_only_eligible_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let now = now_ms();
        let options = Options {
            prune: true,
            ..Default::default()
        };
        assert!(store
            .cache_at(options, now, Some(INTERVAL))
            .unwrap()
            .is_some());
        let image = artifact(&store, "old", b"old", now - 8 * 86_400_000);
        assert!(store
            .cache_at(
                options,
                now + INTERVAL.as_millis() as i64 - 1,
                Some(INTERVAL)
            )
            .unwrap()
            .is_none());
        assert!(store.artifact_path("default", &image.id).is_ok());
        assert!(store
            .cache_at(options, now + INTERVAL.as_millis() as i64, Some(INTERVAL))
            .unwrap()
            .is_some());
        assert!(store.artifact_path("default", &image.id).is_err());

        let pressure_dir = tempfile::tempdir().unwrap();
        let pressure = Store::open(pressure_dir.path()).unwrap();
        let path = pressure_dir
            .path()
            .join("blobs")
            .join(digest(b"abandoned large blob"));
        let file = fs::File::create(&path).unwrap();
        file.set_len(BLOB_LIMIT).unwrap(); // Sparse fixture; does not allocate a GiB.
        file.set_times(
            fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(48 * 3600)),
        )
        .unwrap();
        drop(file);
        let input = pressure
            .artifact("default", "new", "text/plain", b"new", "input")
            .unwrap();
        assert!(!path.exists());
        assert_eq!(
            pressure.artifact_bytes("default", &input.id).unwrap().1,
            b"new"
        );
        let retained_dir = tempfile::tempdir().unwrap();
        let retained = Store::open(retained_dir.path()).unwrap();
        let image = artifact(&retained, "retained", b"pixels", now - 8 * 86_400_000);
        let (_, path) = retained.artifact_path("default", &image.id).unwrap();
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(BLOB_LIMIT).unwrap();
        drop(file);
        retained
            .put(
                "local",
                "agent",
                "saved",
                &json!({"state":"completed","artifacts":[image.id]}),
            )
            .unwrap();
        let error = retained
            .artifact("default", "new", "text/plain", b"new", "input")
            .unwrap_err();
        assert!(error.message.contains("3 more bytes required"));
        assert_eq!(fs::metadata(path).unwrap().len(), BLOB_LIMIT);
        assert!(retained.artifact_path("default", &image.id).is_ok());
    }

    #[test]
    fn cache_command_defaults_to_seven_days_and_rejects_ambiguous_input() {
        assert_eq!(Options::parse(&[]).unwrap(), Options::default());
        assert_eq!(Options::parse(&["prune"]).unwrap().days, 7);
        assert!(
            Options::parse(&["prune", "--dry-run", "7"])
                .unwrap()
                .dry_run
        );
        for args in [
            vec!["prune", "0"],
            vec!["prune", "-1"],
            vec!["prune", "3651"],
            vec!["prune", "7", "14"],
            vec!["stats", "--dry-run"],
            vec!["prune", "--dry-run", "--dry-run"],
        ] {
            assert!(Options::parse(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn cache_waits_for_input_registration_across_store_connections() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let image = artifact(&store, "input.png", b"input", now_ms() - 8 * 86_400_000);
        let pin = store.blob_lease().unwrap();
        let cleaner = Store::open(dir.path()).unwrap();
        let (started, ready) = std::sync::mpsc::channel();
        let (finished, done) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            started.send(()).unwrap();
            finished
                .send(cleaner.cache_report(Options {
                    prune: true,
                    ..Default::default()
                }))
                .unwrap();
        });
        ready.recv().unwrap();
        assert!(matches!(
            done.recv_timeout(Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        store
            .put(
                "local",
                "agent",
                "saved",
                &json!({"state":"completed","artifacts":[image.id]}),
            )
            .unwrap();
        drop(pin);
        assert_eq!(
            done.recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap()
                .eligible_artifacts,
            0
        );
        thread.join().unwrap();
        assert_eq!(
            store.artifact_bytes("default", &image.id).unwrap().1,
            b"input"
        );
    }

    #[test]
    fn cache_entries_handles_missing_dir_and_concurrent_staging_drops() {
        struct JoinOnDrop {
            stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
            handle: Option<std::thread::JoinHandle<()>>,
        }
        impl Drop for JoinOnDrop {
            fn drop(&mut self) {
                self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                if let Some(handle) = self.handle.take() {
                    let _ = handle.join();
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert!(entries(&dir.path().join("does-not-exist"))
            .unwrap()
            .is_empty());

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_dropper = stop.clone();
        let dropper_store = store.clone();
        let mut dropper = JoinOnDrop {
            stop: stop.clone(),
            handle: Some(std::thread::spawn(move || {
                for _ in 0..40 {
                    if stop_dropper.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    let mut writer = dropper_store
                        .artifact_writer(
                            "default",
                            "ephemeral.bin",
                            "application/octet-stream",
                            Some(32),
                            None,
                            "input",
                        )
                        .unwrap();
                    writer.write_chunk(&[7; 16]).unwrap();
                    drop(writer);
                }
            })),
        };
        for _ in 0..40 {
            let stats = store.cache_report(Options::default()).unwrap();
            assert!(!stats.pruned);
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        dropper.handle.take().unwrap().join().unwrap();
    }

    #[test]
    fn cache_retention_prunes_old_non_ownership_events_compacts_aged_steps_and_preserves_recent() {
        use crate::agent::{AgentRunResult, AgentStatus, AgentStepRecord, ToolCallRequest, ToolOutput};

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let now = now_ms();
        let old_ts = now - 10 * 86_400_000;
        let recent_ts = now - 2 * 86_400_000;

        let make_heavy_result = || AgentRunResult {
            status: AgentStatus::Completed,
            final_response: "Final summary preserved".into(),
            files_changed: vec!["src/main.rs".into()],
            elapsed_ms: 420,
            inspector_pruned: false,
            steps: vec![AgentStepRecord {
                step: 1,
                model_response: "Verbose reasoning text".repeat(50),
                tool_calls: vec![
                    ToolCallRequest {
                        tool: "edit_file".into(),
                        args: json!({
                            "path": "src/main.rs",
                            "replace_all": true,
                            "old_text": "old_body".repeat(200),
                            "new_text": "new_body".repeat(200),
                        }),
                    },
                    ToolCallRequest {
                        tool: "run_shell".into(),
                        args: json!({"command": "cargo test --lib"}),
                    },
                ],
                observations: vec![
                    ToolOutput {
                        tool: "edit_file".into(),
                        ok: true,
                        output: "updated src/main.rs".into(),
                        artifacts: vec![],
                        elapsed_ms: 12,
                    },
                    ToolOutput {
                        tool: "run_shell".into(),
                        ok: true,
                        output: "test output ".repeat(300),
                        artifacts: vec![],
                        elapsed_ms: 350,
                    },
                ],
                notes: vec!["verbose note".into()],
            }],
            ..Default::default()
        };

        let make_record = |id: &str, conv: &str, state: &str, updated_at: i64, res: Option<AgentRunResult>| AgentRecord {
            id: id.into(),
            conversation_id: conv.into(),
            state: state.into(),
            request: AgentRequest {
                environment: None,
                task: "test task".into(),
                workspace: dir.path().to_path_buf(),
                account_id: "default".into(),
                model: None,
                thinking: None,
                session_mode: SessionMode::Chat,
                no_shell: false,
                max_steps: 4,
                knowledge: None,
                mcp_config: None,
                artifacts: vec![],
                conversation_id: Some(conv.into()),
                chat_only: false,
                permissions: None,
                session_retention: SessionRetention::Retain,
                tool_approval: ToolApprovalMode::Auto,
            },
            result: res,
            error: None,
            created_at: updated_at - 1000,
            updated_at,
        };

        // 1. Old completed turn (10 days old) + non-ownership & ownership events.
        store
            .put(
                "local",
                "agent",
                "turn-old",
                &make_record("turn-old", "conv-old", "completed", old_ts, Some(make_heavy_result())),
            )
            .unwrap();
        // 2. Old still-running turn (10 days old) — events must NOT be deleted.
        store
            .put(
                "local",
                "agent",
                "turn-active",
                &make_record("turn-active", "conv-active", "running", old_ts, None),
            )
            .unwrap();
        // 3. Recent completed turn (2 days old) — must remain 100% lossless for /resume.
        store
            .put(
                "local",
                "agent",
                "turn-recent",
                &make_record("turn-recent", "conv-recent", "completed", recent_ts, Some(make_heavy_result())),
            )
            .unwrap();

        // Insert events and backdate old ones explicitly.
        for kind in ["tool_started", "tool_finished", "step_started", "browser_turn", "browser_session", "subagent_browser_session"] {
            store.event("turn-old", kind, json!({"k": kind})).unwrap();
        }
        store.event("turn-active", "tool_started", json!({"tool": "read_file"})).unwrap();
        store.event("turn-recent", "tool_started", json!({"tool": "read_file"})).unwrap();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE events SET at = ?1 WHERE run_id IN ('turn-old', 'turn-active')",
                [old_ts],
            )
            .unwrap();
            conn.execute(
                "UPDATE events SET at = ?1 WHERE run_id = 'turn-recent'",
                [recent_ts],
            )
            .unwrap();
        }

        // Dry run reports exact counts without mutating DB.
        let dry = store
            .cache_at(
                Options {
                    days: 7,
                    prune: true,
                    dry_run: true,
                },
                now,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(dry.pruned_events, 3);
        assert_eq!(dry.compacted_turns, 1);
        assert!(!dry.pruned);
        assert_eq!(store.events("turn-old", 0).unwrap().len(), 6);
        assert!(
            !store
                .get::<AgentRecord>("local", "agent", "turn-old")
                .unwrap()
                .result
                .unwrap()
                .inspector_pruned
        );

        // Real prune deletes expired non-ownership events, keeps ownership + active + recent,
        // and compacts old turn steps once.
        let pruned = store
            .cache_at(
                Options {
                    days: 7,
                    prune: true,
                    dry_run: false,
                },
                now,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(pruned.pruned_events, 3);
        assert_eq!(pruned.compacted_turns, 1);

        let old_events: Vec<String> = store
            .events("turn-old", 0)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(
            old_events,
            vec!["browser_turn", "browser_session", "subagent_browser_session"]
        );
        assert_eq!(store.events("turn-active", 0).unwrap().len(), 1);
        assert_eq!(store.events("turn-recent", 0).unwrap().len(), 1);

        // Old turn compacted: inspector_pruned = true, heavy payloads dropped, summary & metadata kept.
        let old_rec: AgentRecord = store.get("local", "agent", "turn-old").unwrap();
        let old_res = old_rec.result.unwrap();
        assert!(old_res.inspector_pruned);
        assert_eq!(old_res.final_response, "Final summary preserved");
        assert_eq!(old_res.files_changed, vec!["src/main.rs"]);
        assert_eq!(old_res.steps.len(), 1);
        assert!(old_res.steps[0].model_response.is_empty());
        assert_eq!(old_res.steps[0].tool_calls[0].tool, "edit_file");
        assert_eq!(old_res.steps[0].tool_calls[0].args["path"], "src/main.rs");
        assert_eq!(old_res.steps[0].tool_calls[0].args["replace_all"], true);
        assert!(old_res.steps[0].tool_calls[0].args.get("old_text").is_none());
        assert!(old_res.steps[0].tool_calls[0].args.get("new_text").is_none());
        assert_eq!(
            old_res.steps[0].tool_calls[1].args["command"],
            "cargo test --lib"
        );
        assert!(old_res.steps[0].observations[1].output.is_empty());
        assert_eq!(old_res.steps[0].observations[1].elapsed_ms, 350);

        // Recent turn (< 7d) is 100% lossless for /resume Tool Inspector restoration.
        let recent_rec: AgentRecord = store.get("local", "agent", "turn-recent").unwrap();
        let recent_res = recent_rec.result.unwrap();
        assert!(!recent_res.inspector_pruned);
        assert!(recent_res.steps[0].tool_calls[0].args["old_text"]
            .as_str()
            .unwrap()
            .starts_with("old_body"));
        assert!(recent_res.steps[0].observations[1]
            .output
            .starts_with("test output"));

        // Second prune is idempotent (0 events, 0 compacted turns).
        let second = store
            .cache_at(
                Options {
                    days: 7,
                    prune: true,
                    dry_run: false,
                },
                now,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(second.pruned_events, 0);
        assert_eq!(second.compacted_turns, 0);

        // Verify EXPLAIN QUERY PLAN uses events_at and PK lookups on agent_turns / run_stats (no table scans).
        let conn = store.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT count(*) FROM events e
                 WHERE e.at < ?1
                   AND e.kind NOT IN ('browser_turn','browser_session','subagent_browser_session')
                   AND NOT EXISTS(
                       SELECT 1 FROM agent_turns t
                       WHERE t.id = e.run_id
                         AND t.state IN ('queued','running')
                   )
                   AND NOT EXISTS(
                       SELECT 1 FROM run_stats r
                       WHERE r.id = e.run_id
                         AND r.state IN ('queued','preparing','submitted','running')
                   )",
            )
            .unwrap();
        let plan_lines: Vec<String> = stmt
            .query_map([now], |row| row.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let plan = plan_lines.join("\n");
        assert!(
            plan.contains("USING INDEX events_at"),
            "expected events_at index: {plan}"
        );
        assert!(
            plan.contains("sqlite_autoindex_agent_turns_1") || plan.contains("USING PRIMARY KEY"),
            "expected agent_turns PK lookup: {plan}"
        );
        assert!(
            plan.contains("sqlite_autoindex_run_stats_1") || plan.contains("USING PRIMARY KEY"),
            "expected run_stats PK lookup: {plan}"
        );
        assert!(
            !plan.contains("SCAN "),
            "retention query must not full-scan any table: {plan}"
        );
    }
}
