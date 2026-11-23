//! A/B benchmark dump & metrics extractor for `tree_sitter_diff` ablation & external baseline experiments.
//!
//! Usage:
//!   cargo run --example diff_ab_bench -- \
//!     --repo /path/to/repo \
//!     --source commit:<sha> | diff | staged \
//!     --arm P0 | P1 | P2 | A | B1 | B2 | B3 | B4 | <custom_hybrid_spec> \
//!     [--budget-kib 512] \
//!     [--out /path/to/output.patch]

use anticrumb::tree_sitter_diff::{
    generate_expanded_diff_with_config, is_lockfile_or_generated, GitDiffSource, HybridConfig,
};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

const MAX_SNAPSHOT_FILE_BYTES: usize = 1024 * 1024;
const L3_MARKER: &str = "\n=== External AST Context (1-hop, unmodified) ===\n";
const P2_FILES_MARKER: &str = "\n=== Full Post-Change Files (snapshot) ===\n";

#[derive(Serialize)]
struct BenchMetrics {
    arm: String,
    source: String,
    budget_kib: usize,
    total_bytes: usize,
    est_tokens_div4: usize,
    total_lines: usize,
    layer1_bytes: usize,
    layer2_bytes: usize,
    layer3_bytes: usize,
    layer3_local_entries: usize,
    layer3_cross_entries: usize,
    layer3_caller_entries: usize,
    layer3_symbols: Vec<String>,
    clamped_hunks: usize,
    sig_only_hunks: usize,
    opaque_more_hunks: usize,
    named_more_hunks: usize,
    pre_omitted_files: usize,
    inline_tests_omitted_files: usize,
    degraded_u3_files: usize,
    degraded_u0_files: usize,
    skeletonized_files: usize,
    truncated_files: usize,
    budget_omitted_files: usize,
    latency_ms: f64,
    warnings: Vec<String>,
}

fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn is_external_baseline_arm(arm: &str) -> bool {
    matches!(arm.to_ascii_uppercase().as_str(), "P0" | "P1" | "P2")
}

fn parse_arm(arm: &str) -> HybridConfig {
    match arm.to_ascii_uppercase().as_str() {
        "A" | "BASELINE" | "OFF" => HybridConfig::off(),
        "B1" => HybridConfig {
            layer1: true,
            layer2: true,
            layer3_local: false,
            layer3_cross: false,
            layer3_callers: false,
        },
        "B2" => HybridConfig {
            layer1: true,
            layer2: true,
            layer3_local: true,
            layer3_cross: false,
            layer3_callers: false,
        },
        "B3" => HybridConfig {
            layer1: true,
            layer2: true,
            layer3_local: true,
            layer3_cross: true,
            layer3_callers: false,
        },
        "B4" | "FULL" | "ALL" => HybridConfig::full(),
        _ => HybridConfig::parse(Some(arm)),
    }
}

fn git_baseline_cmd(repo: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo).args([
        "--literal-pathspecs",
        "-c",
        "core.quotePath=false",
        "-c",
        "diff.noprefix=false",
        "-c",
        "diff.mnemonicPrefix=false",
    ]);
    cmd
}

fn run_git_raw_diff(repo: &Path, source_arg: &str, context_flag: &str) -> Result<String, String> {
    let mut cmd = git_baseline_cmd(repo);
    if let Some(sha) = source_arg.strip_prefix("commit:") {
        cmd.args([
            "show",
            "-p",
            context_flag,
            "-M",
            "--diff-algorithm=histogram",
            "--first-parent",
            "--no-color",
            "--no-ext-diff",
            "--stat=120",
            "--stat-name-width=90",
            sha,
        ]);
    } else if source_arg == "staged" {
        cmd.args([
            "diff",
            context_flag,
            "-M",
            "--diff-algorithm=histogram",
            "--no-color",
            "--no-ext-diff",
            "--staged",
        ]);
    } else {
        cmd.args([
            "diff",
            context_flag,
            "-M",
            "--diff-algorithm=histogram",
            "--no-color",
            "--no-ext-diff",
        ]);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("failed to run git baseline diff: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git baseline diff failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn is_bench_test_path(p: &Path) -> bool {
    let norm = p.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
    let segs: Vec<&str> = norm.split('/').collect();
    if segs.iter().any(|&s| {
        matches!(
            s,
            "test" | "tests" | "__tests__" | "spec" | "specs" | "fixtures" | "testdata"
        )
    }) {
        return true;
    }
    let fname = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    fname.starts_with("test_")
        || fname.ends_with("_test.rs")
        || fname.ends_with("_test.go")
        || fname.ends_with("_test.py")
        || fname.ends_with("_spec.rb")
        || fname.ends_with(".test.ts")
        || fname.ends_with(".test.tsx")
        || fname.ends_with(".test.js")
        || fname.ends_with(".test.jsx")
        || fname.ends_with(".spec.ts")
        || fname.ends_with(".spec.tsx")
        || fname.ends_with(".spec.js")
        || fname.ends_with(".spec.jsx")
        || fname.ends_with("test.java")
        || fname.ends_with("tests.java")
        || fname.ends_with("test.cs")
        || fname.ends_with("tests.cs")
}

fn list_changed_non_deleted_paths(repo: &Path, source_arg: &str) -> Result<Vec<PathBuf>, String> {
    let mut cmd = git_baseline_cmd(repo);
    if let Some(sha) = source_arg.strip_prefix("commit:") {
        cmd.args([
            "show",
            "--first-parent",
            "--name-only",
            "-z",
            "--diff-filter=ACMRT",
            "--format=",
            sha,
        ]);
    } else if source_arg == "staged" {
        cmd.args([
            "diff",
            "--staged",
            "--name-only",
            "-z",
            "--diff-filter=ACMRT",
        ]);
    } else {
        cmd.args(["diff", "--name-only", "-z", "--diff-filter=ACMRT"]);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("failed to list changed paths for P2: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git name-only failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mut seen = std::collections::HashSet::new();
    let mut paths = Vec::new();
    for raw in out.stdout.split(|&b| b == 0).filter(|s| !s.is_empty()) {
        let s = String::from_utf8_lossy(raw)
            .trim_matches(['\r', '\n'])
            .to_string();
        if s.is_empty() {
            continue;
        }
        let p = PathBuf::from(s);
        if !is_lockfile_or_generated(&p) && !is_bench_test_path(&p) && seen.insert(p.clone()) {
            paths.push(p);
        }
    }
    Ok(paths)
}

fn read_snapshot_single_file(repo: &Path, source_arg: &str, path: &Path) -> Option<String> {
    let norm = path.to_string_lossy().replace('\\', "/");
    if let Some(sha) = source_arg.strip_prefix("commit:") {
        let spec = format!("{sha}:{norm}");
        let out = git_baseline_cmd(repo)
            .args(["cat-file", "blob", &spec])
            .output()
            .ok()?;
        if !out.status.success()
            || out.stdout.len() > MAX_SNAPSHOT_FILE_BYTES
            || out.stdout.contains(&0)
        {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else if source_arg == "staged" {
        let spec = format!(":0:{norm}");
        let out = git_baseline_cmd(repo)
            .args(["cat-file", "blob", &spec])
            .output()
            .ok()?;
        if !out.status.success()
            || out.stdout.len() > MAX_SNAPSHOT_FILE_BYTES
            || out.stdout.contains(&0)
        {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let full = repo.join(path);
        let meta = std::fs::symlink_metadata(&full).ok()?;
        if meta.file_type().is_symlink()
            || meta.is_dir()
            || meta.len() > MAX_SNAPSHOT_FILE_BYTES as u64
        {
            return None;
        }
        let bytes = std::fs::read(&full).ok()?;
        if bytes.len() > MAX_SNAPSHOT_FILE_BYTES || bytes.contains(&0) {
            return None;
        }
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn clamp_raw_baseline_to_budget(
    raw: String,
    max_bytes: usize,
    arm: &str,
    warnings: &mut Vec<String>,
) -> String {
    if raw.len() <= max_bytes {
        return raw;
    }
    let note = format!("\n... [truncated at {arm} budget: {max_bytes} bytes]\n");
    let keep_cap = max_bytes.saturating_sub(note.len());
    let cut = truncate_utf8(&raw, keep_cap);
    warnings.push(format!(
        "baseline {arm} diff ({} bytes) exceeded budget ({max_bytes} bytes) and was truncated",
        raw.len()
    ));
    format!("{cut}{note}")
}

fn generate_external_baseline(
    repo: &Path,
    source_arg: &str,
    arm: &str,
    max_bytes: usize,
    warnings: &mut Vec<String>,
) -> Result<String, String> {
    match arm.to_ascii_uppercase().as_str() {
        "P0" => {
            let raw = run_git_raw_diff(repo, source_arg, "-U3")?;
            Ok(clamp_raw_baseline_to_budget(raw, max_bytes, "P0", warnings))
        }
        "P1" => {
            let raw = run_git_raw_diff(repo, source_arg, "-W")?;
            Ok(clamp_raw_baseline_to_budget(raw, max_bytes, "P1", warnings))
        }
        "P2" => {
            let p0_raw = run_git_raw_diff(repo, source_arg, "-U3")?;
            if p0_raw.len() > max_bytes {
                return Ok(clamp_raw_baseline_to_budget(
                    p0_raw, max_bytes, "P2", warnings,
                ));
            }
            let mut out = p0_raw;
            let paths = list_changed_non_deleted_paths(repo, source_arg)?;
            if !paths.is_empty() && max_bytes.saturating_sub(out.len()) > P2_FILES_MARKER.len() + 64
            {
                let mut files_buf = String::from(P2_FILES_MARKER);
                let mut included_any = false;
                for p in paths {
                    let Some(content) = read_snapshot_single_file(repo, source_arg, &p) else {
                        continue;
                    };
                    let norm = p.to_string_lossy().replace('\\', "/");
                    let body = if content.ends_with('\n') {
                        content
                    } else {
                        format!("{content}\n")
                    };
                    let block = format!("--- file: {norm} ---\n{body}");
                    if out.len() + files_buf.len() + block.len() <= max_bytes {
                        files_buf.push_str(&block);
                        included_any = true;
                    }
                }
                if included_any {
                    out.push_str(&files_buf);
                }
            }
            Ok(out)
        }
        other => Err(format!("unsupported external baseline arm: {other}")),
    }
}

fn find_col0_diff_start(s: &str) -> Option<usize> {
    let mut offset = 0usize;
    for line in s.split_inclusive('\n') {
        if line.starts_with("diff --git ")
            || line.starts_with("diff --cc ")
            || line.starts_with("diff --combined ")
        {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

fn split_diff_chunks_for_metrics(diff_body: &str) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut start_opt: Option<usize> = None;
    let mut offset = 0usize;
    for line in diff_body.split_inclusive('\n') {
        let is_start = line.starts_with("diff --git ")
            || line.starts_with("diff --cc ")
            || line.starts_with("diff --combined ");
        if is_start {
            if let Some(prev_start) = start_opt {
                chunks.push(&diff_body[prev_start..offset]);
            }
            start_opt = Some(offset);
        }
        offset += line.len();
    }
    if let Some(last_start) = start_opt {
        chunks.push(&diff_body[last_start..]);
    }
    chunks
}

fn analyze_patch(
    arm: &str,
    source: &str,
    budget_kib: usize,
    patch: &str,
    latency_ms: f64,
    warnings: Vec<String>,
) -> BenchMetrics {
    let total_bytes = patch.len();
    let total_lines = patch.lines().count();

    let l3_idx_opt = patch.find(L3_MARKER);
    let p2_idx_opt = patch.find(P2_FILES_MARKER);
    let tail_cut = match (l3_idx_opt, p2_idx_opt) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    let before_l3 = match tail_cut {
        Some(idx) => &patch[..idx],
        None => patch,
    };
    let l3_part = match (l3_idx_opt, p2_idx_opt) {
        (Some(l3_idx), Some(p2_idx)) if p2_idx > l3_idx => &patch[l3_idx..p2_idx],
        (Some(l3_idx), _) => &patch[l3_idx..],
        (None, _) => "",
    };
    let layer3_bytes = l3_part.len();

    let l1_start = before_l3.find("[Symbol Index:");
    let first_diff = find_col0_diff_start(before_l3);
    let layer1_bytes = match (l1_start, first_diff) {
        (Some(s), Some(d)) if s < d => {
            let slice = &before_l3[s..d];
            if let Some(b_idx) = slice.find("[patch-budget:") {
                b_idx
            } else {
                slice.len()
            }
        }
        (Some(s), None) => before_l3.len() - s,
        _ => 0,
    };
    let layer2_bytes = before_l3
        .len()
        .saturating_sub(first_diff.unwrap_or(before_l3.len()));

    let mut layer3_local_entries = 0usize;
    let mut layer3_cross_entries = 0usize;
    let mut layer3_caller_entries = 0usize;
    let mut layer3_symbols = Vec::new();
    let mut cur_scope = "";
    let mut cur_file = String::new();
    let mut prev_was_loc = false;
    for line in l3_part.lines() {
        if let Some(rest) = line.strip_prefix("--- same-file: ") {
            cur_scope = "local";
            cur_file = rest.trim_end_matches(" ---").to_string();
            prev_was_loc = false;
        } else if let Some(rest) = line.strip_prefix("--- cross-file: ") {
            cur_scope = "cross";
            cur_file = rest.trim_end_matches(" ---").to_string();
            prev_was_loc = false;
        } else if line.starts_with("// L") && line.contains("-L") {
            if cur_scope == "local" {
                layer3_local_entries += 1;
            } else if cur_scope == "cross" {
                layer3_cross_entries += 1;
            }
            prev_was_loc = true;
        } else if prev_was_loc {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                layer3_symbols.push(format!(
                    "[{cur_scope}] {cur_file}: {}",
                    trimmed.chars().take(80).collect::<String>()
                ));
                prev_was_loc = false;
            }
        } else if !cur_scope.is_empty() {
            if let Some((num, body)) = line
                .strip_prefix('L')
                .and_then(|rest| rest.split_once(": "))
            {
                if !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) {
                    if cur_scope == "local" {
                        layer3_local_entries += 1;
                    } else if cur_scope == "cross" {
                        if body.starts_with("[caller of ") {
                            layer3_caller_entries += 1;
                        } else {
                            layer3_cross_entries += 1;
                        }
                    }
                    layer3_symbols.push(format!(
                        "[{cur_scope}] {cur_file}: {}",
                        line.chars().take(80).collect::<String>()
                    ));
                }
            }
        }
    }

    let mut clamped_hunks = 0usize;
    let mut sig_only_hunks = 0usize;
    let mut opaque_more_hunks = 0usize;
    let mut named_more_hunks = 0usize;
    for line in before_l3.lines() {
        if line.starts_with("@@ ") {
            if line.contains("[clamped]") {
                clamped_hunks += 1;
            }
            if line.contains("[sig-only;") {
                sig_only_hunks += 1;
            }
            if line.contains(" more)") {
                opaque_more_hunks += 1;
            }
            if line.contains(" [+ ") {
                named_more_hunks += 1;
            }
        }
    }

    let mut pre_omitted_files = 0usize;
    let mut inline_tests_omitted_files = 0usize;
    let mut degraded_u3_files = 0usize;
    let mut degraded_u0_files = 0usize;
    let mut skeletonized_files = 0usize;
    let mut truncated_files = 0usize;
    let mut budget_omitted_files = 0usize;

    for chunk in split_diff_chunks_for_metrics(before_l3) {
        let mut is_pre_omitted = false;
        let mut has_inline = false;
        let mut has_u3 = false;
        let mut has_u0 = false;
        let mut has_skel = false;
        let mut has_trunc = false;
        let mut has_omit = false;
        let mut in_header = true;

        for line in chunk.lines() {
            if line.starts_with("@@ ") || line.starts_with("@@@ ") {
                in_header = false;
            }
            if in_header {
                if line.starts_with("[omitted: ")
                    || line.starts_with("[omitted: large new test file ")
                {
                    is_pre_omitted = true;
                } else if line == "[degraded to -U3]" {
                    has_u3 = true;
                } else if line == "[degraded to -U0]" {
                    has_u0 = true;
                } else if line.starts_with("[skeletonized:") {
                    has_skel = true;
                } else if line.starts_with("[omitted due to patch budget:") {
                    has_omit = true;
                }
            }
            if line.starts_with("[omitted inline tests: L") {
                has_inline = true;
            } else if line.starts_with("... [truncated due to patch budget:") {
                has_trunc = true;
            } else if line.starts_with("[omitted due to patch budget:") {
                has_omit = true;
            }
        }

        if is_pre_omitted {
            pre_omitted_files += 1;
            continue;
        }
        if has_inline {
            inline_tests_omitted_files += 1;
        }
        if has_u3 {
            degraded_u3_files += 1;
        }
        if has_u0 {
            degraded_u0_files += 1;
        }
        if has_skel {
            skeletonized_files += 1;
        }
        if has_trunc {
            truncated_files += 1;
        }
        if has_omit {
            budget_omitted_files += 1;
        }
    }

    BenchMetrics {
        arm: arm.to_string(),
        source: source.to_string(),
        budget_kib,
        total_bytes,
        est_tokens_div4: total_bytes.div_ceil(4),
        total_lines,
        layer1_bytes,
        layer2_bytes,
        layer3_bytes,
        layer3_local_entries,
        layer3_cross_entries,
        layer3_caller_entries,
        layer3_symbols,
        clamped_hunks,
        sig_only_hunks,
        opaque_more_hunks,
        named_more_hunks,
        pre_omitted_files,
        inline_tests_omitted_files,
        degraded_u3_files,
        degraded_u0_files,
        skeletonized_files,
        truncated_files,
        budget_omitted_files,
        latency_ms,
        warnings,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut repo = PathBuf::from(".");
    let mut source_arg = String::from("diff");
    let mut arm = String::from("B4");
    let mut budget_kib: usize = 512;
    let mut out_path: Option<PathBuf> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--repo" if i + 1 < args.len() => {
                repo = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            "--source" if i + 1 < args.len() => {
                source_arg = args[i + 1].clone();
                i += 2;
            }
            "--arm" if i + 1 < args.len() => {
                arm = args[i + 1].clone();
                i += 2;
            }
            "--budget-kib" if i + 1 < args.len() => {
                budget_kib = args[i + 1].parse().expect("valid --budget-kib integer");
                i += 2;
            }
            "--out" if i + 1 < args.len() => {
                out_path = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            other => {
                eprintln!("Unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let max_bytes = budget_kib.saturating_mul(1024);
    let mut warnings = Vec::new();
    let started = Instant::now();
    let patch_res = if is_external_baseline_arm(&arm) {
        generate_external_baseline(&repo, &source_arg, &arm, max_bytes, &mut warnings)
    } else {
        let hybrid = parse_arm(&arm);
        let res = if let Some(sha) = source_arg.strip_prefix("commit:") {
            generate_expanded_diff_with_config(
                &repo,
                GitDiffSource::Commit(sha),
                max_bytes,
                hybrid,
                &mut warnings,
            )
        } else if source_arg == "staged" {
            generate_expanded_diff_with_config(
                &repo,
                GitDiffSource::Staged,
                max_bytes,
                hybrid,
                &mut warnings,
            )
        } else {
            generate_expanded_diff_with_config(
                &repo,
                GitDiffSource::Unstaged,
                max_bytes,
                hybrid,
                &mut warnings,
            )
        };
        res.map_err(|e| e.to_string())
    };
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;

    let patch = match patch_res {
        Ok(p) => p,
        Err(e) => {
            eprintln!("diff_ab_bench failed: {e}");
            std::process::exit(1);
        }
    };

    if let Some(ref p) = out_path {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("create dump dir");
        }
        std::fs::write(p, patch.as_bytes()).expect("write patch dump");
    }

    let metrics = analyze_patch(&arm, &source_arg, budget_kib, &patch, latency_ms, warnings);
    println!(
        "{}",
        serde_json::to_string_pretty(&metrics).expect("serialize metrics")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_analyze_patch_does_not_count_type_fields_as_layer3_entries() {
        let patch = concat!(
            "[Symbol Index: 1 file, +2/-1]\n",
            "  M pkg/order.go (+2/-1) — Create\n\n",
            "diff --git a/pkg/order.go b/pkg/order.go\n",
            "index 1111111..2222222 100644\n",
            "--- a/pkg/order.go\n",
            "+++ b/pkg/order.go\n",
            "@@ -10,1 +10,2 @@ func Create() {\n",
            "+    x := 1\n",
            "\n=== External AST Context (1-hop, unmodified) ===\n",
            "--- cross-file: pkg/policy.go ---\n",
            "// L12-L16\n",
            "type Policy struct {\n",
            "    Limit: int\n",
            "    Lock: sync.Mutex\n",
            "}\n",
            "L22: func Validate(p *Policy) bool\n",
        );
        let m = analyze_patch("B3", "diff", 512, patch, 1.0, Vec::new());
        assert_eq!(m.layer3_cross_entries, 2);
        assert_eq!(m.layer3_caller_entries, 0);
        assert_eq!(m.layer3_local_entries, 0);
        assert_eq!(m.layer3_symbols.len(), 2);
    }

    #[test]
    fn test_analyze_patch_splits_callers_diff_cc_and_ignores_prose_markers() {
        let patch = concat!(
            "[Symbol Index: 2 files, +3/-1]\n",
            "  M src/a.rs (+2/-1) — foo\n",
            "  M src/b.rs (+1/-0) — bar\n\n",
            "diff --cc src/a.rs\n",
            "index 1111111..2222222\n",
            "--- a/src/a.rs\n",
            "+++ b/src/a.rs\n",
            "@@ -1,1 +1,2 @@ fn foo()\n",
            "+ let _doc = \"[omitted: fake prose] [degraded to -U3]\";\n",
            "diff --git a/src/b.rs b/src/b.rs\n",
            "index 1111111..2222222 100644\n",
            "[degraded to -U3]\n",
            "--- a/src/b.rs\n",
            "+++ b/src/b.rs\n",
            "@@ -1,1 +1,2 @@ fn bar()\n",
            "+ let y = 2;\n",
            "\n=== External AST Context (1-hop, unmodified) ===\n",
            "--- same-file: src/a.rs ---\n",
            "// L1-L1\n",
            "pub const LIMIT: u8 = 5;\n",
            "--- cross-file: src/ext.rs ---\n",
            "L10: pub fn ext_fn(x: u8) -> bool\n",
            "--- cross-file: src/caller.rs ---\n",
            "L25: [caller of foo] pub fn run_all() -> foo(1)\n",
            "\n=== Full Post-Change Files (snapshot) ===\n",
            "--- file: src/a.rs ---\n",
            "fn foo() {}\n",
        );
        let m = analyze_patch("B4", "diff", 512, patch, 1.0, Vec::new());
        assert_eq!(m.layer3_local_entries, 1);
        assert_eq!(m.layer3_cross_entries, 1);
        assert_eq!(m.layer3_caller_entries, 1);
        assert_eq!(
            m.pre_omitted_files, 0,
            "Diff body string prose must not count"
        );
        assert_eq!(
            m.degraded_u3_files, 1,
            "Only header marker in src/b.rs counts"
        );
        assert!(
            m.layer2_bytes < patch.len() / 2,
            "P2 trailing files excluded from layer2_bytes"
        );
    }

    #[test]
    fn test_parse_arm_b3_vs_b4() {
        let b3 = parse_arm("B3");
        assert!(b3.layer1 && b3.layer2 && b3.layer3_local && b3.layer3_cross && !b3.layer3_callers);
        let b4 = parse_arm("B4");
        assert_eq!(b4, HybridConfig::full());
        assert!(is_external_baseline_arm("p0"));
        assert!(is_external_baseline_arm("P2"));
        assert!(!is_external_baseline_arm("B4"));
    }
}
