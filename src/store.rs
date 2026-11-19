//! SQLite owns metadata; immutable blobs own bulk data. No credentials are stored here.
use crate::domain::*;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fmt::Write as _,
    fs,
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub mod cache;

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
    root: PathBuf,
}

impl Store {
    pub fn model_catalog(&self, account: &str) -> Result<Value> {
        match self.get(account, "catalog", "models") {
            Ok(value) => Ok(value),
            Err(error) if error.code == ErrorCode::NotFound => Ok(
                serde_json::json!({"models":[],"thinking":[],"model_thinking":{},"checked_at":null}),
            ),
            Err(error) => Err(error),
        }
    }
    pub fn model_catalog_record(&self, account: &str) -> Result<ModelCatalogRecord> {
        match self.get(account, "catalog", "models") {
            Ok(record) => Ok(record),
            Err(error) if error.code == ErrorCode::NotFound => Ok(ModelCatalogRecord::default()),
            Err(error) => Err(error),
        }
    }
    #[cfg(test)]
    pub(crate) fn total_changes(&self) -> u64 {
        self.conn.lock().unwrap().total_changes()
    }
    pub(crate) fn blob_lease(&self) -> Result<fs::File> {
        use fs2::FileExt;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("blobs.lock"))
            .map_err(ServiceError::storage)?;
        file.lock_exclusive().map_err(ServiceError::storage)?;
        Ok(file)
    }
    pub fn open(root: &Path) -> Result<Self> {
        let tmp_root = Self::ensure_blob_tmp_root_at(root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))
                .map_err(ServiceError::storage)?;
            fs::set_permissions(&tmp_root, fs::Permissions::from_mode(0o700))
                .map_err(ServiceError::storage)?;
        }
        use fs2::FileExt;
        let _init_lock = {
            let file = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(root.join("state.lock"))
                .map_err(ServiceError::storage)?;
            file.lock_exclusive().map_err(ServiceError::storage)?;
            file
        };
        let conn = Connection::open(root.join("state.sqlite3")).map_err(ServiceError::storage)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(ServiceError::storage)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
            PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS objects(
              scope TEXT NOT NULL,
              kind TEXT NOT NULL,
              id TEXT NOT NULL,
              payload TEXT NOT NULL,
              PRIMARY KEY(scope,kind,id)
            );
            CREATE INDEX IF NOT EXISTS objects_kind_scope ON objects(kind, scope);
            CREATE TABLE IF NOT EXISTS events(
              sequence INTEGER PRIMARY KEY AUTOINCREMENT,
              run_id TEXT NOT NULL,
              kind TEXT NOT NULL,
              data TEXT NOT NULL,
              at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS events_run ON events(run_id,sequence);
            CREATE TABLE IF NOT EXISTS idempotency(
              scope TEXT NOT NULL,
              key TEXT NOT NULL,
              digest TEXT NOT NULL,
              run_id TEXT NOT NULL,
              PRIMARY KEY(scope,key)
            );
            CREATE TABLE IF NOT EXISTS cache(
              scope TEXT NOT NULL,
              key TEXT NOT NULL,
              value TEXT NOT NULL,
              expires INTEGER NOT NULL,
              PRIMARY KEY(scope,key)
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS knowledge USING fts5(source UNINDEXED,path UNINDEXED,hash UNINDEXED,line UNINDEXED,body);
            CREATE TABLE IF NOT EXISTS accounts(
              id TEXT PRIMARY KEY,
              provider TEXT NOT NULL,
              port INTEGER NOT NULL,
              profile TEXT NOT NULL,
              headless INTEGER NOT NULL,
              state TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS accounts_provider ON accounts(provider, state);
            CREATE TRIGGER IF NOT EXISTS accounts_insert AFTER INSERT ON objects WHEN NEW.kind='account' BEGIN
              INSERT INTO accounts(id,provider,port,profile,headless,state) VALUES(
                NEW.id,
                json_extract(NEW.payload,'$.provider'),
                coalesce(json_extract(NEW.payload,'$.port'),0),
                coalesce(json_extract(NEW.payload,'$.profile'),''),
                coalesce(json_extract(NEW.payload,'$.headless'),1),
                coalesce(json_extract(NEW.payload,'$.state'),'auth_required')
              )
              ON CONFLICT(id) DO UPDATE SET
                provider=excluded.provider,
                port=excluded.port,
                profile=excluded.profile,
                headless=excluded.headless,
                state=excluded.state;
            END;
            CREATE TRIGGER IF NOT EXISTS accounts_update AFTER UPDATE ON objects WHEN NEW.kind='account' BEGIN
              INSERT INTO accounts(id,provider,port,profile,headless,state) VALUES(
                NEW.id,
                json_extract(NEW.payload,'$.provider'),
                coalesce(json_extract(NEW.payload,'$.port'),0),
                coalesce(json_extract(NEW.payload,'$.profile'),''),
                coalesce(json_extract(NEW.payload,'$.headless'),1),
                coalesce(json_extract(NEW.payload,'$.state'),'auth_required')
              )
              ON CONFLICT(id) DO UPDATE SET
                provider=excluded.provider,
                port=excluded.port,
                profile=excluded.profile,
                headless=excluded.headless,
                state=excluded.state;
            END;
            CREATE TRIGGER IF NOT EXISTS accounts_delete AFTER DELETE ON objects WHEN OLD.kind='account' BEGIN
              DELETE FROM accounts WHERE id=OLD.id;
            END;
            CREATE TABLE IF NOT EXISTS run_stats(
              id TEXT PRIMARY KEY,
              provider TEXT NOT NULL,
              account TEXT NOT NULL,
              model TEXT,
              mode TEXT NOT NULL,
              state TEXT NOT NULL,
              input_chars INTEGER NOT NULL DEFAULT 0,
              output_chars INTEGER NOT NULL DEFAULT 0,
              updated_at INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS run_stats_provider_account ON run_stats(provider, account, updated_at DESC);
            CREATE INDEX IF NOT EXISTS run_stats_account_updated ON run_stats(account, updated_at DESC);
            CREATE INDEX IF NOT EXISTS run_stats_state_updated ON run_stats(state, updated_at);
            CREATE TRIGGER IF NOT EXISTS run_stats_insert AFTER INSERT ON objects WHEN NEW.kind='run' BEGIN
              INSERT INTO run_stats(id,provider,account,model,mode,state,input_chars,output_chars,updated_at) VALUES(
                NEW.id,
                json_extract(NEW.payload,'$.provider'),
                json_extract(NEW.payload,'$.request.account_id'),
                coalesce(nullif(json_extract(NEW.payload,'$.result.model'),''), nullif(json_extract(NEW.payload,'$.request.model'),'')),
                json_extract(NEW.payload,'$.request.mode'),
                json_extract(NEW.payload,'$.state'),
                coalesce((SELECT sum(length(json_extract(b.value,'$.text'))) FROM json_each(NEW.payload,'$.request.messages') m, json_each(m.value,'$.blocks') b),0),
                coalesce(length(json_extract(NEW.payload,'$.result.text')),0),
                coalesce(json_extract(NEW.payload,'$.updated_at'),0)
              )
              ON CONFLICT(id) DO UPDATE SET
                provider=excluded.provider,
                account=excluded.account,
                model=excluded.model,
                mode=excluded.mode,
                state=excluded.state,
                input_chars=excluded.input_chars,
                output_chars=excluded.output_chars,
                updated_at=excluded.updated_at;
            END;
            CREATE TRIGGER IF NOT EXISTS run_stats_update AFTER UPDATE ON objects WHEN NEW.kind='run' BEGIN
              INSERT INTO run_stats(id,provider,account,model,mode,state,input_chars,output_chars,updated_at) VALUES(
                NEW.id,
                json_extract(NEW.payload,'$.provider'),
                json_extract(NEW.payload,'$.request.account_id'),
                coalesce(nullif(json_extract(NEW.payload,'$.result.model'),''), nullif(json_extract(NEW.payload,'$.request.model'),'')),
                json_extract(NEW.payload,'$.request.mode'),
                json_extract(NEW.payload,'$.state'),
                coalesce((SELECT sum(length(json_extract(b.value,'$.text'))) FROM json_each(NEW.payload,'$.request.messages') m, json_each(m.value,'$.blocks') b),0),
                coalesce(length(json_extract(NEW.payload,'$.result.text')),0),
                coalesce(json_extract(NEW.payload,'$.updated_at'),0)
              )
              ON CONFLICT(id) DO UPDATE SET
                provider=excluded.provider,
                account=excluded.account,
                model=excluded.model,
                mode=excluded.mode,
                state=excluded.state,
                input_chars=excluded.input_chars,
                output_chars=excluded.output_chars,
                updated_at=excluded.updated_at;
            END;
            CREATE TRIGGER IF NOT EXISTS run_stats_delete AFTER DELETE ON objects WHEN OLD.kind='run' BEGIN
              DELETE FROM run_stats WHERE id=OLD.id;
            END;
            CREATE TABLE IF NOT EXISTS session_stats(
              id TEXT PRIMARY KEY,
              provider TEXT NOT NULL,
              account TEXT NOT NULL,
              mode TEXT NOT NULL,
              state TEXT NOT NULL,
              model TEXT,
              thinking TEXT,
              reuse_scope TEXT,
              remote_turns INTEGER NOT NULL DEFAULT 0,
              last_used INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS session_stats_provider_account ON session_stats(provider, account, state, last_used DESC);
            CREATE INDEX IF NOT EXISTS session_stats_account_state ON session_stats(account, state, last_used DESC);
            CREATE TRIGGER IF NOT EXISTS session_stats_insert AFTER INSERT ON objects WHEN NEW.kind='session' BEGIN
              INSERT INTO session_stats(id,provider,account,mode,state,model,thinking,reuse_scope,remote_turns,last_used) VALUES(
                NEW.id,
                json_extract(NEW.payload,'$.provider'),
                json_extract(NEW.payload,'$.account_id'),
                json_extract(NEW.payload,'$.mode'),
                json_extract(NEW.payload,'$.state'),
                nullif(json_extract(NEW.payload,'$.model'),''),
                nullif(json_extract(NEW.payload,'$.thinking'),''),
                nullif(json_extract(NEW.payload,'$.reuse_scope'),''),
                coalesce(json_extract(NEW.payload,'$.remote_turns'),0),
                coalesce(json_extract(NEW.payload,'$.last_used'),0)
              )
              ON CONFLICT(id) DO UPDATE SET
                provider=excluded.provider,
                account=excluded.account,
                mode=excluded.mode,
                state=excluded.state,
                model=excluded.model,
                thinking=excluded.thinking,
                reuse_scope=excluded.reuse_scope,
                remote_turns=excluded.remote_turns,
                last_used=excluded.last_used;
            END;
            CREATE TRIGGER IF NOT EXISTS session_stats_update AFTER UPDATE ON objects WHEN NEW.kind='session' BEGIN
              INSERT INTO session_stats(id,provider,account,mode,state,model,thinking,reuse_scope,remote_turns,last_used) VALUES(
                NEW.id,
                json_extract(NEW.payload,'$.provider'),
                json_extract(NEW.payload,'$.account_id'),
                json_extract(NEW.payload,'$.mode'),
                json_extract(NEW.payload,'$.state'),
                nullif(json_extract(NEW.payload,'$.model'),''),
                nullif(json_extract(NEW.payload,'$.thinking'),''),
                nullif(json_extract(NEW.payload,'$.reuse_scope'),''),
                coalesce(json_extract(NEW.payload,'$.remote_turns'),0),
                coalesce(json_extract(NEW.payload,'$.last_used'),0)
              )
              ON CONFLICT(id) DO UPDATE SET
                provider=excluded.provider,
                account=excluded.account,
                mode=excluded.mode,
                state=excluded.state,
                model=excluded.model,
                thinking=excluded.thinking,
                reuse_scope=excluded.reuse_scope,
                remote_turns=excluded.remote_turns,
                last_used=excluded.last_used;
            END;
            CREATE TRIGGER IF NOT EXISTS session_stats_delete AFTER DELETE ON objects WHEN OLD.kind='session' BEGIN
              DELETE FROM session_stats WHERE id=OLD.id;
            END;
            CREATE TABLE IF NOT EXISTS transfer_stats(
              run_id TEXT PRIMARY KEY,
              sent_chars INTEGER NOT NULL DEFAULT 0,
              reused INTEGER NOT NULL DEFAULT 0
            );
            CREATE TRIGGER IF NOT EXISTS transfer_submit AFTER INSERT ON events WHEN NEW.kind='submitted' BEGIN
              INSERT INTO transfer_stats(run_id,sent_chars) VALUES(NEW.run_id,coalesce(json_extract(NEW.data,'$.chars'),0))
              ON CONFLICT(run_id) DO UPDATE SET sent_chars=excluded.sent_chars;
            END;
            CREATE TRIGGER IF NOT EXISTS transfer_reuse AFTER INSERT ON events WHEN NEW.kind='cache_hit' BEGIN
              INSERT INTO transfer_stats(run_id,reused) VALUES(NEW.run_id,1)
              ON CONFLICT(run_id) DO UPDATE SET reused=1;
            END;
            PRAGMA max_page_count=262144;",
        )
        .map_err(ServiceError::storage)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            root: root.into(),
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || f(store))
            .await
            .map_err(ServiceError::storage)?
    }
    pub fn put<T: Serialize>(&self, scope: &str, kind: &str, key: &str, value: &T) -> Result<()> {
        let data = serde_json::to_string(value).map_err(ServiceError::storage)?;
        self.conn.lock().map_err(ServiceError::storage)?.execute("INSERT INTO objects VALUES(?1,?2,?3,?4) ON CONFLICT(scope,kind,id) DO UPDATE SET payload=excluded.payload",params![scope,kind,key,data]).map_err(ServiceError::storage)?;
        Ok(())
    }
    pub fn get<T: DeserializeOwned>(&self, scope: &str, kind: &str, key: &str) -> Result<T> {
        let raw: Option<String> = self
            .conn
            .lock()
            .map_err(ServiceError::storage)?
            .query_row(
                "SELECT payload FROM objects WHERE scope=?1 AND kind=?2 AND id=?3",
                params![scope, kind, key],
                |r| r.get(0),
            )
            .optional()
            .map_err(ServiceError::storage)?;
        serde_json::from_str(&raw.ok_or_else(|| {
            ServiceError::new(ErrorCode::NotFound, "store", format!("{kind} not found"))
        })?)
        .map_err(ServiceError::storage)
    }
    pub fn list<T: DeserializeOwned>(&self, scope: &str, kind: &str) -> Result<Vec<T>> {
        let conn = self.conn.lock().map_err(ServiceError::storage)?;
        let mut stmt = conn
            .prepare("SELECT payload FROM objects WHERE scope=?1 AND kind=?2 ORDER BY id")
            .map_err(ServiceError::storage)?;
        let rows = stmt
            .query_map(params![scope, kind], |r| r.get::<_, String>(0))
            .map_err(ServiceError::storage)?;
        rows.map(|r| {
            serde_json::from_str(&r.map_err(ServiceError::storage)?).map_err(ServiceError::storage)
        })
        .collect()
    }
    pub fn delete(&self, scope: &str, kind: &str, key: &str) -> Result<bool> {
        let deleted = self
            .conn
            .lock()
            .map_err(ServiceError::storage)?
            .execute(
                "DELETE FROM objects WHERE scope=?1 AND kind=?2 AND id=?3",
                params![scope, kind, key],
            )
            .map_err(ServiceError::storage)?;
        Ok(deleted > 0)
    }
    pub fn event(&self, run_id: &str, kind: &str, data: Value) -> Result<Event> {
        let conn = self.conn.lock().map_err(ServiceError::storage)?;
        let at = now_ms();
        conn.execute(
            "INSERT INTO events(run_id,kind,data,at) VALUES(?1,?2,?3,?4)",
            params![run_id, kind, data.to_string(), at],
        )
        .map_err(ServiceError::storage)?;
        Ok(Event {
            sequence: conn.last_insert_rowid(),
            run_id: run_id.into(),
            kind: kind.into(),
            data,
            at,
        })
    }
    pub fn events(&self, run_id: &str, after: i64) -> Result<Vec<Event>> {
        let conn = self.conn.lock().map_err(ServiceError::storage)?;
        let mut stmt=conn.prepare("SELECT sequence,kind,data,at FROM events WHERE run_id=?1 AND sequence>?2 ORDER BY sequence LIMIT 256").map_err(ServiceError::storage)?;
        let rows = stmt
            .query_map(params![run_id, after], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .map_err(ServiceError::storage)?;
        rows.map(|row| {
            let (sequence, kind, data, at) = row.map_err(ServiceError::storage)?;
            Ok(Event {
                sequence,
                kind,
                data: serde_json::from_str(&data).map_err(ServiceError::storage)?,
                at,
                run_id: run_id.into(),
            })
        })
        .collect()
    }
    /// Atomically reserve an idempotency key and persist the accepted run.
    pub fn accept(&self, run: &Run) -> Result<Option<String>> {
        // Publish the active run before maintenance can collect its inputs.
        let _lease = self.blob_lease()?;
        let mut conn = self.conn.lock().map_err(ServiceError::storage)?;
        let tx = conn.transaction().map_err(ServiceError::storage)?;
        let fingerprint = digest(
            serde_json::to_string(&run.request)
                .map_err(ServiceError::storage)?
                .as_bytes(),
        );
        if let Some(key) = &run.request.idempotency_key {
            let previous: Option<(String, String)> = tx
                .query_row(
                    "SELECT digest,run_id FROM idempotency WHERE scope=?1 AND key=?2",
                    params![run.request.account_id, key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(ServiceError::storage)?;
            if let Some((hash, id)) = previous {
                if hash != fingerprint {
                    return Err(ServiceError::invalid(
                        "idempotency key reused with different input",
                    ));
                }
                return Ok(Some(id));
            }
            tx.execute(
                "INSERT INTO idempotency VALUES(?1,?2,?3,?4)",
                params![run.request.account_id, key, fingerprint, run.id],
            )
            .map_err(ServiceError::storage)?;
        }
        tx.execute(
            "INSERT INTO objects VALUES('local','run',?1,?2)",
            params![
                run.id,
                serde_json::to_string(run).map_err(ServiceError::storage)?
            ],
        )
        .map_err(ServiceError::storage)?;
        tx.commit().map_err(ServiceError::storage)?;
        Ok(None)
    }
    pub fn cache_put(&self, scope: &str, key: &str, value: &Value, ttl_secs: u64) -> Result<()> {
        self.conn.lock().map_err(ServiceError::storage)?.execute("INSERT INTO cache VALUES(?1,?2,?3,?4) ON CONFLICT(scope,key) DO UPDATE SET value=excluded.value,expires=excluded.expires",params![scope,key,value.to_string(),now_ms()+ttl_secs.min(86400*30) as i64*1000]).map_err(ServiceError::storage)?;
        Ok(())
    }
    pub fn cache_get(&self, scope: &str, key: &str) -> Result<Option<Value>> {
        let raw: Option<String> = self
            .conn
            .lock()
            .map_err(ServiceError::storage)?
            .query_row(
                "SELECT value FROM cache WHERE scope=?1 AND key=?2 AND expires>?3",
                params![scope, key, now_ms()],
                |r| r.get(0),
            )
            .optional()
            .map_err(ServiceError::storage)?;
        raw.map(|s| serde_json::from_str(&s).map_err(ServiceError::storage))
            .transpose()
    }
    fn blob_tmp_root(&self) -> PathBuf {
        self.root.join("blobs").join(".tmp")
    }
    fn ensure_blob_tmp_root_at(root: &Path) -> Result<PathBuf> {
        let blobs_dir = root.join("blobs");
        fs::create_dir_all(&blobs_dir).map_err(ServiceError::storage)?;
        let blobs_meta = fs::symlink_metadata(&blobs_dir).map_err(ServiceError::storage)?;
        if !blobs_meta.is_dir() || blobs_meta.file_type().is_symlink() {
            return Err(ServiceError::invalid(
                "blob directory must not be a symlink or a file",
            ));
        }
        let tmp_root = blobs_dir.join(".tmp");
        let validate_tmp_dir = || -> Result<()> {
            let meta = fs::symlink_metadata(&tmp_root).map_err(ServiceError::storage)?;
            if meta.is_dir() && !meta.file_type().is_symlink() {
                Ok(())
            } else {
                Err(ServiceError::invalid(
                    "blob staging directory must not be a symlink or a file",
                ))
            }
        };
        match fs::symlink_metadata(&tmp_root) {
            Ok(_) => validate_tmp_dir()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&tmp_root) {
                    Ok(()) => {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            fs::set_permissions(&tmp_root, fs::Permissions::from_mode(0o700))
                                .map_err(ServiceError::storage)?;
                        }
                    }
                    Err(create_err) if create_err.kind() == std::io::ErrorKind::AlreadyExists => {
                        validate_tmp_dir()?;
                    }
                    Err(create_err) => return Err(ServiceError::storage(create_err)),
                }
            }
            Err(error) => return Err(ServiceError::storage(error)),
        }
        Ok(tmp_root)
    }
    fn ensure_blob_tmp_root(&self) -> Result<PathBuf> {
        Self::ensure_blob_tmp_root_at(&self.root)
    }
    fn clean_abandoned_blob_staging_locked(&self, _lease: &fs::File) -> Result<()> {
        use fs2::FileExt;
        let tmp_root = self.blob_tmp_root();
        for entry in cache::entries(&tmp_root)? {
            let path = entry.path();
            let Some(meta) = cache::metadata(&path)? else {
                continue;
            };
            if !meta.is_dir() || meta.file_type().is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !cache::owned_name(&name, "tmp_") {
                continue;
            }
            let lock_path = path.join(".lease");
            let held_lock = match fs::symlink_metadata(&lock_path) {
                Ok(lock_meta) if lock_meta.is_file() && !lock_meta.file_type().is_symlink() => {
                    let file = match fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&lock_path)
                    {
                        Ok(file) => file,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(ServiceError::storage(error)),
                    };
                    if file.try_lock_exclusive().is_err() {
                        continue;
                    }
                    Some(file)
                }
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(ServiceError::storage(error)),
            };
            // Remove payload before unlocking/dropping the lease handle (required on Windows)
            // while still holding `_lease` (`blobs.lock`).
            let _ = fs::remove_file(path.join("data"));
            drop(held_lock);
            match fs::remove_dir_all(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(ServiceError::storage(error)),
            }
        }
        Ok(())
    }
    fn blob_usage_excluding(&self, excluded_staging: Option<&OsStr>) -> Result<u64> {
        let mut total = 0u64;
        for entry in cache::entries(&self.root.join("blobs"))? {
            let Some(meta) = cache::metadata(&entry.path())? else {
                continue;
            };
            if meta.is_file() && !meta.file_type().is_symlink() {
                total += meta.len();
            }
        }
        for entry in cache::entries(&self.blob_tmp_root())? {
            if excluded_staging.is_some_and(|name| entry.file_name() == name) {
                continue;
            }
            let Some(meta) = cache::metadata(&entry.path())? else {
                continue;
            };
            if !meta.is_dir() || meta.file_type().is_symlink() {
                continue;
            }
            let mut newest = std::time::UNIX_EPOCH;
            if let Some(bytes) = cache::directory_size(&entry.path(), &mut newest)? {
                total += bytes;
            }
        }
        Ok(total)
    }
    fn reserve_blob_space(
        &self,
        expected_size: u64,
        dest: Option<&Path>,
        excluding: Option<&OsStr>,
    ) -> Result<fs::File> {
        let mut lease = self.blob_lease()?;
        self.clean_abandoned_blob_staging_locked(&lease)?;
        if dest.is_some_and(Path::exists) {
            return Ok(lease);
        }
        let mut used = self.blob_usage_excluding(excluding)?;
        if used + expected_size >= cache::BLOB_LIMIT * 8 / 10 {
            drop(lease);
            self.clean_cache_under_pressure()?;
            lease = self.blob_lease()?;
            self.clean_abandoned_blob_staging_locked(&lease)?;
            if dest.is_some_and(Path::exists) {
                return Ok(lease);
            }
            used = self.blob_usage_excluding(excluding)?;
        }
        if used + expected_size > cache::BLOB_LIMIT {
            return Err(ServiceError::invalid(format!(
                "blob budget (1 GiB) reached; {} more bytes required; run /cache or anticrumb cache prune; referenced files are retained",
                used + expected_size - cache::BLOB_LIMIT
            )));
        }
        Ok(lease)
    }
    pub fn artifact_writer(
        &self,
        scope: &str,
        name: &str,
        mime: &str,
        expected_size: Option<u64>,
        expected_sha256: Option<&str>,
        origin: &str,
    ) -> Result<ArtifactWriter> {
        use fs2::FileExt;
        if let Some(size) = expected_size {
            if size > MAX_ARTIFACT_BYTES {
                return Err(ServiceError::invalid(format!(
                    "artifact exceeds {MAX_ARTIFACT_LABEL}"
                )));
            }
        }
        if let Some(sha) = expected_sha256 {
            if !is_valid_sha256_hex(sha) {
                return Err(ServiceError::invalid("invalid artifact SHA-256"));
            }
        }
        let normalized_name = Path::new(name)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if !is_valid_artifact_name(&normalized_name) {
            return Err(ServiceError::invalid("invalid artifact name"));
        }
        let expected_dest = expected_sha256.map(|sha| self.root.join("blobs").join(sha));
        let store_lease = if let Some(size) = expected_size {
            self.reserve_blob_space(size, expected_dest.as_deref(), None)?
        } else {
            let lease = self.blob_lease()?;
            self.clean_abandoned_blob_staging_locked(&lease)?;
            lease
        };
        let tmp_root = self.ensure_blob_tmp_root()?;
        let staging_dir = tmp_root.join(id("tmp"));
        fs::create_dir(&staging_dir).map_err(ServiceError::storage)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&staging_dir, fs::Permissions::from_mode(0o700))
                .map_err(ServiceError::storage)?;
        }
        let data_path = staging_dir.join("data");
        let mut staging = TempStagingGuard {
            dir: staging_dir.clone(),
            data_path: data_path.clone(),
            lease: None,
            armed: true,
        };
        let mut lease_opts = fs::OpenOptions::new();
        lease_opts.create_new(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            lease_opts.mode(0o600);
        }
        let lease_file = lease_opts
            .open(staging_dir.join(".lease"))
            .map_err(ServiceError::storage)?;
        lease_file.lock_exclusive().map_err(ServiceError::storage)?;
        staging.lease = Some(lease_file);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&data_path).map_err(ServiceError::storage)?;
        drop(store_lease);
        Ok(ArtifactWriter {
            store: self.clone(),
            scope: scope.into(),
            name: normalized_name,
            mime: mime.into(),
            origin: origin.into(),
            expected_size,
            expected_sha256: expected_sha256.map(str::to_owned),
            hasher: Sha256::new(),
            written: 0,
            writer: Some(BufWriter::with_capacity(64 * 1024, file)),
            staging,
        })
    }
    pub fn artifact(
        &self,
        scope: &str,
        name: &str,
        mime: &str,
        bytes: &[u8],
        origin: &str,
    ) -> Result<Artifact> {
        let size = bytes.len() as u64;
        if size > MAX_ARTIFACT_BYTES {
            return Err(ServiceError::invalid(format!(
                "artifact exceeds {MAX_ARTIFACT_LABEL}"
            )));
        }
        let mut writer = self.artifact_writer(scope, name, mime, Some(size), None, origin)?;
        writer.write_chunk(bytes)?;
        writer.finish()
    }
    pub fn artifact_path(&self, scope: &str, id: &str) -> Result<(Artifact, PathBuf)> {
        let art: Artifact = self.get(scope, "artifact", id)?;
        if !is_valid_sha256_hex(&art.hash) {
            return Err(ServiceError::invalid("invalid blob hash"));
        }
        let path = self.root.join("blobs").join(&art.hash);
        Ok((art, path))
    }
    pub fn open_artifact(&self, scope: &str, id: &str) -> Result<(Artifact, fs::File)> {
        let _lease = self.blob_lease()?;
        let (art, path) = self.artifact_path(scope, id)?;
        if art.size > MAX_ARTIFACT_BYTES {
            return Err(ServiceError::invalid(format!(
                "artifact exceeds {MAX_ARTIFACT_LABEL}"
            )));
        }
        let sym_meta = fs::symlink_metadata(&path).map_err(ServiceError::storage)?;
        if sym_meta.file_type().is_symlink() || !sym_meta.is_file() {
            return Err(ServiceError::new(
                ErrorCode::StorageFailure,
                "blob",
                "artifact integrity check failed",
            ));
        }
        let file = fs::File::open(&path).map_err(ServiceError::storage)?;
        let meta = file.metadata().map_err(ServiceError::storage)?;
        if !meta.is_file() || meta.len() != art.size {
            return Err(ServiceError::new(
                ErrorCode::StorageFailure,
                "blob",
                "artifact integrity check failed",
            ));
        }
        Ok((art, file))
    }
    fn export_open_artifact(art: Artifact, mut src: fs::File, dest: &Path) -> Result<Artifact> {
        let mut dst = BufWriter::with_capacity(
            64 * 1024,
            fs::File::create(dest).map_err(ServiceError::storage)?,
        );
        let result = (|| -> Result<Artifact> {
            let mut buf = vec![0u8; 256 * 1024];
            let mut hasher = Sha256::new();
            let mut total = 0u64;
            loop {
                let n = match src.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(ServiceError::storage(e)),
                };
                let Some(next) = total.checked_add(n as u64) else {
                    return Err(ServiceError::new(
                        ErrorCode::StorageFailure,
                        "blob",
                        "artifact integrity check failed",
                    ));
                };
                if next > art.size {
                    return Err(ServiceError::new(
                        ErrorCode::StorageFailure,
                        "blob",
                        "artifact integrity check failed",
                    ));
                }
                total = next;
                hasher.update(&buf[..n]);
                dst.write_all(&buf[..n]).map_err(ServiceError::storage)?;
            }
            dst.flush().map_err(ServiceError::storage)?;
            let mut actual_hash = String::with_capacity(64);
            for byte in hasher.finalize() {
                let _ = write!(&mut actual_hash, "{byte:02x}");
            }
            if total != art.size || actual_hash != art.hash {
                return Err(ServiceError::new(
                    ErrorCode::StorageFailure,
                    "blob",
                    "artifact integrity check failed",
                ));
            }
            Ok(art)
        })();
        drop(dst);
        if result.is_err() {
            let _ = fs::remove_file(dest);
        }
        result
    }
    pub fn export_artifact_to_path(&self, scope: &str, id: &str, dest: &Path) -> Result<Artifact> {
        let (art, src) = self.open_artifact(scope, id)?;
        Self::export_open_artifact(art, src, dest)
    }
    pub fn export_artifact_under(
        &self,
        scope: &str,
        id: &str,
        dir: &Path,
        prefix: &str,
    ) -> Result<PathBuf> {
        let (art, src) = self.open_artifact(scope, id)?;
        if !is_valid_artifact_name(&art.name) {
            return Err(ServiceError::invalid("invalid artifact name"));
        }
        let dest = dir.join(format!("{prefix}{}", art.name));
        Self::export_open_artifact(art, src, &dest)?;
        Ok(dest)
    }
    pub fn recover(&self) -> Result<usize> {
        {
            let lease = self.blob_lease()?;
            self.clean_abandoned_blob_staging_locked(&lease)?;
        }
        let mut recovered = 0;
        for mut run in self.list::<Run>("local", "run")? {
            if ["queued", "preparing", "submitted", "running"].contains(&run.state.as_str()) {
                let code = if ["submitted", "running"].contains(&run.state.as_str()) {
                    ErrorCode::SubmissionOutcomeUnknown
                } else {
                    ErrorCode::Cancelled
                };
                run.state = if code == ErrorCode::SubmissionOutcomeUnknown {
                    "outcome_unknown"
                } else {
                    "cancelled"
                }
                .into();
                run.error = Some(ServiceError::new(
                    code,
                    "recovery",
                    "host stopped; request was not replayed",
                ));
                run.updated_at = now_ms();
                self.put("local", "run", &run.id, &run)?;
                self.event(&run.id, "recovered", serde_json::json!({"state":run.state}))?;
                recovered += 1;
            }
        }
        for mut session in self.list::<Session>("local", "session")? {
            if session.state == "running" {
                session.state = "outcome_unknown".into();
                self.put("local", "session", &session.id, &session)?;
            }
        }
        Ok(recovered)
    }
}

impl Store {
    pub fn accepted(&self, request: &TurnRequest) -> Result<Option<String>> {
        let Some(key) = &request.idempotency_key else {
            return Ok(None);
        };
        let conn = self.conn.lock().map_err(ServiceError::storage)?;
        let previous: Option<(String, String)> = conn
            .query_row(
                "SELECT digest,run_id FROM idempotency WHERE scope=?1 AND key=?2",
                params![request.account_id, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(ServiceError::storage)?;
        if let Some((hash, id)) = previous {
            if hash
                != digest(
                    serde_json::to_string(request)
                        .map_err(ServiceError::storage)?
                        .as_bytes(),
                )
            {
                return Err(ServiceError::invalid(
                    "idempotency key reused with different input",
                ));
            }
            Ok(Some(id))
        } else {
            Ok(None)
        }
    }
    pub fn summary_records(&self) -> Result<Vec<TurnSummaryRecord>> {
        self.summary_records_filtered(None, None, 100)
    }
    pub fn summary_records_filtered(
        &self,
        provider: Option<&str>,
        account: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TurnSummaryRecord>> {
        let conn = self.conn.lock().map_err(ServiceError::storage)?;
        let limit = limit.clamp(1, 500) as i64;
        let mut stmt = conn
            .prepare(
                "SELECT id,provider,account,model,state,input_chars,output_chars,updated_at
                 FROM run_stats
                 WHERE (?1 IS NULL OR provider=?1)
                   AND (?2 IS NULL OR account=?2)
                 ORDER BY updated_at DESC
                 LIMIT ?3",
            )
            .map_err(ServiceError::storage)?;
        let rows = stmt
            .query_map(params![provider, account, limit], |r| {
                Ok(TurnSummaryRecord {
                    id: r.get(0)?,
                    provider: r.get(1)?,
                    account_id: r.get(2)?,
                    model: r.get(3)?,
                    state: r.get(4)?,
                    input_chars: r.get::<_, i64>(5)?.max(0) as u64,
                    output_chars: r.get::<_, i64>(6)?.max(0) as u64,
                    updated_at: r.get(7)?,
                })
            })
            .map_err(ServiceError::storage)?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(ServiceError::storage)
    }
    pub fn summaries(&self) -> Result<Vec<Value>> {
        self.summary_records()?
            .into_iter()
            .map(|r| serde_json::to_value(r).map_err(ServiceError::storage))
            .collect()
    }
    pub fn usage_record(&self) -> Result<UsageSummary> {
        self.usage_record_for(None, None)
    }
    pub fn usage_record_for(
        &self,
        provider: Option<&str>,
        account: Option<&str>,
    ) -> Result<UsageSummary> {
        self.conn
            .lock()
            .map_err(ServiceError::storage)?
            .query_row(
                "SELECT
                   coalesce(sum(r.state='queued'),0),
                   coalesce(sum(r.state IN ('preparing','submitted','running')),0),
                   coalesce(sum(r.input_chars),0),
                   coalesce(sum(r.output_chars),0),
                   coalesce(sum(r.state='completed'),0),
                   coalesce(sum(t.sent_chars),0),
                   coalesce(sum(t.reused),0)
                 FROM run_stats r
                 LEFT JOIN transfer_stats t ON t.run_id=r.id
                 WHERE (?1 IS NULL OR r.provider=?1)
                   AND (?2 IS NULL OR r.account=?2)",
                params![provider, account],
                |r| {
                    Ok(UsageSummary {
                        queued: r.get::<_, i64>(0)?.max(0) as u64,
                        running: r.get::<_, i64>(1)?.max(0) as u64,
                        input_chars: r.get::<_, i64>(2)?.max(0) as u64,
                        output_chars: r.get::<_, i64>(3)?.max(0) as u64,
                        completed: r.get::<_, i64>(4)?.max(0) as u64,
                        submitted_chars: r.get::<_, i64>(5)?.max(0) as u64,
                        reused_turns: r.get::<_, i64>(6)?.max(0) as u64,
                        input_tokens: None,
                        output_tokens: None,
                        tokens: None,
                    })
                },
            )
            .map_err(ServiceError::storage)
    }
    pub fn usage_by_provider(&self) -> Result<Vec<ScopedUsageSummary>> {
        let conn = self.conn.lock().map_err(ServiceError::storage)?;
        let mut stmt = conn
            .prepare(
                "SELECT
                   r.provider,
                   coalesce(sum(r.state='queued'),0),
                   coalesce(sum(r.state IN ('preparing','submitted','running')),0),
                   coalesce(sum(r.input_chars),0),
                   coalesce(sum(r.output_chars),0),
                   coalesce(sum(r.state='completed'),0),
                   coalesce(sum(t.sent_chars),0),
                   coalesce(sum(t.reused),0)
                 FROM run_stats r
                 LEFT JOIN transfer_stats t ON t.run_id=r.id
                 GROUP BY r.provider
                 ORDER BY r.provider",
            )
            .map_err(ServiceError::storage)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ScopedUsageSummary {
                    provider: r.get(0)?,
                    account_id: None,
                    usage: UsageSummary {
                        queued: r.get::<_, i64>(1)?.max(0) as u64,
                        running: r.get::<_, i64>(2)?.max(0) as u64,
                        input_chars: r.get::<_, i64>(3)?.max(0) as u64,
                        output_chars: r.get::<_, i64>(4)?.max(0) as u64,
                        completed: r.get::<_, i64>(5)?.max(0) as u64,
                        submitted_chars: r.get::<_, i64>(6)?.max(0) as u64,
                        reused_turns: r.get::<_, i64>(7)?.max(0) as u64,
                        input_tokens: None,
                        output_tokens: None,
                        tokens: None,
                    },
                })
            })
            .map_err(ServiceError::storage)?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(ServiceError::storage)
    }
    pub fn usage_by_account(&self) -> Result<Vec<ScopedUsageSummary>> {
        let conn = self.conn.lock().map_err(ServiceError::storage)?;
        let mut stmt = conn
            .prepare(
                "SELECT
                   r.provider,
                   r.account,
                   coalesce(sum(r.state='queued'),0),
                   coalesce(sum(r.state IN ('preparing','submitted','running')),0),
                   coalesce(sum(r.input_chars),0),
                   coalesce(sum(r.output_chars),0),
                   coalesce(sum(r.state='completed'),0),
                   coalesce(sum(t.sent_chars),0),
                   coalesce(sum(t.reused),0)
                 FROM run_stats r
                 LEFT JOIN transfer_stats t ON t.run_id=r.id
                 GROUP BY r.provider, r.account
                 ORDER BY r.provider, r.account",
            )
            .map_err(ServiceError::storage)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ScopedUsageSummary {
                    provider: r.get(0)?,
                    account_id: Some(r.get(1)?),
                    usage: UsageSummary {
                        queued: r.get::<_, i64>(2)?.max(0) as u64,
                        running: r.get::<_, i64>(3)?.max(0) as u64,
                        input_chars: r.get::<_, i64>(4)?.max(0) as u64,
                        output_chars: r.get::<_, i64>(5)?.max(0) as u64,
                        completed: r.get::<_, i64>(6)?.max(0) as u64,
                        submitted_chars: r.get::<_, i64>(7)?.max(0) as u64,
                        reused_turns: r.get::<_, i64>(8)?.max(0) as u64,
                        input_tokens: None,
                        output_tokens: None,
                        tokens: None,
                    },
                })
            })
            .map_err(ServiceError::storage)?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(ServiceError::storage)
    }
    pub fn accounts_for_provider(&self, provider: &str) -> Result<Vec<Account>> {
        let conn = self.conn.lock().map_err(ServiceError::storage)?;
        let mut stmt = conn
            .prepare(
                "SELECT o.payload
                 FROM objects o
                 JOIN accounts a ON a.id=o.id
                 WHERE o.scope='local' AND o.kind='account' AND a.provider=?1
                 ORDER BY a.id",
            )
            .map_err(ServiceError::storage)?;
        let rows = stmt
            .query_map([provider], |r| r.get::<_, String>(0))
            .map_err(ServiceError::storage)?;
        rows.map(|r| {
            serde_json::from_str(&r.map_err(ServiceError::storage)?).map_err(ServiceError::storage)
        })
        .collect()
    }
    pub fn sessions_for_account(&self, account_id: &str) -> Result<Vec<Session>> {
        let conn = self.conn.lock().map_err(ServiceError::storage)?;
        let mut stmt = conn
            .prepare(
                "SELECT o.payload
                 FROM objects o
                 JOIN session_stats s ON s.id=o.id
                 WHERE o.scope='local' AND o.kind='session' AND s.account=?1
                 ORDER BY s.last_used DESC, o.id",
            )
            .map_err(ServiceError::storage)?;
        let rows = stmt
            .query_map([account_id], |r| r.get::<_, String>(0))
            .map_err(ServiceError::storage)?;
        rows.map(|r| {
            serde_json::from_str(&r.map_err(ServiceError::storage)?).map_err(ServiceError::storage)
        })
        .collect()
    }
    pub fn delete_account_state(&self, account_id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().map_err(ServiceError::storage)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(ServiceError::storage)?;
        let deleted = tx
            .execute(
                "DELETE FROM objects WHERE scope='local' AND kind='account' AND id=?1",
                [account_id],
            )
            .map_err(ServiceError::storage)?;
        tx.execute("DELETE FROM objects WHERE scope=?1", [account_id])
            .map_err(ServiceError::storage)?;
        tx.execute("DELETE FROM cache WHERE scope=?1", [account_id])
            .map_err(ServiceError::storage)?;
        tx.execute("DELETE FROM idempotency WHERE scope=?1", [account_id])
            .map_err(ServiceError::storage)?;
        tx.execute(
            "DELETE FROM objects WHERE scope='local' AND kind='session' AND id IN (SELECT id FROM session_stats WHERE account=?1)",
            [account_id],
        )
        .map_err(ServiceError::storage)?;
        tx.execute(
            "DELETE FROM events WHERE run_id IN (SELECT id FROM run_stats WHERE account=?1)",
            [account_id],
        )
        .map_err(ServiceError::storage)?;
        tx.execute(
            "DELETE FROM transfer_stats WHERE run_id IN (SELECT id FROM run_stats WHERE account=?1)",
            [account_id],
        )
        .map_err(ServiceError::storage)?;
        tx.execute(
            "DELETE FROM objects WHERE scope='local' AND kind='run' AND id IN (SELECT id FROM run_stats WHERE account=?1)",
            [account_id],
        )
        .map_err(ServiceError::storage)?;
        tx.commit().map_err(ServiceError::storage)?;
        Ok(deleted > 0)
    }
    pub fn usage(&self) -> Result<Value> {
        let s = self.usage_record()?;
        Ok(serde_json::json!({
            "queued": s.queued,
            "running": s.running,
            "input_chars": s.input_chars,
            "output_chars": s.output_chars,
            "completed": s.completed,
            "submitted_chars": s.submitted_chars,
            "reused_turns": s.reused_turns,
            "tokens": null
        }))
    }
    pub fn artifact_bytes(&self, scope: &str, id: &str) -> Result<(Artifact, Vec<u8>)> {
        let (art, file) = self.open_artifact(scope, id)?;
        let cap = usize::try_from(art.size).map_err(|_| {
            ServiceError::new(
                ErrorCode::StorageFailure,
                "blob",
                "artifact exceeds addressable size",
            )
        })?;
        let mut bytes = Vec::with_capacity(cap.min(256 * 1024));
        file.take(art.size.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(ServiceError::storage)?;
        if bytes.len() as u64 != art.size || digest(&bytes) != art.hash {
            return Err(ServiceError::new(
                ErrorCode::StorageFailure,
                "blob",
                "artifact integrity check failed",
            ));
        }
        Ok((art, bytes))
    }
    pub(crate) fn update_immediate<T: Default + Serialize + DeserializeOwned>(
        &self,
        scope: &str,
        kind: &str,
        key: &str,
        mutate: impl FnOnce(&mut T),
    ) -> Result<T> {
        let mut conn = self.conn.lock().map_err(ServiceError::storage)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(ServiceError::storage)?;
        let raw: Option<String> = tx
            .query_row(
                "SELECT payload FROM objects WHERE scope=?1 AND kind=?2 AND id=?3",
                params![scope, kind, key],
                |r| r.get(0),
            )
            .optional()
            .map_err(ServiceError::storage)?;
        let mut value: T = raw
            .map(|s| serde_json::from_str(&s).map_err(ServiceError::storage))
            .transpose()?
            .unwrap_or_default();
        mutate(&mut value);
        tx.execute(
            "INSERT INTO objects VALUES(?1,?2,?3,?4) ON CONFLICT(scope,kind,id) DO UPDATE SET payload=excluded.payload",
            params![scope, kind, key, serde_json::to_string(&value).map_err(ServiceError::storage)?],
        )
        .map_err(ServiceError::storage)?;
        tx.commit().map_err(ServiceError::storage)?;
        Ok(value)
    }
    pub(crate) fn delete_agent_turn(
        &self,
        id: &str,
        is_active_control: impl Fn(&str) -> bool,
    ) -> Result<()> {
        let mut conn = self.conn.lock().map_err(ServiceError::storage)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(ServiceError::storage)?;
        let raw: Option<String> = tx
            .query_row(
                "SELECT payload FROM objects WHERE scope='local' AND kind='agent' AND id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()
            .map_err(ServiceError::storage)?;
        let job: AgentRecord = serde_json::from_str(&raw.ok_or_else(|| {
            ServiceError::new(ErrorCode::NotFound, "agent", "saved conversation not found")
        })?)
        .map_err(ServiceError::storage)?;
        if is_active_control(id) || job.is_active() {
            return Err(ServiceError::invalid(
                "conversation is still running; interrupt it and wait for it to stop before deleting",
            ));
        }
        let children: Vec<(String, String)> = {
            let mut statement = tx
                .prepare("SELECT id,payload FROM objects WHERE scope='local' AND kind='agent' AND json_extract(payload,'$.request.parent_id')=?1")
                .map_err(ServiceError::storage)?;
            let rows = statement
                .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(ServiceError::storage)?;
            rows.collect::<std::result::Result<_, _>>()
                .map_err(ServiceError::storage)?
        };
        for (child_id, raw) in children {
            let mut child: AgentRecord =
                serde_json::from_str(&raw).map_err(ServiceError::storage)?;
            if is_active_control(&child_id) || child.is_active() {
                return Err(ServiceError::invalid(
                    "a follow-up to this conversation is still running; wait for it to stop before deleting",
                ));
            }
            if child.request.workspace != job.request.workspace
                || child.request.account_id != job.request.account_id
            {
                return Err(ServiceError::invalid(
                    "cannot delete a conversation referenced by another workspace or account",
                ));
            }
            child.request.parent_id = job.request.parent_id.clone();
            tx.execute(
                "UPDATE objects SET payload=?1 WHERE scope='local' AND kind='agent' AND id=?2",
                params![
                    serde_json::to_string(&child).map_err(ServiceError::storage)?,
                    child_id
                ],
            )
            .map_err(ServiceError::storage)?;
        }
        let ownership_events: Vec<(String, Value)> = {
            let mut statement = tx
                .prepare("SELECT kind,data FROM events WHERE run_id=?1 AND kind IN ('browser_turn','browser_session')")
                .map_err(ServiceError::storage)?;
            let rows = statement
                .query_map([id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(ServiceError::storage)?;
            let mut collected = Vec::new();
            for row in rows {
                let (kind, raw) = row.map_err(ServiceError::storage)?;
                let data: Value = serde_json::from_str(&raw).map_err(ServiceError::storage)?;
                collected.push((kind, data));
            }
            collected
        };
        let cleanup_job =
            crate::service::cleanup::build_job(id, &job.request.account_id, ownership_events);
        tx.execute(
            "INSERT INTO objects VALUES('local','chat_cleanup',?1,?2) ON CONFLICT(scope,kind,id) DO UPDATE SET payload=excluded.payload",
            params![
                id,
                serde_json::to_string(&cleanup_job).map_err(ServiceError::storage)?
            ],
        )
        .map_err(ServiceError::storage)?;
        tx.execute("DELETE FROM events WHERE run_id=?1", [id])
            .map_err(ServiceError::storage)?;
        tx.execute(
            "DELETE FROM objects WHERE scope='local' AND kind='agent' AND id=?1",
            [id],
        )
        .map_err(ServiceError::storage)?;
        tx.commit().map_err(ServiceError::storage)?;
        Ok(())
    }
    pub(crate) fn has_shared_browser_session(
        &self,
        session: &str,
        runs: &[String],
    ) -> Result<bool> {
        let conn = self.conn.lock().map_err(ServiceError::storage)?;
        let mut stmt = conn
            .prepare("SELECT e.kind,e.data FROM events e JOIN objects a ON a.scope='local' AND a.kind='agent' AND a.id=e.run_id WHERE e.kind IN ('browser_turn','browser_session')")
            .map_err(ServiceError::storage)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(ServiceError::storage)?;
        for row in rows {
            let (kind, raw) = row.map_err(ServiceError::storage)?;
            let data: Value = serde_json::from_str(&raw).map_err(ServiceError::storage)?;
            if kind == "browser_session" && data["session_id"] == session
                || kind == "browser_turn"
                    && data["run_id"]
                        .as_str()
                        .is_some_and(|id| runs.iter().any(|r| r == id))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
    pub(crate) fn sync_knowledge(
        &self,
        source: &str,
        documents: &[(String, String, String)],
    ) -> Result<usize> {
        let mut conn = self.conn.lock().map_err(ServiceError::storage)?;
        let tx = conn.transaction().map_err(ServiceError::storage)?;
        let existing: std::collections::HashSet<(String, String)> = {
            let mut statement = tx
                .prepare("SELECT DISTINCT path,hash FROM knowledge WHERE source=?1")
                .map_err(ServiceError::storage)?;
            let rows = statement
                .query_map([source], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(ServiceError::storage)?;
            rows.collect::<std::result::Result<_, _>>()
                .map_err(ServiceError::storage)?
        };
        let wanted: std::collections::HashSet<_> = documents
            .iter()
            .map(|(p, h, _)| (p.clone(), h.clone()))
            .collect();
        for (path, hash) in existing.difference(&wanted) {
            tx.execute(
                "DELETE FROM knowledge WHERE source=?1 AND path=?2 AND hash=?3",
                params![source, path, hash],
            )
            .map_err(ServiceError::storage)?;
        }
        for (path, hash, text) in documents {
            if existing.contains(&(path.clone(), hash.clone())) {
                continue;
            }
            let mut body = String::new();
            let mut start = 1;
            let mut count = 0;
            for (index, line) in text.lines().enumerate() {
                // Giant/minified lines are truncated explicitly, not allowed to consume the context budget.
                body.extend(line.chars().take(4000));
                body.push('\n');
                count += 1;
                if body.chars().count() >= 3000 || count >= 60 {
                    tx.execute(
                        "INSERT INTO knowledge(source,path,hash,line,body) VALUES(?1,?2,?3,?4,?5)",
                        params![source, path, hash, start, body],
                    )
                    .map_err(ServiceError::storage)?;
                    body.clear();
                    start = index + 2;
                    count = 0;
                }
            }
            if !body.is_empty() {
                tx.execute(
                    "INSERT INTO knowledge(source,path,hash,line,body) VALUES(?1,?2,?3,?4,?5)",
                    params![source, path, hash, start, body],
                )
                .map_err(ServiceError::storage)?;
            }
        }
        let chunks = tx
            .query_row(
                "SELECT count(*) FROM knowledge WHERE source=?1",
                [source],
                |r| r.get(0),
            )
            .map_err(ServiceError::storage)?;
        tx.commit().map_err(ServiceError::storage)?;
        Ok(chunks)
    }
    pub(crate) fn query_knowledge(
        &self,
        match_expr: &str,
        source: Option<&str>,
        limit: usize,
    ) -> Result<Vec<crate::knowledge::Excerpt>> {
        let conn = self.conn.lock().map_err(ServiceError::storage)?;
        let mut stmt = conn
            .prepare("SELECT source,path,hash,line,body FROM knowledge WHERE knowledge MATCH ?1 AND (?2 IS NULL OR source=?2) ORDER BY bm25(knowledge) LIMIT ?3")
            .map_err(ServiceError::storage)?;
        let rows = stmt
            .query_map(params![match_expr, source, limit.clamp(1, 20)], |r| {
                let source: String = r.get(0)?;
                let path: String = r.get(1)?;
                let line: usize = r.get(3)?;
                Ok(crate::knowledge::Excerpt {
                    citation: format!("[{source}:{path}:L{line}]"),
                    source,
                    path,
                    line,
                    hash: r.get(2)?,
                    text: r.get(4)?,
                })
            })
            .map_err(ServiceError::storage)?;
        let mut budget = 24000usize;
        let mut output = Vec::new();
        for row in rows {
            let mut row = row.map_err(ServiceError::storage)?;
            row.text = row.text.chars().take(budget).collect();
            budget = budget.saturating_sub(row.text.chars().count());
            output.push(row);
            if budget == 0 {
                break;
            }
        }
        Ok(output)
    }
    /// Explicit retention operation. Active runs/sessions are never purged.
    pub fn prune_summary(&self, days: u64) -> Result<PruneSummary> {
        let report = self.cache_report(cache::Options {
            days: if days == 0 {
                cache::DEFAULT_DAYS
            } else {
                u32::try_from(days).map_err(|_| ServiceError::invalid(cache::USAGE))?
            },
            prune: true,
            dry_run: false,
        })?;
        Ok(PruneSummary {
            expired_cache: report.expired_cache,
            runs: 0,
            artifacts: report.eligible_artifacts,
            blob_bytes_reclaimed: report.reclaimed_bytes,
        })
    }
    pub fn prune(&self, days: u64) -> Result<Value> {
        serde_json::to_value(self.prune_summary(days)?).map_err(ServiceError::storage)
    }
}

struct TempStagingGuard {
    dir: PathBuf,
    data_path: PathBuf,
    // Held exclusively while this writer is active so cache cleanup never deletes live staging.
    lease: Option<fs::File>,
    armed: bool,
}

impl TempStagingGuard {
    fn disarm_and_cleanup(&mut self) {
        if self.armed {
            self.armed = false;
            let _ = fs::remove_file(&self.data_path);
            drop(self.lease.take());
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

impl Drop for TempStagingGuard {
    fn drop(&mut self) {
        self.disarm_and_cleanup();
    }
}

pub struct ArtifactWriter {
    store: Store,
    scope: String,
    name: String,
    mime: String,
    origin: String,
    expected_size: Option<u64>,
    expected_sha256: Option<String>,
    hasher: Sha256,
    written: u64,
    // Declared before `staging` so buffered handles close before staging cleanup on drop.
    writer: Option<BufWriter<fs::File>>,
    staging: TempStagingGuard,
}

impl ArtifactWriter {
    pub fn write_chunk(&mut self, bytes: &[u8]) -> Result<()> {
        let next = self
            .written
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| {
                ServiceError::invalid(format!("artifact exceeds {MAX_ARTIFACT_LABEL}"))
            })?;
        if next > MAX_ARTIFACT_BYTES {
            return Err(ServiceError::invalid(format!(
                "artifact exceeds {MAX_ARTIFACT_LABEL}"
            )));
        }
        if let Some(expected) = self.expected_size {
            if next > expected {
                return Err(ServiceError::invalid(
                    "artifact byte count exceeds declared size",
                ));
            }
        }
        if !bytes.is_empty() {
            let writer = self
                .writer
                .as_mut()
                .ok_or_else(|| ServiceError::storage("artifact writer already closed"))?;
            writer.write_all(bytes).map_err(ServiceError::storage)?;
            self.hasher.update(bytes);
            self.written = next;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<Artifact> {
        if let Some(expected) = self.expected_size {
            if self.written != expected {
                return Err(ServiceError::invalid("artifact size mismatch"));
            }
        }
        let mut hash = String::with_capacity(64);
        for byte in self.hasher.clone().finalize() {
            let _ = write!(&mut hash, "{byte:02x}");
        }
        if let Some(expected) = &self.expected_sha256 {
            if hash != *expected {
                return Err(ServiceError::invalid("artifact SHA-256 mismatch"));
            }
        }
        let mut writer = self
            .writer
            .take()
            .ok_or_else(|| ServiceError::storage("artifact writer already closed"))?;
        writer.flush().map_err(ServiceError::storage)?;
        drop(writer);
        let dest = self.store.root.join("blobs").join(&hash);
        let staging_name = self.staging.dir.file_name().map(OsStr::to_os_string);
        let lease =
            self.store
                .reserve_blob_space(self.written, Some(&dest), staging_name.as_deref())?;
        if !dest.exists() {
            fs::rename(&self.staging.data_path, &dest).map_err(ServiceError::storage)?;
        }
        let artifact = Artifact {
            id: if self.origin == "input" {
                format!(
                    "artifact_{}",
                    digest(
                        serde_json::json!([self.scope, self.name, self.mime, hash])
                            .to_string()
                            .as_bytes()
                    )
                )
            } else {
                id("artifact")
            },
            scope: self.scope.clone(),
            hash,
            mime: self.mime.clone(),
            name: self.name.clone(),
            size: self.written,
            origin: self.origin.clone(),
            created_at: now_ms(),
        };
        let put_result = self
            .store
            .put(&self.scope, "artifact", &artifact.id, &artifact);
        self.staging.disarm_and_cleanup();
        drop(lease);
        put_result?;
        Ok(artifact)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_catalog_is_account_scoped_and_survives_restart_and_prune() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        assert_eq!(
            store.model_catalog("a").unwrap()["models"],
            serde_json::json!([])
        );
        let catalog = serde_json::json!({
            "checked_at": 1,
            "models": [{"mode_id":"new-model","sublabel":"Future model"}],
            "model_thinking": {"new-model":[{"level":"fourth","label":"Fourth"}]}
        });
        store.put("a", "catalog", "models", &catalog).unwrap();
        store.prune(0).unwrap();
        drop(store);
        let reopened = Store::open(root.path()).unwrap();
        assert_eq!(reopened.model_catalog("a").unwrap(), catalog);
        assert_eq!(
            reopened.model_catalog("b").unwrap()["models"],
            serde_json::json!([])
        );
    }
    #[test]
    fn scoped_artifacts_cache_and_recovery() {
        let root = tempfile::tempdir().unwrap();
        let s = Store::open(root.path()).unwrap();
        let a = s
            .artifact("a", "x.txt", "text/plain", b"secret", "test")
            .unwrap();
        assert!(s.artifact_path("b", &a.id).is_err());
        let upload = s
            .artifact("a", "upload.txt", "text/plain", b"file", "input")
            .unwrap();
        let repeated = s
            .artifact("a", "upload.txt", "text/plain", b"file", "input")
            .unwrap();
        let other = s
            .artifact("b", "upload.txt", "text/plain", b"file", "input")
            .unwrap();
        assert_eq!(upload.id, repeated.id);
        assert_ne!(upload.id, other.id);
        s.cache_put("a", "x", &Value::Bool(true), 60).unwrap();
        assert!(s.cache_get("b", "x").unwrap().is_none());
        let mut request = TurnRequest::text("hi");
        request.idempotency_key = Some("key".into());
        let run = Run {
            id: id("run"),
            provider: default_provider(),
            state: "submitted".into(),
            request,
            result: None,
            error: None,
            created_at: now_ms(),
            updated_at: now_ms(),
        };
        assert!(s.accept(&run).unwrap().is_none());
        assert_eq!(s.accept(&run).unwrap(), Some(run.id.clone()));
        assert_eq!(s.recover().unwrap(), 1);
        assert_eq!(
            s.get::<Run>("local", "run", &run.id).unwrap().state,
            "outcome_unknown"
        );
        assert_eq!(s.recover().unwrap(), 0);
    }
    #[test]
    fn corrupted_artifact_is_rejected_before_upload_or_download() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let artifact = store
            .artifact("default", "a.txt", "text/plain", b"original", "test")
            .unwrap();
        let (_, path) = store.artifact_path("default", &artifact.id).unwrap();
        std::fs::write(path, b"modified").unwrap();
        assert!(store.artifact_bytes("default", &artifact.id).is_err());
        let export = root.path().join("export.txt");
        assert!(store
            .export_artifact_to_path("default", &artifact.id, &export)
            .is_err());
        assert!(!export.exists());
    }
    #[test]
    fn artifact_writer_streams_verifies_and_cleans_temp_files() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let payload = vec![0x5a_u8; 300 * 1024];
        let hash = digest(&payload);
        let mut writer = store
            .artifact_writer(
                "default",
                "chunked.bin",
                "application/octet-stream",
                Some(payload.len() as u64),
                Some(&hash),
                "input",
            )
            .unwrap();
        for chunk in payload.chunks(64 * 1024) {
            writer.write_chunk(chunk).unwrap();
        }
        let art = writer.finish().unwrap();
        assert_eq!(art.size, payload.len() as u64);
        assert_eq!(art.hash, hash);
        let export = root.path().join("exported.bin");
        store
            .export_artifact_to_path("default", &art.id, &export)
            .unwrap();
        assert_eq!(std::fs::read(&export).unwrap(), payload);

        // Aborted writer removes its temporary file immediately on drop.
        let mut aborted = store
            .artifact_writer(
                "default",
                "aborted.bin",
                "application/octet-stream",
                Some(128),
                None,
                "input",
            )
            .unwrap();
        aborted.write_chunk(&[1, 2, 3, 4]).unwrap();
        drop(aborted);
        let leftovers: Vec<_> = std::fs::read_dir(root.path().join("blobs"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("tmp_"))
            .collect();
        assert!(leftovers.is_empty());
        let staging_leftovers: Vec<_> = std::fs::read_dir(root.path().join("blobs").join(".tmp"))
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(staging_leftovers.is_empty());

        // Size and hash mismatches reject without committing a blob.
        let truncated = store
            .artifact_writer(
                "default",
                "short.bin",
                "application/octet-stream",
                Some(10),
                None,
                "input",
            )
            .unwrap();
        assert!(truncated.finish().is_err());

        let mut bad_hash = store
            .artifact_writer(
                "default",
                "badhash.bin",
                "application/octet-stream",
                Some(4),
                Some(&"00".repeat(32)),
                "input",
            )
            .unwrap();
        bad_hash.write_chunk(b"1234").unwrap();
        assert!(bad_hash.finish().is_err());

        // Oversized expectation above 100 MiB is rejected before allocating temp storage.
        assert!(store
            .artifact_writer(
                "default",
                "too-big.bin",
                "application/octet-stream",
                Some(MAX_ARTIFACT_BYTES + 1),
                None,
                "input",
            )
            .is_err());

        // Stored metadata above MAX_ARTIFACT_BYTES or with invalid name is rejected on read/export.
        let mut oversized_meta = art.clone();
        oversized_meta.size = MAX_ARTIFACT_BYTES + 1;
        store
            .put("default", "artifact", "oversized-id", &oversized_meta)
            .unwrap();
        assert!(store.open_artifact("default", "oversized-id").is_err());
        assert!(store.artifact_bytes("default", "oversized-id").is_err());
        assert!(store
            .export_artifact_to_path("default", "oversized-id", &root.path().join("nope.bin"))
            .is_err());

        let mut bad_name_meta = art.clone();
        bad_name_meta.name = "../escape.bin".into();
        store
            .put("default", "artifact", "bad-name-id", &bad_name_meta)
            .unwrap();
        let export_dir = root.path().join("export-under");
        std::fs::create_dir_all(&export_dir).unwrap();
        let err = store
            .export_artifact_under("default", "bad-name-id", &export_dir, "0-")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn blob_staging_protects_live_uploads_and_reclaims_crash_leftovers() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();

        // Active upload holds `.lease` under `blobs/.tmp/tmp_*` and survives concurrent
        // pressure cleanup and recover().
        let mut live = store
            .artifact_writer(
                "default",
                "live.bin",
                "application/octet-stream",
                Some(12),
                None,
                "input",
            )
            .unwrap();
        live.write_chunk(b"hello ").unwrap();

        let abandoned = root.path().join("blobs").join(".tmp").join(id("tmp"));
        std::fs::create_dir_all(&abandoned).unwrap();
        std::fs::write(abandoned.join(".lease"), b"").unwrap();
        let abandoned_data = std::fs::File::create(abandoned.join("data")).unwrap();
        abandoned_data.set_len(cache::BLOB_LIMIT).unwrap();
        drop(abandoned_data);

        // Even while a job is active (busy), starting another writer reclaims the unlocked
        // crash leftover without touching the live writer.
        store
            .put(
                "local",
                "agent",
                "active",
                &serde_json::json!({"state":"running"}),
            )
            .unwrap();
        let next = store
            .artifact("default", "other.txt", "text/plain", b"ok", "input")
            .unwrap();
        assert_eq!(next.size, 2);
        assert!(!abandoned.exists());

        live.write_chunk(b"world!").unwrap();
        let committed = live.finish().unwrap();
        assert_eq!(
            store.artifact_bytes("default", &committed.id).unwrap().1,
            b"hello world!"
        );

        // recover() also reclaims unlocked staging leftovers immediately.
        let orphan = root.path().join("blobs").join(".tmp").join(id("tmp"));
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join(".lease"), b"").unwrap();
        std::fs::write(orphan.join("data"), b"stale").unwrap();
        store.recover().unwrap();
        assert!(!orphan.exists());

        // Direct/internal callers of `artifact_writer` and `artifact` also validate normalized names.
        for bad in [
            "",
            ".",
            "..",
            "nested/..",
            "bad:name",
            "bad\\name",
            "bad\0name",
        ] {
            assert!(
                matches!(
                    store.artifact("default", bad, "text/plain", b"x", "input"),
                    Err(err) if err.code == ErrorCode::InvalidRequest
                ),
                "expected invalid artifact name for {bad:?}"
            );
        }
        let from_path = store
            .artifact(
                "default",
                "/tmp/dir/clean.txt",
                "text/plain",
                b"ok",
                "input",
            )
            .unwrap();
        assert_eq!(from_path.name, "clean.txt");
    }

    #[test]
    fn store_open_is_race_safe_and_rejects_symlink_staging_root() {
        let root = tempfile::tempdir().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = root.path().to_path_buf();
                let b = barrier.clone();
                std::thread::spawn(move || {
                    b.wait();
                    Store::open(&path).unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let store = Store::open(root.path()).unwrap();
        assert_eq!(store.blob_usage_excluding(None).unwrap(), 0);

        #[cfg(unix)]
        {
            let link_root = tempfile::tempdir().unwrap();
            let target = link_root.path().join("outside");
            std::fs::create_dir_all(&target).unwrap();
            std::fs::create_dir_all(link_root.path().join("blobs")).unwrap();
            std::os::unix::fs::symlink(&target, link_root.path().join("blobs").join(".tmp"))
                .unwrap();
            let err = match Store::open(link_root.path()) {
                Ok(_) => panic!("expected symlink staging root to fail"),
                Err(err) => err,
            };
            assert_eq!(err.code, ErrorCode::InvalidRequest);
        }
    }
    #[test]
    fn multi_provider_multi_account_stats_accounts_and_sessions() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();

        let claude_acct = Account {
            id: "claude-work".into(),
            provider: "claude-web".into(),
            profile: root.path().join("accounts/claude-work/profile"),
            subprofile: None,
            port: 9223,
            headless: true,
            state: "ready".into(),
        };
        let chatgpt_acct = Account {
            id: "chatgpt-personal".into(),
            provider: "chatgpt-web".into(),
            profile: root.path().join("accounts/chatgpt-personal/profile"),
            subprofile: None,
            port: 9224,
            headless: true,
            state: "ready".into(),
        };
        store
            .put("local", "account", &claude_acct.id, &claude_acct)
            .unwrap();
        store
            .put("local", "account", &chatgpt_acct.id, &chatgpt_acct)
            .unwrap();

        let mut req_claude = TurnRequest::text("hello claude");
        req_claude.account_id = claude_acct.id.clone();
        req_claude.model = Some("claude-sonnet-4".into());
        let run_claude = Run {
            id: "run_claude_1".into(),
            provider: "claude-web".into(),
            state: "completed".into(),
            request: req_claude,
            result: Some(TurnResult {
                session_id: "sess_c1".into(),
                text: "hi from claude".into(),
                tool_calls: vec![],
                artifacts: vec![],
                model: Some("claude-sonnet-4".into()),
                elapsed_ms: 120,
            }),
            error: None,
            created_at: 100,
            updated_at: 200,
        };
        store
            .put("local", "run", &run_claude.id, &run_claude)
            .unwrap();
        store
            .event(
                &run_claude.id,
                "submitted",
                serde_json::json!({"chars": 12}),
            )
            .unwrap();

        let mut req_gpt = TurnRequest::text("hello gpt");
        req_gpt.account_id = chatgpt_acct.id.clone();
        req_gpt.model = Some("gpt-4o".into());
        let run_gpt = Run {
            id: "run_gpt_1".into(),
            provider: "chatgpt-web".into(),
            state: "completed".into(),
            request: req_gpt,
            result: Some(TurnResult {
                session_id: "sess_g1".into(),
                text: "hi from gpt".into(),
                tool_calls: vec![],
                artifacts: vec![],
                model: Some("gpt-4o".into()),
                elapsed_ms: 90,
            }),
            error: None,
            created_at: 150,
            updated_at: 300,
        };
        store.put("local", "run", &run_gpt.id, &run_gpt).unwrap();
        store
            .event(&run_gpt.id, "submitted", serde_json::json!({"chars": 9}))
            .unwrap();
        store
            .event(&run_gpt.id, "cache_hit", serde_json::json!({}))
            .unwrap();

        let sess_c = Session {
            id: "sess_c1".into(),
            provider: "claude-web".into(),
            account_id: claude_acct.id.clone(),
            mode: SessionMode::Chat,
            target_id: None,
            remote_url: None,
            remote_session_id: None,
            history: vec![],
            state: "ready".into(),
            model: Some("claude-sonnet-4".into()),
            thinking: None,
            tool_digest: String::new(),
            remote_turns: 1,
            last_used: 200,
            reuse_scope: None,
        };
        let sess_g = Session {
            id: "sess_g1".into(),
            provider: "chatgpt-web".into(),
            account_id: chatgpt_acct.id.clone(),
            mode: SessionMode::Chat,
            target_id: None,
            remote_url: None,
            remote_session_id: None,
            history: vec![],
            state: "ready".into(),
            model: Some("gpt-4o".into()),
            thinking: None,
            tool_digest: String::new(),
            remote_turns: 1,
            last_used: 300,
            reuse_scope: None,
        };
        store.put("local", "session", &sess_c.id, &sess_c).unwrap();
        store.put("local", "session", &sess_g.id, &sess_g).unwrap();

        let claude_accts = store.accounts_for_provider("claude-web").unwrap();
        assert_eq!(claude_accts.len(), 1);
        assert_eq!(claude_accts[0].id, "claude-work");

        let by_provider = store.usage_by_provider().unwrap();
        assert_eq!(by_provider.len(), 2);
        assert_eq!(by_provider[0].provider, "chatgpt-web");
        assert_eq!(by_provider[0].usage.completed, 1);
        assert_eq!(by_provider[0].usage.submitted_chars, 9);
        assert_eq!(by_provider[0].usage.reused_turns, 1);
        assert_eq!(by_provider[1].provider, "claude-web");
        assert_eq!(by_provider[1].usage.submitted_chars, 12);
        assert_eq!(by_provider[1].usage.reused_turns, 0);

        let claude_summaries = store
            .summary_records_filtered(Some("claude-web"), None, 10)
            .unwrap();
        assert_eq!(claude_summaries.len(), 1);
        assert_eq!(
            claude_summaries[0].model.as_deref(),
            Some("claude-sonnet-4")
        );

        let c_sessions = store.sessions_for_account("claude-work").unwrap();
        assert_eq!(c_sessions.len(), 1);
        assert_eq!(c_sessions[0].provider, "claude-web");

        assert!(store.delete_account_state("claude-work").unwrap());
        assert!(store
            .sessions_for_account("claude-work")
            .unwrap()
            .is_empty());
        assert_eq!(store.usage_by_provider().unwrap().len(), 1);
    }
}
