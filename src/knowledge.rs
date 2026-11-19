//! Local, bounded source retrieval. Notebook URLs are provenance, never authentication handles.
use crate::{domain::*, store::Store};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
#[derive(Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub path: PathBuf,
    pub notebook_url: Option<String>,
    pub files: usize,
    pub chunks: usize,
    pub updated_at: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Excerpt {
    pub source: String,
    pub path: String,
    pub hash: String,
    pub line: usize,
    pub text: String,
    pub citation: String,
}
const MAX_SOURCE_FILES: usize = 20_000;

fn is_excluded_name(name: &str) -> bool {
    matches!(name, ".git" | "node_modules" | "target" | ".anticrumb")
        || name.starts_with(".env")
        || name.ends_with(".pem")
        || name.ends_with(".key")
}

fn walk_files_fallback(root: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries_iter = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if dir == root => return Err(ServiceError::storage(err)),
            Err(_) => continue,
        };
        let mut entries: Vec<_> = entries_iter.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        let mut subdirs = Vec::new();
        for entry in entries {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            if is_excluded_name(name) {
                continue;
            }
            if ft.is_dir() {
                subdirs.push(entry.path());
            } else if ft.is_file() {
                paths.push(entry.path());
                if paths.len() > MAX_SOURCE_FILES {
                    return Err(ServiceError::invalid(
                        "source exceeds 20000 files; select a smaller directory",
                    ));
                }
            }
        }
        for sub in subdirs.into_iter().rev() {
            stack.push(sub);
        }
    }
    Ok(paths)
}

pub fn files(root: &Path) -> Result<Vec<PathBuf>> {
    if root.is_file() {
        return Ok(vec![root.to_owned()]);
    }
    match std::process::Command::new("rg")
        .args([
            "--files",
            "--hidden",
            "-0",
            "-g",
            "!.git",
            "-g",
            "!.env*",
            "-g",
            "!*.pem",
            "-g",
            "!*.key",
            "-g",
            "!node_modules",
            "-g",
            "!target",
            "-g",
            "!.anticrumb",
        ])
        .current_dir(root)
        .output()
    {
        Ok(output) => {
            if !output.status.success() && output.status.code() != Some(1) {
                return Err(ServiceError::invalid("rg could not enumerate source"));
            }
            let paths = output
                .stdout
                .split(|b| *b == 0)
                .filter(|b| !b.is_empty())
                .filter_map(|b| std::str::from_utf8(b).ok())
                .take(MAX_SOURCE_FILES + 1)
                .map(|p| root.join(p))
                .collect::<Vec<_>>();
            if paths.len() > MAX_SOURCE_FILES {
                return Err(ServiceError::invalid(
                    "source exceeds 20000 files; select a smaller directory",
                ));
            }
            Ok(paths)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => walk_files_fallback(root),
        Err(err) => Err(ServiceError::storage(err)),
    }
}
pub fn add(store: &Store, name: &str, path: &Path, url: Option<String>) -> Result<Source> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ServiceError::invalid(
            "source ID must contain letters, digits, - or _",
        ));
    }
    if let Some(url) = &url {
        let url = crate::url_lite::Url::parse(url).map_err(ServiceError::storage)?;
        if url.scheme() != "https"
            || !matches!(
                url.host_str(),
                Some("notebooklm.google.com" | "gemini.google.com")
            )
        {
            return Err(ServiceError::invalid(
                "expected a Gemini or NotebookLM HTTPS URL",
            ));
        }
    }
    let source = Source {
        id: name.into(),
        path: fs::canonicalize(path).map_err(ServiceError::storage)?,
        notebook_url: url,
        files: 0,
        chunks: 0,
        updated_at: 0,
    };
    store.put("local", "source", name, &source)?;
    sync(store, name)
}
pub fn sync(store: &Store, name: &str) -> Result<Source> {
    let mut source: Source = store.get("local", "source", name)?;
    let root = if source.path.is_file() {
        source.path.parent().unwrap()
    } else {
        &source.path
    };
    let mut documents = Vec::new();
    let mut total = 0usize;
    for path in files(&source.path)? {
        let canonical = match fs::canonicalize(&path) {
            Ok(p) if p.starts_with(root) => p,
            _ => continue,
        };
        let metadata = fs::metadata(&canonical).map_err(ServiceError::storage)?;
        if metadata.len() > 2 * 1024 * 1024 || !metadata.is_file() {
            continue;
        }
        let bytes = fs::read(&canonical).map_err(ServiceError::storage)?;
        if bytes.contains(&0) {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        total += text.len();
        if total > 64 * 1024 * 1024 {
            return Err(ServiceError::invalid(
                "source text exceeds 64 MiB; select a smaller directory",
            ));
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        documents.push((relative, digest(text.as_bytes()), text));
    }
    source.files = documents.len();
    source.chunks = store.sync_knowledge(name, &documents)?;
    source.updated_at = now_ms();
    store.put("local", "source", name, &source)?;
    Ok(source)
}
pub fn query(
    store: &Store,
    query: &str,
    source: Option<&str>,
    limit: usize,
) -> Result<Vec<Excerpt>> {
    let terms: Vec<_> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .take(24)
        .map(|s| format!("\"{s}\"*"))
        .collect();
    if terms.is_empty() {
        return Err(ServiceError::invalid("query needs search terms"));
    }
    store.query_knowledge(&terms.join(" OR "), source, limit)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn index_updates_and_cites() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.rs"), "fn lexical_cache() {}\n").unwrap();
        let store = Store::open(&temp.path().join("state")).unwrap();
        add(&store, "repo", &root, None).unwrap();
        let hit = query(&store, "lexical_cache", None, 5).unwrap();
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].line, 1);
        fs::write(root.join("a.rs"), "fn replaced() {}\n").unwrap();
        sync(&store, "repo").unwrap();
        assert!(query(&store, "lexical_cache", None, 5).unwrap().is_empty());
        assert_eq!(query(&store, "replaced", Some("repo"), 5).unwrap().len(), 1);
    }

    #[test]
    fn fallback_enumeration_excludes_sensitive_and_ignored_entries() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::create_dir_all(root.join(".anticrumb")).unwrap();

        fs::write(root.join("src/lib.rs"), "pub fn ok() {}\n").unwrap();
        fs::write(root.join(".hidden"), "visible hidden file\n").unwrap();
        fs::write(root.join(".git/config"), "[core]\n").unwrap();
        fs::write(root.join(".env"), "SECRET=1\n").unwrap();
        fs::write(root.join(".env.local"), "SECRET=2\n").unwrap();
        fs::write(root.join("cert.pem"), "PEM\n").unwrap();
        fs::write(root.join("secret.key"), "KEY\n").unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "module\n").unwrap();
        fs::write(root.join("target/debug/bin"), "bin\n").unwrap();
        fs::write(root.join(".anticrumb/state"), "state\n").unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(temp.path().join("outside.txt"), root.join("link.txt")).unwrap();

        let listed = walk_files_fallback(&root).unwrap();
        let rel: Vec<_> = listed
            .iter()
            .map(|p| {
                p.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(rel, vec![".hidden", "src/lib.rs"]);

        let missing = walk_files_fallback(&temp.path().join("does-not-exist")).unwrap_err();
        assert_eq!(missing.code, ErrorCode::StorageFailure);
    }
}
