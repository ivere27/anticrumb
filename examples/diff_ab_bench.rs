//! A/B benchmark dump & metrics extractor for `tree_sitter_diff` ablation experiments.
//!
//! Usage:
//!   cargo run --example diff_ab_bench -- \
//!     --repo /path/to/repo \
//!     --source commit:<sha> | diff | staged \
//!     --arm A | B1 | B2 | B3 | <custom_hybrid_spec> \
//!     [--budget-kib 512] \
//!     [--out /path/to/output.patch]

use anticrumb::tree_sitter_diff::{
    generate_expanded_diff_with_config, GitDiffSource, HybridConfig,
};
use serde::Serialize;
use std::path::PathBuf;
use std::time::Instant;

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

fn parse_arm(arm: &str) -> HybridConfig {
    match arm.to_ascii_uppercase().as_str() {
        "A" | "BASELINE" | "OFF" => HybridConfig::off(),
        "B1" => HybridConfig {
            layer1: true,
            layer2: true,
            layer3_local: false,
            layer3_cross: false,
        },
        "B2" => HybridConfig {
            layer1: true,
            layer2: true,
            layer3_local: true,
            layer3_cross: false,
        },
        "B3" | "FULL" | "ALL" => HybridConfig::full(),
        _ => HybridConfig::parse(Some(arm)),
    }
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

    let l3_marker = "\n=== External AST Context (1-hop, unmodified) ===\n";
    let (before_l3, l3_part) = match patch.find(l3_marker) {
        Some(idx) => (&patch[..idx], &patch[idx..]),
        None => (patch, ""),
    };
    let layer3_bytes = l3_part.len();

    let l1_start = before_l3.find("[Symbol Index:");
    let first_diff = before_l3.find("diff --git ");
    let layer1_bytes = match (l1_start, first_diff) {
        (Some(s), Some(d)) if s < d => {
            // Layer 1 ends at first blank line before `[patch-budget:` or `diff --git `
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
        } else if !cur_scope.is_empty()
            && line
                .strip_prefix('L')
                .and_then(|rest| rest.split_once(": "))
                .is_some_and(|(num, _)| !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()))
        {
            if cur_scope == "local" {
                layer3_local_entries += 1;
            } else if cur_scope == "cross" {
                layer3_cross_entries += 1;
            }
            layer3_symbols.push(format!(
                "[{cur_scope}] {cur_file}: {}",
                line.chars().take(80).collect::<String>()
            ));
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

    let first_chunk_iter = before_l3
        .strip_prefix("diff --git ")
        .into_iter()
        .chain(before_l3.split("\ndiff --git ").skip(1));
    for chunk in first_chunk_iter {
        if chunk.contains("[omitted: ") || chunk.contains("[omitted: large new test file ") {
            pre_omitted_files += 1;
            continue;
        }
        if chunk.contains("[omitted inline tests: L") {
            inline_tests_omitted_files += 1;
        }
        if chunk.contains("\n[degraded to -U3]\n") {
            degraded_u3_files += 1;
        }
        if chunk.contains("\n[degraded to -U0]\n") {
            degraded_u0_files += 1;
        }
        if chunk.contains("\n[skeletonized:") {
            skeletonized_files += 1;
        }
        if chunk.contains("... [truncated due to patch budget:") {
            truncated_files += 1;
        }
        if chunk.contains("[omitted due to patch budget:") {
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
    let mut arm = String::from("A");
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

    let hybrid = parse_arm(&arm);
    let max_bytes = budget_kib.saturating_mul(1024);
    let mut warnings = Vec::new();
    let started = Instant::now();
    let patch_res = if let Some(sha) = source_arg.strip_prefix("commit:") {
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
        assert_eq!(m.layer3_local_entries, 0);
        assert_eq!(m.layer3_symbols.len(), 2);
    }
}
