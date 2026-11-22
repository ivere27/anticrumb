#[cfg(feature = "tree-sitter")]
use super::{
    ast_compact, check_primary_definition, find_enclosing_definition_detail, signature,
    test_scope::{is_ast_node_attr_or_comment, is_attribute_or_comment_only},
    SupportedLanguage, ATTR_ONLY_MAX_EXPANSION_LINES, CLAMPED_CONTEXT_LINES,
    MAX_TREESITTER_FILE_BYTES, TREESITTER_PARSE_TIMEOUT_MICROS,
};
use super::{
    get_repo_root, git_cmd, is_test_file_path, resolve_patch_budget_bytes, unquote_c_style,
    GitDiffSource,
};
use crate::domain::{Result, ServiceError};
#[cfg(feature = "tree-sitter")]
use rayon::prelude::*;
#[cfg(feature = "tree-sitter")]
use sha2::{Digest, Sha256};
#[cfg(feature = "tree-sitter")]
use std::collections::HashMap;
use std::path::{Path, PathBuf};
#[cfg(feature = "tree-sitter")]
use tree_sitter::Parser;

#[cfg(feature = "tree-sitter")]
const LOOKAHEAD_MAX_LINES: usize = 12;
#[cfg(feature = "tree-sitter")]
const LARGE_NEW_TEST_SUMMARY_MIN_LINES: usize = 500;
#[cfg(feature = "tree-sitter")]
const MIN_MOVED_BLOCK_NON_EMPTY_LINES: usize = 8;
#[cfg(feature = "tree-sitter")]
const MIN_MOVED_BLOCK_BYTES: usize = 200;

#[cfg(feature = "tree-sitter")]
#[derive(Clone, Debug)]
struct ParsedHunk {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
    signature: String,
    lines: Vec<String>,
    moved_summary: Option<String>,
}

pub(super) fn parse_hunk_header(line: &str) -> Option<(usize, usize, usize, usize, String)> {
    if !line.starts_with("@@ -") {
        return None;
    }
    let rest = &line[4..];
    let (old_part, rest) = rest.split_once(" +")?;
    let (new_part, sig_part) = rest.split_once(" @@")?;

    let (old_start, old_count) = if let Some((s, c)) = old_part.split_once(',') {
        (s.parse::<usize>().ok()?, c.parse::<usize>().ok()?)
    } else {
        (old_part.parse::<usize>().ok()?, 1)
    };

    let (new_start, new_count) = if let Some((s, c)) = new_part.split_once(',') {
        (s.parse::<usize>().ok()?, c.parse::<usize>().ok()?)
    } else {
        (new_part.parse::<usize>().ok()?, 1)
    };

    let sig = sig_part.trim().to_string();
    Some((old_start, old_count, new_start, new_count, sig))
}

#[cfg(feature = "tree-sitter")]
struct MergedHunk {
    exp_start: usize,
    exp_end: usize,
    full_start: usize,
    full_end: usize,
    is_inline_test: bool,
    sigs: Vec<(String, bool, usize, usize)>,
    hunks: Vec<ParsedHunk>,
}

/// Helper to extract path from a diff git header token (e.g. `diff --git <src> <dst>`).
fn parse_diff_git_header(rest: &str) -> Option<(PathBuf, Option<PathBuf>)> {
    let (a_str, b_str) = if rest.starts_with('"') {
        let mut escaped = false;
        let mut end_quote = None;
        for (idx, b) in rest.bytes().enumerate().skip(1) {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                end_quote = Some(idx);
                break;
            }
        }
        let end_idx = end_quote?;
        let a = &rest[..=end_idx];
        let b = rest[end_idx + 1..].strip_prefix(' ')?;
        (a, b)
    } else {
        // Try symmetric central space split first for non-rename diffs (e.g. "a/path b/path")
        let symmetric_split = if rest.len() >= 5 && rest.len() % 2 == 1 {
            let mid = rest.len() / 2;
            if rest.as_bytes()[mid] == b' ' {
                let left = &rest[..mid];
                let right = &rest[mid + 1..];
                if left.starts_with("a/") && right.starts_with("b/") && left[2..] == right[2..] {
                    Some((left, right))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        if let Some(pair) = symmetric_split {
            pair
        } else if let Some(idx) = rest.find(" \"b/") {
            (&rest[..idx], &rest[idx + 1..])
        } else {
            let idx = rest.rfind(" b/")?;
            (&rest[..idx], &rest[idx + 1..])
        }
    };

    let clean_a = a_str.strip_suffix('\t').unwrap_or(a_str);
    let clean_b = b_str.strip_suffix('\t').unwrap_or(b_str);
    let unquoted_a = unquote_c_style(clean_a);
    let unquoted_b = unquote_c_style(clean_b);

    let target_path = unquoted_b.strip_prefix("b/").map(PathBuf::from)?;
    let old_path = unquoted_a.strip_prefix("a/").map(PathBuf::from);
    Some((target_path, old_path))
}

pub(super) fn is_special_git_mode_line(line: &str) -> bool {
    // 120000 = symlink, 160000 = submodule (gitlink)
    let is_mode = line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("old mode ")
        || line.starts_with("new mode ")
        || line.starts_with("index ");
    if !is_mode {
        return false;
    }
    matches!(
        line.split_whitespace().last(),
        Some("120000") | Some("160000")
    )
}

/// Parses the target file path, old path (if renamed), status flags, and special mode flags (symlinks, submodules)
/// from a single file's unified diff chunk. Returns (target_path, old_path, is_deleted, is_new, is_special_mode).
pub(super) fn extract_target_path_from_diff_chunk(
    chunk: &str,
) -> Option<(PathBuf, Option<PathBuf>, bool, bool, bool)> {
    let mut is_deleted = false;
    let mut is_new = false;
    let mut is_special_mode = false;
    let mut target_path = None;
    let mut old_path = None;

    for raw_line in chunk.split_inclusive('\n') {
        let line = raw_line.trim_end_matches(['\r', '\n']);
        if line.starts_with("@@ ") {
            break;
        }
        if is_special_git_mode_line(line) {
            is_special_mode = true;
        }
        if line == "+++ /dev/null" || line.starts_with("deleted file mode") {
            is_deleted = true;
        } else if line == "--- /dev/null" || line.starts_with("new file mode") {
            is_new = true;
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            let token = rest.strip_suffix('\t').unwrap_or(rest);
            if token == "/dev/null" {
                is_deleted = true;
            } else {
                let unquoted = unquote_c_style(token);
                if let Some(p) = unquoted.strip_prefix("b/") {
                    target_path = Some(PathBuf::from(p));
                }
            }
        } else if let Some(rest) = line.strip_prefix("--- ") {
            let token = rest.strip_suffix('\t').unwrap_or(rest);
            if token == "/dev/null" {
                is_new = true;
            } else {
                let unquoted = unquote_c_style(token);
                if let Some(p) = unquoted.strip_prefix("a/") {
                    old_path = Some(PathBuf::from(p));
                }
            }
        } else if let Some(p) = line.strip_prefix("rename to ") {
            if target_path.is_none() {
                let token = p.strip_suffix('\t').unwrap_or(p);
                target_path = Some(PathBuf::from(unquote_c_style(token)));
            }
        } else if let Some(p) = line.strip_prefix("rename from ") {
            if old_path.is_none() {
                let token = p.strip_suffix('\t').unwrap_or(p);
                old_path = Some(PathBuf::from(unquote_c_style(token)));
            }
        }
    }

    if target_path.is_none() {
        if let Some(first_line) = chunk.split_inclusive('\n').next() {
            let line = first_line.trim_end_matches(['\r', '\n']);
            if let Some(rest) = line.strip_prefix("diff --git ") {
                if let Some((target, old)) = parse_diff_git_header(rest) {
                    target_path = Some(target);
                    if old_path.is_none() {
                        old_path = old;
                    }
                }
            }
        }
    }

    target_path.map(|p| (p, old_path, is_deleted, is_new, is_special_mode))
}

#[cfg(feature = "tree-sitter")]
#[derive(Clone, Debug)]
struct MovedSource {
    path: PathBuf,
    signature: Option<String>,
}

#[cfg(feature = "tree-sitter")]
type PrimaryDefHash = ([u8; 32], usize, usize, usize, Option<String>);

#[cfg(feature = "tree-sitter")]
fn normalize_block_hash<'a>(lines: impl IntoIterator<Item = &'a str>) -> Option<([u8; 32], usize)> {
    let mut hasher = Sha256::new();
    let mut non_empty_count = 0usize;
    let mut total_bytes = 0usize;
    for raw in lines {
        let t = raw.trim();
        if t.is_empty() {
            continue;
        }
        non_empty_count += 1;
        total_bytes += t.len();
        hasher.update(t.as_bytes());
        hasher.update(b"\n");
    }
    if non_empty_count < MIN_MOVED_BLOCK_NON_EMPTY_LINES || total_bytes < MIN_MOVED_BLOCK_BYTES {
        return None;
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Some((out, non_empty_count))
}

#[cfg(feature = "tree-sitter")]
pub(super) fn dedent_lines(lines: &[&str]) -> String {
    let min_indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start_matches([' ', '\t']).len())
        .min()
        .unwrap_or(0);
    let mut out = String::new();
    for l in lines {
        let stripped = l.trim_end_matches(['\r', '\n']);
        if stripped.trim().is_empty() {
            out.push('\n');
        } else {
            let ascii_indent = stripped.len() - stripped.trim_start_matches([' ', '\t']).len();
            let cut = min_indent.min(ascii_indent);
            out.push_str(&stripped[cut..]);
            out.push('\n');
        }
    }
    out
}

#[cfg(feature = "tree-sitter")]
pub(super) fn parse_with_timeout(
    lang: SupportedLanguage,
    source: &str,
) -> std::result::Result<tree_sitter::Tree, &'static str> {
    let mut parser = Parser::new();
    parser
        .set_language(&lang.language_fn())
        .map_err(|_| "failed to set tree-sitter language")?;

    let start_time = std::time::Instant::now();
    let mut timed_out = false;
    let mut progress_cb = |_state: &tree_sitter::ParseState| {
        if start_time.elapsed().as_micros() as u64 > TREESITTER_PARSE_TIMEOUT_MICROS {
            timed_out = true;
            true
        } else {
            false
        }
    };
    let options = tree_sitter::ParseOptions::new().progress_callback(&mut progress_cb);
    let mut payload = |offset: usize, _point: tree_sitter::Point| -> &[u8] {
        if offset < source.len() {
            &source.as_bytes()[offset..]
        } else {
            &[]
        }
    };
    match parser.parse_with_options(&mut payload, None, Some(options)) {
        Some(t) => Ok(t),
        None => {
            if timed_out {
                Err("tree-sitter parse timed out (exceeded 500ms)")
            } else {
                Err("tree-sitter parse failed")
            }
        }
    }
}

#[cfg(feature = "tree-sitter")]
pub(super) fn is_container_definition_node(node: &tree_sitter::Node) -> bool {
    let mut target = *node;
    if matches!(target.kind(), "export_statement" | "decorated_definition") {
        if let Some(d) = target
            .child_by_field_name("declaration")
            .or_else(|| target.child_by_field_name("definition"))
        {
            target = d;
        }
    }
    matches!(
        target.kind(),
        "class_declaration"
            | "abstract_class_declaration"
            | "class_definition"
            | "class"
            | "module"
            | "mod_item"
            | "impl_item"
            | "trait_item"
            | "interface_declaration"
            | "struct_specifier"
            | "class_specifier"
            | "struct_declaration"
            | "record_declaration"
    )
}

#[cfg(feature = "tree-sitter")]
fn collect_primary_definition_hashes(
    lang: SupportedLanguage,
    source: &str,
    out: &mut Vec<PrimaryDefHash>,
) {
    if source.len() > MAX_TREESITTER_FILE_BYTES {
        return;
    }
    let Ok(tree) = parse_with_timeout(lang, source) else {
        return;
    };
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return;
    }

    fn walk_node(
        lang: SupportedLanguage,
        node: tree_sitter::Node,
        lines: &[&str],
        source: &str,
        out: &mut Vec<PrimaryDefHash>,
    ) {
        if let Some(matched) = check_primary_definition(lang, &node) {
            let s_row = matched.start_position().row;
            let e_row = matched
                .end_position()
                .row
                .min(lines.len().saturating_sub(1));
            if s_row <= e_row {
                if let Some((hash, non_empty)) =
                    normalize_block_hash(lines[s_row..=e_row].iter().copied())
                {
                    let raw_sig = signature::extract_signature(lang, &matched, lines, source);
                    let parent =
                        signature::extract_parent_scope_prefix(lang, &matched, lines, source);
                    let sig = signature::compact_definition_signature_with_ast(
                        lang,
                        &matched,
                        parent.as_deref(),
                        raw_sig.as_deref(),
                        source,
                        120,
                    );
                    let s_line = s_row + 1;
                    let e_line = e_row + 1;
                    let is_dup = out
                        .last()
                        .is_some_and(|prev| prev.0 == hash && prev.1 == s_line && prev.2 == e_line);
                    if !is_dup {
                        out.push((hash, s_line, e_line, non_empty, sig));
                    }
                    if !is_container_definition_node(&matched) {
                        return;
                    }
                }
            }
        }
        let mut cursor = node.walk();
        for ch in node.named_children(&mut cursor) {
            walk_node(lang, ch, lines, source, out);
        }
    }

    walk_node(lang, tree.root_node(), &lines, source, out);
}

/// Fast pre-check: returns true if any chunk in the diff could actually consume `moved_deleted_map`
/// (either a newly added non-test/non-lockfile code file or a modified file with a large pure
/// insertion hunk). Avoids parsing deleted blocks when no destination exists.
#[cfg(feature = "tree-sitter")]
fn has_potential_move_destinations(chunks: &[String]) -> bool {
    for chunk in chunks {
        let Some((target_path, _old_path, is_deleted, is_new, is_special)) =
            extract_target_path_from_diff_chunk(chunk)
        else {
            continue;
        };
        if is_deleted
            || is_special
            || is_lockfile_or_generated(&target_path)
            || has_generated_file_header(chunk)
        {
            continue;
        }
        if SupportedLanguage::from_path(&target_path).is_none() {
            continue;
        }
        if is_new {
            if !is_test_file_path(&target_path) {
                let added_lines = chunk
                    .lines()
                    .filter(|l| l.starts_with('+') && !l.starts_with("+++ "))
                    .take(MIN_MOVED_BLOCK_NON_EMPTY_LINES)
                    .count();
                if added_lines >= MIN_MOVED_BLOCK_NON_EMPTY_LINES {
                    return true;
                }
            }
            continue;
        }
        for line in chunk.lines() {
            if let Some((_, old_count, _, new_count, _)) = parse_hunk_header(line) {
                if old_count == 0 && new_count >= MIN_MOVED_BLOCK_NON_EMPTY_LINES {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(feature = "tree-sitter")]
fn collect_moved_deleted_blocks_for_chunk(chunk: &str) -> Vec<([u8; 32], MovedSource)> {
    let Some((target_path, old_path, _, is_new, is_special)) =
        extract_target_path_from_diff_chunk(chunk)
    else {
        return Vec::new();
    };
    if is_new
        || is_special
        || is_lockfile_or_generated(&target_path)
        || has_generated_file_header(chunk)
    {
        return Vec::new();
    }
    let src_path = old_path.unwrap_or(target_path);
    let lang = SupportedLanguage::from_path(&src_path);

    let mut entries: Vec<([u8; 32], MovedSource)> = Vec::new();
    let mut in_hunk = false;
    let mut current_deleted: Vec<&str> = Vec::new();
    let flush_run = |run: &mut Vec<&str>,
                     entries: &mut Vec<([u8; 32], MovedSource)>,
                     lang: Option<SupportedLanguage>,
                     src_path: &Path| {
        if run.is_empty() {
            return;
        }
        if let Some((hash, _non_empty)) = normalize_block_hash(run.iter().copied()) {
            if let Some(l) = lang {
                let dedented = dedent_lines(run);
                let mut defs = Vec::new();
                collect_primary_definition_hashes(l, &dedented, &mut defs);
                for (def_hash, _s, _e, _def_lines, def_sig) in defs {
                    entries.push((
                        def_hash,
                        MovedSource {
                            path: src_path.to_path_buf(),
                            signature: def_sig,
                        },
                    ));
                }
            }
            // Always index the full contiguous deleted run as well (e.g. a complete standalone
            // function, multi-def move, or unsupported language block).
            let fallback_sig = run
                .iter()
                .find(|l| !l.trim().is_empty())
                .map(|s| s.trim().to_string());
            entries.push((
                hash,
                MovedSource {
                    path: src_path.to_path_buf(),
                    signature: fallback_sig,
                },
            ));
        }
        run.clear();
    };

    for raw_line in chunk.split_inclusive('\n') {
        let trimmed = raw_line.trim_end_matches(['\r', '\n']);
        if trimmed.starts_with("@@") {
            flush_run(&mut current_deleted, &mut entries, lang, &src_path);
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if let Some(deleted_text) = trimmed.strip_prefix('-') {
            current_deleted.push(deleted_text);
        } else if !trimmed.starts_with('\\') {
            flush_run(&mut current_deleted, &mut entries, lang, &src_path);
        }
    }
    flush_run(&mut current_deleted, &mut entries, lang, &src_path);
    entries
}

/// Scans all raw diff chunks in parallel to index verbatim deleted definition blocks (`-` runs)
/// for cross-file move dedup, preserving deterministic chunk order on hash collisions.
#[cfg(feature = "tree-sitter")]
fn collect_moved_deleted_blocks(chunks: &[String]) -> HashMap<[u8; 32], MovedSource> {
    if !has_potential_move_destinations(chunks) {
        return HashMap::new();
    }
    let per_chunk: Vec<Vec<([u8; 32], MovedSource)>> = chunks
        .par_iter()
        .map(|chunk| collect_moved_deleted_blocks_for_chunk(chunk))
        .collect();
    let mut map = HashMap::new();
    for entries in per_chunk {
        for (hash, src) in entries {
            map.entry(hash).or_insert(src);
        }
    }
    map
}

#[cfg(feature = "tree-sitter")]
pub(super) fn extract_chunk_header_and_added_source(chunk: &str) -> (String, String, usize) {
    let mut header = String::new();
    let mut added_source = String::new();
    let mut added_count = 0usize;
    let mut in_hunk = false;

    for raw_line in chunk.split_inclusive('\n') {
        if !in_hunk {
            if raw_line.starts_with("@@") {
                in_hunk = true;
            } else {
                header.push_str(raw_line);
                continue;
            }
        }
        if raw_line.starts_with("@@") {
            continue;
        }
        if let Some(rest) = raw_line.strip_prefix('+') {
            added_source.push_str(rest);
            added_count += 1;
        }
    }
    (header, added_source, added_count)
}

#[cfg(feature = "tree-sitter")]
fn summarize_large_new_test_chunk(chunk: &str, path: &Path) -> Option<String> {
    if !is_test_file_path(path) {
        return None;
    }
    let (header, added_source, added_count) = extract_chunk_header_and_added_source(chunk);
    if added_count < LARGE_NEW_TEST_SUMMARY_MIN_LINES {
        return None;
    }

    let display_path = path.to_string_lossy();
    let mut toc_lines = Vec::new();
    let mut total_defs = 0usize;

    if let Some(lang) = SupportedLanguage::from_path(path) {
        if added_source.len() <= MAX_TREESITTER_FILE_BYTES {
            if let Ok(tree) = parse_with_timeout(lang, &added_source) {
                let lines: Vec<&str> = added_source.lines().collect();
                let mut collected = Vec::new();
                fn collect_defs(
                    lang: SupportedLanguage,
                    node: tree_sitter::Node,
                    lines: &[&str],
                    source: &str,
                    out: &mut Vec<(usize, usize, String)>,
                ) {
                    if let Some(matched) = check_primary_definition(lang, &node) {
                        let s = matched.start_position().row + 1;
                        let e = (matched.end_position().row + 1).min(lines.len().max(1));
                        let raw_sig = signature::extract_signature(lang, &matched, lines, source);
                        let parent =
                            signature::extract_parent_scope_prefix(lang, &matched, lines, source);
                        if let Some(sig) = signature::compact_definition_signature_with_ast(
                            lang,
                            &matched,
                            parent.as_deref(),
                            raw_sig.as_deref(),
                            source,
                            110,
                        ) {
                            out.push((s, e, sig));
                            if !is_container_definition_node(&matched) {
                                return;
                            }
                        }
                    }
                    let mut cursor = node.walk();
                    for ch in node.named_children(&mut cursor) {
                        collect_defs(lang, ch, lines, source, out);
                    }
                }
                collect_defs(
                    lang,
                    tree.root_node(),
                    &lines,
                    &added_source,
                    &mut collected,
                );
                total_defs = collected.len();
                for (s, e, sig) in collected.into_iter().take(40) {
                    toc_lines.push(format!("  - L{s}-L{e}: {sig}"));
                }
            }
        }
    }

    if toc_lines.is_empty() {
        return Some(format!(
            "{header}[omitted: large new test file {display_path} (+{added_count} lines)]\n\n"
        ));
    }

    let mut summary = format!(
        "{header}[omitted: large new test file {display_path} (+{added_count} lines, {total_defs} definitions):\n"
    );
    for l in &toc_lines {
        summary.push_str(l);
        summary.push('\n');
    }
    if total_defs > toc_lines.len() {
        summary.push_str(&format!(
            "  ... (+{} more definitions)\n",
            total_defs - toc_lines.len()
        ));
    }
    summary.push_str("]\n\n");
    Some(summary)
}

#[cfg(feature = "tree-sitter")]
type MovedChunkSummary = (String, super::SourceLineMap);

#[cfg(feature = "tree-sitter")]
fn summarize_moved_new_file_chunk(
    chunk: &str,
    path: &Path,
    moved_map: &HashMap<[u8; 32], MovedSource>,
) -> Option<MovedChunkSummary> {
    if moved_map.is_empty() {
        return None;
    }
    let lang = SupportedLanguage::from_path(path)?;
    let (header, added_source, added_count) = extract_chunk_header_and_added_source(chunk);
    if added_count < MIN_MOVED_BLOCK_NON_EMPTY_LINES {
        return None;
    }

    let mut defs = Vec::new();
    collect_primary_definition_hashes(lang, &added_source, &mut defs);
    if defs.is_empty() {
        return None;
    }

    let lines: Vec<&str> = added_source.split_inclusive('\n').collect();
    let mut moved_ranges: Vec<(usize, usize, &MovedSource, Option<&str>)> = Vec::new();

    for (hash, s, e, _non_empty, sig) in &defs {
        if let Some(src) = moved_map.get(hash) {
            if src.path != path {
                moved_ranges.push((*s, *e, src, sig.as_deref()));
            }
        }
    }
    if moved_ranges.is_empty() {
        return None;
    }

    // Only summarize moved definitions when at least one non-trivial definition matched.
    let mut body = String::new();
    let mut body_line_count = 0usize;
    let mut line_idx = 1usize;
    let mut move_cursor = 0usize;
    let mut body_line_map: Vec<Option<(usize, usize)>> = Vec::with_capacity(lines.len());
    // Sort by start line ascending, preferring wider enclosing ranges first on ties.
    moved_ranges.sort_by_key(|r| (r.0, std::cmp::Reverse(r.1)));

    while line_idx <= lines.len() {
        while move_cursor < moved_ranges.len() && moved_ranges[move_cursor].0 < line_idx {
            move_cursor += 1;
        }
        if move_cursor < moved_ranges.len() && line_idx == moved_ranges[move_cursor].0 {
            let (s, e, src, sig_opt) = moved_ranges[move_cursor];
            let span = e.saturating_sub(s) + 1;
            let sig_label = sig_opt.or(src.signature.as_deref()).unwrap_or("definition");
            body.push_str(&format!(
                "+[moved verbatim from {}: {sig_label} (+{span} lines)]\n",
                src.path.display()
            ));
            body_line_map.push(None);
            body_line_count += 1;
            line_idx = (e + 1).max(line_idx + 1);
            move_cursor += 1;
        } else {
            let raw = lines[line_idx - 1];
            body.push('+');
            body.push_str(raw);
            if !raw.ends_with('\n') {
                body.push('\n');
            }
            body_line_map.push(Some((line_idx, line_idx)));
            body_line_count += 1;
            line_idx += 1;
        }
    }
    let header_lines_count = header.split_inclusive('\n').count();
    let mut orig_line_map = vec![None; header_lines_count + 1];
    orig_line_map.extend(body_line_map);
    Some((
        format!("{header}@@ -0,0 +1,{body_line_count} @@\n{body}"),
        orig_line_map,
    ))
}

#[cfg(feature = "tree-sitter")]
fn find_enclosing_parent_scope_for_line(
    lang: SupportedLanguage,
    root: &tree_sitter::Node,
    line_1idx: usize,
    lines: &[&str],
    source: &str,
) -> Option<String> {
    let row = line_1idx.saturating_sub(1);
    let col = lines
        .get(row)
        .map(|l| l.len() - l.trim_start().len())
        .unwrap_or(0);
    let pt = tree_sitter::Point { row, column: col };
    let node = root
        .named_descendant_for_point_range(pt, pt)
        .or_else(|| root.descendant_for_point_range(pt, pt))?;
    signature::extract_parent_scope_prefix(lang, &node, lines, source)
}

#[cfg(feature = "tree-sitter")]
fn select_merged_signature(m: &MergedHunk, is_clamped: bool) -> Option<String> {
    let mut unique: Vec<(&str, bool, usize, usize)> = Vec::new();
    for (sig, clamped, span, extra) in &m.sigs {
        if let Some(existing) = unique.iter_mut().find(|(s, _, _, _)| *s == sig.as_str()) {
            existing.1 |= *clamped;
            existing.2 = existing.2.max(*span);
            existing.3 = existing.3.max(*extra);
        } else {
            unique.push((sig.as_str(), *clamped, *span, *extra));
        }
    }
    if unique.is_empty() {
        return None;
    }

    let chosen = if is_clamped {
        unique
            .iter()
            .filter(|(_, c, _, _)| *c)
            .max_by_key(|(_, _, span, _)| *span)
            .or_else(|| unique.iter().max_by_key(|(_, _, span, _)| *span))
            .copied()
            .unwrap_or(unique[0])
            .0
    } else {
        unique[0].0
    };

    let total_defs: usize = unique.iter().map(|(_, _, _, extra)| 1 + *extra).sum();
    if total_defs > 1 {
        Some(format!("{chosen} (+{} more)", total_defs - 1))
    } else {
        Some(chosen.to_string())
    }
}

#[cfg(feature = "tree-sitter")]
enum TreeSitterExpandResult {
    Expanded(String, Vec<ast_compact::InlineTestBlock>),
    Generated,
}

/// Expands a single file's raw diff chunk with Tree-Sitter AST context, also returning any
/// detected explicit inline test blocks inside production files for Tier 2 budget compaction
/// (or signaling `Generated` when the full file's top comment header marks it auto-generated).
#[cfg(feature = "tree-sitter")]
fn expand_chunk_with_treesitter(
    repo_root: &Path,
    path: &Path,
    chunk: &str,
    source: GitDiffSource<'_>,
    lang: SupportedLanguage,
    moved_deleted_map: &HashMap<[u8; 32], MovedSource>,
) -> Result<Option<TreeSitterExpandResult>> {
    // Read new content with size limit checks to prevent memory spikes on huge files
    let new_content = match source {
        GitDiffSource::Staged | GitDiffSource::Commit(_) => {
            let obj_prefix = match source {
                GitDiffSource::Staged => ":0:".to_string(),
                GitDiffSource::Commit(sha) => format!("{sha}:"),
                GitDiffSource::Unstaged => unreachable!(),
            };
            let mut cat_arg = std::ffi::OsString::from(&obj_prefix);
            cat_arg.push(path.as_os_str());
            let mut cat_cmd = git_cmd(repo_root);
            cat_cmd.arg("cat-file").arg("blob").arg(&cat_arg);
            let mut child = match cat_cmd
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(c) => c,
                Err(_) => return Ok(None),
            };

            let mut stdout = child.stdout.take().expect("stdout piped");
            let mut bytes = Vec::new();
            use std::io::Read;
            let mut take_reader = (&mut stdout).take(MAX_TREESITTER_FILE_BYTES as u64 + 1);
            if take_reader.read_to_end(&mut bytes).is_err() {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(None);
            }
            if bytes.len() > MAX_TREESITTER_FILE_BYTES {
                drop(stdout);
                let _ = child.kill();
                let _ = child.wait();
                return Err(ServiceError::invalid(format!(
                    "file size ({} bytes) exceeds tree-sitter limit (1 MiB)",
                    bytes.len()
                )));
            }
            drop(stdout);
            let status = match child.wait() {
                Ok(s) => s,
                Err(_) => return Ok(None),
            };
            if !status.success() {
                return Ok(None);
            }
            String::from_utf8_lossy(&bytes).into_owned()
        }
        GitDiffSource::Unstaged => {
            let full_path = repo_root.join(path);
            if let Ok(meta) = std::fs::symlink_metadata(&full_path) {
                if meta.file_type().is_symlink() || meta.is_dir() {
                    return Ok(None);
                }
                if meta.len() > MAX_TREESITTER_FILE_BYTES as u64 {
                    return Err(ServiceError::invalid(format!(
                        "file size ({} bytes) exceeds tree-sitter limit (1 MiB)",
                        meta.len()
                    )));
                }
            }
            match std::fs::read(&full_path) {
                Ok(bytes) => {
                    if bytes.len() as u64 > MAX_TREESITTER_FILE_BYTES as u64 {
                        return Err(ServiceError::invalid(format!(
                            "file size ({} bytes) exceeds tree-sitter limit (1 MiB)",
                            bytes.len()
                        )));
                    }
                    String::from_utf8_lossy(&bytes).into_owned()
                }
                Err(_) => return Ok(None),
            }
        }
    };

    if has_generated_source_header(&new_content, Some(path)) {
        return Ok(Some(TreeSitterExpandResult::Generated));
    }

    let new_lines: Vec<&str> = new_content.split_inclusive('\n').collect();
    if new_lines.is_empty() {
        return Ok(Some(TreeSitterExpandResult::Expanded(
            chunk.to_string(),
            Vec::new(),
        )));
    }
    let plain_lines: Vec<&str> = new_content.lines().collect();

    let tree = parse_with_timeout(lang, &new_content).map_err(ServiceError::invalid)?;
    let root = tree.root_node();
    let inline_test_blocks = if is_test_file_path(path) {
        Vec::new()
    } else {
        ast_compact::find_inline_test_blocks(lang, &root, &new_content)
    };

    // Parse header and hunks from git diff chunk, preserving original line endings for hunks
    let mut file_header = Vec::new();
    let mut hunks: Vec<ParsedHunk> = Vec::new();
    let mut current_hunk: Option<ParsedHunk> = None;

    for raw_line in chunk.split_inclusive('\n') {
        let trimmed_line = raw_line.trim_end_matches(['\r', '\n']);
        if let Some((old_start, old_count, new_start, new_count, sig)) =
            parse_hunk_header(trimmed_line)
        {
            if let Some(h) = current_hunk.take() {
                hunks.push(h);
            }
            current_hunk = Some(ParsedHunk {
                old_start,
                old_count,
                new_start,
                new_count,
                signature: sig,
                lines: Vec::new(),
                moved_summary: None,
            });
        } else if let Some(h) = current_hunk.as_mut() {
            h.lines.push(raw_line.to_string());
        } else {
            file_header.push(raw_line.to_string());
        }
    }
    if let Some(h) = current_hunk {
        hunks.push(h);
    }

    if hunks.is_empty() {
        return Ok(Some(TreeSitterExpandResult::Expanded(
            chunk.to_string(),
            inline_test_blocks,
        )));
    }

    struct ExpandedEntry {
        exp_start: usize,
        exp_end: usize,
        full_start: usize,
        full_end: usize,
        signature: Option<String>,
        def_span: usize,
        extra_defs: usize,
        is_entry_clamped: bool,
        use_raw_sig_fallback: bool,
        hunk: ParsedHunk,
    }

    let mut entries = Vec::new();
    for mut hunk in hunks {
        // Verbatim move dedup for large pure insertions in modified files
        if !moved_deleted_map.is_empty()
            && hunk.old_count == 0
            && hunk.new_count >= MIN_MOVED_BLOCK_NON_EMPTY_LINES
        {
            if let Some((h_hash, _non_empty)) = normalize_block_hash(
                hunk.lines
                    .iter()
                    .filter_map(|l| l.strip_prefix('+'))
                    .collect::<Vec<_>>(),
            ) {
                if let Some(src) = moved_deleted_map.get(&h_hash) {
                    if src.path != path {
                        let sig_label = src.signature.as_deref().unwrap_or("definition");
                        hunk.moved_summary = Some(format!(
                            "+[moved verbatim from {}: {sig_label} (+{} lines)]\n",
                            src.path.display(),
                            hunk.new_count
                        ));
                    }
                }
            }
        }

        if hunk.moved_summary.is_some() {
            let exp_start = hunk.new_start.saturating_sub(2).max(1);
            let exp_end = (hunk.new_start + hunk.new_count + 1).min(new_lines.len());
            let sig_opt = (!hunk.signature.is_empty()).then(|| hunk.signature.clone());
            entries.push(ExpandedEntry {
                exp_start,
                exp_end,
                full_start: exp_start,
                full_end: exp_end,
                signature: sig_opt,
                def_span: 0,
                extra_defs: 0,
                is_entry_clamped: false,
                use_raw_sig_fallback: true,
                hunk,
            });
            continue;
        }

        // Pure top-of-file deletion (`new_start == 0, new_count == 0`):
        // Never attribute to line 1's new definition.
        if hunk.new_count == 0 && hunk.new_start == 0 {
            let exp_start = 1;
            let exp_end = 3.min(new_lines.len()).max(1);
            entries.push(ExpandedEntry {
                exp_start,
                exp_end,
                full_start: exp_start,
                full_end: exp_end,
                signature: None,
                def_span: 0,
                extra_defs: 0,
                is_entry_clamped: false,
                use_raw_sig_fallback: false,
                hunk,
            });
            continue;
        }

        let change_start = if hunk.new_count == 0 {
            hunk.new_start.max(1)
        } else {
            hunk.new_start
        };
        let change_end = if hunk.new_count == 0 {
            change_start
        } else {
            hunk.new_start + hunk.new_count.saturating_sub(1)
        };

        // Trim empty lines from boundaries to avoid jumping to parent container
        let mut eff_start = change_start;
        let mut eff_end = change_end;
        while eff_start <= eff_end
            && eff_start <= new_lines.len()
            && new_lines[eff_start - 1].trim().is_empty()
        {
            eff_start += 1;
        }
        while eff_end >= eff_start
            && eff_end >= 1
            && eff_end <= new_lines.len()
            && new_lines[eff_end - 1].trim().is_empty()
        {
            eff_end = eff_end.saturating_sub(1);
        }
        let (lookup_start, lookup_end) = if eff_start <= eff_end {
            (eff_start, eff_end)
        } else {
            (change_start, change_end)
        };

        let matched = find_enclosing_definition_detail(
            lang,
            &root,
            lookup_start,
            lookup_end,
            &new_content,
            Some(path),
            hunk.old_count,
        );

        let full_start_raw = matched.full_start.max(1);
        let full_end_raw = matched.full_end.min(new_lines.len());

        // Guard against pure deletion misattribution when `hunk.new_count == 0` sits right on/after
        // the closing boundary of a preceding definition (`matched.full_end`).
        if hunk.new_count == 0 && matched.signature.is_some() && hunk.new_start >= full_end_raw {
            let is_after_def = match lang {
                SupportedLanguage::Python => {
                    let def_indent = plain_lines
                        .get(full_start_raw.saturating_sub(1))
                        .map(|l| l.len() - l.trim_start().len())
                        .unwrap_or(0);
                    let deleted_indent = hunk
                        .lines
                        .iter()
                        .filter_map(|l| l.strip_prefix('-'))
                        .find(|l| !l.trim().is_empty())
                        .map(|l| l.len() - l.trim_start().len())
                        .unwrap_or(0);
                    deleted_indent <= def_indent
                }
                _ => true,
            };
            if is_after_def {
                let exp_start = hunk.new_start.saturating_sub(2).max(1);
                let exp_end = (hunk.new_start + 3).min(new_lines.len()).max(exp_start);
                let container_sig = matched.parent_scope.or_else(|| {
                    find_enclosing_parent_scope_for_line(
                        lang,
                        &root,
                        (hunk.new_start + 1).min(new_lines.len()),
                        &plain_lines,
                        &new_content,
                    )
                });
                entries.push(ExpandedEntry {
                    exp_start,
                    exp_end,
                    full_start: exp_start,
                    full_end: exp_end,
                    signature: container_sig,
                    def_span: 0,
                    extra_defs: 0,
                    is_entry_clamped: false,
                    use_raw_sig_fallback: false,
                    hunk,
                });
                continue;
            }
        }

        let mut sig = matched.signature;
        let mut extra_defs = matched.extra_defs;
        let mut full_start = full_start_raw;
        let mut full_end = full_end_raw;
        let mut exp_start = matched.exp_start.min(change_start).max(1);
        let mut exp_end = matched.exp_end.max(change_end).min(new_lines.len());
        let mut is_entry_clamped = matched.is_clamped;
        let mut def_span = full_end.saturating_sub(full_start) + 1;

        // If a hunk modifies the tail of a definition AND simultaneously deletes a large block
        // extending beyond the definition boundary (`old_count > 2 * def_span`), mark trailing deletions.
        if hunk.new_count > 0
            && change_end >= full_end_raw
            && hunk.old_count > def_span.saturating_mul(2).max(10)
        {
            if let Some(ref s) = sig {
                sig = Some(format!("{s} (+trailing deletions)"));
            }
        }

        let changed_is_attr_or_comment =
            is_attribute_or_comment_only(&new_lines, change_start, change_end, lang)
                || (change_start <= change_end
                    && change_end <= new_lines.len()
                    && (change_start..=change_end).any(|l| !new_lines[l - 1].trim().is_empty())
                    && (change_start..=change_end).all(|l| {
                        new_lines[l - 1].trim().is_empty()
                            || is_attribute_or_comment_only(&new_lines, l, l, lang)
                            || is_ast_node_attr_or_comment(lang, &root, &new_lines, l)
                    }));

        if sig.is_none() {
            let mut found_definition = false;
            if changed_is_attr_or_comment {
                // Extended lookahead across stacked attributes, decorators, docstrings, and comments
                for lookahead_line in
                    (change_end + 1)..=(change_end + LOOKAHEAD_MAX_LINES).min(new_lines.len())
                {
                    let line_text = new_lines[lookahead_line - 1].trim();
                    if line_text.is_empty() {
                        break;
                    }
                    let la = find_enclosing_definition_detail(
                        lang,
                        &root,
                        lookahead_line,
                        lookahead_line,
                        &new_content,
                        Some(path),
                        0,
                    );
                    if let Some(la_sig) = la.signature {
                        if !la_sig.starts_with("#[")
                            && !la_sig.starts_with('@')
                            && !la_sig.starts_with("//")
                            && !la_sig.starts_with("/*")
                        {
                            let la_full_start = la.full_start.max(1);
                            let la_full_end = la.full_end.min(new_lines.len());
                            full_start = change_start.min(la_full_start).max(1);
                            full_end = change_end.max(la_full_end).min(new_lines.len());
                            def_span = full_end.saturating_sub(full_start) + 1;
                            extra_defs = la.extra_defs;

                            if def_span > ATTR_ONLY_MAX_EXPANSION_LINES {
                                // Cap attribute/comment-only edits on large functions to signature + top of body
                                exp_start = change_start.min(la.exp_start).max(1);
                                exp_end = change_end
                                    .max((la.exp_start + CLAMPED_CONTEXT_LINES).min(la.exp_end))
                                    .min(new_lines.len());
                                is_entry_clamped = full_start < exp_start || full_end > exp_end;
                            } else {
                                exp_start = change_start.min(la.exp_start).max(1);
                                exp_end = change_end.max(la.exp_end).min(new_lines.len());
                                is_entry_clamped = la.is_clamped;
                            }
                            sig = Some(la_sig);
                            found_definition = true;
                            break;
                        }
                    } else if !is_attribute_or_comment_only(
                        &new_lines,
                        lookahead_line,
                        lookahead_line,
                        lang,
                    ) && !is_ast_node_attr_or_comment(
                        lang,
                        &root,
                        &new_lines,
                        lookahead_line,
                    ) {
                        break;
                    }
                }
            }
            if !found_definition {
                exp_start = change_start.saturating_sub(3).max(1);
                exp_end = (change_end + 3).min(new_lines.len());
                full_start = exp_start;
                full_end = exp_end;
                is_entry_clamped = false;
                def_span = 0;
                extra_defs = 0;
                if hunk.new_count == 0 {
                    sig = find_enclosing_parent_scope_for_line(
                        lang,
                        &root,
                        lookup_start,
                        &plain_lines,
                        &new_content,
                    );
                }
            }
        } else if changed_is_attr_or_comment
            && hunk.new_count > 0
            && hunk.old_count <= CLAMPED_CONTEXT_LINES
            && def_span > ATTR_ONLY_MAX_EXPANSION_LINES
        {
            // Comment/attribute-only edit inside a large function (>15 lines):
            // clamp to context around the comment rather than dumping the entire function.
            exp_start = change_start
                .saturating_sub(CLAMPED_CONTEXT_LINES)
                .max(full_start);
            exp_end = (change_end + CLAMPED_CONTEXT_LINES).min(full_end);
            is_entry_clamped = full_start < exp_start || full_end > exp_end;
        }

        // Pure insertion with 0 context around the newly added definition:
        // guarantee 1-2 lines of surrounding anchor context for git apply & LLM review.
        if hunk.old_count == 0
            && hunk.new_count > 0
            && exp_start == hunk.new_start
            && exp_end == hunk.new_start + hunk.new_count.saturating_sub(1)
        {
            exp_start = exp_start.saturating_sub(2).max(1);
            exp_end = (exp_end + 2).min(new_lines.len());
            full_start = full_start.min(exp_start);
            full_end = full_end.max(exp_end);
        }

        let use_raw_sig_fallback = hunk.new_count > 0;
        entries.push(ExpandedEntry {
            exp_start,
            exp_end,
            full_start,
            full_end,
            signature: sig,
            def_span,
            extra_defs,
            is_entry_clamped,
            use_raw_sig_fallback,
            hunk,
        });
    }

    entries.sort_by_key(|e| (e.exp_start, e.hunk.new_start));

    // Merge overlapping or adjacent expanded hunks (avoiding merging non-overlapping production
    // hunks with adjacent inline-test hunks so Tier 2 can cleanly fold inline test hunks).
    let mut merged: Vec<MergedHunk> = Vec::new();
    for entry in entries {
        let entry_is_inline_test = inline_test_blocks
            .iter()
            .any(|b| entry.exp_start >= b.start_line && entry.exp_end <= b.end_line);
        let mut entry_sigs = Vec::new();
        if let Some(s) = entry.signature {
            entry_sigs.push((s, entry.is_entry_clamped, entry.def_span, entry.extra_defs));
        } else if entry.use_raw_sig_fallback && !entry.hunk.signature.is_empty() {
            entry_sigs.push((
                entry.hunk.signature.clone(),
                entry.is_entry_clamped,
                entry.def_span,
                entry.extra_defs,
            ));
        }

        if let Some(last) = merged.last_mut() {
            let can_merge = entry.exp_start <= last.exp_end
                || (entry.exp_start == last.exp_end + 1
                    && entry_is_inline_test == last.is_inline_test);
            if can_merge {
                last.exp_end = last.exp_end.max(entry.exp_end);
                last.full_start = last.full_start.min(entry.full_start);
                last.full_end = last.full_end.max(entry.full_end);
                last.is_inline_test &= entry_is_inline_test;
                last.sigs.extend(entry_sigs);
                last.hunks.push(entry.hunk);
                continue;
            }
        }
        merged.push(MergedHunk {
            exp_start: entry.exp_start,
            exp_end: entry.exp_end,
            full_start: entry.full_start,
            full_end: entry.full_end,
            is_inline_test: entry_is_inline_test,
            sigs: entry_sigs,
            hunks: vec![entry.hunk],
        });
    }

    for m in &mut merged {
        m.hunks.sort_by_key(|h| (h.new_start, h.old_start));
    }

    let file_has_no_newline = !new_content.ends_with('\n');

    let mut result = String::new();
    for line in &file_header {
        result.push_str(line);
    }

    for m in merged {
        let exp_start = m.exp_start;
        let exp_end = m.exp_end.min(new_lines.len());
        let mut hunk_body = Vec::new();
        let mut curr_new = exp_start;
        let mut pre_context = 0;
        let mut post_context = 0;
        let mut inter_context = 0;
        let mut total_added = 0;
        let mut total_removed = 0;

        for (idx, h) in m.hunks.iter().enumerate() {
            let target_new = if h.new_count == 0 {
                h.new_start
            } else {
                h.new_start.saturating_sub(1)
            };

            if curr_new <= target_new {
                for l in curr_new..=target_new {
                    if l >= 1 && l <= new_lines.len() {
                        let line = new_lines[l - 1];
                        if line.ends_with('\n') {
                            hunk_body.push(format!(" {line}"));
                        } else {
                            hunk_body.push(format!(" {line}\n"));
                        }
                        if idx == 0 {
                            pre_context += 1;
                        } else {
                            inter_context += 1;
                        }
                    }
                }
                curr_new = target_new + 1;
            }

            if let Some(ref moved_line) = h.moved_summary {
                hunk_body.push(moved_line.clone());
                total_added += 1;
            } else {
                for line in &h.lines {
                    hunk_body.push(line.clone());
                }
                total_added += h.new_count;
            }
            total_removed += h.old_count;
            curr_new += h.new_count;
        }

        if curr_new <= exp_end {
            for l in curr_new..=exp_end {
                if l >= 1 && l <= new_lines.len() {
                    let line = new_lines[l - 1];
                    if line.ends_with('\n') {
                        hunk_body.push(format!(" {line}"));
                    } else {
                        hunk_body.push(format!(" {line}\n"));
                    }
                    post_context += 1;
                }
            }
        }

        // Output \ No newline at end of file marker if applicable.
        if file_has_no_newline && exp_end == new_lines.len() && post_context > 0 {
            hunk_body.push("\\ No newline at end of file\n".to_string());
        }

        let first_hunk = &m.hunks[0];
        let old_exp_count = pre_context + total_removed + inter_context + post_context;
        let new_exp_start = exp_start;
        let new_exp_count = pre_context + total_added + inter_context + post_context;

        let old_exp_start = if first_hunk.old_count == 0 {
            if pre_context > 0 {
                (first_hunk.old_start + 1)
                    .saturating_sub(pre_context)
                    .max(1)
            } else if old_exp_count == 0 {
                first_hunk.old_start
            } else {
                first_hunk.old_start + 1
            }
        } else {
            first_hunk.old_start.saturating_sub(pre_context).max(1)
        };

        let is_clamped = m.full_start < exp_start || m.full_end > exp_end;
        let chosen_sig = select_merged_signature(&m, is_clamped);
        let base_sig = chosen_sig.as_deref().unwrap_or("");
        let clamped_suffix = format!(
            "[clamped] (lines {exp_start}-{exp_end} of {}-{})",
            m.full_start, m.full_end
        );
        let header_sig = if is_clamped {
            if base_sig.is_empty() {
                clamped_suffix
            } else if !base_sig.contains("[clamped]") {
                format!("{base_sig} {clamped_suffix}")
            } else {
                base_sig.to_string()
            }
        } else {
            base_sig.to_string()
        };

        let header = if header_sig.is_empty() {
            format!("@@ -{old_exp_start},{old_exp_count} +{new_exp_start},{new_exp_count} @@\n")
        } else {
            format!(
                "@@ -{old_exp_start},{old_exp_count} +{new_exp_start},{new_exp_count} @@ {header_sig}\n"
            )
        };

        result.push_str(&header);
        for line in hunk_body {
            result.push_str(&line);
        }
    }

    Ok(Some(TreeSitterExpandResult::Expanded(
        result,
        inline_test_blocks,
    )))
}

#[cfg(not(feature = "tree-sitter"))]
fn git_diff_w_all(repo_root: &Path, source: GitDiffSource<'_>) -> Result<String> {
    let mut cmd = git_cmd(repo_root);
    match source {
        GitDiffSource::Unstaged => {
            cmd.args([
                "diff",
                "-W",
                "-M",
                "--diff-algorithm=histogram",
                "--no-color",
                "--no-ext-diff",
            ]);
        }
        GitDiffSource::Staged => {
            cmd.args([
                "diff",
                "-W",
                "-M",
                "--diff-algorithm=histogram",
                "--no-color",
                "--no-ext-diff",
                "--staged",
            ]);
        }
        GitDiffSource::Commit(sha) => {
            cmd.args([
                "show",
                "-p",
                "-W",
                "-M",
                "--diff-algorithm=histogram",
                "--first-parent",
                "--no-color",
                "--no-ext-diff",
                "--stat=120",
                "--stat-name-width=90",
                sha,
            ]);
        }
    }
    let output = cmd
        .output()
        .map_err(|e| ServiceError::invalid(format!("git command failed: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ServiceError::invalid(format!(
            "git fallback diff -W failed: {stderr}"
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if stdout.trim().is_empty() {
        return Err(ServiceError::invalid(match source {
            GitDiffSource::Staged => "no staged git changes to attach",
            GitDiffSource::Unstaged => "no unstaged git changes to attach",
            GitDiffSource::Commit(_) => "git commit produced empty output",
        }));
    }
    Ok(stdout)
}

/// Fallback helper to run `git diff -W` or `git show -W` for a single file, preserving rename information when applicable.
#[cfg(feature = "tree-sitter")]
fn git_diff_w_for_file(
    repo_root: &Path,
    path: &Path,
    old_path: Option<&Path>,
    source: GitDiffSource<'_>,
) -> Result<String> {
    let mut cmd = git_cmd(repo_root);
    match source {
        GitDiffSource::Unstaged => {
            cmd.args([
                "diff",
                "-W",
                "-M",
                "--diff-algorithm=histogram",
                "--no-color",
                "--no-ext-diff",
            ]);
        }
        GitDiffSource::Staged => {
            cmd.args([
                "diff",
                "-W",
                "-M",
                "--diff-algorithm=histogram",
                "--no-color",
                "--no-ext-diff",
                "--staged",
            ]);
        }
        GitDiffSource::Commit(sha) => {
            cmd.args([
                "show",
                "-W",
                "-M",
                "--diff-algorithm=histogram",
                "--first-parent",
                "--no-color",
                "--no-ext-diff",
                sha,
            ]);
        }
    }
    cmd.arg("--");
    if let Some(old) = old_path {
        if old != path {
            cmd.arg(old);
        }
    }
    cmd.arg(path);
    let output = cmd
        .output()
        .map_err(|e| ServiceError::invalid(format!("git command failed: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ServiceError::invalid(format!(
            "git fallback diff -W failed: {stderr}"
        )));
    }
    let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();

    // For commit source, git show outputs commit headers before diff --git.
    // Strip commit headers so only diff chunk is returned for this file.
    if matches!(source, GitDiffSource::Commit(_)) {
        if stdout.starts_with("diff --git ") {
            // Already at the diff chunk
        } else if let Some(idx) = stdout.find("\ndiff --git ") {
            stdout = stdout[idx + 1..].to_string();
        } else {
            stdout.clear();
        }
    }

    // If old_path was passed, git diff -W might output chunks for both old_path and path when old_path
    // was also recreated. In that case, keep only chunks whose destination matches path to avoid duplication.
    if old_path.is_some_and(|old| old != path) && stdout.matches("diff --git ").count() > 1 {
        let mut matching_chunks = String::new();
        let mut cur = String::new();
        for line in stdout.split_inclusive('\n') {
            if line.starts_with("diff --git ") && !cur.is_empty() {
                if let Some((target, _, _, _, _)) = extract_target_path_from_diff_chunk(&cur) {
                    if target == path {
                        matching_chunks.push_str(&cur);
                    }
                }
                cur.clear();
            }
            cur.push_str(line);
        }
        if !cur.is_empty() {
            if let Some((target, _, _, _, _)) = extract_target_path_from_diff_chunk(&cur) {
                if target == path {
                    matching_chunks.push_str(&cur);
                }
            }
        }
        if !matching_chunks.is_empty() {
            return Ok(matching_chunks);
        }
    }
    Ok(stdout)
}

/// Determines if a path belongs to a package lockfile, minified bundle, or generated asset.
pub fn is_lockfile_or_generated(path: &Path) -> bool {
    if let Some(parent) = path.parent() {
        for comp in parent.components() {
            if let std::path::Component::Normal(seg) = comp {
                if let Some(s) = seg.to_str() {
                    if s.eq_ignore_ascii_case("generated") {
                        return true;
                    }
                }
            }
        }
    }

    let file_name = match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => name,
        None => return false,
    };

    let lower = file_name.to_ascii_lowercase();

    if matches!(
        lower.as_str(),
        "cargo.lock"
            | "package-lock.json"
            | "npm-shrinkwrap.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "composer.lock"
            | "gemfile.lock"
            | "pipfile.lock"
            | "poetry.lock"
            | "flake.lock"
            | "go.sum"
            | "bun.lockb"
            | "bun.lock"
            | "deno.lock"
            | "uv.lock"
            | "pdm.lock"
            | "package.resolved"
            | "podfile.lock"
            | "pubspec.lock"
            | "mix.lock"
            | "gradle.lockfile"
            | "packages.lock.json"
    ) {
        return true;
    }

    if lower.ends_with(".min.js")
        || lower.ends_with(".min.css")
        || lower.ends_with(".bundle.js")
        || lower.ends_with(".bundle.css")
        || lower.ends_with(".chunk.js")
        || lower.ends_with(".chunk.css")
        || lower.ends_with(".js.map")
        || lower.ends_with(".mjs.map")
        || lower.ends_with(".cjs.map")
        || lower.ends_with(".css.map")
        || lower.ends_with(".svg")
        || lower.ends_with(".pb.go")
        || lower.ends_with(".pb.rs")
        || lower.ends_with(".pb.cc")
        || lower.ends_with(".pb.h")
        || lower.ends_with("_pb2.py")
        || lower.ends_with("_pb2.pyi")
        || lower.ends_with("_pb2_grpc.py")
        || lower.ends_with(".g.dart")
        || lower.ends_with(".freezed.dart")
        || lower.ends_with(".designer.cs")
        || lower.ends_with(".g.cs")
        || lower.ends_with(".generated.cs")
    {
        return true;
    }

    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HashCommentPolicy {
    Allowed,
    Disallowed,
    Heuristic,
}

fn hash_comment_policy_for_path(path: Option<&Path>) -> HashCommentPolicy {
    let Some(path) = path else {
        return HashCommentPolicy::Heuristic;
    };
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let lower = ext.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "rs" | "c"
                | "h"
                | "cc"
                | "cpp"
                | "cxx"
                | "hh"
                | "hpp"
                | "hxx"
                | "m"
                | "mm"
                | "cs"
                | "java"
                | "kt"
                | "kts"
                | "scala"
                | "go"
                | "js"
                | "jsx"
                | "mjs"
                | "cjs"
                | "ts"
                | "tsx"
                | "mts"
                | "cts"
                | "swift"
                | "dart"
                | "zig"
                | "proto"
                | "sql"
                | "lua"
                | "html"
                | "htm"
                | "xml"
                | "xaml"
                | "plist"
                | "csproj"
                | "vbproj"
                | "gradle"
                | "groovy"
                | "hs"
                | "elm"
                | "vue"
                | "svelte"
                | "css"
                | "scss"
                | "less"
                | "json"
                | "jsonc"
                | "md"
                | "markdown"
                | "mdx"
        ) {
            return HashCommentPolicy::Disallowed;
        }
        if matches!(
            lower.as_str(),
            "py" | "pyi"
                | "rb"
                | "sh"
                | "bash"
                | "zsh"
                | "ksh"
                | "fish"
                | "pl"
                | "pm"
                | "r"
                | "yaml"
                | "yml"
                | "toml"
                | "ini"
                | "cfg"
                | "conf"
                | "cmake"
                | "mk"
                | "mak"
                | "tf"
                | "tfvars"
                | "hcl"
                | "nix"
                | "ex"
                | "exs"
                | "jl"
                | "bzl"
                | "bazel"
                | "star"
                | "graphql"
                | "gql"
                | "ps1"
                | "cr"
                | "nim"
                | "properties"
                | "env"
        ) {
            return HashCommentPolicy::Allowed;
        }
    }
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "makefile"
                | "gnumakefile"
                | "dockerfile"
                | "rakefile"
                | "gemfile"
                | "justfile"
                | "cmakelists.txt"
        ) {
            return HashCommentPolicy::Allowed;
        }
    }
    HashCommentPolicy::Heuristic
}

/// Returns true for Rust attributes (`#[...]`, `#![...]`) and C/C++/C# preprocessor directives
/// (`#include`, `#define`, `#pragma`, `#if`, etc.) so they are never treated as `#` comments.
fn is_preprocessor_or_attr_hash_line(trimmed: &str) -> bool {
    if trimmed.starts_with("#[") || trimmed.starts_with("#![") {
        return true;
    }
    if let Some(after_hash) = trimmed.strip_prefix('#') {
        let rest = after_hash.trim_start();
        let kw_len = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        let kw = &rest[..kw_len];
        let after_kw = &rest[kw_len..];
        let has_boundary = after_kw.is_empty()
            || after_kw.starts_with(|c: char| c.is_whitespace() || matches!(c, '(' | '<' | '"'));
        if has_boundary
            && matches!(
                kw,
                "include"
                    | "import"
                    | "define"
                    | "undef"
                    | "pragma"
                    | "if"
                    | "ifdef"
                    | "ifndef"
                    | "else"
                    | "elif"
                    | "endif"
                    | "error"
                    | "warning"
                    | "line"
                    | "region"
                    | "endregion"
                    | "nullable"
            )
        {
            return true;
        }
    }
    false
}

/// Strips leading/trailing comment delimiters (`//`, `///`, `//!`, `/*`, `*`, `#`, `--`, `<!--`,
/// `*/`, `-->`) to obtain the trimmed inner comment text.
fn strip_comment_delimiters(trimmed: &str, in_c_block: bool, in_html: bool) -> &str {
    let mut s = trimmed;
    if let Some(rest) = s.strip_prefix("<!--") {
        s = rest;
    } else if let Some(rest) = s.strip_prefix("/*") {
        s = rest.trim_start_matches(['*', '!']);
    } else if let Some(rest) = s.strip_prefix("//") {
        s = rest.trim_start_matches(['/', '!']);
    } else if let Some(rest) = s.strip_prefix("--") {
        s = rest.trim_start_matches('-');
    } else if let Some(rest) = s.strip_prefix('#') {
        s = rest.trim_start_matches('#');
    } else if in_c_block {
        s = s.trim_start_matches('*');
    }
    if in_html || trimmed.starts_with("<!--") {
        if let Some(before) = s.strip_suffix("-->") {
            s = before;
        }
    }
    if in_c_block || trimmed.starts_with("/*") {
        if let Some(before) = s.strip_suffix("*/") {
            s = before.trim_end_matches('*');
        }
    }
    s.trim()
}

/// Checks whether `s` contains `@generated` as a standalone token (not glued to ASCII identifier
/// characters or `-` such as `foo@generated`, `@generated_helper`, or `@not-generated`).
fn contains_at_generated_token(lower: &str) -> bool {
    if lower.contains("@not-generated") {
        return false;
    }
    let needle = "@generated";
    let mut offset = 0usize;
    while let Some(rel) = lower[offset..].find(needle) {
        let pos = offset + rel;
        let before_ok = lower[..pos]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
        let after = &lower[pos + needle.len()..];
        let after_ok = after
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
        if before_ok && after_ok {
            return true;
        }
        offset = pos + needle.len();
    }
    false
}

/// Returns true when a comment line uses `@generated` as an actual generated-file marker
/// (e.g. `// @generated`, `/* @generated by foo */`, `// @generated SignedSource<<...>>`,
/// `// This file is @generated`) rather than hand-written prose mentioning it (e.g.
/// `//! Handles @generated files`).
fn is_explicit_at_generated_marker(body_lower: &str, is_doc_comment: bool) -> bool {
    if body_lower.contains("@not-generated") {
        return false;
    }
    if let Some(after) = body_lower.strip_prefix("@generated") {
        let right_boundary = after
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
        if right_boundary {
            if !is_doc_comment {
                return true;
            }
            let rest = after.trim_start();
            if rest.is_empty()
                || rest.starts_with([':', '-', '.', ',', ';', '(', '[', '<'])
                || rest.starts_with("signedsource")
                || rest.starts_with("by ")
                || rest.starts_with("from ")
                || rest.starts_with("on ")
                || rest.starts_with("using ")
                || rest.starts_with("with ")
                || rest.starts_with("do not ")
            {
                return true;
            }
        }
    }
    contains_at_generated_token(body_lower)
        && (body_lower.contains("signedsource")
            || body_lower.contains("this file is @generated")
            || body_lower.contains("this file was @generated")
            || body_lower.contains("file is @generated")
            || body_lower.contains("automatically @generated")
            || body_lower.contains("auto @generated")
            || body_lower.contains("code @generated"))
}

/// Shared detector for machine-generated markers in the leading comment header (first 5 lines),
/// tracking multi-line `/* ... */` and `<!-- ... -->` block comments so continuation lines without
/// leading `*` are still recognized while non-comment code lines stop inspection immediately.
fn has_generated_header_in_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    hash_policy: HashCommentPolicy,
) -> bool {
    let mut in_c_block_comment = false;
    let mut in_html_comment = false;
    let mut seen_gen_phrase = false;
    let mut seen_do_not_edit = false;
    let mut seen_at_generated_token = false;
    let mut inspected_lines = 0usize;

    for content in lines {
        inspected_lines += 1;
        if inspected_lines > 5 {
            break;
        }
        let cleaned = if inspected_lines == 1 {
            content.trim_start_matches('\u{feff}')
        } else {
            content
        };
        let trimmed = cleaned.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Skip leading Unix shebang (`#!/...`, not Rust `#![...]`), XML declaration (`<?xml ...?>`),
        // or PHP opening tag (`<?php`) on line 1 so generated comments on lines 2..=5 are detected.
        if inspected_lines == 1
            && ((trimmed.starts_with("#!") && !trimmed.starts_with("#!["))
                || (trimmed.starts_with("<?xml") && trimmed.ends_with("?>"))
                || trimmed.eq_ignore_ascii_case("<?php"))
        {
            continue;
        }

        let is_hash_comment = trimmed.starts_with('#')
            && match hash_policy {
                HashCommentPolicy::Allowed => true,
                HashCommentPolicy::Disallowed => false,
                HashCommentPolicy::Heuristic => !is_preprocessor_or_attr_hash_line(trimmed),
            };
        let was_in_c_block = in_c_block_comment;
        let was_in_html = in_html_comment;
        let is_comment = if was_in_c_block || was_in_html {
            true
        } else {
            trimmed.starts_with("//")
                || trimmed.starts_with("/*")
                || is_hash_comment
                || trimmed.starts_with("--")
                || trimmed.starts_with("<!--")
        };
        if !is_comment {
            break;
        }

        if !in_c_block_comment && trimmed.starts_with("/*") && !trimmed.contains("*/") {
            in_c_block_comment = true;
        } else if in_c_block_comment && trimmed.contains("*/") {
            in_c_block_comment = false;
        }
        if !in_html_comment && trimmed.starts_with("<!--") && !trimmed.contains("-->") {
            in_html_comment = true;
        } else if in_html_comment && trimmed.contains("-->") {
            in_html_comment = false;
        }

        let is_doc_comment = trimmed.starts_with("//!") || trimmed.starts_with("///");
        let body_lower =
            strip_comment_delimiters(trimmed, was_in_c_block, was_in_html).to_ascii_lowercase();
        let has_at_generated = is_explicit_at_generated_marker(&body_lower, is_doc_comment);
        seen_at_generated_token |= contains_at_generated_token(&body_lower);
        let has_do_not_edit =
            body_lower.contains("do not edit") || body_lower.contains("do not modify");
        let has_gen_phrase = body_lower.contains("code generated by")
            || body_lower.contains("auto-generated")
            || body_lower.contains("automatically generated")
            || body_lower.contains("this file is generated")
            || body_lower.contains("this file was generated")
            || body_lower.contains("generated by protoc")
            || body_lower.contains("generated by synurang")
            || body_lower.contains("generated by flatc")
            || body_lower.contains("generated by openapi")
            || body_lower.contains("generated by sqlc")
            || body_lower.contains("generated by mockgen");
        seen_gen_phrase |= has_gen_phrase;
        seen_do_not_edit |= has_do_not_edit;
        if has_at_generated
            || ((seen_gen_phrase || seen_at_generated_token) && seen_do_not_edit)
            || body_lower.contains("generated by protoc")
            || body_lower.contains("generated by synurang")
        {
            return true;
        }
    }
    false
}

/// Checks whether a full source file's leading comment header (lines 1..=5) marks it generated.
pub(super) fn has_generated_source_header(source: &str, path: Option<&Path>) -> bool {
    has_generated_header_in_lines(source.lines(), hash_comment_policy_for_path(path))
}

/// Lightweight check of the leading ~2 KiB (lines 1..=5) of a modified file in `source` so
/// unsupported languages (e.g. `.kt`, `.swift`, `.dart`, `.sql`, `.xml`) and oversized files are
/// still pre-omitted when edited far below line 1 without reading the entire file.
fn has_generated_source_header_for_file(
    repo_root: &Path,
    path: &Path,
    source: GitDiffSource<'_>,
) -> bool {
    const HEADER_READ_LIMIT: u64 = 2048;
    use std::io::Read;

    let bytes = match source {
        GitDiffSource::Unstaged => {
            let full_path = repo_root.join(path);
            let Ok(meta) = std::fs::symlink_metadata(&full_path) else {
                return false;
            };
            if meta.file_type().is_symlink() || meta.is_dir() {
                return false;
            }
            let Ok(file) = std::fs::File::open(&full_path) else {
                return false;
            };
            let mut buf = Vec::with_capacity(HEADER_READ_LIMIT as usize);
            if file.take(HEADER_READ_LIMIT).read_to_end(&mut buf).is_err() {
                return false;
            }
            buf
        }
        GitDiffSource::Staged | GitDiffSource::Commit(_) => {
            let obj_prefix = match source {
                GitDiffSource::Staged => ":0:".to_string(),
                GitDiffSource::Commit(sha) => format!("{sha}:"),
                GitDiffSource::Unstaged => unreachable!(),
            };
            let mut cat_arg = std::ffi::OsString::from(&obj_prefix);
            cat_arg.push(path.as_os_str());
            let mut cat_cmd = git_cmd(repo_root);
            cat_cmd.arg("cat-file").arg("blob").arg(&cat_arg);
            let Ok(mut child) = cat_cmd
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
            else {
                return false;
            };
            let mut stdout = child.stdout.take().expect("stdout piped");
            let mut buf = Vec::with_capacity(HEADER_READ_LIMIT as usize);
            let read_res = (&mut stdout).take(HEADER_READ_LIMIT).read_to_end(&mut buf);
            drop(stdout);
            if read_res.is_err() {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            if buf.len() as u64 >= HEADER_READ_LIMIT {
                let _ = child.kill();
                let _ = child.wait();
            } else if !child.wait().is_ok_and(|s| s.success()) {
                return false;
            }
            buf
        }
    };

    if bytes.is_empty() || bytes.contains(&0) {
        return false;
    }
    let header_text = String::from_utf8_lossy(&bytes);
    has_generated_source_header(&header_text, Some(path))
}

/// Checks whether the leading comment header (lines 1..=5) of a diff chunk marks the file as
/// machine-generated (e.g. `// Code generated by ... DO NOT EDIT`, `// @generated`, `# Auto-generated`).
/// Inspects leading comment lines starting at file line 1 (and `-` lines for fully deleted files)
/// while respecting language comment syntax to avoid false positives on code strings, Rust attributes,
/// or C/C++ preprocessor directives.
pub(super) fn has_generated_file_header(chunk: &str) -> bool {
    let chunk_info = extract_target_path_from_diff_chunk(chunk);
    let (path_opt, is_deleted) = match &chunk_info {
        Some((target, old, is_del, _, _)) => (
            Some(if *is_del {
                old.as_deref().unwrap_or(target.as_path())
            } else {
                target.as_path()
            }),
            *is_del,
        ),
        None => (None, false),
    };
    let mut in_top_hunk = false;
    let mut inspect_deleted = false;
    let mut top_lines: Vec<&str> = Vec::with_capacity(5);

    for raw_line in chunk.split_inclusive('\n') {
        let trimmed_nl = raw_line.trim_end_matches(['\r', '\n']);
        if let Some((old_start, _oc, new_start, new_count, _sig)) = parse_hunk_header(trimmed_nl) {
            if new_start == 1 {
                in_top_hunk = true;
                inspect_deleted = false;
                continue;
            } else if is_deleted && old_start == 1 && new_start == 0 && new_count == 0 {
                in_top_hunk = true;
                inspect_deleted = true;
                continue;
            } else {
                break;
            }
        }
        if !in_top_hunk {
            continue;
        }
        let content = if inspect_deleted {
            match trimmed_nl.strip_prefix('-') {
                Some(rest) => rest,
                None => continue,
            }
        } else if let Some(rest) = trimmed_nl.strip_prefix('+') {
            rest
        } else if let Some(rest) = trimmed_nl.strip_prefix(' ') {
            rest
        } else {
            continue;
        };
        top_lines.push(content);
        if top_lines.len() >= 5 {
            break;
        }
    }
    has_generated_header_in_lines(top_lines, hash_comment_policy_for_path(path_opt))
}

pub(super) fn count_diff_churn(chunk: &str) -> (usize, usize) {
    let mut added = 0;
    let mut deleted = 0;
    let mut in_hunk = false;
    for line in chunk.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            deleted += 1;
        }
    }
    (added, deleted)
}

/// Summarizes lockfiles, minified assets, or binaries omitted from the LLM prompt.
/// Note: Because omitted chunks emit `[omitted: ...]` without unified diff hunks,
/// the resulting output is optimized for LLM context inclusion rather than direct `git apply`.
pub(super) fn summarize_omitted_chunk(chunk: &str, path: &Path) -> String {
    let mut header = String::new();
    let mut is_binary = false;
    for line in chunk.split_inclusive('\n') {
        if line.starts_with("@@") {
            break;
        }
        if line.starts_with("GIT binary patch") {
            is_binary = true;
            break;
        }
        if line.starts_with("Binary files ") {
            is_binary = true;
        }
        header.push_str(line);
    }
    let display_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    if is_binary {
        format!("{header}[omitted: {display_name} (binary)]\n\n")
    } else {
        let (added, deleted) = count_diff_churn(chunk);
        format!("{header}[omitted: {display_name} (+{added}/-{deleted} lines)]\n\n")
    }
}

const COMPACT_CONTEXT_LINES: usize = 3;
const BUDGET_BANNER_RESERVE_BYTES: usize = 256;

fn format_budget_size(bytes: usize) -> String {
    if bytes >= 1024 && bytes.is_multiple_of(1024) {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

pub(super) fn strip_clamped_suffix(sig: &str) -> String {
    if let Some(idx) = sig.find(" [clamped] (lines ") {
        sig[..idx].trim_end().to_string()
    } else if sig.starts_with("[clamped] (lines ") {
        String::new()
    } else {
        sig.to_string()
    }
}

/// Computes a 1-based `(start_line, end_line)` span map for each line of a unified diff chunk
/// (new-file line numbers for new/modified chunks, old-file line numbers for fully deleted chunks,
/// and `None` when a pure top-of-file deletion hunk has `new_start == 0` so `L0` anchors are never
/// emitted).
pub(super) fn compute_diff_line_numbers(lines: &[&str]) -> Vec<Option<(usize, usize)>> {
    let mut map = vec![None; lines.len()];
    let mut is_deleted_chunk = false;
    for raw in lines {
        let trimmed = raw.trim_end_matches(['\r', '\n']);
        if trimmed.starts_with("@@ ") {
            break;
        }
        if trimmed.strip_suffix('\t').unwrap_or(trimmed) == "+++ /dev/null"
            || trimmed.starts_with("deleted file mode ")
        {
            is_deleted_chunk = true;
        }
    }

    let mut in_hunk = false;
    let mut cur_old = 0usize;
    let mut cur_new = 0usize;

    for (i, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim_end_matches(['\r', '\n']);
        if let Some((old_start, _, new_start, _, _)) = parse_hunk_header(trimmed) {
            in_hunk = true;
            cur_old = old_start;
            cur_new = new_start;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if is_deleted_chunk {
            if raw.starts_with('-') {
                map[i] = (cur_old > 0).then_some((cur_old, cur_old));
                cur_old += 1;
            } else if raw.starts_with('\\') {
                map[i] = None;
            } else {
                in_hunk = false;
            }
        } else if raw.starts_with(' ') || raw.starts_with('+') {
            map[i] = (cur_new > 0).then_some((cur_new, cur_new));
            cur_new += 1;
        } else if raw.starts_with('-') {
            map[i] = (cur_new > 0).then_some((cur_new, cur_new));
        } else if raw.starts_with('\\') {
            map[i] = None;
        } else {
            in_hunk = false;
        }
    }
    map
}

/// Inserts a file-level metadata marker (e.g. `[degraded to -U3]`, `[degraded to -U0]`)
/// immediately after the diff file header (before the first hunk or inline-test marker),
/// replacing any prior `[degraded to ...]` header marker so markers never duplicate.
fn set_degraded_header_marker(chunk: &str, marker_line: &str) -> String {
    let mut out = String::with_capacity(chunk.len() + marker_line.len() + 2);
    let mut inserted = false;
    for raw_line in chunk.split_inclusive('\n') {
        if raw_line.starts_with("[degraded to ") {
            continue;
        }
        if !inserted
            && (raw_line.starts_with("@@ ")
                || raw_line.starts_with("[omitted inline tests:")
                || raw_line.starts_with("[skeletonized:"))
        {
            out.push_str(marker_line);
            if !marker_line.ends_with('\n') {
                out.push('\n');
            }
            inserted = true;
        }
        out.push_str(raw_line);
    }
    if !inserted {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(marker_line);
        if !marker_line.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Checks whether a chunk's file header section (before the first `@@ `) contains a marker line
/// starting with `prefix`.
fn has_file_header_marker(chunk: &str, prefix: &str) -> bool {
    for line in chunk.split_inclusive('\n') {
        if line.starts_with("@@ ") {
            break;
        }
        if line.starts_with(prefix) {
            return true;
        }
    }
    false
}

/// Checks whether a chunk contains any emitted `[omitted inline tests: L...]` marker (either
/// standalone in a modified file or `+`-prefixed in a new file).
fn has_inline_test_omission_marker(chunk: &str) -> bool {
    chunk.lines().any(|l| {
        l.starts_with("[omitted inline tests: L") || l.starts_with("+[omitted inline tests: L")
    })
}

/// Trims unchanged context lines in a unified diff chunk down to at most `context_lines`
/// around each change, splitting wide hunks cleanly while preserving AST `@@ ... @@`
/// signatures and exact `git apply`-compatible line numbers. Standalone `[omitted inline tests: ...]`
/// markers between hunks are preserved verbatim.
pub(super) fn trim_unified_diff_context(diff: &str, context_lines: usize) -> Option<String> {
    enum LineKind {
        Context,
        Remove,
        Add,
    }
    struct HunkRec<'a> {
        kind: LineKind,
        text: &'a str,
        no_newline: Option<&'a str>,
        old_no: usize,
        new_no: usize,
    }

    let mut out = String::with_capacity(diff.len());
    let mut in_hunk = false;
    let mut parsed_any_hunk = false;
    let mut cur_header: Option<(usize, usize, usize, usize, String)> = None;
    let mut cur_old = 0usize;
    let mut cur_new = 0usize;
    let mut records: Vec<HunkRec<'_>> = Vec::new();

    let flush_hunk = |header: (usize, usize, usize, usize, String),
                      recs: &[HunkRec<'_>],
                      out: &mut String| {
        let (old_start, old_count, new_start, new_count, raw_sig) = header;
        let clean_sig = strip_clamped_suffix(&raw_sig);
        let sig_suffix = if clean_sig.is_empty() {
            String::new()
        } else {
            format!(" {clean_sig}")
        };

        let change_indices: Vec<usize> = recs
            .iter()
            .enumerate()
            .filter(|(_, r)| !matches!(r.kind, LineKind::Context))
            .map(|(i, _)| i)
            .collect();

        if change_indices.is_empty() {
            out.push_str(&format!(
                "@@ -{old_start},{old_count} +{new_start},{new_count} @@{sig_suffix}\n"
            ));
            for r in recs {
                out.push_str(r.text);
                if let Some(nl) = r.no_newline {
                    out.push_str(nl);
                }
            }
            return;
        }

        let mut windows: Vec<(usize, usize)> = Vec::new();
        for idx in change_indices {
            let s = idx.saturating_sub(context_lines);
            let e = (idx + context_lines).min(recs.len() - 1);
            if let Some(last) = windows.last_mut() {
                if s <= last.1 + 1 {
                    last.1 = last.1.max(e);
                    continue;
                }
            }
            windows.push((s, e));
        }

        for (a, b) in windows {
            let slice = &recs[a..=b];
            let sub_old_count = slice
                .iter()
                .filter(|r| !matches!(r.kind, LineKind::Add))
                .count();
            let sub_new_count = slice
                .iter()
                .filter(|r| !matches!(r.kind, LineKind::Remove))
                .count();
            let sub_old_start = if sub_old_count == 0 {
                if old_count == 0 {
                    old_start
                } else {
                    slice[0].old_no.saturating_sub(1)
                }
            } else {
                slice[0].old_no
            };
            let sub_new_start = if sub_new_count == 0 {
                if new_count == 0 {
                    new_start
                } else {
                    slice[0].new_no.saturating_sub(1)
                }
            } else {
                slice[0].new_no
            };

            out.push_str(&format!(
                "@@ -{sub_old_start},{sub_old_count} +{sub_new_start},{sub_new_count} @@{sig_suffix}\n"
            ));
            for r in slice {
                out.push_str(r.text);
                if let Some(nl) = r.no_newline {
                    out.push_str(nl);
                }
            }
        }
    };

    for raw_line in diff.split_inclusive('\n') {
        let trimmed = raw_line.trim_end_matches(['\r', '\n']);
        if let Some(hdr) = parse_hunk_header(trimmed) {
            if let Some(prev_hdr) = cur_header.take() {
                flush_hunk(prev_hdr, &records, &mut out);
                records.clear();
            }
            cur_old = hdr.0;
            cur_new = hdr.2;
            cur_header = Some(hdr);
            in_hunk = true;
            parsed_any_hunk = true;
            continue;
        }

        if raw_line.starts_with("[omitted inline tests:") {
            if let Some(prev_hdr) = cur_header.take() {
                flush_hunk(prev_hdr, &records, &mut out);
                records.clear();
            }
            in_hunk = false;
            out.push_str(raw_line);
            continue;
        }

        if !in_hunk {
            out.push_str(raw_line);
            continue;
        }

        if raw_line.starts_with('\\') {
            records.last_mut()?.no_newline = Some(raw_line);
        } else if raw_line.starts_with(' ') {
            records.push(HunkRec {
                kind: LineKind::Context,
                text: raw_line,
                no_newline: None,
                old_no: cur_old,
                new_no: cur_new,
            });
            cur_old += 1;
            cur_new += 1;
        } else if raw_line.starts_with('-') {
            records.push(HunkRec {
                kind: LineKind::Remove,
                text: raw_line,
                no_newline: None,
                old_no: cur_old,
                new_no: cur_new,
            });
            cur_old += 1;
        } else if raw_line.starts_with('+') {
            records.push(HunkRec {
                kind: LineKind::Add,
                text: raw_line,
                no_newline: None,
                old_no: cur_old,
                new_no: cur_new,
            });
            cur_new += 1;
        } else {
            return None;
        }
    }

    if let Some(prev_hdr) = cur_header.take() {
        flush_hunk(prev_hdr, &records, &mut out);
    }

    if parsed_any_hunk && out.len() < diff.len() {
        Some(out)
    } else {
        None
    }
}

fn summarize_budget_omitted_chunk_with_toc(
    raw_chunk: &str,
    rendered: &str,
    path: &Path,
    is_new: bool,
    is_deleted: bool,
    max_bytes: usize,
) -> String {
    let mut header = String::new();
    for line in raw_chunk.split_inclusive('\n') {
        if line.starts_with("@@") || line.starts_with("GIT binary patch") {
            break;
        }
        header.push_str(line);
    }
    let (added, deleted) = count_diff_churn(raw_chunk);
    let display_path = path.to_string_lossy();
    let plain = format!(
        "{header}[omitted due to patch budget: {display_path} (+{added}/-{deleted} lines)]\n\n"
    );

    #[cfg(feature = "tree-sitter")]
    {
        let toc_head = format!(
            "{header}[omitted due to patch budget: {display_path} (+{added}/-{deleted} lines, "
        );
        // Cap TOC bytes so omitted summaries never blow the overall patch budget.
        let max_toc_bytes = max_bytes.saturating_sub(toc_head.len() + 32).min(2048);
        if let Some((toc_lines, total_defs)) = ast_compact::build_budget_omitted_toc(
            raw_chunk,
            rendered,
            path,
            is_new,
            is_deleted,
            max_toc_bytes,
        ) {
            let mut summary = format!("{toc_head}{total_defs} definitions):\n");
            for l in &toc_lines {
                summary.push_str(l);
                summary.push('\n');
            }
            if total_defs > toc_lines.len() {
                summary.push_str(&format!(
                    "  ... (+{} more definitions)\n",
                    total_defs - toc_lines.len()
                ));
            }
            summary.push_str("]\n\n");
            if summary.len() <= max_bytes {
                return summary;
            }
        }
    }
    #[cfg(not(feature = "tree-sitter"))]
    let _ = (rendered, is_new, is_deleted, max_bytes);

    plain
}

#[cfg(feature = "tree-sitter")]
struct AstDefSpan {
    start_line: usize,
    end_line: usize,
    bare_name: String,
    scope_label: String,
    is_container: bool,
}

#[cfg(feature = "tree-sitter")]
fn short_def_label(
    lang: SupportedLanguage,
    matched: &tree_sitter::Node,
    lines: &[&str],
    source: &str,
) -> Option<String> {
    let kind_prefix = match matched.kind() {
        "function_item" => Some("fn"),
        "struct_item" | "struct_declaration" | "struct_specifier" => Some("struct"),
        "enum_item" | "enum_declaration" | "enum_specifier" => Some("enum"),
        "trait_item" => Some("trait"),
        "type_item" | "type_alias_declaration" | "type_definition" => Some("type"),
        "macro_definition" => Some("macro"),
        "const_item" => Some("const"),
        "static_item" => Some("static"),
        "function_definition" => match lang {
            SupportedLanguage::Python => Some("def"),
            _ => Some("fn"),
        },
        "function_declaration" => match lang {
            SupportedLanguage::Go => Some("func"),
            _ => Some("function"),
        },
        "method_declaration" => match lang {
            SupportedLanguage::Go => Some("func"),
            _ => None,
        },
        "method_definition" | "method" | "singleton_method" => Some("fn"),
        "class_definition"
        | "class_declaration"
        | "abstract_class_declaration"
        | "class_specifier"
        | "class" => Some("class"),
        "interface_declaration" => Some("interface"),
        "record_declaration" => Some("record"),
        "module" => Some("module"),
        _ => None,
    };

    let target = match matched.kind() {
        "decorated_definition" => matched
            .child_by_field_name("definition")
            .unwrap_or(*matched),
        "export_statement" => matched
            .child_by_field_name("declaration")
            .unwrap_or(*matched),
        _ => *matched,
    };
    if let Some(name_node) = target.child_by_field_name("name") {
        if let Some(name) = super::get_node_text_same_line(&name_node, lines) {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                return Some(match kind_prefix {
                    Some(k) => signature::truncate_preview(&format!("{k} {trimmed}"), 40),
                    None => signature::truncate_preview(trimmed, 40),
                });
            }
        }
    }

    let raw = signature::extract_signature(lang, matched, lines, source)?;
    let cut = ast_compact::find_sig_head_cut(&raw, &['(', '{']);
    let head = raw[..cut].trim().trim_end_matches('=').trim();
    if head.is_empty() {
        Some(signature::truncate_preview(&raw, 40))
    } else {
        Some(signature::truncate_preview(head, 40))
    }
}

#[cfg(feature = "tree-sitter")]
fn collect_ast_def_spans_for_new_chunk(raw_chunk: &str, path: &Path) -> Option<Vec<AstDefSpan>> {
    let lang = SupportedLanguage::from_path(path)?;
    let (_hdr, added_source, added_count) = extract_chunk_header_and_added_source(raw_chunk);
    if added_count == 0 || added_source.len() > MAX_TREESITTER_FILE_BYTES {
        return None;
    }

    let tree = parse_with_timeout(lang, &added_source).ok()?;
    let src_lines: Vec<&str> = added_source.lines().collect();
    if src_lines.is_empty() {
        return None;
    }

    let mut defs = Vec::new();
    fn walk_defs(
        lang: SupportedLanguage,
        node: tree_sitter::Node,
        lines: &[&str],
        source: &str,
        out: &mut Vec<AstDefSpan>,
    ) {
        if let Some(matched) = check_primary_definition(lang, &node) {
            let s_line = matched.start_position().row + 1;
            let e_line = (matched.end_position().row + 1).min(lines.len().max(1));
            let is_dup = out
                .last()
                .is_some_and(|prev| prev.start_line == s_line && prev.end_line == e_line);
            if !is_dup {
                if let Some(short_label) = short_def_label(lang, &matched, lines, source) {
                    let bare_name = ast_compact::bare_def_name(lang, &matched, lines, source)
                        .unwrap_or_else(|| short_label.clone());
                    let parent =
                        signature::extract_parent_scope_prefix(lang, &matched, lines, source);
                    let scope_label = match parent {
                        Some(p) => signature::truncate_preview(
                            &format!("{} > {short_label}", signature::truncate_preview(&p, 30)),
                            60,
                        ),
                        None => short_label,
                    };
                    let is_container = is_container_definition_node(&matched);
                    out.push(AstDefSpan {
                        start_line: s_line,
                        end_line: e_line,
                        bare_name,
                        scope_label,
                        is_container,
                    });
                    if !is_container {
                        return;
                    }
                }
            }
        }
        let mut cursor = node.walk();
        for ch in node.named_children(&mut cursor) {
            walk_defs(lang, ch, lines, source, out);
        }
    }

    walk_defs(lang, tree.root_node(), &src_lines, &added_source, &mut defs);
    if defs.is_empty() {
        return None;
    }
    Some(defs)
}

fn build_hunk_scope_truncation_marker(
    lines: &[&str],
    src_line_map: &[Option<super::SourceLineSpan>],
    head_end: usize,
    tail_start: usize,
    display_path: &str,
    churn: (usize, usize),
    max_syms_bytes: usize,
) -> String {
    let (added, deleted) = churn;
    let omitted_lines = tail_start.saturating_sub(head_end);
    let mut omitted_sigs: Vec<String> = Vec::new();
    for line in &lines[head_end..tail_start] {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if let Some((_, _, _, _, sig)) = parse_hunk_header(trimmed) {
            let clean = strip_clamped_suffix(&sig);
            let label = if clean.is_empty() {
                trimmed.to_string()
            } else {
                clean
            };
            if !omitted_sigs.contains(&label) {
                omitted_sigs.push(label);
            }
        }
    }

    let resuming_scope = if lines.get(tail_start).is_some_and(|l| l.starts_with("@@")) {
        None
    } else {
        lines[..tail_start].iter().rev().find_map(|l| {
            let trimmed = l.trim_end_matches(['\r', '\n']);
            parse_hunk_header(trimmed).map(|(_, _, _, _, sig)| {
                let clean = strip_clamped_suffix(&sig);
                if clean.is_empty() {
                    trimmed.to_string()
                } else {
                    clean
                }
            })
        })
    };

    let omitted_first = (head_end..tail_start).find_map(|i| src_line_map[i].map(|(s, _)| s));
    let omitted_last = (head_end..tail_start)
        .rev()
        .find_map(|i| src_line_map[i].map(|(_, e)| e));
    let resumed_src = (tail_start..lines.len()).find_map(|i| src_line_map[i].map(|(s, _)| s));

    let mut details = match (omitted_first, omitted_last) {
        (Some(s), Some(e)) => vec![format!("omitted L{s}-L{e} ({omitted_lines} diff lines)")],
        _ => vec![format!("omitted {omitted_lines} middle diff lines")],
    };
    if !omitted_sigs.is_empty() {
        let mut packed = String::new();
        for (idx, s) in omitted_sigs.iter().enumerate() {
            let short = if s.chars().count() > 40 {
                let p: String = s.chars().take(37).collect();
                format!("{p}...")
            } else {
                s.clone()
            };
            let sep = if idx == 0 { "" } else { ", " };
            let remaining_after = omitted_sigs.len().saturating_sub(idx + 1);
            let reserve = if remaining_after > 0 { 16 } else { 0 };
            if idx > 0 && packed.len() + sep.len() + short.len() + reserve > max_syms_bytes {
                packed.push_str(&format!(", +{} more", omitted_sigs.len() - idx));
                break;
            }
            packed.push_str(sep);
            packed.push_str(&short);
        }
        details.push(format!("hunks: {packed}"));
    }
    if let Some(scope) = resuming_scope {
        let short_scope = if scope.chars().count() > 48 {
            let p: String = scope.chars().take(45).collect();
            format!("{p}...")
        } else {
            scope
        };
        details.push(format!("resuming in {short_scope}"));
    }
    details.push(format!("total +{added}/-{deleted}"));
    let anchor = resumed_src
        .map(|r| format!("[resuming at L{r}]\n"))
        .unwrap_or_default();
    format!(
        "... [truncated due to patch budget: {display_path} ({})]\n{anchor}",
        details.join("; ")
    )
}

/// Truncates a single oversized diff chunk within `budget` bytes, preserving both head
/// (file header + opening diff lines) and tail (closing diff lines). Uses `orig_line_map`
/// (or diff hunk line counting) so `[resuming at L...]` anchors remain exact even after
/// prior line-collapsing tiers, and lists omitted bare symbol names bounded by budget.
fn truncate_chunk_head_tail(
    rendered: &str,
    raw_chunk: &str,
    orig_line_map: Option<&[Option<(usize, usize)>]>,
    path: &Path,
    is_new: bool,
    budget: usize,
) -> Option<String> {
    let lines: Vec<&str> = rendered.split_inclusive('\n').collect();
    if lines.len() < 4 {
        return None;
    }
    let src_line_map: Vec<Option<(usize, usize)>> = match orig_line_map {
        Some(m) if m.len() == lines.len() => m.to_vec(),
        _ => compute_diff_line_numbers(&lines),
    };

    let (added, deleted) = count_diff_churn(raw_chunk);
    let display_path = path.to_string_lossy();
    let base_trailer_for = |omitted: usize,
                            omitted_range: (Option<usize>, Option<usize>),
                            resumed: Option<usize>| {
        let range_label = match omitted_range {
            (Some(s), Some(e)) => format!("omitted L{s}-L{e} ({omitted} diff lines)"),
            _ => format!("omitted {omitted} middle diff lines"),
        };
        let anchor = resumed
            .map(|r| format!("[resuming at L{r}]\n"))
            .unwrap_or_default();
        format!(
            "... [truncated due to patch budget: {display_path} ({range_label}; total +{added}/-{deleted})]\n{anchor}"
        )
    };
    let max_base_trailer =
        base_trailer_for(lines.len(), (Some(1), Some(lines.len())), Some(lines.len()));
    let rich_mode = budget >= 1024;
    let max_syms_bytes = (budget / 3).clamp(160, 2048);
    let initial_reserve = if rich_mode {
        (max_base_trailer.len() + max_syms_bytes.min(512)).min(budget / 3)
    } else {
        max_base_trailer.len() + 8
    };
    if budget <= initial_reserve + 128 {
        return None;
    }
    let avail = budget - initial_reserve;
    let head_budget = avail / 2;
    let tail_budget = avail - head_budget;

    let mut head_end = 0usize;
    let mut head_bytes = 0usize;
    for l in &lines {
        if head_bytes + l.len() > head_budget {
            break;
        }
        head_bytes += l.len();
        head_end += 1;
    }
    if head_end == 0 {
        return None;
    }

    let mut tail_start = lines.len();
    let mut tail_bytes = 0usize;
    while tail_start > head_end {
        let l = lines[tail_start - 1];
        if tail_bytes + l.len() > tail_budget {
            break;
        }
        tail_bytes += l.len();
        tail_start -= 1;
    }
    if tail_start <= head_end {
        return None;
    }

    // Find end of file header (lines up to and including first `@@`) so snapping never cuts the header
    // or any `[degraded to ...]` / `[skeletonized: ...]` header marker.
    let header_end = lines
        .iter()
        .position(|l| l.starts_with("@@"))
        .map(|i| i + 1)
        .unwrap_or(1)
        .min(head_end);

    #[cfg(feature = "tree-sitter")]
    let ast_defs = if is_new && budget >= 1024 {
        collect_ast_def_spans_for_new_chunk(raw_chunk, path)
    } else {
        None
    };

    #[cfg(feature = "tree-sitter")]
    if let Some(ref defs) = ast_defs {
        // Snap `head_end` backward onto the end of a completed AST definition if within 30% of head_bytes.
        // Uses each rendered line's closing span `.1` so skeletonized functions (e.g. L10-L39 collapsed
        // into one line) are recognized as completed definitions at L39.
        if let Some(last_head_src) = lines[..head_end]
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, _)| src_line_map[i].map(|(_, end)| end))
        {
            let min_head_bytes = (head_bytes * 7) / 10;
            let best_def_end = defs
                .iter()
                .filter(|d| !d.is_container && d.end_line <= last_head_src)
                .map(|d| d.end_line)
                .max();
            if let Some(target_end_src) = best_def_end {
                if let Some(snap_diff_idx) = src_line_map.iter().enumerate().find_map(|(i, sl)| {
                    sl.is_some_and(|(_, end)| end == target_end_src)
                        .then_some(i + 1)
                }) {
                    if snap_diff_idx >= header_end && snap_diff_idx < head_end {
                        let new_head_bytes: usize =
                            lines[..snap_diff_idx].iter().map(|l| l.len()).sum();
                        if new_head_bytes >= min_head_bytes {
                            head_end = snap_diff_idx;
                            head_bytes = new_head_bytes;
                        }
                    }
                }
            }
        }

        // Snap `tail_start` forward onto the start of an AST definition if within 30% of tail_bytes.
        if let Some(first_tail_src) = lines[tail_start..]
            .iter()
            .enumerate()
            .find_map(|(rel, _)| src_line_map[tail_start + rel].map(|(start, _)| start))
        {
            let min_tail_bytes = (tail_bytes * 7) / 10;
            let best_def_start = defs
                .iter()
                .filter(|d| !d.is_container && d.start_line >= first_tail_src)
                .map(|d| d.start_line)
                .min();
            if let Some(target_start_src) = best_def_start {
                if let Some(snap_diff_idx) = src_line_map.iter().enumerate().find_map(|(i, sl)| {
                    sl.is_some_and(|(start, _)| start == target_start_src)
                        .then_some(i)
                }) {
                    if snap_diff_idx > tail_start && snap_diff_idx < lines.len() {
                        let new_tail_bytes: usize =
                            lines[snap_diff_idx..].iter().map(|l| l.len()).sum();
                        if new_tail_bytes >= min_tail_bytes {
                            tail_start = snap_diff_idx;
                            tail_bytes = new_tail_bytes;
                        }
                    }
                }
            }
        }
    }

    let build_marker = |he: usize, ts: usize| -> String {
        let omitted_first_src = (he..ts).find_map(|i| src_line_map[i].map(|(s, _)| s));
        let omitted_last_src = (he..ts).rev().find_map(|i| src_line_map[i].map(|(_, e)| e));
        let resumed_src = (ts..lines.len()).find_map(|i| src_line_map[i].map(|(s, _)| s));

        #[cfg(feature = "tree-sitter")]
        if let Some(ref defs) = ast_defs {
            if let (Some(om_start), Some(om_end)) = (omitted_first_src, omitted_last_src) {
                let omitted_lines = ts - he;
                let mut omitted_defs: Vec<&str> = Vec::new();
                for d in defs {
                    if d.start_line >= om_start
                        && d.start_line <= om_end
                        && !omitted_defs.contains(&d.bare_name.as_str())
                    {
                        omitted_defs.push(d.bare_name.as_str());
                    }
                }

                let resuming_in = resumed_src.and_then(|r_line| {
                    defs.iter()
                        .filter(|d| d.start_line < r_line && r_line <= d.end_line)
                        .min_by_key(|d| d.end_line.saturating_sub(d.start_line))
                        .map(|d| d.scope_label.as_str())
                });

                let mut parts = vec![format!(
                    "omitted L{om_start}-L{om_end} ({omitted_lines} diff lines)"
                )];
                if !omitted_defs.is_empty() {
                    let mut packed = String::new();
                    for (idx, name) in omitted_defs.iter().enumerate() {
                        let sep = if idx == 0 { "" } else { ", " };
                        let remaining_after = omitted_defs.len().saturating_sub(idx + 1);
                        let reserve = if remaining_after > 0 { 16 } else { 0 };
                        if idx > 0
                            && packed.len() + sep.len() + name.len() + reserve > max_syms_bytes
                        {
                            packed.push_str(&format!(", +{} more", omitted_defs.len() - idx));
                            break;
                        }
                        packed.push_str(sep);
                        packed.push_str(name);
                    }
                    parts.push(format!("defs: {packed}"));
                }
                if let Some(scope) = resuming_in {
                    parts.push(format!("resuming in {scope}"));
                }
                parts.push(format!("total +{added}/-{deleted}"));
                let anchor = resumed_src
                    .map(|r| format!("[resuming at L{r}]\n"))
                    .unwrap_or_default();
                return format!(
                    "... [truncated due to patch budget: {display_path} ({})]\n{anchor}",
                    parts.join("; ")
                );
            }
        }

        if !is_new && budget >= 1024 {
            build_hunk_scope_truncation_marker(
                &lines,
                &src_line_map,
                he,
                ts,
                &display_path,
                (added, deleted),
                max_syms_bytes,
            )
        } else {
            base_trailer_for(ts - he, (omitted_first_src, omitted_last_src), resumed_src)
        }
    };

    let mut marker = build_marker(head_end, tail_start);
    let extra_newlines_for = |he: usize| {
        usize::from(!lines[he - 1].ends_with('\n'))
            + usize::from(!lines[lines.len() - 1].ends_with('\n'))
    };

    // Strictly enforce `budget` even if the enriched marker is wider than `initial_reserve`.
    while head_bytes + marker.len() + tail_bytes + extra_newlines_for(head_end) > budget {
        if head_end > header_end && (head_bytes >= tail_bytes || tail_start + 1 >= lines.len()) {
            head_end -= 1;
            head_bytes = head_bytes.saturating_sub(lines[head_end].len());
        } else if tail_start + 1 < lines.len() {
            tail_bytes = tail_bytes.saturating_sub(lines[tail_start].len());
            tail_start += 1;
        } else {
            let om_first = (head_end..tail_start).find_map(|i| src_line_map[i].map(|(s, _)| s));
            let om_last = (head_end..tail_start)
                .rev()
                .find_map(|i| src_line_map[i].map(|(_, e)| e));
            let resumed = (tail_start..lines.len()).find_map(|i| src_line_map[i].map(|(s, _)| s));
            marker = base_trailer_for(tail_start - head_end, (om_first, om_last), resumed);
            break;
        }
        marker = build_marker(head_end, tail_start);
        if head_bytes + marker.len() + tail_bytes + extra_newlines_for(head_end) <= budget {
            break;
        }
    }

    if tail_start <= head_end {
        return None;
    }

    let mut out = String::with_capacity(head_bytes + tail_bytes + marker.len() + 2);
    for l in &lines[..head_end] {
        out.push_str(l);
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&marker);
    for l in &lines[tail_start..] {
        out.push_str(l);
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if out.len() < rendered.len() && out.len() <= budget {
        Some(out)
    } else {
        None
    }
}

/// Computes the maximum uniform per-chunk byte ceiling `C` such that
/// `sum(min(size, C)) <= budget`. Chunks smaller than or equal to `C` are untouched.
fn compute_waterfill_cap(sizes: &[usize], budget: usize) -> usize {
    if sizes.is_empty() {
        return budget;
    }
    let mut sorted = sizes.to_vec();
    sorted.sort_unstable();
    let mut remaining_budget = budget;
    for (i, &sz) in sorted.iter().enumerate() {
        let remaining_count = sorted.len() - i;
        let fair = remaining_budget / remaining_count;
        if sz <= fair {
            remaining_budget = remaining_budget.saturating_sub(sz);
        } else {
            return fair;
        }
    }
    sorted.last().copied().unwrap_or(budget)
}

pub(super) struct RenderedChunk {
    pub(super) path: Option<PathBuf>,
    pub(super) is_test: bool,
    pub(super) is_new: bool,
    pub(super) is_deleted: bool,
    pub(super) pre_omitted: bool,
    pub(super) is_omitted: bool,
    pub(super) raw_chunk: String,
    pub(super) rendered: String,
    pub(super) orig_line_map: Option<Vec<Option<(usize, usize)>>>,
    #[cfg(feature = "tree-sitter")]
    pub(super) inline_test_blocks: Vec<ast_compact::InlineTestBlock>,
}

fn split_raw_diff_chunks(raw_diff: &str) -> (String, Vec<String>) {
    let mut commit_header = String::new();
    let mut chunks: Vec<String> = Vec::new();
    let mut current_chunk = String::new();
    for line in raw_diff.split_inclusive('\n') {
        let is_chunk_start = line.starts_with("diff --git ")
            || line.starts_with("diff --cc ")
            || line.starts_with("diff --combined ");
        if is_chunk_start && !current_chunk.is_empty() {
            chunks.push(std::mem::take(&mut current_chunk));
        }
        if chunks.is_empty() && !is_chunk_start && current_chunk.is_empty() {
            commit_header.push_str(line);
        } else {
            current_chunk.push_str(line);
        }
    }
    if !current_chunk.is_empty() {
        chunks.push(current_chunk);
    }
    (commit_header, chunks)
}

pub(super) fn apply_patch_budget(
    prefix: &str,
    mut items: Vec<RenderedChunk>,
    max_bytes: usize,
    warnings: &mut Vec<String>,
) -> String {
    let total_bytes = |list: &[RenderedChunk]| -> usize {
        prefix.len() + list.iter().map(|c| c.rendered.len()).sum::<usize>()
    };

    if total_bytes(&items) <= max_bytes {
        let mut out = String::with_capacity(total_bytes(&items));
        out.push_str(prefix);
        for item in items {
            out.push_str(&item.rendered);
        }
        return out;
    }

    // Reserve space for the top `[patch-budget: ...]` metadata banner on real budgets (>= 16 KiB)
    // so the banner always fits without violating `max_bytes`.
    let effective_max = if max_bytes >= 16 * 1024 {
        max_bytes.saturating_sub(BUDGET_BANNER_RESERVE_BYTES)
    } else {
        max_bytes
    };

    // Tier 2a: shrink separate modified test chunks down to compact `-U3` (preserving AST signatures)
    {
        let mut test_u3: Vec<(usize, String)> = items
            .iter()
            .enumerate()
            .filter(|(_, c)| c.is_test && !c.is_omitted && !c.is_new && !c.is_deleted)
            .filter_map(|(i, c)| {
                let compact = trim_unified_diff_context(&c.rendered, COMPACT_CONTEXT_LINES)?;
                let marked = set_degraded_header_marker(&compact, "[degraded to -U3]");
                (marked.len() < c.rendered.len()).then_some((i, marked))
            })
            .collect();
        test_u3.sort_by(|(a, ca), (b, cb)| {
            let save_a = items[*a].rendered.len().saturating_sub(ca.len());
            let save_b = items[*b].rendered.len().saturating_sub(cb.len());
            save_b.cmp(&save_a)
        });
        for (idx, marked) in test_u3 {
            if total_bytes(&items) <= effective_max {
                break;
            }
            items[idx].rendered = marked;
            items[idx].orig_line_map = None;
        }
    }

    // Tier 2b: fold explicit inline test blocks inside production files into AST TOCs before
    // degrading or cutting any production logic (largest savings first).
    #[cfg(feature = "tree-sitter")]
    if total_bytes(&items) > effective_max {
        type InlineTestCandidate = (usize, String, Option<super::SourceLineMap>);
        let mut inline_candidates: Vec<InlineTestCandidate> = items
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.is_test && !c.is_omitted && !c.is_deleted)
            .filter_map(|(i, c)| {
                if c.is_new {
                    let p = c.path.as_deref()?;
                    let (folded, map) = ast_compact::fold_inline_tests_in_new_chunk(
                        &c.raw_chunk,
                        &c.rendered,
                        c.orig_line_map.as_deref(),
                        p,
                    )?;
                    Some((i, folded, Some(map)))
                } else {
                    let folded = ast_compact::fold_inline_tests_in_modified_chunk(
                        &c.rendered,
                        &c.inline_test_blocks,
                    )?;
                    Some((i, folded, None))
                }
            })
            .collect();
        inline_candidates.sort_by(|(a, fa, _), (b, fb, _)| {
            let save_a = items[*a].rendered.len().saturating_sub(fa.len());
            let save_b = items[*b].rendered.len().saturating_sub(fb.len());
            save_b.cmp(&save_a)
        });
        for (idx, folded, map) in inline_candidates {
            if total_bytes(&items) <= effective_max {
                break;
            }
            items[idx].rendered = folded;
            if map.is_some() {
                items[idx].orig_line_map = map;
            }
        }
    }

    // Tier 2c: shrink separate test chunks to `-U0`, then omit remaining separate test files with AST TOC
    if total_bytes(&items) > effective_max {
        let mut test_u0: Vec<(usize, String)> = items
            .iter()
            .enumerate()
            .filter(|(_, c)| c.is_test && !c.is_omitted && !c.is_new && !c.is_deleted)
            .filter_map(|(i, c)| {
                let candidate = trim_unified_diff_context(&c.rendered, 0)
                    .filter(|u0| u0.len() <= c.raw_chunk.len() + 120)
                    .unwrap_or_else(|| c.raw_chunk.clone());
                let marked = set_degraded_header_marker(&candidate, "[degraded to -U0]");
                (marked.len() < c.rendered.len()).then_some((i, marked))
            })
            .collect();
        test_u0.sort_by(|(a, ca), (b, cb)| {
            let save_a = items[*a].rendered.len().saturating_sub(ca.len());
            let save_b = items[*b].rendered.len().saturating_sub(cb.len());
            save_b.cmp(&save_a)
        });
        for (idx, marked) in test_u0 {
            if total_bytes(&items) <= effective_max {
                break;
            }
            items[idx].rendered = marked;
            items[idx].orig_line_map = None;
        }
    }

    if total_bytes(&items) > effective_max {
        let mut test_indices: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, c)| c.is_test && !c.is_omitted)
            .map(|(i, _)| i)
            .collect();
        test_indices.sort_by(|&a, &b| items[b].rendered.len().cmp(&items[a].rendered.len()));

        let test_count = test_indices.len().max(1);
        let toc_cap = ((effective_max / 8) / test_count)
            .clamp(128, 1536)
            .min(effective_max / 2);

        for idx in test_indices {
            if total_bytes(&items) <= effective_max {
                break;
            }
            if let Some(ref p) = items[idx].path {
                let toc_budget = (items[idx].rendered.len() / 2).min(toc_cap);
                let summary = summarize_budget_omitted_chunk_with_toc(
                    &items[idx].raw_chunk,
                    &items[idx].rendered,
                    p,
                    items[idx].is_new,
                    items[idx].is_deleted,
                    toc_budget,
                );
                if summary.len() < items[idx].rendered.len() {
                    items[idx].rendered = summary;
                    items[idx].is_omitted = true;
                    items[idx].orig_line_map = None;
                }
            }
        }
    }

    // Tier 3: shrink expanded modified production chunks from full AST/`-W` down to compact `-U3`
    // while preserving AST `@@ ... @@` signatures, `-`/`+` edits, and `[omitted inline tests: ...]` markers.
    if total_bytes(&items) > effective_max {
        let mut prod_u3: Vec<(usize, String)> = items
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.is_test && !c.is_omitted && !c.is_new && !c.is_deleted)
            .filter_map(|(i, c)| {
                let compact = trim_unified_diff_context(&c.rendered, COMPACT_CONTEXT_LINES)?;
                let marked = set_degraded_header_marker(&compact, "[degraded to -U3]");
                (marked.len() < c.rendered.len()).then_some((i, marked))
            })
            .collect();
        prod_u3.sort_by(|(a, ca), (b, cb)| {
            let save_a = items[*a].rendered.len().saturating_sub(ca.len());
            let save_b = items[*b].rendered.len().saturating_sub(cb.len());
            save_b.cmp(&save_a)
        });
        for (idx, marked) in prod_u3 {
            if total_bytes(&items) <= effective_max {
                break;
            }
            items[idx].rendered = marked;
            items[idx].orig_line_map = None;
        }
    }

    // Tier 4: progressive water-fill-aware AST skeletonization for oversized NEW production files
    // (preserves imports, types, impl/class headers, doc comments, and all function signatures;
    // folds longest function bodies first with exact original line ranges).
    #[cfg(feature = "tree-sitter")]
    if total_bytes(&items) > effective_max {
        let mut new_prod_indices: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                !c.is_test && !c.is_omitted && c.is_new && !c.is_deleted && c.path.is_some()
            })
            .map(|(i, _)| i)
            .collect();
        new_prod_indices.sort_by(|&a, &b| items[b].rendered.len().cmp(&items[a].rendered.len()));

        for pos in 0..new_prod_indices.len() {
            let cur_total = total_bytes(&items);
            if cur_total <= effective_max {
                break;
            }
            let idx = new_prod_indices[pos];
            // Water-fill ONLY across remaining eligible NEW production files; modified files
            // and already-processed chunks remain fixed so new files absorb compression before
            // Tier 5 ever truncates modified production diffs.
            let remaining_new_sizes: Vec<usize> = new_prod_indices[pos..]
                .iter()
                .map(|&i| items[i].rendered.len())
                .collect();
            let remaining_new_sum: usize = remaining_new_sizes.iter().sum();
            let fixed = cur_total.saturating_sub(remaining_new_sum);
            let avail = effective_max.saturating_sub(fixed);
            let cap = compute_waterfill_cap(&remaining_new_sizes, avail);

            // Preserve small new files already at or below the water-fill level.
            if items[idx].rendered.len() <= cap {
                continue;
            }

            let other_bytes = cur_total.saturating_sub(items[idx].rendered.len());
            let needed_only_from_this = effective_max.saturating_sub(other_bytes);
            let target_budget = cap.max(needed_only_from_this);
            if target_budget >= items[idx].rendered.len() {
                continue;
            }

            if let Some(ref p) = items[idx].path {
                if let Some((skel, new_map, _, _)) = ast_compact::skeletonize_new_chunk_progressive(
                    &items[idx].raw_chunk,
                    &items[idx].rendered,
                    items[idx].orig_line_map.as_deref(),
                    p,
                    target_budget,
                ) {
                    items[idx].rendered = skel;
                    items[idx].orig_line_map = Some(new_map);
                }
            }
        }
    }

    // Tier 5: last-resort extreme `-U0` (if critically tight), water-filling Head+Tail truncation
    // with explicit `[resuming at L...]` anchors + bare symbol lists, or budget-omitted AST TOC.
    if total_bytes(&items) > effective_max {
        let tier5_max = if max_bytes < 16 * 1024 && max_bytes > 1024 + BUDGET_BANNER_RESERVE_BYTES {
            max_bytes.saturating_sub(BUDGET_BANNER_RESERVE_BYTES)
        } else {
            effective_max
        };

        let initial_rem_sizes: Vec<usize> = items
            .iter()
            .filter(|c| !c.is_omitted)
            .map(|c| c.rendered.len())
            .collect();
        let initial_rem_total: usize = initial_rem_sizes.iter().sum();
        let initial_fixed = total_bytes(&items).saturating_sub(initial_rem_total);
        let initial_avail = tier5_max.saturating_sub(initial_fixed);
        let initial_cap = compute_waterfill_cap(&initial_rem_sizes, initial_avail);

        if initial_cap < 1024 {
            let mut prod_u0: Vec<(usize, String)> = items
                .iter()
                .enumerate()
                .filter(|(_, c)| !c.is_test && !c.is_omitted && !c.is_new && !c.is_deleted)
                .filter_map(|(i, c)| {
                    // Never fall back to `c.raw_chunk` if Tier 2b already folded inline test hunks,
                    // otherwise folded inline tests would be resurrected.
                    let has_folded_inline = has_inline_test_omission_marker(&c.rendered);
                    let candidate = if has_folded_inline {
                        trim_unified_diff_context(&c.rendered, 0)?
                    } else {
                        trim_unified_diff_context(&c.rendered, 0)
                            .filter(|u0| u0.len() <= c.raw_chunk.len() + 120)
                            .unwrap_or_else(|| c.raw_chunk.clone())
                    };
                    let marked = set_degraded_header_marker(&candidate, "[degraded to -U0]");
                    (marked.len() < c.rendered.len()).then_some((i, marked))
                })
                .collect();
            prod_u0.sort_by(|(a, ca), (b, cb)| {
                let save_a = items[*a].rendered.len().saturating_sub(ca.len());
                let save_b = items[*b].rendered.len().saturating_sub(cb.len());
                save_b.cmp(&save_a)
            });
            for (idx, marked) in prod_u0 {
                if total_bytes(&items) <= tier5_max {
                    break;
                }
                items[idx].rendered = marked;
                items[idx].orig_line_map = None;
            }
        }

        if total_bytes(&items) > tier5_max {
            let mut rem_indices: Vec<usize> = items
                .iter()
                .enumerate()
                .filter(|(_, c)| !c.is_omitted && c.path.is_some())
                .map(|(i, _)| i)
                .collect();
            rem_indices.sort_by(|&a, &b| items[b].rendered.len().cmp(&items[a].rendered.len()));

            for pos in 0..rem_indices.len() {
                let cur_total = total_bytes(&items);
                if cur_total <= tier5_max {
                    break;
                }
                let idx = rem_indices[pos];
                let remaining_sizes: Vec<usize> = rem_indices[pos..]
                    .iter()
                    .map(|&i| items[i].rendered.len())
                    .collect();
                let remaining_sum: usize = remaining_sizes.iter().sum();
                let fixed_now = cur_total.saturating_sub(remaining_sum);
                let avail_now = tier5_max.saturating_sub(fixed_now);
                let cap = compute_waterfill_cap(&remaining_sizes, avail_now);

                let other_bytes = cur_total.saturating_sub(items[idx].rendered.len());
                let needed_only_from_this = tier5_max.saturating_sub(other_bytes);

                // Water-filling invariant: chunks <= cap are preserved untouched unless rounding
                // leaves a few bytes over budget on the largest remaining chunk.
                if items[idx].rendered.len() <= cap
                    && items[idx].rendered.len() <= needed_only_from_this
                {
                    continue;
                }

                let chunk_budget = cap
                    .max(needed_only_from_this)
                    .min(items[idx].rendered.len());
                if let Some(ref p) = items[idx].path {
                    let replacement = if chunk_budget >= 512 {
                        match truncate_chunk_head_tail(
                            &items[idx].rendered,
                            &items[idx].raw_chunk,
                            items[idx].orig_line_map.as_deref(),
                            p,
                            items[idx].is_new,
                            chunk_budget,
                        ) {
                            Some(truncated) => truncated,
                            None => summarize_budget_omitted_chunk_with_toc(
                                &items[idx].raw_chunk,
                                &items[idx].rendered,
                                p,
                                items[idx].is_new,
                                items[idx].is_deleted,
                                chunk_budget,
                            ),
                        }
                    } else {
                        summarize_budget_omitted_chunk_with_toc(
                            &items[idx].raw_chunk,
                            &items[idx].rendered,
                            p,
                            items[idx].is_new,
                            items[idx].is_deleted,
                            chunk_budget,
                        )
                    };
                    if replacement.len() < items[idx].rendered.len() {
                        items[idx].rendered = replacement;
                        items[idx].is_omitted = true;
                        items[idx].orig_line_map = None;
                    }
                }
            }
        }
    }

    // Derive banner counts directly from final emitted markers so banner and body match 1:1.
    let pre_omitted_count = items.iter().filter(|c| c.pre_omitted).count();
    let mut inline_tests_omitted_count = 0usize;
    let mut degraded_u3 = 0usize;
    let mut degraded_u0 = 0usize;
    let mut skeletonized_count = 0usize;
    let mut truncated_count = 0usize;
    let mut budget_omitted_count = 0usize;
    let mut compacted_files = 0usize;

    for c in &items {
        if c.pre_omitted {
            continue;
        }
        let has_inline = has_inline_test_omission_marker(&c.rendered);
        let has_u3 = has_file_header_marker(&c.rendered, "[degraded to -U3]");
        let has_u0 = has_file_header_marker(&c.rendered, "[degraded to -U0]");
        let has_skel = has_file_header_marker(&c.rendered, "[skeletonized:");
        let has_trunc = c
            .rendered
            .lines()
            .any(|l| l.starts_with("... [truncated due to patch budget:"));
        let has_omit = c
            .rendered
            .lines()
            .any(|l| l.starts_with("[omitted due to patch budget:"));

        if has_inline {
            inline_tests_omitted_count += 1;
        }
        if has_u3 {
            degraded_u3 += 1;
        }
        if has_u0 {
            degraded_u0 += 1;
        }
        if has_skel {
            skeletonized_count += 1;
        }
        if has_trunc {
            truncated_count += 1;
        }
        if has_omit {
            budget_omitted_count += 1;
        }
        if has_inline || has_u3 || has_u0 || has_skel || has_trunc || has_omit {
            compacted_files += 1;
        }
    }

    let mut banner = String::new();
    if compacted_files > 0 {
        let mut detail_parts = Vec::new();
        if pre_omitted_count > 0 {
            detail_parts.push(format!("{pre_omitted_count} pre-omitted"));
        }
        if inline_tests_omitted_count > 0 {
            detail_parts.push(format!("{inline_tests_omitted_count} inline-tests-omitted"));
        }
        if degraded_u3 > 0 {
            detail_parts.push(format!("{degraded_u3} degraded to -U3"));
        }
        if degraded_u0 > 0 {
            detail_parts.push(format!("{degraded_u0} degraded to -U0"));
        }
        if skeletonized_count > 0 {
            detail_parts.push(format!("{skeletonized_count} skeletonized"));
        }
        if truncated_count > 0 {
            detail_parts.push(format!("{truncated_count} truncated"));
        }
        if budget_omitted_count > 0 {
            detail_parts.push(format!("{budget_omitted_count} budget-omitted"));
        }
        let budget_label = format_budget_size(max_bytes);
        let candidate_banner = format!(
            "[patch-budget: {budget_label} hit; {}]\n\n",
            detail_parts.join(", ")
        );
        if total_bytes(&items) + candidate_banner.len() <= max_bytes {
            banner = candidate_banner;
        }

        let msg = format!(
            "diff output exceeded patch budget ({max_bytes} bytes); compacted {compacted_files} file chunk(s) ({})",
            detail_parts.join(", ")
        );
        if !warnings.contains(&msg) {
            warnings.push(msg);
        }
    }

    let mut out = String::with_capacity(total_bytes(&items) + banner.len());
    out.push_str(prefix);
    out.push_str(&banner);
    for item in items {
        out.push_str(&item.rendered);
    }
    out
}

fn generate_diff_preamble(repo_root: &Path, source: GitDiffSource<'_>) -> String {
    let target_desc = match source {
        GitDiffSource::Staged => "staged (HEAD -> index)",
        GitDiffSource::Unstaged => "unstaged (index -> worktree)",
        GitDiffSource::Commit(_) => return String::new(),
    };

    let mut stat_cmd = git_cmd(repo_root);
    stat_cmd.args([
        "diff",
        "--stat=120",
        "--stat-name-width=90",
        "-M",
        "--no-color",
    ]);
    if matches!(source, GitDiffSource::Staged) {
        stat_cmd.arg("--staged");
    }

    let stat_str = stat_cmd
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    let trimmed = stat_str.trim_matches(|c| c == '\r' || c == '\n');
    if trimmed.is_empty() {
        format!("Target: {target_desc}\n\n")
    } else {
        format!("Target: {target_desc}\n\n{trimmed}\n\n")
    }
}

/// Generates the full combined diff for the workspace, expanding supported languages
/// with Tree-Sitter AST context and falling back to `git diff -W` for unsupported ones.
/// Uses a single `git diff -U0 -M` (or `git show -U0 -M`) execution to eliminate per-file subprocess overhead.
pub fn generate_expanded_diff_for_source(
    workspace: &Path,
    source: GitDiffSource<'_>,
    warnings: &mut Vec<String>,
) -> Result<String> {
    let repo_root = get_repo_root(workspace)?;

    #[cfg(not(feature = "tree-sitter"))]
    {
        warnings.push("tree-sitter feature is disabled; falling back to git diff -W".to_string());
        let raw = git_diff_w_all(&repo_root, source)?;
        let (commit_header, chunks) = split_raw_diff_chunks(&raw);
        let preamble = generate_diff_preamble(&repo_root, source);
        let prefix = format!("{preamble}{commit_header}");

        if chunks.is_empty() {
            return Ok(format!("{preamble}{raw}"));
        }

        let rendered: Vec<RenderedChunk> = chunks
            .into_iter()
            .map(|chunk| {
                let info = extract_target_path_from_diff_chunk(&chunk);
                if let Some((target_path, _, is_deleted, is_new, is_special_mode)) = info {
                    let path_is_test = is_test_file_path(&target_path);
                    let is_generated = is_lockfile_or_generated(&target_path)
                        || has_generated_file_header(&chunk)
                        || (!is_deleted
                            && !is_new
                            && !is_special_mode
                            && has_generated_source_header_for_file(
                                &repo_root,
                                &target_path,
                                source,
                            ));
                    if is_generated {
                        let omitted = summarize_omitted_chunk(&chunk, &target_path);
                        return RenderedChunk {
                            path: Some(target_path),
                            is_test: path_is_test,
                            is_new,
                            is_deleted,
                            pre_omitted: true,
                            is_omitted: true,
                            raw_chunk: chunk,
                            rendered: omitted,
                            orig_line_map: None,
                        };
                    }
                    return RenderedChunk {
                        path: Some(target_path),
                        is_test: path_is_test,
                        is_new,
                        is_deleted,
                        pre_omitted: false,
                        is_omitted: false,
                        raw_chunk: chunk.clone(),
                        rendered: chunk,
                        orig_line_map: None,
                    };
                }
                RenderedChunk {
                    path: None,
                    is_test: false,
                    is_new: false,
                    is_deleted: false,
                    pre_omitted: false,
                    is_omitted: false,
                    raw_chunk: chunk.clone(),
                    rendered: chunk,
                    orig_line_map: None,
                }
            })
            .collect();

        Ok(apply_patch_budget(
            &prefix,
            rendered,
            resolve_patch_budget_bytes(),
            warnings,
        ))
    }

    #[cfg(feature = "tree-sitter")]
    {
        // 1. Run git diff -U0 -M (or git show -U0 -M) once using histogram diff
        let mut cmd = git_cmd(&repo_root);
        match source {
            GitDiffSource::Unstaged => {
                cmd.args([
                    "diff",
                    "-U0",
                    "-M",
                    "--diff-algorithm=histogram",
                    "--no-color",
                    "--no-ext-diff",
                ]);
            }
            GitDiffSource::Staged => {
                cmd.args([
                    "diff",
                    "-U0",
                    "-M",
                    "--diff-algorithm=histogram",
                    "--no-color",
                    "--no-ext-diff",
                    "--staged",
                ]);
            }
            GitDiffSource::Commit(sha) => {
                cmd.args([
                    "show",
                    "-U0",
                    "-M",
                    "--diff-algorithm=histogram",
                    "--first-parent",
                    "--no-color",
                    "--no-ext-diff",
                    sha,
                ]);
            }
        }
        let output = cmd
            .output()
            .map_err(|e| ServiceError::invalid(format!("git command failed: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.to_ascii_lowercase().contains("not a git repository") {
                return Err(ServiceError::invalid("not a git repository"));
            }
            return Err(ServiceError::invalid(format!(
                "git command failed: {stderr}"
            )));
        }

        let raw_diff = String::from_utf8_lossy(&output.stdout);

        if raw_diff.trim().is_empty() {
            return Err(ServiceError::invalid(match source {
                GitDiffSource::Staged => "no staged git changes to attach",
                GitDiffSource::Unstaged => "no unstaged git changes to attach",
                GitDiffSource::Commit(_) => "git commit produced empty output",
            }));
        }

        let (mut commit_header, chunks) = split_raw_diff_chunks(&raw_diff);

        if let GitDiffSource::Commit(sha) = source {
            let mut stat_cmd = git_cmd(&repo_root);
            stat_cmd.args([
                "show",
                "--stat=120",
                "--stat-name-width=90",
                "-M",
                "--first-parent",
                "--no-color",
                "--format=",
                sha,
            ]);
            if let Ok(stat_output) = stat_cmd.output() {
                if stat_output.status.success() {
                    let stat_str = String::from_utf8_lossy(&stat_output.stdout);
                    let trimmed = stat_str.trim_matches(|c| c == '\r' || c == '\n');
                    if !trimmed.is_empty() {
                        if !commit_header.ends_with('\n') {
                            commit_header.push('\n');
                        }
                        commit_header.push_str("---\n");
                        commit_header.push_str(trimmed);
                        commit_header.push_str("\n\n");
                    }
                }
            }
        }

        if chunks.is_empty() {
            if !commit_header.trim().is_empty() {
                return Ok(commit_header);
            }
            return Err(ServiceError::invalid(match source {
                GitDiffSource::Staged => "no staged git changes to attach",
                GitDiffSource::Unstaged => "no unstaged git changes to attach",
                GitDiffSource::Commit(_) => "git commit produced empty output",
            }));
        }

        // Index verbatim deleted definition blocks across all chunks for cross-file move dedup
        let moved_deleted_map = collect_moved_deleted_blocks(&chunks);

        // Process chunks in parallel using rayon
        let results: Vec<Result<(RenderedChunk, Vec<String>)>> = chunks
            .par_iter()
            .map(|chunk| {
                let mut file_warnings = Vec::new();

                let (target_path, old_path, is_deleted, is_new, is_special_mode) =
                    match extract_target_path_from_diff_chunk(chunk) {
                        Some(info) => info,
                        None => {
                            return Ok((
                                RenderedChunk {
                                    path: None,
                                    is_test: false,
                                    is_new: false,
                                    is_deleted: false,
                                    pre_omitted: false,
                                    is_omitted: false,
                                    raw_chunk: chunk.clone(),
                                    rendered: chunk.clone(),
                                    orig_line_map: None,
                                    inline_test_blocks: Vec::new(),
                                },
                                file_warnings,
                            ))
                        }
                    };

                let path_is_test = is_test_file_path(&target_path);

                if is_lockfile_or_generated(&target_path) || has_generated_file_header(chunk) {
                    let omitted = summarize_omitted_chunk(chunk, &target_path);
                    return Ok((
                        RenderedChunk {
                            path: Some(target_path),
                            is_test: path_is_test,
                            is_new,
                            is_deleted,
                            pre_omitted: true,
                            is_omitted: true,
                            raw_chunk: chunk.clone(),
                            rendered: omitted,
                            orig_line_map: None,
                            inline_test_blocks: Vec::new(),
                        },
                        file_warnings,
                    ));
                }

                // Summarize large newly added test files with a TOC of definitions
                if chunk.starts_with("diff --git ") && is_new && !is_special_mode {
                    if let Some(test_summary) = summarize_large_new_test_chunk(chunk, &target_path)
                    {
                        return Ok((
                            RenderedChunk {
                                path: Some(target_path),
                                is_test: true,
                                is_new: true,
                                is_deleted: false,
                                pre_omitted: true,
                                is_omitted: true,
                                raw_chunk: chunk.clone(),
                                rendered: test_summary,
                                orig_line_map: None,
                                inline_test_blocks: Vec::new(),
                            },
                            file_warnings,
                        ));
                    }
                    if let Some((moved_summary, moved_line_map)) =
                        summarize_moved_new_file_chunk(chunk, &target_path, &moved_deleted_map)
                    {
                        return Ok((
                            RenderedChunk {
                                path: Some(target_path),
                                is_test: path_is_test,
                                is_new: true,
                                is_deleted: false,
                                pre_omitted: false,
                                is_omitted: false,
                                raw_chunk: chunk.clone(),
                                rendered: moved_summary,
                                orig_line_map: Some(moved_line_map),
                                inline_test_blocks: Vec::new(),
                            },
                            file_warnings,
                        ));
                    }
                }

                // Non-standard chunks (combined diff --cc, diff --combined), deleted, newly added,
                // symlinks/submodules, or chunks without hunks (mode changes, pure 100% rename, binary) are passed as-is
                if !chunk.starts_with("diff --git ")
                    || is_deleted
                    || is_new
                    || is_special_mode
                    || !chunk.contains("\n@@")
                {
                    return Ok((
                        RenderedChunk {
                            path: Some(target_path),
                            is_test: path_is_test,
                            is_new,
                            is_deleted,
                            pre_omitted: false,
                            is_omitted: false,
                            raw_chunk: chunk.clone(),
                            rendered: chunk.clone(),
                            orig_line_map: None,
                            inline_test_blocks: Vec::new(),
                        },
                        file_warnings,
                    ));
                }

                let (diff_str, inline_test_blocks) = match SupportedLanguage::from_path(&target_path)
                {
                    Some(lang) => {
                        match expand_chunk_with_treesitter(
                            &repo_root,
                            &target_path,
                            chunk,
                            source,
                            lang,
                            &moved_deleted_map,
                        ) {
                            Ok(Some(TreeSitterExpandResult::Generated)) => {
                                let omitted = summarize_omitted_chunk(chunk, &target_path);
                                return Ok((
                                    RenderedChunk {
                                        path: Some(target_path),
                                        is_test: path_is_test,
                                        is_new: false,
                                        is_deleted: false,
                                        pre_omitted: true,
                                        is_omitted: true,
                                        raw_chunk: chunk.clone(),
                                        rendered: omitted,
                                        orig_line_map: None,
                                        inline_test_blocks: Vec::new(),
                                    },
                                    file_warnings,
                                ));
                            }
                            Ok(Some(TreeSitterExpandResult::Expanded(expanded, blocks)))
                                if !expanded.trim().is_empty() =>
                            {
                                (expanded, blocks)
                            }
                            Ok(Some(TreeSitterExpandResult::Expanded(_, _))) | Ok(None) => {
                                file_warnings.push(format!(
                                    "tree-sitter: could not expand '{}'; falling back to git diff -W",
                                    target_path.display()
                                ));
                                (
                                    git_diff_w_for_file(
                                        &repo_root,
                                        &target_path,
                                        old_path.as_deref(),
                                        source,
                                    )?,
                                    Vec::new(),
                                )
                            }
                            Err(e) => {
                                if has_generated_source_header_for_file(
                                    &repo_root,
                                    &target_path,
                                    source,
                                ) {
                                    let omitted = summarize_omitted_chunk(chunk, &target_path);
                                    return Ok((
                                        RenderedChunk {
                                            path: Some(target_path),
                                            is_test: path_is_test,
                                            is_new: false,
                                            is_deleted: false,
                                            pre_omitted: true,
                                            is_omitted: true,
                                            raw_chunk: chunk.clone(),
                                            rendered: omitted,
                                            orig_line_map: None,
                                            inline_test_blocks: Vec::new(),
                                        },
                                        file_warnings,
                                    ));
                                }
                                file_warnings.push(format!(
                                    "tree-sitter: failed for '{}' ({e}); falling back to git diff -W",
                                    target_path.display()
                                ));
                                (
                                    git_diff_w_for_file(
                                        &repo_root,
                                        &target_path,
                                        old_path.as_deref(),
                                        source,
                                    )?,
                                    Vec::new(),
                                )
                            }
                        }
                    }
                    None => {
                        if has_generated_source_header_for_file(&repo_root, &target_path, source) {
                            let omitted = summarize_omitted_chunk(chunk, &target_path);
                            return Ok((
                                RenderedChunk {
                                    path: Some(target_path),
                                    is_test: path_is_test,
                                    is_new: false,
                                    is_deleted: false,
                                    pre_omitted: true,
                                    is_omitted: true,
                                    raw_chunk: chunk.clone(),
                                    rendered: omitted,
                                    orig_line_map: None,
                                    inline_test_blocks: Vec::new(),
                                },
                                file_warnings,
                            ));
                        }
                        file_warnings.push(format!(
                            "tree-sitter: unsupported language for '{}'; falling back to git diff -W",
                            target_path.display()
                        ));
                        (
                            git_diff_w_for_file(
                                &repo_root,
                                &target_path,
                                old_path.as_deref(),
                                source,
                            )?,
                            Vec::new(),
                        )
                    }
                };
                Ok((
                    RenderedChunk {
                        path: Some(target_path),
                        is_test: path_is_test,
                        is_new: false,
                        is_deleted: false,
                        pre_omitted: false,
                        is_omitted: false,
                        raw_chunk: chunk.clone(),
                        rendered: diff_str,
                        orig_line_map: None,
                        inline_test_blocks,
                    },
                    file_warnings,
                ))
            })
            .collect();

        let mut rendered_chunks = Vec::with_capacity(results.len());
        let mut file_warnings = Vec::new();
        for res in results {
            let (rc, fw) = res?;
            rendered_chunks.push(rc);
            for w in fw {
                if !file_warnings.contains(&w) {
                    file_warnings.push(w);
                }
            }
        }

        for w in file_warnings {
            if !warnings.contains(&w) {
                warnings.push(w);
            }
        }

        let has_chunk_content = rendered_chunks
            .iter()
            .any(|c| !c.rendered.trim().is_empty());
        if commit_header.trim().is_empty() && !has_chunk_content {
            return Err(ServiceError::invalid(match source {
                GitDiffSource::Staged => "no staged git changes to attach",
                GitDiffSource::Unstaged => "no unstaged git changes to attach",
                GitDiffSource::Commit(_) => "git commit produced empty output",
            }));
        }

        let preamble = match source {
            GitDiffSource::Commit(_) => String::new(),
            GitDiffSource::Staged | GitDiffSource::Unstaged => {
                generate_diff_preamble(&repo_root, source)
            }
        };

        let mut prefix = preamble;
        prefix.push_str(&commit_header);

        Ok(apply_patch_budget(
            &prefix,
            rendered_chunks,
            resolve_patch_budget_bytes(),
            warnings,
        ))
    }
}

/// Generates the full combined diff for unstaged or staged changes with Tree-Sitter AST context.
pub fn generate_expanded_diff(
    workspace: &Path,
    staged: bool,
    warnings: &mut Vec<String>,
) -> Result<String> {
    let source = if staged {
        GitDiffSource::Staged
    } else {
        GitDiffSource::Unstaged
    };
    generate_expanded_diff_for_source(workspace, source, warnings)
}

/// Generates the full combined diff for a commit with Tree-Sitter AST context.
pub fn generate_expanded_commit_diff(
    workspace: &Path,
    full_sha: &str,
    warnings: &mut Vec<String>,
) -> Result<String> {
    generate_expanded_diff_for_source(workspace, GitDiffSource::Commit(full_sha), warnings)
}

/// Generates an expanded git patch as a temporary patch file on disk.
pub fn git_expanded_patch_file(
    workspace: &Path,
    source: GitDiffSource<'_>,
    warnings: &mut Vec<String>,
) -> Result<PathBuf> {
    let filename = match source {
        GitDiffSource::Staged => "git-staged+.patch".to_string(),
        GitDiffSource::Unstaged => "git-diff+.patch".to_string(),
        GitDiffSource::Commit(sha) => {
            let short = &sha[..sha.len().min(12)];
            format!("git-{short}+.patch")
        }
    };

    let mut local_warnings = Vec::new();
    let diff_text = generate_expanded_diff_for_source(workspace, source, &mut local_warnings)?;

    // Validate size before creating temporary directory
    if diff_text.len() as u64 > crate::domain::MAX_ARTIFACT_BYTES {
        return Err(ServiceError::invalid(format!(
            "attachments must be regular files of at most {}",
            crate::domain::MAX_ARTIFACT_LABEL
        )));
    }

    let temp_dir =
        std::env::temp_dir().join(format!("anticrumb-git-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&temp_dir).map_err(ServiceError::storage)?;
    let patch_path = temp_dir.join(filename);

    if let Err(e) = std::fs::write(&patch_path, diff_text.as_bytes()) {
        let _ = std::fs::remove_dir_all(&temp_dir);
        return Err(ServiceError::storage(e));
    }

    for w in local_warnings {
        if !warnings.contains(&w) {
            warnings.push(w);
        }
    }
    Ok(patch_path)
}

/// Generates the expanded git diff as a temporary patch file on disk (`git-diff+.patch` or `git-staged+.patch`).
pub fn git_diff_expanded_file(
    workspace: &Path,
    staged: bool,
    warnings: &mut Vec<String>,
) -> Result<PathBuf> {
    let source = if staged {
        GitDiffSource::Staged
    } else {
        GitDiffSource::Unstaged
    };
    git_expanded_patch_file(workspace, source, warnings)
}

/// Generates the expanded git commit patch as a temporary patch file on disk (`git-<sha>+.patch`).
pub fn git_commit_expanded_file(
    workspace: &Path,
    full_sha: &str,
    warnings: &mut Vec<String>,
) -> Result<PathBuf> {
    git_expanded_patch_file(workspace, GitDiffSource::Commit(full_sha), warnings)
}

#[cfg(test)]
mod common_tests {
    use super::*;
    use std::process::Command;

    fn mk_chunk(
        path: &str,
        is_test: bool,
        is_new: bool,
        pre_omitted: bool,
        raw_chunk: String,
        rendered: String,
    ) -> RenderedChunk {
        RenderedChunk {
            path: Some(PathBuf::from(path)),
            is_test,
            is_new,
            is_deleted: false,
            pre_omitted,
            is_omitted: pre_omitted,
            raw_chunk,
            rendered,
            orig_line_map: None,
            #[cfg(feature = "tree-sitter")]
            inline_test_blocks: Vec::new(),
        }
    }

    #[test]
    fn test_expanded_lockfile_and_generated_detection() {
        for p in [
            "npm-shrinkwrap.json",
            "deno.lock",
            "dist/app.chunk.js",
            "dist/style.chunk.css",
            "dist/index.mjs.map",
            "dist/index.cjs.map",
            "assets/logo.svg",
            "proto/api.pb.rs",
            "proto/msg.pb.cc",
            "proto/msg.pb.h",
            "py/svc_pb2.pyi",
            "py/svc_pb2_grpc.py",
            "lib/user.freezed.dart",
            "Models/Order.g.cs",
            "Models/Order.generated.cs",
            "src/rpc/generated/anticrumb_ffi.rs",
            "pkg/Generated/types.go",
        ] {
            assert!(
                is_lockfile_or_generated(Path::new(p)),
                "Expected generated/lockfile detection for {p}"
            );
        }
        for p in [
            "src/main.rs",
            "src/app.ts",
            "src/order.cs",
            "src/generated_helper.rs",
            "README.md",
        ] {
            assert!(
                !is_lockfile_or_generated(Path::new(p)),
                "Unexpected generated/lockfile false positive for {p}"
            );
        }

        let gen_go = "diff --git a/pkg/db.go b/pkg/db.go\nnew file mode 100644\n--- /dev/null\n+++ b/pkg/db.go\n@@ -0,0 +1,4 @@\n+// Code generated by sqlc. DO NOT EDIT.\n+package db\n";
        assert!(has_generated_file_header(gen_go));

        let gen_at = "diff --git a/src/a.rs b/src/a.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/a.rs\n@@ -0,0 +1,3 @@\n+// @generated\n+pub const X: i32 = 1;\n";
        assert!(has_generated_file_header(gen_at));

        let gen_py = "diff --git a/api.py b/api.py\nnew file mode 100644\n--- /dev/null\n+++ b/api.py\n@@ -0,0 +1,3 @@\n+# Automatically generated - DO NOT MODIFY\n+X = 1\n";
        assert!(has_generated_file_header(gen_py));

        // Code containing "@generated" inside a string literal must NOT false-positive
        let normal_rs = "diff --git a/src/check.rs b/src/check.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/check.rs\n@@ -0,0 +1,4 @@\n+pub fn is_gen(s: &str) -> bool {\n+    s.contains(\"@generated\")\n+}\n";
        assert!(!has_generated_file_header(normal_rs));
    }

    #[test]
    fn test_parse_patch_budget_kib_override_and_clamping() {
        use super::super::{parse_patch_budget_kib, DEFAULT_PATCH_BUDGET_BYTES};
        assert_eq!(DEFAULT_PATCH_BUDGET_BYTES, 512 * 1024);
        assert_eq!(parse_patch_budget_kib(None), 512 * 1024);
        assert_eq!(parse_patch_budget_kib(Some("")), 512 * 1024);
        assert_eq!(parse_patch_budget_kib(Some("0")), 512 * 1024);
        assert_eq!(parse_patch_budget_kib(Some("invalid")), 512 * 1024);
        assert_eq!(parse_patch_budget_kib(Some("256")), 256 * 1024);
        assert_eq!(parse_patch_budget_kib(Some(" 768 ")), 768 * 1024);
        assert_eq!(parse_patch_budget_kib(Some("1")), 16 * 1024);
        assert_eq!(parse_patch_budget_kib(Some("999999")), 16 * 1024 * 1024);
    }

    #[test]
    fn test_apply_patch_budget_progressive_tiers() {
        let make_chunk = |file: &str, body_lines: usize| -> String {
            let mut s = format!(
                "diff --git a/{file} b/{file}\nindex 1111111..2222222 100644\n--- a/{file}\n+++ b/{file}\n@@ -1,1 +1,{body_lines} @@\n"
            );
            for i in 0..body_lines {
                s.push_str(&format!(
                    "+let line_{i} = \"padding_payload_0123456789\";\n"
                ));
            }
            s
        };

        // Tier 2: Expanded test chunk shrinks to its raw `-U0` diff (with marker) before being omitted
        let test_raw = make_chunk("tests/foo_test.rs", 8);
        let test_expanded = make_chunk("tests/foo_test.rs", 25);
        let prod_raw = make_chunk("src/prod.rs", 10);
        let prod_expanded = make_chunk("src/prod.rs", 25);
        let items_t1 = vec![
            mk_chunk(
                "tests/foo_test.rs",
                true,
                false,
                false,
                test_raw.clone(),
                test_expanded,
            ),
            mk_chunk(
                "src/prod.rs",
                false,
                false,
                false,
                prod_raw.clone(),
                prod_expanded.clone(),
            ),
        ];
        let mut warnings = Vec::new();
        let out_t1 = apply_patch_budget(
            "",
            items_t1,
            test_raw.len() + prod_expanded.len() + 30,
            &mut warnings,
        );
        assert!(
            out_t1.contains("+let line_7"),
            "Tier 2 must preserve raw -U0 test diff lines instead of omitting test file!"
        );
        assert!(out_t1.contains("[degraded to -U0]"));
        assert!(!out_t1.contains("[omitted due to patch budget"));
        assert!(out_t1.contains("+let line_24"));
        assert!(warnings.iter().any(|w| w.contains("exceeded patch budget")));

        // Tier 2c: When even raw `-U0` exceeds budget, omit test chunk while preserving production chunk
        let items_t3 = vec![
            mk_chunk(
                "tests/foo_test.rs",
                true,
                false,
                false,
                test_raw.clone(),
                test_raw,
            ),
            mk_chunk(
                "src/prod.rs",
                false,
                false,
                false,
                prod_raw.clone(),
                prod_expanded.clone(),
            ),
        ];
        let mut warnings3 = Vec::new();
        let out_t3 = apply_patch_budget("", items_t3, prod_raw.len() + 300, &mut warnings3);
        assert!(out_t3.contains("[omitted due to patch budget: tests/foo_test.rs"));
        assert!(out_t3.contains("+let line_9"));

        // Tier 5: Multiple equal-sized oversized production chunks (non-function statements so
        // skeletonization does not apply) are water-fill head/tail truncated with `[resuming at L...]`.
        let p1 = make_chunk("src/core_a.rs", 80);
        let p2 = make_chunk("src/core_b.rs", 80);
        let items_t4 = vec![
            mk_chunk("src/core_a.rs", false, true, false, p1.clone(), p1),
            mk_chunk("src/core_b.rs", false, true, false, p2.clone(), p2),
        ];
        let mut warnings4 = Vec::new();
        let out_t4 = apply_patch_budget("", items_t4, 2600, &mut warnings4);
        assert!(
            out_t4.len() <= 2600,
            "Tier 5 exceeded budget: {}",
            out_t4.len()
        );
        assert!(
            out_t4.contains("[truncated due to patch budget: src/core_a.rs"),
            "Expected core_a.rs to be head/tail truncated, not omitted: {out_t4}"
        );
        assert!(
            out_t4.contains("[truncated due to patch budget: src/core_b.rs"),
            "Expected core_b.rs to be head/tail truncated, not omitted: {out_t4}"
        );
        assert!(
            out_t4.contains("[resuming at L"),
            "Truncated chunks must include explicit [resuming at L...] line anchors: {out_t4}"
        );
        assert!(out_t4.contains("+let line_79"));
        assert!(
            out_t4.contains("[patch-budget: 2600 B hit; 2 truncated]"),
            "Expected top patch-budget metadata banner in compacted output: {out_t4}"
        );
    }

    #[test]
    fn test_apply_patch_budget_preserves_u3_context_and_protects_small_modified_files() {
        // Build a modified file whose expanded AST diff has 40 unchanged context lines around 1 edit
        let mut expanded_mod = String::from(
            "diff --git a/src/small_mod.rs b/src/small_mod.rs\nindex 1111111..2222222 100644\n--- a/src/small_mod.rs\n+++ b/src/small_mod.rs\n@@ -1,41 +1,41 @@ impl Router > fn dispatch(req: &Req) {\n",
        );
        for i in 1..=20 {
            expanded_mod.push_str(&format!("     let pre_{i} = compute_step({i});\n"));
        }
        expanded_mod.push_str("-    let answer = old_value();\n");
        expanded_mod.push_str("+    let answer = new_value();\n");
        for i in 22..=41 {
            expanded_mod.push_str(&format!("     let post_{i} = finish_step({i});\n"));
        }
        let raw_mod_u0 = String::from(
            "diff --git a/src/small_mod.rs b/src/small_mod.rs\nindex 1111111..2222222 100644\n--- a/src/small_mod.rs\n+++ b/src/small_mod.rs\n@@ -21,1 +21,1 @@\n-    let answer = old_value();\n+    let answer = new_value();\n",
        );

        // Build a tiny modified file (~350 bytes) that water-filling must keep intact
        let tiny_mod = String::from(
            "diff --git a/src/tiny.rs b/src/tiny.rs\nindex 1111111..2222222 100644\n--- a/src/tiny.rs\n+++ b/src/tiny.rs\n@@ -1,4 +1,4 @@ fn tiny() {\n fn tiny() {\n-    let a = 1;\n+    let a = 2;\n     a\n }\n",
        );

        // Build a pre-omitted lockfile to verify banner distinguishes pre-omitted from budget-omitted
        let pre_omitted_lock = String::from(
            "diff --git a/Cargo.lock b/Cargo.lock\nindex 1111111..2222222 100644\n--- a/Cargo.lock\n+++ b/Cargo.lock\n[omitted: Cargo.lock (+500/-100 lines)]\n\n",
        );

        // Build a large newly added production file (~25 KiB) that forces budget compaction
        let mut huge_new = String::from(
            "diff --git a/src/huge_new.rs b/src/huge_new.rs\nnew file mode 100644\nindex 0000000..3333333\n--- /dev/null\n+++ b/src/huge_new.rs\n@@ -0,0 +1,750 @@\n",
        );
        for f in 0..50 {
            huge_new.push_str(&format!("+fn generated_helper_{f}(x: i32) -> i32 {{\n"));
            for k in 0..13 {
                huge_new.push_str(&format!("+    let step_{k} = x + {k} + {f};\n"));
            }
            huge_new.push_str("+    step_12\n+}\n");
        }

        let items = vec![
            mk_chunk(
                "Cargo.lock",
                false,
                false,
                true,
                pre_omitted_lock.clone(),
                pre_omitted_lock,
            ),
            mk_chunk(
                "src/small_mod.rs",
                false,
                false,
                false,
                raw_mod_u0,
                expanded_mod,
            ),
            mk_chunk(
                "src/tiny.rs",
                false,
                false,
                false,
                tiny_mod.clone(),
                tiny_mod,
            ),
            mk_chunk(
                "src/huge_new.rs",
                false,
                true,
                false,
                huge_new.clone(),
                huge_new,
            ),
        ];

        let mut warnings = Vec::new();
        let budget = 16 * 1024;
        let out = apply_patch_budget(
            "commit abc123\n\n---\n 4 files changed\n\n",
            items,
            budget,
            &mut warnings,
        );

        assert!(
            out.len() <= budget,
            "Output {} exceeded budget {}",
            out.len(),
            budget
        );
        // Modified file carries `[degraded to -U3]` right after file header & retains AST signature
        assert!(
            out.contains("+++ b/src/small_mod.rs\n[degraded to -U3]\n@@ -18,7 +18,7 @@ impl Router > fn dispatch(req: &Req) {"),
            "Modified file must mark [degraded to -U3] and retain -U3 context + AST signature:\n{out}"
        );
        assert!(out.contains("     let pre_18 = compute_step(18);"));
        assert!(out.contains("     let post_24 = finish_step(24);"));
        assert!(
            !out.contains("     let pre_1 = compute_step(1);"),
            "Unchanged outer AST context beyond -U3 should have been trimmed"
        );
        // Tiny modified file is 100% intact
        assert!(out.contains("-    let a = 1;\n+    let a = 2;\n"));
        assert!(!out.contains("truncated due to patch budget: src/tiny.rs"));
        assert!(!out.contains("omitted due to patch budget: src/tiny.rs"));

        // Under tree-sitter, Tier 4 progressively skeletonizes huge_new.rs preserving ALL 50 signatures!
        #[cfg(feature = "tree-sitter")]
        {
            assert!(
                out.contains(
                    "[patch-budget: 16 KiB hit; 1 pre-omitted, 1 degraded to -U3, 1 skeletonized]"
                ),
                "Expected accurate 1:1 patch-budget banner, got:\n{}",
                &out[..out.len().min(600)]
            );
            assert!(
                out.contains("+++ b/src/huge_new.rs\n[skeletonized:"),
                "Expected [skeletonized:] file header marker for huge_new.rs:\n{out}"
            );
            assert!(
                out.contains("{ /* ... 16 lines (L"),
                "Expected folded body with line range:\n{out}"
            );
            for f in 0..50 {
                assert!(
                    out.contains(&format!("fn generated_helper_{f}(x: i32) -> i32")),
                    "Tier 4 skeleton must preserve every function signature (missing {f})"
                );
            }
        }
        // Without tree-sitter, falls back cleanly to Tier 5 Head+Tail truncation with [resuming at L...]
        #[cfg(not(feature = "tree-sitter"))]
        {
            assert!(
                out.contains(
                    "[patch-budget: 16 KiB hit; 1 pre-omitted, 1 degraded to -U3, 1 truncated]"
                ),
                "Expected accurate patch-budget banner without tree-sitter, got:\n{}",
                &out[..out.len().min(600)]
            );
            assert!(out.contains("[truncated due to patch budget: src/huge_new.rs"));
            assert!(out.contains("[resuming at L"));
        }

        // Mid-function truncation on a giant single function annotates both `resuming in fn mega_engine`
        // AND explicit `[resuming at L...]` line anchor
        #[cfg(feature = "tree-sitter")]
        {
            let mut mega = String::from(
                "diff --git a/src/mega.rs b/src/mega.rs\nnew file mode 100644\nindex 0000000..4444444\n--- /dev/null\n+++ b/src/mega.rs\n@@ -0,0 +1,402 @@\n+fn mega_engine(mut acc: i64) -> i64 {\n",
            );
            for i in 0..400 {
                mega.push_str(&format!("+    acc += compute_weighted_term(acc, {i});\n"));
            }
            mega.push_str("+    acc\n+}\n");
            let truncated =
                truncate_chunk_head_tail(&mega, &mega, None, Path::new("src/mega.rs"), true, 4096)
                    .expect("expected mega.rs to truncate within 4096 B");
            assert!(truncated.len() <= 4096);
            assert!(
                truncated.contains("resuming in fn mega_engine"),
                "Mid-function truncation must report resuming scope, got:\n{truncated}"
            );
            assert!(
                truncated.contains("[resuming at L"),
                "Mid-function truncation must emit explicit [resuming at L...] anchor, got:\n{truncated}"
            );
        }
    }

    #[test]
    fn test_lockfile_omitted_in_generate_expanded_diff_regardless_of_feature() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(repo)
                .status()
                .unwrap();
            assert!(status.success(), "git {:?} failed", args);
        };
        run(&["init"]);
        run(&["config", "user.name", "test"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "core.autocrlf", "false"]);

        std::fs::write(repo.join("Cargo.lock"), "version = 3\nold_dep = \"1.0\"\n").unwrap();
        std::fs::write(repo.join("notes.txt"), "hello\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);

        std::fs::write(repo.join("Cargo.lock"), "version = 3\nnew_dep = \"2.0\"\n").unwrap();
        std::fs::write(repo.join("notes.txt"), "hello world\n").unwrap();

        let mut warnings = Vec::new();
        let patch = generate_expanded_diff(repo, false, &mut warnings).unwrap();
        assert!(
            patch.contains("[omitted: Cargo.lock"),
            "Lockfile must be omitted even without tree-sitter feature! Got:\n{patch}"
        );
        assert!(
            !patch.contains("+new_dep"),
            "Lockfile body must not appear in patch! Got:\n{patch}"
        );
        assert!(patch.contains("+hello world"));
    }

    #[test]
    fn test_truncate_head_tail_shrinks_head_with_long_single_tail_and_avoids_l0() {
        // 1. Long single tail line (`tail_start + 1 == lines.len()` with `head_bytes < tail_bytes`)
        //    must still shrink `head_end` down toward `header_end` instead of prematurely breaking.
        let mut long_tail_chunk = String::from(
            "diff --git a/src/long_tail.txt b/src/long_tail.txt\nnew file mode 100644\n--- /dev/null\n+++ b/src/long_tail.txt\n@@ -0,0 +1,40 @@\n",
        );
        for i in 1..40 {
            long_tail_chunk.push_str(&format!("+short_head_line_{i:02}_padding_012345\n"));
        }
        // Single long final line (~360 B)
        long_tail_chunk.push_str(&format!("+final_tail_{}\n", "x".repeat(350)));

        // Budget where 1 tail line (~363 B) > initial head_budget after header (~250 B) once shrunk,
        // yet header + a few head lines + marker + 1 tail line fits strictly within budget!
        let budget = 950;
        let truncated = truncate_chunk_head_tail(
            &long_tail_chunk,
            &long_tail_chunk,
            None,
            Path::new("src/long_tail.txt"),
            true,
            budget,
        )
        .expect("Expected head to continue shrinking so long single tail still fits within budget");
        assert!(
            truncated.len() <= budget,
            "Truncated len {} exceeded budget {}",
            truncated.len(),
            budget
        );
        assert!(truncated.contains("+final_tail_"));
        assert!(truncated.contains("diff lines)"));

        // 2. Deleted file (`@@ -1,120 +0,0 @@`) must NEVER emit `L0` anchors; tracks old line numbers.
        let mut deleted_chunk = String::from(
            "diff --git a/src/legacy.rs b/src/legacy.rs\ndeleted file mode 100644\nindex 1111111..0000000\n--- a/src/legacy.rs\n+++ /dev/null\n@@ -1,120 +0,0 @@\n",
        );
        for i in 1..=120 {
            deleted_chunk.push_str(&format!("-let legacy_item_{i:03} = {i} * 42;\n"));
        }
        let del_lines: Vec<&str> = deleted_chunk.split_inclusive('\n').collect();
        let del_map = compute_diff_line_numbers(&del_lines);
        assert!(
            !del_map.contains(&Some((0, 0))),
            "Deleted file line map must never contain Some((0, 0))"
        );
        let first_del_idx = del_lines
            .iter()
            .position(|l| l.starts_with("-let "))
            .unwrap();
        assert_eq!(del_map[first_del_idx], Some((1, 1)));
        assert_eq!(*del_map.last().unwrap(), Some((120, 120)));

        let del_trunc = truncate_chunk_head_tail(
            &deleted_chunk,
            &deleted_chunk,
            None,
            Path::new("src/legacy.rs"),
            false,
            1200,
        )
        .expect("Deleted file should head/tail truncate");
        assert!(del_trunc.len() <= 1200);
        assert!(
            !del_trunc.contains("L0"),
            "Deleted file truncation must never emit L0 anchor:\n{del_trunc}"
        );
        assert!(
            del_trunc.contains("[resuming at L"),
            "Deleted file truncation should resume at old-file line number:\n{del_trunc}"
        );

        // 3. Modified file top-of-file pure deletion hunk (`@@ -1,40 +0,0 @@`) must map `-` lines
        //    to `None` (never `Some(0)` -> never `L0`), while subsequent hunks keep new-file lines.
        let mut top_del_mod = String::from(
            "diff --git a/src/mod.rs b/src/mod.rs\nindex 1111111..2222222 100644\n--- a/src/mod.rs\n+++ b/src/mod.rs\n@@ -1,40 +0,0 @@\n",
        );
        for i in 1..=40 {
            top_del_mod.push_str(&format!("-let removed_header_{i:02} = {i};\n"));
        }
        top_del_mod.push_str("@@ -50,2 +10,2 @@ fn kept() {\n-let a = 1;\n+let a = 2;\n");
        let mod_lines: Vec<&str> = top_del_mod.split_inclusive('\n').collect();
        let mod_map = compute_diff_line_numbers(&mod_lines);
        assert!(
            !mod_map.contains(&Some((0, 0))),
            "Top-of-file pure deletion hunk in modified file must never emit Some((0, 0))"
        );
        let top_del_trunc = truncate_chunk_head_tail(
            &top_del_mod,
            &top_del_mod,
            None,
            Path::new("src/mod.rs"),
            false,
            900,
        )
        .expect("Modified file with top deletion should truncate");
        assert!(top_del_trunc.len() <= 900);
        assert!(
            !top_del_trunc.contains("L0"),
            "Top-of-file pure deletion truncation must never emit L0:\n{top_del_trunc}"
        );
    }

    #[test]
    fn test_generated_header_deleted_files_and_hash_preprocessor_guards() {
        // 1. Fully deleted generated file (`queries.sql.go`, not matched by filename pattern)
        let deleted_sqlc = "\
diff --git a/db/queries.sql.go b/db/queries.sql.go
deleted file mode 100644
index 1111111..0000000
--- a/db/queries.sql.go
+++ /dev/null
@@ -1,10 +0,0 @@
-// Code generated by sqlc. DO NOT EDIT.
-package db
";
        assert!(
            has_generated_file_header(deleted_sqlc),
            "Deleted auto-generated file must be detected via `-` lines"
        );

        // 2. Modified file that REMOVES `@generated` header at line 1 (converting to hand-maintained)
        //    must NOT be falsely classified as generated!
        let removed_header_mod = "\
diff --git a/src/hand_owned.rs b/src/hand_owned.rs
index 1111111..2222222 100644
--- a/src/hand_owned.rs
+++ b/src/hand_owned.rs
@@ -1,2 +0,0 @@
-// @generated
-// Do not edit
";
        assert!(
            !has_generated_file_header(removed_header_mod),
            "Modified file removing its generated header must NOT be treated as generated"
        );

        // 3. C/C++ preprocessor `#define`, Rust crate attribute `#![doc = ...]`, and Markdown heading
        //    containing `@generated` must NOT false-positive as `#` comments!
        let cpp_define = "\
diff --git a/include/markers.h b/include/markers.h
new file mode 100644
--- /dev/null
+++ b/include/markers.h
@@ -0,0 +1,3 @@
+#define GENERATED_MARKER \"@generated\"
+int check_header(const char *s);
";
        assert!(
            !has_generated_file_header(cpp_define),
            "C/C++ #define with @generated must NOT false-positive"
        );

        let rust_inner_attr = "\
diff --git a/src/detector.rs b/src/detector.rs
new file mode 100644
--- /dev/null
+++ b/src/detector.rs
@@ -0,0 +1,3 @@
+#![doc = \"Checks @generated headers in diff chunks\"]
+pub fn detect() {}
";
        assert!(
            !has_generated_file_header(rust_inner_attr),
            "Rust #![doc = ...] with @generated must NOT false-positive"
        );

        let md_heading = "\
diff --git a/docs/generated.md b/docs/generated.md
new file mode 100644
--- /dev/null
+++ b/docs/generated.md
@@ -0,0 +1,3 @@
+# Handling @generated files
+Documentation text.
";
        assert!(
            !has_generated_file_header(md_heading),
            "Markdown heading with @generated must NOT false-positive"
        );

        // 4. Unknown extension with `#define` / `#[doc = ...]` heuristic guard
        let unknown_define = "\
diff --git a/scripts/markers.custom b/scripts/markers.custom
new file mode 100644
--- /dev/null
+++ b/scripts/markers.custom
@@ -0,0 +1,2 @@
+#define TAG \"@generated\"
";
        assert!(!has_generated_file_header(unknown_define));

        // 5. JS script with shebang on line 1 + `// @generated` on line 2 MUST still be detected
        let js_shebang_gen = "\
diff --git a/bin/cli.js b/bin/cli.js
new file mode 100644
--- /dev/null
+++ b/bin/cli.js
@@ -0,0 +1,3 @@
+#!/usr/bin/env node
+// @generated
+console.log('hi');
";
        assert!(
            has_generated_file_header(js_shebang_gen),
            "Script with shebang + line-2 @generated comment should be detected"
        );

        // 6. Hand-written doc/prose comments mentioning `@generated` must NOT false-positive,
        //    while real `@generated` markers across languages (including XML `<?xml ...?>`,
        //    PHP `<?php`, SQL `--`, and UTF-8 BOM) MUST be detected.
        for non_gen in [
            "diff --git a/src/handler.rs b/src/handler.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/handler.rs\n@@ -0,0 +1,2 @@\n+//! Handles @generated files and lockfiles.\n+pub fn run() {}\n",
            "diff --git a/src/handler2.rs b/src/handler2.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/handler2.rs\n@@ -0,0 +1,2 @@\n+//! @generated files are ignored by this pass.\n+pub fn run() {}\n",
            "diff --git a/src/util.ts b/src/util.ts\nnew file mode 100644\n--- /dev/null\n+++ b/src/util.ts\n@@ -0,0 +1,2 @@\n+// Utility for detecting @generated markers in diffs\n+export const x = 1;\n",
            "diff --git a/src/util.py b/src/util.py\nnew file mode 100644\n--- /dev/null\n+++ b/src/util.py\n@@ -0,0 +1,2 @@\n+# Checks @generated headers safely\n+X = 1\n",
            "diff --git a/src/not_gen.rs b/src/not_gen.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/not_gen.rs\n@@ -0,0 +1,2 @@\n+// @not-generated\n+pub fn run() {}\n",
            "diff --git a/src/ident.rs b/src/ident.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/ident.rs\n@@ -0,0 +1,2 @@\n+// @generated_helper\n+pub fn run() {}\n",
        ] {
            assert!(
                !has_generated_file_header(non_gen),
                "Hand-written comment mentioning @generated must NOT false-positive:\n{non_gen}"
            );
        }

        for real_gen in [
            "diff --git a/src/a.rs b/src/a.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/a.rs\n@@ -0,0 +1,2 @@\n+// @generated SignedSource<<abcdef123456>>\n+pub const A: i32 = 1;\n",
            "diff --git a/src/b.kt b/src/b.kt\nnew file mode 100644\n--- /dev/null\n+++ b/src/b.kt\n@@ -0,0 +1,2 @@\n+/* @generated by ksp */\n+class B\n",
            "diff --git a/schema/mig.sql b/schema/mig.sql\nnew file mode 100644\n--- /dev/null\n+++ b/schema/mig.sql\n@@ -0,0 +1,2 @@\n+-- @generated\n+SELECT 1;\n",
            "diff --git a/res/layout.xml b/res/layout.xml\nnew file mode 100644\n--- /dev/null\n+++ b/res/layout.xml\n@@ -0,0 +1,3 @@\n+<?xml version=\"1.0\" encoding=\"utf-8\"?>\n+<!-- Auto-generated by xjc. Do not edit. -->\n+<root/>\n",
            "diff --git a/web/client.php b/web/client.php\nnew file mode 100644\n--- /dev/null\n+++ b/web/client.php\n@@ -0,0 +1,3 @@\n+<?php\n+// @generated\n+class Client {}\n",
            "diff --git a/src/bom.cs b/src/bom.cs\nnew file mode 100644\n--- /dev/null\n+++ b/src/bom.cs\n@@ -0,0 +1,2 @@\n+\u{feff}// @generated\n+class Bom {}\n",
            "diff --git a/src/two.go b/src/two.go\nnew file mode 100644\n--- /dev/null\n+++ b/src/two.go\n@@ -0,0 +1,3 @@\n+// Copyright 2026 Acme (@generated)\n+// Do not edit manually.\n+package two\n",
        ] {
            assert!(
                has_generated_file_header(real_gen),
                "Real @generated header must be detected:\n{real_gen}"
            );
        }
    }
}
