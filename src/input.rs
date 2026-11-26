//! File mentions are references to native uploads, never expanded file contents.
use crate::domain::{Result, ServiceError};
use std::{
    collections::HashMap,
    io::{Read, Write},
    ops::Range,
    path::{Path, PathBuf},
};

pub const MAX_FILES: usize = 30;
pub const MAX_FILE_BYTES: u64 = crate::domain::MAX_ARTIFACT_BYTES;

#[derive(Clone, Debug, Default)]
pub struct PreparedInput {
    pub text: String,
    pub files: Vec<PathBuf>,
    pub temp_files: Vec<PathBuf>,
    pub warnings: Vec<String>,
    pub resolved_mentions: Vec<String>,
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

pub(crate) fn expand_home_path(path: &str) -> Option<PathBuf> {
    path.strip_prefix("~/")
        .and_then(|tail| Some(home_dir()?.join(tail)))
}

pub(crate) fn is_mention_boundary(before: Option<char>) -> bool {
    before.is_none_or(char::is_whitespace)
}

pub(crate) fn parse_mention(
    tail: &str,
) -> std::result::Result<Option<(&str, usize)>, &'static str> {
    if !tail.starts_with('@') {
        return Ok(None);
    }
    let quote = tail[1..].chars().next().filter(|c| *c == '"' || *c == '\'');
    if let Some(quote) = quote {
        let inner = &tail[2..];
        let Some(end) = inner.find(quote) else {
            return Err("unclosed quoted attachment path");
        };
        Ok(Some((&inner[..end], 2 + end + 1)))
    } else {
        let end = tail[1..]
            .find(char::is_whitespace)
            .map(|n| 1 + n)
            .unwrap_or(tail.len());
        Ok(Some((&tail[1..end], end)))
    }
}

pub fn file(workspace: &Path, path: &Path) -> Result<PathBuf> {
    let path = if let Some(tail) = path.to_str().and_then(|s| s.strip_prefix("~/")) {
        home_dir().unwrap_or_default().join(tail)
    } else {
        workspace.join(path)
    };
    let path = path
        .canonicalize()
        .map_err(|e| ServiceError::invalid(format!("cannot attach {}: {e}", path.display())))?;
    let metadata = path.metadata().map_err(ServiceError::storage)?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return Err(ServiceError::invalid(format!(
            "attachments must be regular files of at most {}",
            crate::domain::MAX_ARTIFACT_LABEL
        )));
    }
    Ok(path)
}

fn run_git_patch(
    workspace: &Path,
    args: &[&str],
    filename: &str,
    operation: &str,
    empty_err: &str,
) -> Result<PathBuf> {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(workspace).args(args);
    let temp_dir =
        std::env::temp_dir().join(format!("anticrumb-git-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&temp_dir).map_err(ServiceError::storage)?;
    let patch_path = temp_dir.join(filename);

    let mut child = match cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            let _ = std::fs::remove_dir(&temp_dir);
            return Err(ServiceError::invalid(format!("git command failed: {e}")));
        }
    };

    let mut file = match std::fs::File::create(&patch_path) {
        Ok(f) => f,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir(&temp_dir);
            return Err(ServiceError::storage(e));
        }
    };

    let cleanup = || {
        let _ = std::fs::remove_file(&patch_path);
        let _ = std::fs::remove_dir(&temp_dir);
    };

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut buffer = [0u8; 64 * 1024];
    let mut total_bytes = 0u64;
    let mut has_non_whitespace = false;
    let mut exceeded = false;

    loop {
        let n = match stdout.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                drop(file);
                cleanup();
                return Err(ServiceError::invalid(format!(
                    "failed to read {operation}: {e}"
                )));
            }
        };
        total_bytes += n as u64;
        if total_bytes > MAX_FILE_BYTES {
            exceeded = true;
            let _ = child.kill();
            break;
        }
        if !has_non_whitespace && buffer[..n].iter().any(|b| !b.is_ascii_whitespace()) {
            has_non_whitespace = true;
        }
        if let Err(e) = file.write_all(&buffer[..n]) {
            let _ = child.kill();
            let _ = child.wait();
            drop(file);
            cleanup();
            return Err(ServiceError::storage(e));
        }
    }

    if exceeded {
        let _ = child.wait();
        drop(file);
        cleanup();
        return Err(ServiceError::invalid(format!(
            "attachments must be regular files of at most {}",
            crate::domain::MAX_ARTIFACT_LABEL
        )));
    }

    if let Err(e) = file.flush() {
        let _ = child.wait();
        drop(file);
        cleanup();
        return Err(ServiceError::storage(e));
    }
    drop(file);

    let mut stderr_bytes = Vec::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_end(&mut stderr_bytes);
    }
    let status = match child.wait() {
        Ok(status) => status,
        Err(e) => {
            cleanup();
            return Err(ServiceError::invalid(format!("git command failed: {e}")));
        }
    };

    if !status.success() {
        cleanup();
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        if stderr.to_ascii_lowercase().contains("not a git repository") {
            return Err(ServiceError::invalid("not a git repository"));
        }
        return Err(ServiceError::invalid(format!(
            "{operation} failed: {}",
            stderr.trim()
        )));
    }

    if total_bytes == 0 || !has_non_whitespace {
        cleanup();
        return Err(ServiceError::invalid(empty_err));
    }

    Ok(patch_path)
}

pub fn git_diff_file(workspace: &Path, staged: bool) -> Result<PathBuf> {
    if staged {
        run_git_patch(
            workspace,
            &["diff", "--staged", "--no-color", "--no-ext-diff"],
            "git-staged.patch",
            "git diff",
            "no staged git changes to attach",
        )
    } else {
        run_git_patch(
            workspace,
            &["diff", "--no-color", "--no-ext-diff"],
            "git-diff.patch",
            "git diff",
            "no unstaged git changes to attach",
        )
    }
}

fn resolve_git_commit(workspace: &Path, rev: &str) -> Result<String> {
    if rev.is_empty() {
        return Err(ServiceError::invalid("missing git commit id after git:"));
    }
    if rev.starts_with('-') || rev.chars().any(char::is_control) {
        return Err(ServiceError::invalid(format!("invalid git commit: {rev}")));
    }
    let output = std::process::Command::new("git")
        .current_dir(workspace)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ])
        .output()
        .map_err(|e| ServiceError::invalid(format!("git command failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.to_ascii_lowercase().contains("not a git repository") {
            return Err(ServiceError::invalid("not a git repository"));
        }
        return Err(ServiceError::invalid(format!("unknown git commit: {rev}")));
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if sha.is_empty() || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ServiceError::invalid(format!("unknown git commit: {rev}")));
    }
    Ok(sha)
}

fn git_commit_patch_for_sha(workspace: &Path, full_sha: &str) -> Result<PathBuf> {
    let short = &full_sha[..full_sha.len().min(12)];
    let filename = format!("git-{short}.patch");
    run_git_patch(
        workspace,
        &[
            "show",
            "-m",
            "--first-parent",
            "--no-color",
            "--no-ext-diff",
            full_sha,
        ],
        &filename,
        "git show",
        "git commit produced empty output",
    )
}

pub fn git_commit_file(workspace: &Path, rev: &str) -> Result<PathBuf> {
    let full_sha = resolve_git_commit(workspace, rev)?;
    git_commit_patch_for_sha(workspace, &full_sha)
}

pub use crate::tree_sitter_diff::{git_commit_expanded_file, git_diff_expanded_file};

pub fn cleanup_temp_files(temp_files: &[PathBuf]) {
    let temp_root = std::env::temp_dir();
    for path in temp_files {
        let _ = std::fs::remove_file(path);
        if let Some(parent) = path.parent() {
            if parent != temp_root {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum GitPatchTarget {
    Diff,
    DiffPlus,
    Staged,
    StagedPlus,
    Commit(String),
    CommitPlus(String),
}

fn resolve_git_mention(
    workspace: &Path,
    spec: &str,
    git_cache: &mut HashMap<GitPatchTarget, PathBuf>,
    temp_files: &mut Vec<PathBuf>,
    warnings: &mut Vec<String>,
) -> Result<PathBuf> {
    let target = match spec {
        "diff" => GitPatchTarget::Diff,
        "diff+" => GitPatchTarget::DiffPlus,
        "staged" => GitPatchTarget::Staged,
        "staged+" => GitPatchTarget::StagedPlus,
        other => {
            let mut resolved = None;
            if let Some(rev) = other.strip_suffix('+') {
                if !rev.is_empty() {
                    if let Ok(sha) = resolve_git_commit(workspace, rev) {
                        resolved = Some(GitPatchTarget::CommitPlus(sha));
                    }
                }
            }
            if let Some(target) = resolved {
                target
            } else {
                GitPatchTarget::Commit(resolve_git_commit(workspace, other)?)
            }
        }
    };
    if let Some(existing) = git_cache.get(&target) {
        return Ok(existing.clone());
    }
    let patch = match &target {
        GitPatchTarget::Diff => git_diff_file(workspace, false)?,
        GitPatchTarget::DiffPlus => {
            crate::tree_sitter_diff::git_diff_expanded_file(workspace, false, warnings)?
        }
        GitPatchTarget::Staged => git_diff_file(workspace, true)?,
        GitPatchTarget::StagedPlus => {
            crate::tree_sitter_diff::git_diff_expanded_file(workspace, true, warnings)?
        }
        GitPatchTarget::Commit(full_sha) => git_commit_patch_for_sha(workspace, full_sha)?,
        GitPatchTarget::CommitPlus(full_sha) => {
            crate::tree_sitter_diff::git_commit_expanded_file(workspace, full_sha, warnings)?
        }
    };
    git_cache.insert(target, patch.clone());
    if !temp_files.contains(&patch) {
        temp_files.push(patch.clone());
    }
    Ok(patch)
}

pub fn prepare(text: &str, workspace: &Path, explicit: &[PathBuf]) -> Result<PreparedInput> {
    prepare_with_literals(text, workspace, explicit, &[])
}

#[derive(Debug)]
struct PreparedBody {
    text: String,
    files: Vec<PathBuf>,
    warnings: Vec<String>,
    resolved_mentions: Vec<String>,
}

/// Pasted text is literal, so examples containing @ are not file uploads.
pub fn prepare_with_literals(
    text: &str,
    workspace: &Path,
    explicit: &[PathBuf],
    literals: &[Range<usize>],
) -> Result<PreparedInput> {
    let mut temp_files = Vec::new();
    match prepare_internal(text, workspace, explicit, literals, &mut temp_files) {
        Ok(body) => Ok(PreparedInput {
            text: body.text,
            files: body.files,
            temp_files,
            warnings: body.warnings,
            resolved_mentions: body.resolved_mentions,
        }),
        Err(e) => {
            cleanup_temp_files(&temp_files);
            Err(e)
        }
    }
}

fn format_rel_mention_path(rel: &Path) -> String {
    let mut parts = Vec::new();
    for comp in rel.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(seg) => parts.push(seg.to_string_lossy().into_owned()),
            std::path::Component::ParentDir => parts.push("..".to_owned()),
            _ => return rel.display().to_string(),
        }
    }
    parts.join("/")
}

fn normalize_resolved_file_mention(
    workspace: &Path,
    raw_path: &str,
    canonical_file: &Path,
) -> String {
    if raw_path.starts_with("~/") {
        if let Some(canon_home) = home_dir().and_then(|h| h.canonicalize().ok()) {
            if let Ok(rel) = canonical_file.strip_prefix(&canon_home) {
                let rel_str = format_rel_mention_path(rel);
                if !rel_str.is_empty() {
                    return format!("~/{rel_str}");
                }
            }
        }
        return raw_path.to_owned();
    }
    if let Ok(canon_ws) = workspace.canonicalize() {
        if let Ok(rel) = canonical_file.strip_prefix(&canon_ws) {
            let rel_str = format_rel_mention_path(rel);
            if !rel_str.is_empty() {
                return rel_str;
            }
        }
    }
    let raw = Path::new(raw_path);
    if raw.is_relative() {
        let cleaned = format_rel_mention_path(raw);
        if !cleaned.is_empty() {
            return cleaned;
        }
    }
    canonical_file.display().to_string()
}

fn push_warning(warnings: &mut Vec<String>, warning: String) {
    if !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

fn prepare_internal(
    text: &str,
    workspace: &Path,
    explicit: &[PathBuf],
    literals: &[Range<usize>],
    temp_files: &mut Vec<PathBuf>,
) -> Result<PreparedBody> {
    let is_literal = |position| literals.iter().any(|range| range.contains(&position));
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    let mut resolved_mentions = Vec::new();
    for path in explicit {
        add(&mut files, file(workspace, path)?)?;
    }
    let mut output = String::new();
    let mut fenced = None;
    let mut line_offset = 0;
    let mut git_cache = HashMap::new();
    for line in text.split_inclusive('\n') {
        let offset = line_offset;
        line_offset += line.len();
        let trimmed = line.trim_start();
        let marker_start = offset + line.len() - trimmed.len();
        if !(marker_start..marker_start + 3).any(&is_literal)
            && crate::tool_codec::toggle_fence(trimmed, &mut fenced)
        {
            output.push_str(line);
            continue;
        }
        if fenced.is_some() {
            output.push_str(line);
            continue;
        }
        let mut pos = 0;
        let mut inline = false;
        while pos < line.len() {
            let c = line[pos..].chars().next().unwrap();
            if c == '`' && !is_literal(offset + pos) {
                inline = !inline;
            }
            let boundary = is_mention_boundary(line[..pos].chars().next_back());
            if c != '@' || !boundary || inline || is_literal(offset + pos) {
                output.push(c);
                pos += c.len_utf8();
                continue;
            }
            if line[pos..].starts_with("@@") {
                output.push('@');
                pos += 2;
                continue;
            }
            let (path, end_rel) = match parse_mention(&line[pos..]) {
                Ok(Some(parsed)) => parsed,
                Ok(None) => unreachable!("starts with @"),
                Err(msg) => {
                    push_warning(&mut warnings, format!("{msg}; keeping literal @ text"));
                    output.push('@');
                    pos += 1;
                    continue;
                }
            };
            if path.is_empty() {
                output.push('@');
                pos += 1;
                continue;
            }
            let raw_mention = &line[pos..pos + end_rel];
            let is_git_mention = path.starts_with("git:");
            let resolved = if let Some(spec) = path.strip_prefix("git:") {
                resolve_git_mention(workspace, spec, &mut git_cache, temp_files, &mut warnings)
            } else {
                file(workspace, Path::new(path))
            };
            match resolved {
                Ok(resolved_path) => {
                    let mention_display = if is_git_mention {
                        path.to_owned()
                    } else {
                        normalize_resolved_file_mention(workspace, path, &resolved_path)
                    };
                    output.push_str(&format!(
                        "[Attached file: {}]",
                        resolved_path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                    ));
                    add(&mut files, resolved_path)?;
                    resolved_mentions.push(mention_display);
                }
                Err(e) => {
                    push_warning(
                        &mut warnings,
                        format!("{e}; keeping {raw_mention} as prompt text"),
                    );
                    output.push_str(raw_mention);
                }
            }
            pos += end_rel;
        }
    }
    Ok(PreparedBody {
        text: output,
        files,
        warnings,
        resolved_mentions,
    })
}

fn add(files: &mut Vec<PathBuf>, path: PathBuf) -> Result<()> {
    if !files.contains(&path) {
        files.push(path);
    }
    if files.len() > MAX_FILES {
        return Err(ServiceError::invalid(
            "at most 30 attachments per conversation; start /new to attach a different set",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mentions_upload_without_inlining_and_support_spaces_unicode() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("notes \u{2603}.txt"),
            "SECRET".repeat(10000),
        )
        .unwrap();
        let p = prepare(
            "Read @\"notes \u{2603}.txt\" and @'notes \u{2603}.txt'",
            dir.path(),
            &[],
        )
        .unwrap();
        assert_eq!(p.files.len(), 1);
        assert!(p.text.len() < 150);
        assert!(!p.text.contains("SECRET"));
    }
    #[test]
    fn examples_emails_and_escaped_mentions_are_literal() {
        let text = "mail me@host.test @@literal `@code`\n```\n@no-file\n```\n";
        let p = prepare(text, Path::new("/tmp"), &[]).unwrap();
        assert!(p.files.is_empty());
        assert!(p.text.contains("@literal"));
        assert!(p.text.contains("@no-file"));
    }
    #[test]
    fn missing_files_directories_and_unclosed_quotes_warn_and_keep_literal_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.txt"), "file").unwrap();

        let missing = prepare("@missing", dir.path(), &[]).unwrap();
        assert!(missing.files.is_empty());
        assert_eq!(missing.text, "@missing");
        assert_eq!(missing.warnings.len(), 1);
        assert!(missing.warnings[0].contains("cannot attach"));
        assert!(missing.warnings[0].contains("keeping @missing as prompt text"));

        let prose = prepare("@fdsjifdsoidsf fdsijdsf", dir.path(), &[]).unwrap();
        assert!(prose.files.is_empty());
        assert_eq!(prose.text, "@fdsjifdsoidsf fdsijdsf");
        assert_eq!(prose.warnings.len(), 1);
        assert!(prose.warnings[0].contains("cannot attach"));
        assert!(prose.warnings[0].contains("keeping @fdsjifdsoidsf as prompt text"));

        let directory = prepare("@.", dir.path(), &[]).unwrap();
        assert!(directory.files.is_empty());
        assert_eq!(directory.text, "@.");
        assert_eq!(directory.warnings.len(), 1);
        assert!(directory.warnings[0].contains("regular files"));
        assert!(directory.warnings[0].contains("keeping @. as prompt text"));

        let unclosed = prepare("@\"unclosed quote", dir.path(), &[]).unwrap();
        assert!(unclosed.files.is_empty());
        assert_eq!(unclosed.text, "@\"unclosed quote");
        assert_eq!(unclosed.warnings.len(), 1);
        assert!(unclosed.warnings[0].contains("unclosed quoted attachment path"));

        let repeated = prepare("@missing and @missing", dir.path(), &[]).unwrap();
        assert!(repeated.files.is_empty());
        assert_eq!(repeated.text, "@missing and @missing");
        assert_eq!(repeated.warnings.len(), 1);

        let mixed = prepare("Read @real.txt and @missing", dir.path(), &[]).unwrap();
        assert_eq!(mixed.files.len(), 1);
        assert_eq!(mixed.text, "Read [Attached file: real.txt] and @missing");
        assert_eq!(mixed.warnings.len(), 1);

        let huge_path = dir.path().join("huge.bin");
        std::fs::File::create(&huge_path)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        let oversized = prepare("Inspect @huge.bin", dir.path(), &[]).unwrap();
        assert!(oversized.files.is_empty());
        assert_eq!(oversized.text, "Inspect @huge.bin");
        assert_eq!(oversized.warnings.len(), 1);
        assert!(oversized.warnings[0].contains("100 MiB"));
        assert!(oversized.warnings[0].contains("keeping @huge.bin as prompt text"));

        assert!(prepare("hi", dir.path(), &[PathBuf::from("missing")]).is_err());
        assert!(prepare("hi", dir.path(), &[PathBuf::from("huge.bin")]).is_err());
    }
    #[test]
    fn pasted_mentions_stay_exact_while_typed_mentions_attach() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.txt"), "file").unwrap();
        let pasted = "@missing @@literal\n@another\n";
        let text = format!("Read @real.txt\n{pasted}then @real.txt");
        let start = text.find(pasted).unwrap();
        let literal = start..start + pasted.len();
        let prepared =
            prepare_with_literals(&text, dir.path(), &[], std::slice::from_ref(&literal)).unwrap();
        assert_eq!(prepared.files.len(), 1);
        assert_eq!(
            prepared.text,
            format!("Read [Attached file: real.txt]\n{pasted}then [Attached file: real.txt]")
        );
    }
    #[test]
    fn pasted_markdown_does_not_disable_subsequent_typed_attachments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.txt"), "file").unwrap();
        for pasted in ["```\nexample\n", "~~~\nexample\n", "`snippet "] {
            let text = format!("{pasted}@real.txt");
            let literal = 0..pasted.len();
            let prepared =
                prepare_with_literals(&text, dir.path(), &[], std::slice::from_ref(&literal))
                    .unwrap();
            assert_eq!(prepared.files.len(), 1);
            assert_eq!(prepared.text, format!("{pasted}[Attached file: real.txt]"));
        }
    }
    #[test]
    fn git_diff_unstaged_and_staged_mentions_attach_patch_files() {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap();
            assert!(status.success());
        };
        run(&["init"]);
        run(&["config", "user.name", "test"]);
        run(&["config", "user.email", "test@example.com"]);
        std::fs::write(dir.path().join("code.rs"), "fn main() {}\n").unwrap();
        run(&["add", "code.rs"]);
        run(&["commit", "-m", "init"]);

        // Modify unstaged
        std::fs::write(
            dir.path().join("code.rs"),
            "fn main() { println!(\"hello\"); }\n",
        )
        .unwrap();

        let unstaged = prepare("Review @git:diff please", dir.path(), &[]).unwrap();
        assert_eq!(unstaged.files.len(), 1);
        assert_eq!(unstaged.temp_files.len(), 1);
        assert_eq!(unstaged.files[0], unstaged.temp_files[0]);
        assert_eq!(unstaged.files[0].file_name().unwrap(), "git-diff.patch");
        assert_eq!(
            unstaged.text,
            "Review [Attached file: git-diff.patch] please"
        );
        let patch_content = std::fs::read_to_string(&unstaged.files[0]).unwrap();
        assert!(patch_content.contains("+fn main() { println!(\"hello\"); }"));

        // Cleanup deletes temp patch and its isolated folder
        assert!(unstaged.files[0].exists());
        cleanup_temp_files(&unstaged.temp_files);
        assert!(!unstaged.files[0].exists());

        // Regular files are NOT in temp_files and are never deleted by cleanup
        let normal = prepare("Read @code.rs", dir.path(), &[]).unwrap();
        assert_eq!(normal.files.len(), 1);
        assert!(normal.temp_files.is_empty());
        cleanup_temp_files(&normal.temp_files);
        assert!(dir.path().join("code.rs").exists());

        // Stage the change
        run(&["add", "code.rs"]);

        let staged = prepare("Commit @git:staged please", dir.path(), &[]).unwrap();
        assert_eq!(staged.files.len(), 1);
        assert_eq!(staged.temp_files.len(), 1);
        assert_eq!(staged.files[0].file_name().unwrap(), "git-staged.patch");
        assert_eq!(
            staged.text,
            "Commit [Attached file: git-staged.patch] please"
        );
        cleanup_temp_files(&staged.temp_files);
        assert!(!staged.files[0].exists());

        // Deduplication in same turn
        let duplicate = prepare("@git:staged and @git:staged", dir.path(), &[]).unwrap();
        assert_eq!(duplicate.files.len(), 1);
        assert_eq!(
            duplicate.text,
            "[Attached file: git-staged.patch] and [Attached file: git-staged.patch]"
        );
        cleanup_temp_files(&duplicate.temp_files);
        assert!(!duplicate.files[0].exists());
    }
    #[test]
    fn git_diff_cleans_up_temp_files_if_subsequent_mention_fails() {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap();
            assert!(status.success());
        };
        run(&["init"]);
        run(&["config", "user.name", "test"]);
        run(&["config", "user.email", "test@example.com"]);
        std::fs::write(dir.path().join("code.rs"), "fn main() {}\n").unwrap();
        run(&["add", "code.rs"]);
        run(&["commit", "-m", "init"]);

        // Make an unstaged change so @git:diff would generate a patch
        std::fs::write(dir.path().join("code.rs"), "fn main() { 1; }\n").unwrap();

        // Missing regular file mention emits warning while keeping @git:diff attached
        let with_missing = prepare("Check @git:diff and @missing.txt", dir.path(), &[]).unwrap();
        assert_eq!(with_missing.files.len(), 1);
        assert_eq!(with_missing.temp_files.len(), 1);
        assert_eq!(with_missing.files[0].file_name().unwrap(), "git-diff.patch");
        assert_eq!(
            with_missing.text,
            "Check [Attached file: git-diff.patch] and @missing.txt"
        );
        assert_eq!(with_missing.warnings.len(), 1);
        assert!(with_missing.warnings[0].contains("keeping @missing.txt as prompt text"));
        cleanup_temp_files(&with_missing.temp_files);
        assert!(!with_missing.files[0].exists());

        // Failing @git: mention also warns and keeps @git:nonexistent literal while keeping @git:diff attached
        let with_bad_git =
            prepare("Check @git:diff and @git:nonexistent", dir.path(), &[]).unwrap();
        assert_eq!(with_bad_git.files.len(), 1);
        assert_eq!(with_bad_git.temp_files.len(), 1);
        assert_eq!(with_bad_git.files[0].file_name().unwrap(), "git-diff.patch");
        assert_eq!(
            with_bad_git.text,
            "Check [Attached file: git-diff.patch] and @git:nonexistent"
        );
        assert_eq!(with_bad_git.warnings.len(), 1);
        assert!(with_bad_git.warnings[0].contains("unknown git commit"));
        assert!(with_bad_git.warnings[0].contains("keeping @git:nonexistent as prompt text"));
        cleanup_temp_files(&with_bad_git.temp_files);
        assert!(!with_bad_git.files[0].exists());

        // Exceeding MAX_FILES after @git:diff errors cleanly and cleans up prior temp patch files
        let mut explicit = Vec::new();
        for i in 0..MAX_FILES {
            let p = dir.path().join(format!("limit_{i}.txt"));
            std::fs::write(&p, "x").unwrap();
            explicit.push(p);
        }
        let err = prepare("Check @git:diff", dir.path(), &explicit).unwrap_err();
        assert!(err.to_string().contains("at most 10 attachments"));

        let mut recorded_temps = Vec::new();
        let err = prepare_internal(
            "Check @git:diff",
            dir.path(),
            &explicit,
            &[],
            &mut recorded_temps,
        )
        .unwrap_err();
        assert!(err.to_string().contains("at most 10 attachments"));
        assert_eq!(recorded_temps.len(), 1);
        assert!(recorded_temps[0].exists());
        cleanup_temp_files(&recorded_temps);
        assert!(!recorded_temps[0].exists());
        assert!(!recorded_temps[0].parent().unwrap().exists());
    }
    #[test]
    fn cleanup_temp_files_safe_with_non_empty_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let file1 = dir.path().join("file1.txt");
        let file2 = dir.path().join("file2.txt");
        std::fs::write(&file1, "hello").unwrap();
        std::fs::write(&file2, "world").unwrap();
        cleanup_temp_files(std::slice::from_ref(&file1));
        assert!(!file1.exists());
        assert!(file2.exists());
        assert!(dir.path().exists());
    }
    #[test]
    fn git_diff_warns_on_empty_diff_and_non_git_repo_while_direct_helper_errors() {
        let dir = tempfile::tempdir().unwrap();
        // Non-git repo: direct helper errors, prompt prepare warns and keeps literal text
        let err = git_diff_file(dir.path(), false).unwrap_err();
        assert!(err.to_string().contains("not a git repository"));
        let non_git_prompt = prepare("@git:diff", dir.path(), &[]).unwrap();
        assert!(non_git_prompt.files.is_empty());
        assert!(non_git_prompt.temp_files.is_empty());
        assert_eq!(non_git_prompt.text, "@git:diff");
        assert_eq!(non_git_prompt.warnings.len(), 1);
        assert!(non_git_prompt.warnings[0].contains("not a git repository"));
        assert!(non_git_prompt.warnings[0].contains("keeping @git:diff as prompt text"));

        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap();
            assert!(status.success());
        };
        run(&["init"]);
        run(&["config", "user.name", "test"]);
        run(&["config", "user.email", "test@example.com"]);
        std::fs::write(dir.path().join("file.txt"), "committed\n").unwrap();
        run(&["add", "file.txt"]);
        run(&["commit", "-m", "init"]);

        // Empty diffs: direct helpers error, prompt prepare warns and keeps literal text
        let err = git_diff_file(dir.path(), false).unwrap_err();
        assert!(err.to_string().contains("no unstaged git changes"));
        let clean_diff = prepare("@git:diff", dir.path(), &[]).unwrap();
        assert!(clean_diff.files.is_empty());
        assert_eq!(clean_diff.text, "@git:diff");
        assert_eq!(clean_diff.warnings.len(), 1);
        assert!(clean_diff.warnings[0].contains("no unstaged git changes"));

        let err = git_diff_file(dir.path(), true).unwrap_err();
        assert!(err.to_string().contains("no staged git changes"));
        let clean_staged = prepare("@git:staged", dir.path(), &[]).unwrap();
        assert!(clean_staged.files.is_empty());
        assert_eq!(clean_staged.text, "@git:staged");
        assert_eq!(clean_staged.warnings.len(), 1);
        assert!(clean_staged.warnings[0].contains("no staged git changes"));
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn git_diff_plus_and_staged_plus_expand_tree_sitter_ast_context_and_warn_on_unsupported_language(
    ) {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap();
            assert!(status.success());
        };
        run(&["init"]);
        run(&["config", "user.name", "test"]);
        run(&["config", "user.email", "test@example.com"]);

        let rust_code = "\
fn helper() -> i32 {
    1
}

fn compute(a: i32, b: i32) -> i32 {
    let x = a;
    let y = b;
    x + y
}

fn other() -> i32 {
    3
}
";
        let css_code = "\
.container {
    margin: 0;
    padding: 10px;
}
";
        std::fs::write(dir.path().join("math.rs"), rust_code).unwrap();
        std::fs::write(dir.path().join("styles.css"), css_code).unwrap();
        run(&["add", "math.rs", "styles.css"]);
        run(&["commit", "-m", "init"]);

        // Modify unstaged
        let rust_mod = rust_code.replace("x + y", "x * 2 + y");
        let css_mod = css_code.replace("padding: 10px;", "padding: 20px;");
        std::fs::write(dir.path().join("math.rs"), rust_mod).unwrap();
        std::fs::write(dir.path().join("styles.css"), css_mod).unwrap();

        let unstaged = prepare("Review @git:diff+ please", dir.path(), &[]).unwrap();
        assert_eq!(unstaged.files.len(), 1);
        assert_eq!(unstaged.temp_files.len(), 1);
        assert_eq!(unstaged.files[0], unstaged.temp_files[0]);
        assert_eq!(unstaged.files[0].file_name().unwrap(), "git-diff+.patch");
        assert_eq!(
            unstaged.text,
            "Review [Attached file: git-diff+.patch] please"
        );

        let patch_content = std::fs::read_to_string(&unstaged.files[0]).unwrap();
        // Supported Rust file expanded to include enclosing function `fn compute`
        assert!(patch_content.contains("fn compute(a: i32, b: i32) -> i32 {"));
        assert!(patch_content.contains("+    x * 2 + y"));
        // Unsupported CSS file included via git diff -W fallback
        assert!(patch_content.contains(".container {"));
        assert!(patch_content.contains("+    padding: 20px;"));
        // Warning logged for the unsupported language file
        assert!(unstaged
            .warnings
            .iter()
            .any(|w| w
                .contains("unsupported language for 'styles.css'; falling back to git diff -W")));

        cleanup_temp_files(&unstaged.temp_files);
        assert!(!unstaged.files[0].exists());

        // Stage the changes and test @git:staged+
        run(&["add", "math.rs", "styles.css"]);
        let staged = prepare("Commit @git:staged+ please", dir.path(), &[]).unwrap();
        assert_eq!(staged.files.len(), 1);
        assert_eq!(staged.temp_files.len(), 1);
        assert_eq!(staged.files[0].file_name().unwrap(), "git-staged+.patch");
        assert_eq!(
            staged.text,
            "Commit [Attached file: git-staged+.patch] please"
        );
        let staged_patch = std::fs::read_to_string(&staged.files[0]).unwrap();
        assert!(staged_patch.contains("fn compute(a: i32, b: i32) -> i32 {"));
        assert!(staged_patch.contains("+    x * 2 + y"));
        assert!(staged_patch.contains("+    padding: 20px;"));
        assert!(staged
            .warnings
            .iter()
            .any(|w| w
                .contains("unsupported language for 'styles.css'; falling back to git diff -W")));
        cleanup_temp_files(&staged.temp_files);
        assert!(!staged.files[0].exists());

        // Deduplication in same turn
        let duplicate = prepare("@git:staged+ and @git:staged+", dir.path(), &[]).unwrap();
        assert_eq!(duplicate.files.len(), 1);
        assert_eq!(
            duplicate.text,
            "[Attached file: git-staged+.patch] and [Attached file: git-staged+.patch]"
        );
        cleanup_temp_files(&duplicate.temp_files);
        assert!(!duplicate.files[0].exists());

        // Non-git repository test
        let non_git = tempfile::tempdir().unwrap();
        let non_git_prompt = prepare("@git:diff+", non_git.path(), &[]).unwrap();
        assert!(non_git_prompt.files.is_empty());
        assert_eq!(non_git_prompt.text, "@git:diff+");
        assert!(non_git_prompt.warnings[0].contains("not a git repository"));
        assert!(non_git_prompt.warnings[0].contains("keeping @git:diff+ as prompt text"));

        // Clean repository test
        run(&["commit", "-m", "update"]);
        let clean = prepare("@git:diff+", dir.path(), &[]).unwrap();
        assert!(clean.files.is_empty());
        assert_eq!(clean.text, "@git:diff+");
        assert!(clean.warnings[0].contains("no unstaged git changes"));
        assert!(clean.warnings[0].contains("keeping @git:diff+ as prompt text"));
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn git_commit_plus_expands_tree_sitter_ast_context_and_warns_on_unsupported_language() {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap();
            assert!(status.success());
        };
        run(&["init"]);
        run(&["config", "user.name", "test"]);
        run(&["config", "user.email", "test@example.com"]);

        let rust_code = "\
fn helper() -> i32 {
    1
}

fn compute(a: i32, b: i32) -> i32 {
    let x = a;
    let y = b;
    x + y
}

fn other() -> i32 {
    3
}
";
        let css_code = "\
.container {
    margin: 0;
    padding: 10px;
}
";
        std::fs::write(dir.path().join("math.rs"), rust_code).unwrap();
        std::fs::write(dir.path().join("styles.css"), css_code).unwrap();
        run(&["add", "math.rs", "styles.css"]);
        run(&["commit", "-m", "init"]);

        // Commit modifications
        let rust_mod = rust_code.replace("x + y", "x * 2 + y");
        let css_mod = css_code.replace("padding: 10px;", "padding: 20px;");
        std::fs::write(dir.path().join("math.rs"), rust_mod).unwrap();
        std::fs::write(dir.path().join("styles.css"), css_mod).unwrap();
        run(&["add", "math.rs", "styles.css"]);
        run(&["commit", "-m", "commit with tree-sitter changes"]);

        let head_sha_out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let full_sha = String::from_utf8_lossy(&head_sha_out.stdout)
            .trim()
            .to_string();
        let short_sha = &full_sha[..12];
        let expected_patch_filename = format!("git-{short_sha}+.patch");

        // 1. Test @git:HEAD+
        let prepared = prepare("Review @git:HEAD+ please", dir.path(), &[]).unwrap();
        assert_eq!(prepared.files.len(), 1);
        assert_eq!(prepared.temp_files.len(), 1);
        assert_eq!(
            prepared.files[0].file_name().unwrap().to_str().unwrap(),
            expected_patch_filename
        );
        assert_eq!(
            prepared.text,
            format!("Review [Attached file: {expected_patch_filename}] please")
        );

        let patch_content = std::fs::read_to_string(&prepared.files[0]).unwrap();
        // Preserves commit message
        assert!(patch_content.contains("commit with tree-sitter changes"));
        // Supported Rust file expanded to include enclosing function `fn compute`
        assert!(patch_content.contains("fn compute(a: i32, b: i32) -> i32 {"));
        assert!(patch_content.contains("+    x * 2 + y"));
        // Unsupported CSS file included via fallback
        assert!(patch_content.contains(".container {"));
        assert!(patch_content.contains("+    padding: 20px;"));
        // Warning logged for unsupported language file
        assert!(prepared
            .warnings
            .iter()
            .any(|w| w.contains("unsupported language for 'styles.css'")));

        cleanup_temp_files(&prepared.temp_files);
        assert!(!prepared.files[0].exists());

        // 2. Test short SHA+ and deduplication with HEAD+
        let short_mention = format!("@git:{}+ and @git:HEAD+", &full_sha[..7]);
        let dedup = prepare(&short_mention, dir.path(), &[]).unwrap();
        assert_eq!(dedup.files.len(), 1);
        assert_eq!(
            dedup.text,
            format!("[Attached file: {expected_patch_filename}] and [Attached file: {expected_patch_filename}]")
        );
        cleanup_temp_files(&dedup.temp_files);
        assert!(!dedup.files[0].exists());

        // 3. Test invalid commit with + preserves literal text and warns
        let bad = prepare("@git:nonexistent+ hello", dir.path(), &[]).unwrap();
        assert!(bad.files.is_empty());
        assert_eq!(bad.text, "@git:nonexistent+ hello");
        assert!(bad.warnings[0].contains("unknown git commit: nonexistent"));
        assert!(bad.warnings[0].contains("keeping @git:nonexistent+ as prompt text"));
    }

    #[test]
    fn git_commit_mentions_attach_dedup_and_support_refs_and_merges() {
        let dir = tempfile::tempdir().unwrap();
        let run_out = |args: &[&str]| -> String {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };

        run_out(&["init"]);
        run_out(&["config", "user.name", "test"]);
        run_out(&["config", "user.email", "test@example.com"]);
        std::fs::write(dir.path().join("code.rs"), "fn main() {}\n").unwrap();
        run_out(&["add", "code.rs"]);
        run_out(&["commit", "-m", "initial commit"]);
        let first_full = run_out(&["rev-parse", "HEAD"]);
        let first_short = run_out(&["rev-parse", "--short", "HEAD"]);

        run_out(&["checkout", "-b", "feat/login"]);
        std::fs::write(dir.path().join("login.rs"), "pub fn login() {}\n").unwrap();
        run_out(&["add", "login.rs"]);
        run_out(&["commit", "-m", "add login"]);
        let branch_full = run_out(&["rev-parse", "HEAD"]);
        let expected_branch_file = format!("git-{}.patch", &branch_full[..12]);

        // Switch back to initial branch and create a clean merge commit.
        run_out(&["checkout", "-"]);
        std::fs::write(dir.path().join("README.md"), "# docs\n").unwrap();
        run_out(&["add", "README.md"]);
        run_out(&["commit", "-m", "add docs"]);
        run_out(&["merge", "--no-ff", "feat/login", "-m", "merge feat/login"]);
        let merge_full = run_out(&["rev-parse", "HEAD"]);
        let expected_merge_file = format!("git-{}.patch", &merge_full[..12]);
        let expected_first_file = format!("git-{}.patch", &first_full[..12]);

        // 1. HEAD, short SHA, full SHA, and slash branch name resolve and deduplicate properly.
        let p = prepare(
            &format!("Compare @git:HEAD @git:{merge_full} and @git:feat/login"),
            dir.path(),
            &[],
        )
        .unwrap();
        assert_eq!(p.files.len(), 2);
        assert_eq!(p.temp_files.len(), 2);
        assert_eq!(
            p.files[0].file_name().unwrap().to_string_lossy(),
            expected_merge_file
        );
        assert_eq!(
            p.files[1].file_name().unwrap().to_string_lossy(),
            expected_branch_file
        );
        assert_eq!(
            p.text,
            format!(
                "Compare [Attached file: {expected_merge_file}] [Attached file: {expected_merge_file}] and [Attached file: {expected_branch_file}]"
            )
        );
        let merge_patch = std::fs::read_to_string(&p.files[0]).unwrap();
        assert!(merge_patch.contains("merge feat/login"));
        assert!(
            merge_patch.contains("+pub fn login() {}"),
            "merge commit patch should include first-parent diff"
        );
        cleanup_temp_files(&p.temp_files);
        assert!(!p.files[0].exists());
        assert!(!p.files[1].exists());

        // 2. Short SHA and full SHA for an older commit deduplicate to one patch file.
        let dedup = prepare(
            &format!("@git:{first_short} and @git:{first_full}"),
            dir.path(),
            &[],
        )
        .unwrap();
        assert_eq!(dedup.files.len(), 1);
        assert_eq!(
            dedup.files[0].file_name().unwrap().to_string_lossy(),
            expected_first_file
        );
        let first_patch = std::fs::read_to_string(&dedup.files[0]).unwrap();
        assert!(first_patch.contains("initial commit"));
        assert!(first_patch.contains("+fn main() {}"));
        cleanup_temp_files(&dedup.temp_files);
        assert!(!dedup.files[0].exists());

        // 3. Direct git_commit_file validates and supports refs with slashes safely.
        let direct = git_commit_file(dir.path(), "feat/login").unwrap();
        assert_eq!(
            direct.file_name().unwrap().to_string_lossy(),
            expected_branch_file
        );
        cleanup_temp_files(&[direct]);
    }

    #[test]
    fn git_commit_warns_on_invalid_inputs_in_prompts_and_cleans_up_on_max_files() {
        let non_git = tempfile::tempdir().unwrap();
        assert!(git_commit_file(non_git.path(), "HEAD")
            .unwrap_err()
            .to_string()
            .contains("not a git repository"));
        let non_git_prompt = prepare("@git:HEAD", non_git.path(), &[]).unwrap();
        assert!(non_git_prompt.files.is_empty());
        assert_eq!(non_git_prompt.text, "@git:HEAD");
        assert_eq!(non_git_prompt.warnings.len(), 1);
        assert!(non_git_prompt.warnings[0].contains("not a git repository"));
        assert!(non_git_prompt.warnings[0].contains("keeping @git:HEAD as prompt text"));

        let dir = tempfile::tempdir().unwrap();
        let run_out = |args: &[&str]| -> String {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        run_out(&["init"]);
        run_out(&["config", "user.name", "test"]);
        run_out(&["config", "user.email", "test@example.com"]);
        std::fs::write(dir.path().join("file.txt"), "hello\n").unwrap();
        run_out(&["add", "file.txt"]);
        run_out(&["commit", "-m", "init"]);
        let blob_sha = run_out(&["rev-parse", "HEAD:file.txt"]);

        for (raw, expected_err) in [
            ("@git:", "missing git commit id"),
            ("@git:nonexistent", "unknown git commit"),
            ("@git:--help", "invalid git commit"),
            ("@git:HEAD^{tree}", "unknown git commit"),
        ] {
            let prepared = prepare(raw, dir.path(), &[]).unwrap();
            assert!(prepared.files.is_empty(), "{raw} should not attach files");
            assert!(prepared.temp_files.is_empty());
            assert_eq!(prepared.text, raw);
            assert_eq!(prepared.warnings.len(), 1);
            assert!(
                prepared.warnings[0].contains(expected_err),
                "warning {:?} should contain {:?}",
                prepared.warnings[0],
                expected_err
            );
            assert!(prepared.warnings[0].contains(&format!("keeping {raw} as prompt text")));
        }

        let blob_mention = format!("@git:{blob_sha}");
        let blob_prepared = prepare(&blob_mention, dir.path(), &[]).unwrap();
        assert!(blob_prepared.files.is_empty());
        assert_eq!(blob_prepared.text, blob_mention);
        assert_eq!(blob_prepared.warnings.len(), 1);
        assert!(blob_prepared.warnings[0].contains("unknown git commit"));

        // Direct API still strictly rejects invalid commits and blocks flag injection.
        assert!(git_commit_file(dir.path(), "").is_err());
        assert!(git_commit_file(dir.path(), "nonexistent").is_err());
        assert!(git_commit_file(dir.path(), "--output=/tmp/pwn").is_err());
        assert!(git_commit_file(dir.path(), "HEAD^{tree}").is_err());
        assert!(git_commit_file(dir.path(), &blob_sha).is_err());

        // Exceeding MAX_FILES after @git:HEAD records the temp patch for cleanup on Err.
        let mut explicit = Vec::new();
        for i in 0..MAX_FILES {
            let p = dir.path().join(format!("extra_{i}.txt"));
            std::fs::write(&p, "x").unwrap();
            explicit.push(p);
        }
        let err = prepare("@git:HEAD", dir.path(), &explicit).unwrap_err();
        assert!(err.to_string().contains("at most 10 attachments"));

        let mut recorded_temps = Vec::new();
        let err = prepare_internal("@git:HEAD", dir.path(), &explicit, &[], &mut recorded_temps)
            .unwrap_err();
        assert!(err.to_string().contains("at most 10 attachments"));
        assert_eq!(recorded_temps.len(), 1);
        assert!(recorded_temps[0].exists());
        cleanup_temp_files(&recorded_temps);
        assert!(!recorded_temps[0].exists());
        assert!(!recorded_temps[0].parent().unwrap().exists());
    }

    #[test]
    fn git_commit_plus_and_branch_ending_with_plus() {
        let dir = tempfile::tempdir().unwrap();
        let run_out = |args: &[&str]| -> String {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        run_out(&["init"]);
        run_out(&["config", "user.name", "test"]);
        run_out(&["config", "user.email", "test@example.com"]);
        std::fs::write(dir.path().join("code.rs"), "fn hello() {}\n").unwrap();
        run_out(&["add", "code.rs"]);
        run_out(&["commit", "-m", "init"]);
        run_out(&["branch", "c++"]);

        // 1. @git:c++ resolves to branch c++ (since branch `c+` does not exist)
        let prep = prepare("@git:c++", dir.path(), &[]).unwrap();
        assert_eq!(prep.files.len(), 1);
        assert!(
            !prep.files[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("+.patch"),
            "literal branch c++ should produce a regular patch"
        );
        cleanup_temp_files(&prep.temp_files);

        // 2. When both `foo` and `foo+` exist, documented precedence resolves `@git:foo+`
        //    as AST-expanded `foo` (CommitPlus) rather than literal `foo+`.
        run_out(&["branch", "foo"]);
        let foo_sha = run_out(&["rev-parse", "foo"]);
        std::fs::write(dir.path().join("code.rs"), "fn hello() { let x = 1; }\n").unwrap();
        run_out(&["add", "code.rs"]);
        run_out(&["commit", "-m", "second"]);
        run_out(&["branch", "foo+"]);
        let prep_conflict = prepare("@git:foo+", dir.path(), &[]).unwrap();
        assert_eq!(prep_conflict.files.len(), 1);
        let conflict_name = prep_conflict.files[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(conflict_name, format!("git-{}+.patch", &foo_sha[..12]));
        cleanup_temp_files(&prep_conflict.temp_files);

        // 3. @git:+ is handled gracefully as an invalid commit rather than panic/empty error
        let prep_plus = prepare("@git:+", dir.path(), &[]).unwrap();
        assert!(prep_plus.files.is_empty());
        assert!(prep_plus.warnings[0].contains("unknown git commit"));
    }

    #[test]
    fn resolved_file_mentions_normalize_dot_prefix_and_workspace_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let file_path = dir.path().join("src/foo.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let abs_str = file_path.display().to_string();
        let prompt = format!("check @./src/foo.rs and @\"{abs_str}\" and @src/./foo.rs");
        let prepared = prepare(&prompt, dir.path(), &[]).unwrap();
        assert_eq!(prepared.files.len(), 1);
        assert_eq!(
            prepared.resolved_mentions,
            vec!["src/foo.rs", "src/foo.rs", "src/foo.rs"]
        );
    }
}
