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

fn owned_name(name: &str, prefix: &str) -> bool {
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

fn entries(path: &Path) -> Result<Vec<fs::DirEntry>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => fs::read_dir(path)
            .map_err(ServiceError::storage)?
            .collect::<std::io::Result<_>>()
            .map_err(ServiceError::storage),
        Ok(_) => Err(ServiceError::invalid(
            "cache directory must not be a symlink or a file",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(ServiceError::storage(error)),
    }
}

fn metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ServiceError::storage(error)),
    }
}

// Refuse to traverse links, including links inside an abandoned directory.
fn directory_size(path: &Path, newest: &mut SystemTime) -> Result<Option<u64>> {
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
            ..Default::default()
        };
        let mut removals = Vec::new();
        for entry in entries(&self.root.join("blobs"))? {
            let meta = fs::symlink_metadata(entry.path()).map_err(ServiceError::storage)?;
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
                let lease = match fs::symlink_metadata(&lock) {
                    Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {
                        let file = match fs::OpenOptions::new().read(true).write(true).open(lock) {
                            Ok(file) => file,
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                            Err(error) => return Err(ServiceError::storage(error)),
                        };
                        if file.try_lock_exclusive().is_err() {
                            continue;
                        }
                        Some(file)
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound && !busy => None,
                    _ => continue,
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
        stats.reclaimable_bytes = removals.iter().map(|file| file.bytes).sum();
        if options.prune && !options.dry_run {
            tx.execute("DELETE FROM cache WHERE expires<?1", [now])
                .map_err(ServiceError::storage)?;
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
                    fs::remove_dir_all(&file.path)
                } else {
                    fs::remove_file(&file.path)
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
}
