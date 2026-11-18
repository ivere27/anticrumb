//! File mentions are references to native uploads, never expanded file contents.
use crate::domain::{Result, ServiceError};
use std::{
    io::{Read, Write},
    ops::Range,
    path::{Path, PathBuf},
};

pub const MAX_FILES: usize = 10;
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct PreparedInput {
    pub text: String,
    pub files: Vec<PathBuf>,
    pub temp_files: Vec<PathBuf>,
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
        return Err(ServiceError::invalid(
            "attachments must be regular files of at most 32 MiB",
        ));
    }
    Ok(path)
}

pub fn git_diff_file(workspace: &Path, staged: bool) -> Result<PathBuf> {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(workspace);
    if staged {
        cmd.args(["diff", "--staged", "--no-color", "--no-ext-diff"]);
    } else {
        cmd.args(["diff", "--no-color", "--no-ext-diff"]);
    }
    let temp_dir =
        std::env::temp_dir().join(format!("anticrumb-git-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&temp_dir).map_err(ServiceError::storage)?;
    let filename = if staged {
        "git-staged.patch"
    } else {
        "git-diff.patch"
    };
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
                    "failed to read git diff: {e}"
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
        return Err(ServiceError::invalid(
            "attachments must be regular files of at most 32 MiB",
        ));
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
    let status = child
        .wait()
        .map_err(|e| ServiceError::invalid(format!("git command failed: {e}")))?;

    if !status.success() {
        cleanup();
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        if stderr.to_ascii_lowercase().contains("not a git repository") {
            return Err(ServiceError::invalid("not a git repository"));
        }
        return Err(ServiceError::invalid(format!(
            "git diff failed: {}",
            stderr.trim()
        )));
    }

    if total_bytes == 0 || !has_non_whitespace {
        cleanup();
        return Err(ServiceError::invalid(if staged {
            "no staged git changes to attach"
        } else {
            "no unstaged git changes to attach"
        }));
    }

    Ok(patch_path)
}

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

fn resolve_mention(
    workspace: &Path,
    path: &str,
    unstaged_patch: &mut Option<PathBuf>,
    staged_patch: &mut Option<PathBuf>,
    temp_files: &mut Vec<PathBuf>,
) -> Result<PathBuf> {
    match path {
        "git:diff" => {
            if let Some(existing) = unstaged_patch {
                Ok(existing.clone())
            } else {
                let p = git_diff_file(workspace, false)?;
                *unstaged_patch = Some(p.clone());
                if !temp_files.contains(&p) {
                    temp_files.push(p.clone());
                }
                Ok(p)
            }
        }
        "git:staged" => {
            if let Some(existing) = staged_patch {
                Ok(existing.clone())
            } else {
                let p = git_diff_file(workspace, true)?;
                *staged_patch = Some(p.clone());
                if !temp_files.contains(&p) {
                    temp_files.push(p.clone());
                }
                Ok(p)
            }
        }
        _ => file(workspace, Path::new(path)),
    }
}

pub fn prepare(text: &str, workspace: &Path, explicit: &[PathBuf]) -> Result<PreparedInput> {
    prepare_with_literals(text, workspace, explicit, &[])
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
        Ok((text, files)) => Ok(PreparedInput {
            text,
            files,
            temp_files,
        }),
        Err(e) => {
            cleanup_temp_files(&temp_files);
            Err(e)
        }
    }
}

fn prepare_internal(
    text: &str,
    workspace: &Path,
    explicit: &[PathBuf],
    literals: &[Range<usize>],
    temp_files: &mut Vec<PathBuf>,
) -> Result<(String, Vec<PathBuf>)> {
    let is_literal = |position| literals.iter().any(|range| range.contains(&position));
    let mut files = Vec::new();
    for path in explicit {
        add(&mut files, file(workspace, path)?)?;
    }
    let mut output = String::new();
    let mut fenced = None;
    let mut line_offset = 0;
    let mut unstaged_patch = None;
    let mut staged_patch = None;
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
            let (path, end_rel) = parse_mention(&line[pos..])
                .map_err(ServiceError::invalid)?
                .expect("starts with @");
            if path.is_empty() {
                output.push('@');
                pos += 1;
                continue;
            }
            let path = resolve_mention(
                workspace,
                path,
                &mut unstaged_patch,
                &mut staged_patch,
                temp_files,
            )?;
            output.push_str(&format!(
                "[Attached file: {}]",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
            add(&mut files, path)?;
            pos += end_rel;
        }
    }
    Ok((output, files))
}

fn add(files: &mut Vec<PathBuf>, path: PathBuf) -> Result<()> {
    if !files.contains(&path) {
        files.push(path);
    }
    if files.len() > MAX_FILES {
        return Err(ServiceError::invalid(
            "at most 10 attachments per conversation; start /new to attach a different set",
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
    fn missing_files_and_directories_are_rejected_before_submission() {
        let dir = tempfile::tempdir().unwrap();
        assert!(prepare("@missing", dir.path(), &[]).is_err());
        assert!(prepare("@.", dir.path(), &[]).is_err());
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

        // Mention @git:diff followed by a non-existent file
        let err = prepare("Check @git:diff and @missing.txt", dir.path(), &[]).unwrap_err();
        assert!(err.to_string().contains("cannot attach"));
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
    fn git_diff_rejects_empty_diff_and_non_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        // Non-git repo
        let err = prepare("@git:diff", dir.path(), &[]).unwrap_err();
        assert!(err.to_string().contains("not a git repository"));

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

        // Empty diffs
        let err = prepare("@git:diff", dir.path(), &[]).unwrap_err();
        assert!(err.to_string().contains("no unstaged git changes"));

        let err = prepare("@git:staged", dir.path(), &[]).unwrap_err();
        assert!(err.to_string().contains("no staged git changes"));
    }
}
