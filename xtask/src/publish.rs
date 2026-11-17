use crate::archive::{self, is_valid_sha256_digest};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub use crate::archive::ArchiveMember;
pub use crate::github::{
    api, ensure_pages, pages_request, releases, run_command, ProcessOutput, PublishError,
    RealRunner, Runner, BRANCH, REPOSITORY,
};

pub const TARGETS: &[(&str, &str)] = &[
    ("x86_64-unknown-linux-gnu", ".tar.gz"),
    ("aarch64-unknown-linux-gnu", ".tar.gz"),
    ("x86_64-apple-darwin", ".tar.gz"),
    ("aarch64-apple-darwin", ".tar.gz"),
    ("x86_64-pc-windows-gnu", ".zip"),
    ("x86_64-pc-windows-msvc", ".zip"),
];

pub fn target_extension(target: &str) -> Option<&'static str> {
    TARGETS
        .iter()
        .find(|(name, _)| *name == target)
        .map(|(_, ext)| *ext)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetPlan {
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub version: String,
    pub tag: String,
    pub assets: BTreeMap<String, AssetPlan>,
}

pub fn is_stable_semver(value: &str) -> bool {
    let mut parts = value.split('.');
    let Some(a) = parts.next() else {
        return false;
    };
    let Some(b) = parts.next() else {
        return false;
    };
    let Some(c) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    [a, b, c].into_iter().all(is_no_leading_zero_uint)
}

fn is_no_leading_zero_uint(s: &str) -> bool {
    if s == "0" {
        return true;
    }
    let bytes = s.as_bytes();
    !bytes.is_empty()
        && matches!(bytes[0], b'1'..=b'9')
        && bytes[1..].iter().all(|b| b.is_ascii_digit())
}

fn parse_semver_tuple(value: &str) -> Option<(u64, u64, u64)> {
    if !is_stable_semver(value) {
        return None;
    }
    let mut parts = value.split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

pub fn version_from_cargo(root: &Path) -> Result<String, PublishError> {
    let raw = fs::read_to_string(root.join("Cargo.toml"))
        .map_err(|e| PublishError::new(format!("cannot read Cargo.toml: {e}")))?;
    let mut in_package = false;
    for raw_line in raw.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some(rest) = line.strip_prefix("version") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let quote = match rest.chars().next() {
            Some('"') => '"',
            Some('\'') => '\'',
            _ => {
                return Err(PublishError::new(
                    "Cargo.toml [package].version must be a stable X.Y.Z version",
                ));
            }
        };
        let after_open = &rest[1..];
        let Some(end) = after_open.find(quote) else {
            return Err(PublishError::new(
                "Cargo.toml [package].version must be a stable X.Y.Z version",
            ));
        };
        let ver = &after_open[..end];
        let trailing = after_open[end + 1..].trim_start();
        if !trailing.is_empty() && !trailing.starts_with('#') {
            return Err(PublishError::new(
                "Cargo.toml [package].version must be a stable X.Y.Z version",
            ));
        }
        if is_stable_semver(ver) {
            return Ok(ver.to_string());
        }
        return Err(PublishError::new(
            "Cargo.toml [package].version must be a stable X.Y.Z version",
        ));
    }
    Err(PublishError::new(
        "Cargo.toml has no literal [package].version",
    ))
}

pub fn sha256_file(path: &Path) -> Result<String, PublishError> {
    archive::sha256_file(path).map_err(PublishError::new)
}

pub fn archive_members(path: &Path) -> Result<Vec<ArchiveMember>, PublishError> {
    archive::archive_members(path).map_err(PublishError::new)
}

pub fn validate_archive(path: &Path, target: &str) -> Result<(), PublishError> {
    let fname = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let root = format!("anticrumb-{target}");
    let windows = target.contains("windows");
    let executable = if windows {
        "anticrumb.exe"
    } else {
        "anticrumb"
    };
    let mut regular_files = BTreeSet::new();
    let mut seen = BTreeSet::new();

    for member in archive_members(path)? {
        let name = member.name.as_str();
        let trimmed = name.trim_end_matches('/');
        let parts: Vec<&str> = trimmed.split('/').collect();
        if name.is_empty()
            || name.starts_with('/')
            || name.contains('\\')
            || name.contains(':')
            || parts
                .iter()
                .any(|p| p.is_empty() || *p == "." || *p == "..")
            || name.chars().any(|c| (c as u32) < 32 || (c as u32) == 127)
            || parts[0] != root
        {
            return Err(PublishError::new(format!(
                "{fname}: unsafe or unexpected archive path"
            )));
        }
        let normalized = parts.join("/");
        let folded = normalized.to_lowercase();
        if !seen.insert(folded) {
            return Err(PublishError::new(format!(
                "{fname}: duplicate archive member {normalized}"
            )));
        }
        if (member.mode & 0o6000) != 0 {
            return Err(PublishError::new(format!(
                "{fname}: setuid/setgid members are not allowed"
            )));
        }
        if parts.len() == 1 {
            if !member.directory {
                return Err(PublishError::new(format!(
                    "{fname}: package root must be a directory"
                )));
            }
            continue;
        }
        let relative = parts[1..].join("/");
        if parts[1] == "docs" {
            if parts.len() == 2 && !member.directory {
                return Err(PublishError::new(format!(
                    "{fname}: docs must be a directory"
                )));
            }
        } else if parts.len() != 2
            || !(relative == executable || relative == "README.md" || relative == "LICENSE")
            || member.directory
        {
            return Err(PublishError::new(format!(
                "{fname}: unexpected package member {relative}"
            )));
        }
        if !member.directory {
            regular_files.insert(relative.clone());
            if (relative == executable || relative == "LICENSE") && member.size == 0 {
                return Err(PublishError::new(format!("{fname}: {relative} is empty")));
            }
            if relative == executable && !windows && (member.mode & 0o111) == 0 {
                return Err(PublishError::new(format!(
                    "{fname}: anticrumb is not marked executable"
                )));
            }
        }
    }

    let mut missing = Vec::new();
    for req in [executable, "LICENSE"] {
        if !regular_files.contains(req) {
            missing.push(req);
        }
    }
    if !missing.is_empty() {
        missing.sort_unstable();
        return Err(PublishError::new(format!(
            "{fname}: missing required files: {}",
            missing.join(", ")
        )));
    }
    Ok(())
}

fn walk_sorted(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), PublishError> {
    let entries = fs::read_dir(dir)
        .map_err(|e| PublishError::new(format!("cannot scan {}: {e}", dir.display())))?;
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| PublishError::new(format!("cannot scan {}: {e}", dir.display())))?;
        paths.push(entry.path());
    }
    paths.sort();
    for path in paths {
        let meta = fs::symlink_metadata(&path)
            .map_err(|e| PublishError::new(format!("cannot inspect {}: {e}", path.display())))?;
        if meta.file_type().is_symlink() {
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if name.starts_with("anticrumb-")
                && (name.ends_with(".tar.gz") || name.ends_with(".zip"))
            {
                return Err(PublishError::new(format!(
                    "package must be a regular file inside dist: {name}"
                )));
            }
            continue;
        }
        if meta.is_dir() {
            walk_sorted(&path, out)?;
        } else if meta.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

fn is_docker_archive(path: &Path, dist: &Path) -> bool {
    let rel = match path.strip_prefix(dist) {
        Ok(r) => r,
        Err(_) => path,
    };
    for component in rel.components() {
        if let std::path::Component::Normal(name) = component {
            let s = name.to_string_lossy();
            if s.starts_with("docker-") || s == "docker" {
                return true;
            }
        }
    }
    false
}

pub fn discover(dist: &Path) -> Result<BTreeMap<String, PathBuf>, PublishError> {
    discover_filtered(dist, false)
}

pub fn discover_filtered(
    dist: &Path,
    docker_only: bool,
) -> Result<BTreeMap<String, PathBuf>, PublishError> {
    if !dist.exists() {
        return Err(PublishError::new(format!(
            "no release archives in {}; build packages first",
            dist.display()
        )));
    }
    let dist_canon = fs::canonicalize(dist)
        .map_err(|e| PublishError::new(format!("cannot resolve {}: {e}", dist.display())))?;
    let mut files = Vec::new();
    walk_sorted(dist, &mut files)?;

    let mut packages: BTreeMap<String, PathBuf> = BTreeMap::new();
    for path in files {
        if docker_only && !is_docker_archive(&path, dist) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.starts_with("anticrumb-") {
            continue;
        }
        let suffix = if name.ends_with(".tar.gz") {
            ".tar.gz"
        } else if name.ends_with(".zip") {
            ".zip"
        } else {
            continue;
        };
        let target = &name["anticrumb-".len()..name.len() - suffix.len()];
        if target_extension(target) != Some(suffix) {
            return Err(PublishError::new(format!(
                "unsupported package name or extension: {name}"
            )));
        }
        let canon = fs::canonicalize(&path).map_err(|_| {
            PublishError::new(format!(
                "package must be a regular file inside dist: {name}"
            ))
        })?;
        if !canon.starts_with(&dist_canon) {
            return Err(PublishError::new(format!(
                "package must be a regular file inside dist: {name}"
            )));
        }
        if packages.contains_key(target) {
            return Err(PublishError::new(format!(
                "duplicate package for {target}; keep only one archive in dist"
            )));
        }
        validate_archive(&path, target)?;
        packages.insert(target.to_string(), path);
    }

    if packages.is_empty() {
        let msg = if docker_only {
            format!(
                "no docker release archives in {}; build docker packages first (e.g. make docker-linux / make docker-windows)",
                dist.display()
            )
        } else {
            format!(
                "no release archives in {}; build packages first",
                dist.display()
            )
        };
        return Err(PublishError::new(msg));
    }
    Ok(packages)
}

pub fn prepare(root: &Path, dist: &Path) -> Result<(Plan, Vec<PathBuf>), PublishError> {
    prepare_filtered(root, dist, false)
}

pub fn prepare_filtered(
    root: &Path,
    dist: &Path,
    docker_only: bool,
) -> Result<(Plan, Vec<PathBuf>), PublishError> {
    let version = version_from_cargo(root)?;
    let tag = format!("v{version}");
    let packages = discover_filtered(dist, docker_only)?;
    let mut assets = BTreeMap::new();
    let mut uploads = Vec::with_capacity(packages.len());
    for (target, path) in packages {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let sha256 = sha256_file(&path)?;
        let size = fs::metadata(&path)
            .map_err(|e| PublishError::new(format!("cannot stat {}: {e}", path.display())))?
            .len();
        assets.insert(target, AssetPlan { name, sha256, size });
        uploads.push(path);
    }
    Ok((
        Plan {
            version,
            tag,
            assets,
        },
        uploads,
    ))
}

pub fn preflight(runner: &dyn Runner, root: &Path, plan: &Plan) -> Result<String, PublishError> {
    let status = run_command(
        runner,
        &[
            "git",
            "status",
            "--porcelain",
            "--untracked-files=no",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>(),
        root,
    )?;
    if !status.is_empty() {
        return Err(PublishError::new(
            "commit your changes before publishing; the Git working tree must be clean",
        ));
    }
    let branch = run_command(
        runner,
        &["git", "branch", "--show-current"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>(),
        root,
    )?;
    if branch != BRANCH {
        return Err(PublishError::new(format!(
            "publish from the {BRANCH} branch"
        )));
    }
    let head = run_command(
        runner,
        &["git", "rev-parse", "HEAD"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>(),
        root,
    )?;
    let repository = api(runner, &format!("repos/{REPOSITORY}"), root, false)?;
    let can_push = repository
        .get("permissions")
        .and_then(|p| p.get("push"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !can_push {
        return Err(PublishError::new(format!(
            "gh must be authenticated with write access to {REPOSITORY}"
        )));
    }
    let remote = api(
        runner,
        &format!("repos/{REPOSITORY}/git/ref/heads/{BRANCH}"),
        root,
        false,
    )?;
    let remote_sha = remote
        .get("object")
        .and_then(|o| o.get("sha"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if remote_sha != head {
        return Err(PublishError::new(format!(
            "push the current {BRANCH} commit to {REPOSITORY} before publishing"
        )));
    }
    let tag = &plan.tag;
    let plan_tuple = parse_semver_tuple(&plan.version).ok_or_else(|| {
        PublishError::new("Cargo.toml [package].version must be a stable X.Y.Z version")
    })?;
    for release in releases(runner, root)? {
        let tag_name = release
            .get("tag_name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if tag_name == tag {
            return Err(PublishError::new(format!(
                "release {tag} already exists (including drafts); it will not be overwritten"
            )));
        }
        let previous = tag_name.strip_prefix('v').unwrap_or(tag_name);
        let is_draft = release
            .get("draft")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let is_prerelease = release
            .get("prerelease")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !is_draft && !is_prerelease {
            if let Some(prev_tuple) = parse_semver_tuple(previous) {
                if prev_tuple >= plan_tuple {
                    return Err(PublishError::new(format!(
                        "version must be newer than published stable release {tag_name}"
                    )));
                }
            }
        }
    }

    let refs = api(
        runner,
        &format!("repos/{REPOSITORY}/git/matching-refs/tags/{tag}"),
        root,
        false,
    )?;
    if let Some(list) = refs.as_array() {
        for reference in list {
            if reference.get("ref").and_then(Value::as_str) != Some(&format!("refs/tags/{tag}")) {
                continue;
            }
            let Some(mut target_obj) = reference.get("object").cloned() else {
                continue;
            };
            for _ in 0..8 {
                if target_obj.get("type").and_then(Value::as_str) != Some("tag") {
                    break;
                }
                let sha = target_obj
                    .get("sha")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let tag_res = api(
                    runner,
                    &format!("repos/{REPOSITORY}/git/tags/{sha}"),
                    root,
                    false,
                )?;
                target_obj = tag_res.get("object").cloned().unwrap_or(Value::Null);
            }
            if target_obj.get("type").and_then(Value::as_str) != Some("commit")
                || target_obj.get("sha").and_then(Value::as_str) != Some(head.as_str())
            {
                return Err(PublishError::new(format!(
                    "existing tag {tag} does not point to the current commit"
                )));
            }
        }
    }
    Ok(head)
}

pub fn release_commands(
    plan: &Plan,
    uploads: &[PathBuf],
    head: &str,
) -> (Vec<String>, Vec<String>) {
    let tag = &plan.tag;
    let repository = format!("github.com/{REPOSITORY}");
    let mut create = vec![
        "gh".to_string(),
        "release".to_string(),
        "create".to_string(),
        tag.clone(),
    ];
    for path in uploads {
        create.push(path.to_string_lossy().into_owned());
    }
    create.extend([
        "--repo".to_string(),
        repository.clone(),
        "--target".to_string(),
        head.to_string(),
        "--draft".to_string(),
        "--title".to_string(),
        format!("anticrumb {}", plan.version),
        "--generate-notes".to_string(),
    ]);
    let publish = vec![
        "gh".to_string(),
        "release".to_string(),
        "edit".to_string(),
        tag.clone(),
        "--repo".to_string(),
        repository,
        "--draft=false".to_string(),
        "--latest".to_string(),
    ];
    (create, publish)
}

pub fn verify_uploads(
    runner: &dyn Runner,
    root: &Path,
    tag: &str,
    uploads: &[PathBuf],
) -> Result<(), PublishError> {
    let found: Vec<Value> = releases(runner, root)?
        .into_iter()
        .filter(|r| r.get("tag_name").and_then(Value::as_str) == Some(tag))
        .collect();
    if found.len() != 1
        || !found[0]
            .get("draft")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || found[0]
            .get("prerelease")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return Err(PublishError::new(
            "expected one unpublished stable draft after upload; refusing to publish",
        ));
    }
    let mut expected: BTreeMap<String, (u64, String)> = BTreeMap::new();
    for path in uploads {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let size = fs::metadata(path)
            .map_err(|e| PublishError::new(format!("cannot stat {}: {e}", path.display())))?
            .len();
        let digest = format!("sha256:{}", sha256_file(path)?);
        expected.insert(name, (size, digest));
    }
    let actual = found[0]
        .get("assets")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            PublishError::new(
                "draft assets do not match the complete package set; refusing to publish",
            )
        })?;
    let actual_names: BTreeSet<String> = actual
        .iter()
        .filter_map(|a| a.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();
    let expected_names: BTreeSet<String> = expected.keys().cloned().collect();
    if actual.len() != expected.len() || actual_names != expected_names {
        return Err(PublishError::new(
            "draft assets do not match the complete package set; refusing to publish",
        ));
    }
    for asset in actual {
        let name = asset
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (size, digest) = expected.get(name).ok_or_else(|| {
            PublishError::new(
                "draft assets do not match the complete package set; refusing to publish",
            )
        })?;
        let state_ok = asset.get("state").and_then(Value::as_str) == Some("uploaded");
        let size_ok = asset.get("size").and_then(Value::as_u64) == Some(*size);
        if !state_ok || !size_ok {
            return Err(PublishError::new(format!(
                "draft asset is incomplete: {name}"
            )));
        }
        let reported = asset.get("digest").and_then(Value::as_str);
        if !reported.is_some_and(is_valid_sha256_digest) {
            return Err(PublishError::new(format!(
                "draft asset has no valid GitHub SHA-256 digest: {name}; refusing to publish"
            )));
        }
        if reported != Some(digest.as_str()) {
            return Err(PublishError::new(format!(
                "draft asset checksum does not match: {name}; refusing to publish"
            )));
        }
    }
    Ok(())
}

pub fn publish(
    runner: &dyn Runner,
    root: &Path,
    plan: &Plan,
    uploads: &[PathBuf],
    out: &mut dyn Write,
) -> Result<(), PublishError> {
    let head = preflight(runner, root, plan)?;
    let temp = tempfile::Builder::new()
        .prefix("anticrumb-release-")
        .tempdir()
        .map_err(|e| {
            PublishError::new(format!("cannot create temporary staging directory: {e}"))
        })?;
    let mut staged = Vec::with_capacity(uploads.len());
    for source in uploads {
        let name = source
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let destination = temp.path().join(name);
        let expected = sha256_file(source)?;
        fs::copy(source, &destination)
            .map_err(|e| PublishError::new(format!("cannot stage {}: {e}", source.display())))?;
        if sha256_file(&destination)? != expected {
            return Err(PublishError::new(format!(
                "{name} changed while preparing upload; try again"
            )));
        }
        staged.push(destination);
    }
    for asset in plan.assets.values() {
        let snapshot = temp.path().join(&asset.name);
        let snap_size = fs::metadata(&snapshot)
            .map_err(|e| PublishError::new(format!("cannot stat {}: {e}", snapshot.display())))?
            .len();
        if sha256_file(&snapshot)? != asset.sha256 || snap_size != asset.size {
            return Err(PublishError::new(format!(
                "{} changed after validation; try again",
                asset.name
            )));
        }
    }
    let (create, finalize) = release_commands(plan, &staged, &head);
    ensure_pages(runner, root, out)?;
    let flow = (|| -> Result<(), PublishError> {
        run_command(runner, &create, root)?;
        verify_uploads(runner, root, &plan.tag, &staged)?;
        run_command(runner, &finalize, root)?;
        Ok(())
    })();
    if let Err(error) = flow {
        return Err(PublishError::new(format!(
            "{error}\nInspect https://github.com/{REPOSITORY}/releases before retrying. \
            A failed upload may have left a draft; existing releases are never overwritten."
        )));
    }
    let _ = writeln!(
        out,
        "Published https://github.com/{REPOSITORY}/releases/tag/{}",
        plan.tag
    );
    Ok(())
}

fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if !arg.is_empty()
                && arg
                    .bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'@' | b'%' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'-'))
            {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', "'\"'\"'"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn run_with_runner(
    runner: &dyn Runner,
    root: &Path,
    args: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let mut dist = root.join("dist");
    let mut dry_run = false;
    let mut docker_only = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            "--docker-only" => {
                docker_only = true;
                i += 1;
            }
            "--dist" => match crate::next_flag_value(args, &mut i, "--dist") {
                Ok(val) => dist = PathBuf::from(val),
                Err(_) => {
                    let _ = writeln!(err, "Publish failed: --dist requires a directory argument");
                    return 2;
                }
            },
            "-h" | "--help" => {
                let _ = writeln!(
                    out,
                    "Usage: publish [--dist DIR] [--docker-only] [--dry-run]"
                );
                return 0;
            }
            other => {
                let _ = writeln!(err, "Publish failed: unknown argument {other}");
                return 2;
            }
        }
    }
    let dist_resolved = if dist.is_absolute() {
        dist
    } else {
        root.join(dist)
    };
    let result = (|| -> Result<(), PublishError> {
        let (plan, uploads) = prepare_filtered(root, &dist_resolved, docker_only)?;
        let _ = writeln!(
            out,
            "Prepared {}: {} platform package(s)",
            plan.tag,
            plan.assets.len()
        );
        for path in &uploads {
            let _ = writeln!(out, "  {}", path.display());
        }
        if dry_run {
            let _ = writeln!(
                out,
                "Dry run: no GitHub requests, tags, releases or uploads performed."
            );
            let _ = writeln!(
                out,
                "After clean master/remote/tag checks, publish would run:"
            );
            let _ = writeln!(
                out,
                "  Configure GitHub Pages to publish the master branch from /(root)."
            );
            let (create, finalize) = release_commands(&plan, &uploads, "<verified-master-commit>");
            let _ = writeln!(out, "  {}", shell_join(&create));
            let _ = writeln!(out, "  {}", shell_join(&finalize));
            let _ = writeln!(
                out,
                "Every draft asset must match its local name, size and GitHub SHA-256 digest before publication."
            );
        } else {
            publish(runner, root, &plan, &uploads, out)?;
        }
        Ok(())
    })();

    if let Err(error) = result {
        let _ = writeln!(err, "Publish failed: {error}");
        return 1;
    }
    0
}

pub fn run_cli(root: &Path, args: &[String]) -> i32 {
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    run_with_runner(&RealRunner, root, args, &mut stdout, &mut stderr)
}
