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
pub fn files(root: &Path) -> Result<Vec<PathBuf>> {
    if root.is_file() {
        return Ok(vec![root.to_owned()]);
    }
    let output = std::process::Command::new("rg")
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
        .map_err(ServiceError::storage)?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(ServiceError::invalid("rg could not enumerate source"));
    }
    let paths = output
        .stdout
        .split(|b| *b == 0)
        .filter(|b| !b.is_empty())
        .filter_map(|b| std::str::from_utf8(b).ok())
        .take(20001)
        .map(|p| root.join(p))
        .collect::<Vec<_>>();
    if paths.len() > 20000 {
        return Err(ServiceError::invalid(
            "source exceeds 20000 files; select a smaller directory",
        ));
    }
    Ok(paths)
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
}
