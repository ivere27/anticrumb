#[cfg(feature = "tree-sitter")]
use super::{
    check_primary_definition, find_enclosing_definition_detail, signature,
    test_scope::{is_ast_node_attr_or_comment, is_attribute_or_comment_only},
    SupportedLanguage, ATTR_ONLY_MAX_EXPANSION_LINES, CLAMPED_CONTEXT_LINES,
    MAX_TREESITTER_FILE_BYTES, TREESITTER_PARSE_TIMEOUT_MICROS,
};
use super::{
    get_repo_root, git_cmd, is_test_file_path, unquote_c_style, GitDiffSource,
    DEFAULT_PATCH_BUDGET_BYTES,
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
const LARGE_NEW_TEST_SUMMARY_MIN_LINES: usize = 200;
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

fn parse_hunk_header(line: &str) -> Option<(usize, usize, usize, usize, String)> {
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
fn parse_with_timeout(
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
fn is_container_definition_node(node: &tree_sitter::Node) -> bool {
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
        if is_deleted || is_special || is_lockfile_or_generated(&target_path) {
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
    if is_new || is_special || is_lockfile_or_generated(&target_path) {
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
fn extract_chunk_header_and_added_source(chunk: &str) -> (String, String, usize) {
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
fn summarize_moved_new_file_chunk(
    chunk: &str,
    path: &Path,
    moved_map: &HashMap<[u8; 32], MovedSource>,
) -> Option<String> {
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
            body_line_count += 1;
            line_idx += 1;
        }
    }
    Some(format!("{header}@@ -0,0 +1,{body_line_count} @@\n{body}"))
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

/// Expands a single file's raw diff chunk with Tree-Sitter AST context.
#[cfg(feature = "tree-sitter")]
fn expand_chunk_with_treesitter(
    repo_root: &Path,
    path: &Path,
    chunk: &str,
    source: GitDiffSource<'_>,
    lang: SupportedLanguage,
    moved_deleted_map: &HashMap<[u8; 32], MovedSource>,
) -> Result<Option<String>> {
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

    let new_lines: Vec<&str> = new_content.split_inclusive('\n').collect();
    if new_lines.is_empty() {
        return Ok(Some(chunk.to_string()));
    }
    let plain_lines: Vec<&str> = new_content.lines().collect();

    let tree = parse_with_timeout(lang, &new_content).map_err(ServiceError::invalid)?;
    let root = tree.root_node();

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
        return Ok(Some(chunk.to_string()));
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

    // Merge overlapping or adjacent expanded hunks
    let mut merged: Vec<MergedHunk> = Vec::new();
    for entry in entries {
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
            if entry.exp_start <= last.exp_end + 1 {
                last.exp_end = last.exp_end.max(entry.exp_end);
                last.full_start = last.full_start.min(entry.full_start);
                last.full_end = last.full_end.max(entry.full_end);
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

    Ok(Some(result))
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
const BUDGET_BANNER_RESERVE_BYTES: usize = 128;

fn format_budget_size(bytes: usize) -> String {
    if bytes >= 1024 && bytes.is_multiple_of(1024) {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

fn strip_clamped_suffix(sig: &str) -> String {
    if let Some(idx) = sig.find(" [clamped] (lines ") {
        sig[..idx].trim_end().to_string()
    } else if sig.starts_with("[clamped] (lines ") {
        String::new()
    } else {
        sig.to_string()
    }
}

/// Trims unchanged context lines in a unified diff chunk down to at most `context_lines`
/// around each change, splitting wide hunks cleanly while preserving AST `@@ ... @@`
/// signatures and exact `git apply`-compatible line numbers.
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

fn summarize_budget_omitted_chunk(chunk: &str, path: &Path) -> String {
    let mut header = String::new();
    for line in chunk.split_inclusive('\n') {
        if line.starts_with("@@") || line.starts_with("GIT binary patch") {
            break;
        }
        header.push_str(line);
    }
    let (added, deleted) = count_diff_churn(chunk);
    let display_path = path.to_string_lossy();
    format!("{header}[omitted due to patch budget: {display_path} (+{added}/-{deleted} lines)]\n\n")
}

#[cfg(feature = "tree-sitter")]
struct AstDefSpan {
    start_line: usize,
    end_line: usize,
    short_label: String,
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
    let cut = raw
        .find(['(', '{'])
        .filter(|&pos| {
            // Skip `pub(...)` visibility prefix when trimming at `(`
            !(raw.as_bytes().get(pos) == Some(&b'(') && raw[..pos].trim_end().ends_with("pub"))
        })
        .unwrap_or(raw.len());
    let head = raw[..cut].trim().trim_end_matches('=').trim();
    if head.is_empty() {
        Some(signature::truncate_preview(&raw, 40))
    } else {
        Some(signature::truncate_preview(head, 40))
    }
}

#[cfg(feature = "tree-sitter")]
fn collect_ast_def_spans_for_new_chunk(
    lines: &[&str],
    path: &Path,
) -> Option<(Vec<Option<usize>>, Vec<AstDefSpan>)> {
    let lang = SupportedLanguage::from_path(path)?;
    let mut src_line_by_diff_idx: Vec<Option<usize>> = vec![None; lines.len()];
    let mut added_source = String::new();
    let mut in_hunk = false;
    let mut src_line = 0usize;

    for (idx, raw) in lines.iter().enumerate() {
        if !in_hunk {
            if raw.starts_with("@@") {
                in_hunk = true;
            }
            continue;
        }
        if raw.starts_with("@@") {
            continue;
        }
        if let Some(rest) = raw.strip_prefix('+') {
            src_line += 1;
            src_line_by_diff_idx[idx] = Some(src_line);
            added_source.push_str(rest);
        }
    }

    if src_line == 0 || added_source.len() > MAX_TREESITTER_FILE_BYTES {
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
                    let parent =
                        signature::extract_parent_scope_prefix(lang, &matched, lines, source);
                    let scope_label = match parent {
                        Some(p) => signature::truncate_preview(
                            &format!("{} > {short_label}", signature::truncate_preview(&p, 30)),
                            60,
                        ),
                        None => short_label.clone(),
                    };
                    let is_container = is_container_definition_node(&matched);
                    out.push(AstDefSpan {
                        start_line: s_line,
                        end_line: e_line,
                        short_label,
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
    Some((src_line_by_diff_idx, defs))
}

fn build_hunk_scope_truncation_marker(
    lines: &[&str],
    head_end: usize,
    tail_start: usize,
    display_path: &str,
    added: usize,
    deleted: usize,
) -> String {
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

    let mut details = vec![format!("omitted {omitted_lines} middle lines")];
    if !omitted_sigs.is_empty() {
        let shown: Vec<String> = omitted_sigs
            .iter()
            .take(4)
            .map(|s| {
                if s.chars().count() > 36 {
                    let p: String = s.chars().take(33).collect();
                    format!("{p}...")
                } else {
                    s.clone()
                }
            })
            .collect();
        let more = if omitted_sigs.len() > shown.len() {
            format!(", +{} more", omitted_sigs.len() - shown.len())
        } else {
            String::new()
        };
        details.push(format!("hunks: {}{more}", shown.join(", ")));
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
    format!(
        "... [truncated due to patch budget: {display_path} ({})]\n",
        details.join("; ")
    )
}

/// Truncates a single oversized diff chunk within `budget` bytes, preserving both head
/// (file header + opening diff lines) and tail (closing diff lines). For new code files
/// with Tree-Sitter enabled, snaps nearby cuts to AST definition boundaries and annotates
/// the marker with omitted line numbers, a mini-TOC of omitted definitions, and the
/// resuming scope.
fn truncate_chunk_head_tail(
    rendered: &str,
    raw_chunk: &str,
    path: &Path,
    is_new: bool,
    budget: usize,
) -> Option<String> {
    let lines: Vec<&str> = rendered.split_inclusive('\n').collect();
    if lines.len() < 4 {
        return None;
    }
    let (added, deleted) = count_diff_churn(raw_chunk);
    let display_path = path.to_string_lossy();
    let base_trailer_for = |omitted: usize| {
        format!(
            "... [truncated due to patch budget: {display_path} (omitted {omitted} middle lines; total +{added}/-{deleted})]\n"
        )
    };
    let max_base_trailer = base_trailer_for(lines.len());
    let rich_mode = budget >= 2048;
    let initial_reserve = if rich_mode {
        (max_base_trailer.len() + 200).min(budget / 3)
    } else {
        max_base_trailer.len() + 4
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

    // Find end of file header (lines before first body line) so snapping never cuts the header.
    let header_end = lines
        .iter()
        .position(|l| l.starts_with("@@"))
        .map(|i| i + 1)
        .unwrap_or(1)
        .min(head_end);

    #[cfg(feature = "tree-sitter")]
    let ast_info = if is_new && budget >= 1024 {
        collect_ast_def_spans_for_new_chunk(&lines, path)
    } else {
        None
    };

    #[cfg(feature = "tree-sitter")]
    if let Some((ref src_line_map, ref defs)) = ast_info {
        // Snap `head_end` backward onto the end of a completed AST definition if within 30% of head_bytes.
        if let Some(last_head_src) = lines[..head_end]
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, _)| src_line_map[i])
        {
            let min_head_bytes = (head_bytes * 7) / 10;
            let best_def_end = defs
                .iter()
                .filter(|d| !d.is_container && d.end_line <= last_head_src)
                .map(|d| d.end_line)
                .max();
            if let Some(target_end_src) = best_def_end {
                if let Some(snap_diff_idx) = src_line_map
                    .iter()
                    .enumerate()
                    .find_map(|(i, sl)| (*sl == Some(target_end_src)).then_some(i + 1))
                {
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
            .find_map(|(rel, _)| src_line_map[tail_start + rel])
        {
            let min_tail_bytes = (tail_bytes * 7) / 10;
            let best_def_start = defs
                .iter()
                .filter(|d| !d.is_container && d.start_line >= first_tail_src)
                .map(|d| d.start_line)
                .min();
            if let Some(target_start_src) = best_def_start {
                if let Some(snap_diff_idx) = src_line_map
                    .iter()
                    .enumerate()
                    .find_map(|(i, sl)| (*sl == Some(target_start_src)).then_some(i))
                {
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
        #[cfg(feature = "tree-sitter")]
        if let Some((ref src_line_map, ref defs)) = ast_info {
            let omitted_first_src = (he..ts).find_map(|i| src_line_map[i]);
            let omitted_last_src = (he..ts).rev().find_map(|i| src_line_map[i]);
            let resumed_src = (ts..lines.len()).find_map(|i| src_line_map[i]);

            if let (Some(om_start), Some(om_end)) = (omitted_first_src, omitted_last_src) {
                let omitted_lines = ts - he;
                let mut omitted_defs: Vec<&str> = Vec::new();
                for d in defs {
                    if d.start_line >= om_start
                        && d.start_line <= om_end
                        && !omitted_defs.contains(&d.short_label.as_str())
                    {
                        omitted_defs.push(d.short_label.as_str());
                    }
                }

                let resuming_in = resumed_src.and_then(|r_line| {
                    defs.iter()
                        .filter(|d| d.start_line < r_line && r_line <= d.end_line)
                        .min_by_key(|d| d.end_line.saturating_sub(d.start_line))
                        .map(|d| d.scope_label.as_str())
                });

                let mut parts = vec![format!(
                    "omitted L{om_start}-L{om_end} ({omitted_lines} lines)"
                )];
                if !omitted_defs.is_empty() {
                    let max_shown = 6;
                    let shown = &omitted_defs[..omitted_defs.len().min(max_shown)];
                    let more = if omitted_defs.len() > max_shown {
                        format!(", +{} more", omitted_defs.len() - max_shown)
                    } else {
                        String::new()
                    };
                    parts.push(format!("defs: {}{more}", shown.join(", ")));
                }
                if let Some(scope) = resuming_in {
                    parts.push(format!("resuming in {scope}"));
                }
                parts.push(format!("total +{added}/-{deleted}"));
                return format!(
                    "... [truncated due to patch budget: {display_path} ({})]\n",
                    parts.join("; ")
                );
            }
        }

        if !is_new && budget >= 1024 {
            build_hunk_scope_truncation_marker(&lines, he, ts, &display_path, added, deleted)
        } else {
            base_trailer_for(ts - he)
        }
    };

    let mut marker = build_marker(head_end, tail_start);
    let extra_newlines = usize::from(!lines[head_end - 1].ends_with('\n'))
        + usize::from(!lines[lines.len() - 1].ends_with('\n'));

    // Strictly enforce `budget` even if the enriched marker is wider than `initial_reserve`.
    while head_bytes + marker.len() + tail_bytes + extra_newlines > budget {
        if head_end > header_end && head_bytes >= tail_bytes {
            head_end -= 1;
            head_bytes = head_bytes.saturating_sub(lines[head_end].len());
        } else if tail_start + 1 < lines.len() {
            tail_bytes = tail_bytes.saturating_sub(lines[tail_start].len());
            tail_start += 1;
        } else {
            marker = base_trailer_for(tail_start - head_end);
            break;
        }
        marker = build_marker(head_end, tail_start);
        if head_bytes + marker.len() + tail_bytes + extra_newlines <= budget {
            break;
        }
        let fallback = base_trailer_for(tail_start - head_end);
        if head_bytes + fallback.len() + tail_bytes + extra_newlines <= budget
            && marker.len() > fallback.len() + 240
        {
            marker = fallback;
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
    pub(super) is_omitted: bool,
    pub(super) raw_chunk: String,
    pub(super) rendered: String,
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

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum ChunkState {
        Intact,
        DegradedU3,
        DegradedU0,
        Truncated,
        Omitted,
    }

    let mut states = vec![ChunkState::Intact; items.len()];

    // Reserve space for the top `[patch-budget: ...]` metadata banner on real budgets (>= 16 KiB)
    // so the banner always fits without violating `max_bytes`.
    let effective_max = if max_bytes >= 16 * 1024 {
        max_bytes.saturating_sub(BUDGET_BANNER_RESERVE_BYTES)
    } else {
        max_bytes
    };

    // Tier 1a: shrink expanded test chunks down to compact `-U3` (preserving AST signatures) first
    {
        let mut test_u3: Vec<(usize, String)> = items
            .iter()
            .enumerate()
            .filter(|(_, c)| c.is_test && !c.is_omitted && !c.is_new)
            .filter_map(|(i, c)| {
                trim_unified_diff_context(&c.rendered, COMPACT_CONTEXT_LINES)
                    .map(|compact| (i, compact))
            })
            .collect();
        test_u3.sort_by(|(a, ca), (b, cb)| {
            let save_a = items[*a].rendered.len().saturating_sub(ca.len());
            let save_b = items[*b].rendered.len().saturating_sub(cb.len());
            save_b.cmp(&save_a)
        });
        for (idx, compact) in test_u3 {
            if total_bytes(&items) <= effective_max {
                break;
            }
            items[idx].rendered = compact;
            states[idx] = ChunkState::DegradedU3;
        }
    }

    // Tier 1b: if still over budget, shrink expanded test chunks to raw `-U0`
    if total_bytes(&items) > effective_max {
        let mut test_expand_indices: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, c)| c.is_test && !c.is_omitted && c.raw_chunk.len() < c.rendered.len())
            .map(|(i, _)| i)
            .collect();
        test_expand_indices.sort_by(|&a, &b| {
            let save_a = items[a]
                .rendered
                .len()
                .saturating_sub(items[a].raw_chunk.len());
            let save_b = items[b]
                .rendered
                .len()
                .saturating_sub(items[b].raw_chunk.len());
            save_b.cmp(&save_a)
        });
        for idx in test_expand_indices {
            if total_bytes(&items) <= effective_max {
                break;
            }
            items[idx].rendered = items[idx].raw_chunk.clone();
            states[idx] = ChunkState::DegradedU0;
        }
    }

    // Tier 2: shrink expanded modified production chunks from full AST/`-W` down to compact `-U3`
    // while preserving their AST enclosing signatures in `@@ ... @@` headers (never `-U0` yet).
    if total_bytes(&items) > effective_max {
        let mut prod_u3: Vec<(usize, String)> = items
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.is_test && !c.is_omitted && !c.is_new)
            .filter_map(|(i, c)| {
                trim_unified_diff_context(&c.rendered, COMPACT_CONTEXT_LINES)
                    .map(|compact| (i, compact))
            })
            .collect();
        prod_u3.sort_by(|(a, ca), (b, cb)| {
            let save_a = items[*a].rendered.len().saturating_sub(ca.len());
            let save_b = items[*b].rendered.len().saturating_sub(cb.len());
            save_b.cmp(&save_a)
        });
        for (idx, compact) in prod_u3 {
            if total_bytes(&items) <= effective_max {
                break;
            }
            items[idx].rendered = compact;
            states[idx] = ChunkState::DegradedU3;
        }
    }

    // Tier 3: omit remaining large test chunks (largest first)
    if total_bytes(&items) > effective_max {
        let mut test_indices: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, c)| c.is_test && !c.is_omitted)
            .map(|(i, _)| i)
            .collect();
        test_indices.sort_by(|&a, &b| items[b].rendered.len().cmp(&items[a].rendered.len()));

        for idx in test_indices {
            if total_bytes(&items) <= effective_max {
                break;
            }
            if let Some(ref p) = items[idx].path {
                let summary = summarize_budget_omitted_chunk(&items[idx].raw_chunk, p);
                if summary.len() < items[idx].rendered.len() {
                    items[idx].rendered = summary;
                    items[idx].is_omitted = true;
                    states[idx] = ChunkState::Omitted;
                }
            }
        }
    }

    // Tier 4: water-filling head/tail truncation for oversized chunks (small chunks below the
    // water-level cap remain completely untouched).
    if total_bytes(&items) > effective_max {
        let tier4_max = if max_bytes < 16 * 1024 && max_bytes > 1024 + BUDGET_BANNER_RESERVE_BYTES {
            max_bytes.saturating_sub(BUDGET_BANNER_RESERVE_BYTES)
        } else {
            effective_max
        };

        // Check if waterfill cap is critically small (< 1 KiB); only in that extreme crunch
        // degrade modified production chunks from `-U3` to `-U0` before omitting/truncating.
        let initial_rem_sizes: Vec<usize> = items
            .iter()
            .filter(|c| !c.is_omitted)
            .map(|c| c.rendered.len())
            .collect();
        let initial_rem_total: usize = initial_rem_sizes.iter().sum();
        let initial_fixed = total_bytes(&items).saturating_sub(initial_rem_total);
        let initial_avail = tier4_max.saturating_sub(initial_fixed);
        let initial_cap = compute_waterfill_cap(&initial_rem_sizes, initial_avail);

        if initial_cap < 1024 {
            let mut prod_u0_indices: Vec<usize> = items
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    !c.is_test && !c.is_omitted && c.raw_chunk.len() < c.rendered.len()
                })
                .map(|(i, _)| i)
                .collect();
            prod_u0_indices.sort_by(|&a, &b| {
                let save_a = items[a]
                    .rendered
                    .len()
                    .saturating_sub(items[a].raw_chunk.len());
                let save_b = items[b]
                    .rendered
                    .len()
                    .saturating_sub(items[b].raw_chunk.len());
                save_b.cmp(&save_a)
            });
            for idx in prod_u0_indices {
                if total_bytes(&items) <= tier4_max {
                    break;
                }
                items[idx].rendered = items[idx].raw_chunk.clone();
                states[idx] = ChunkState::DegradedU0;
            }
        }

        if total_bytes(&items) > tier4_max {
            let mut rem_indices: Vec<usize> = items
                .iter()
                .enumerate()
                .filter(|(_, c)| !c.is_omitted && c.path.is_some())
                .map(|(i, _)| i)
                .collect();
            rem_indices.sort_by(|&a, &b| items[b].rendered.len().cmp(&items[a].rendered.len()));

            for pos in 0..rem_indices.len() {
                let cur_total = total_bytes(&items);
                if cur_total <= tier4_max {
                    break;
                }
                let idx = rem_indices[pos];
                let remaining_sizes: Vec<usize> = rem_indices[pos..]
                    .iter()
                    .map(|&i| items[i].rendered.len())
                    .collect();
                let remaining_sum: usize = remaining_sizes.iter().sum();
                let fixed_now = cur_total.saturating_sub(remaining_sum);
                let avail_now = tier4_max.saturating_sub(fixed_now);
                let cap = compute_waterfill_cap(&remaining_sizes, avail_now);

                let other_bytes = cur_total.saturating_sub(items[idx].rendered.len());
                let needed_only_from_this = tier4_max.saturating_sub(other_bytes);

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
                    let (replacement, new_state) = if chunk_budget >= 512 {
                        match truncate_chunk_head_tail(
                            &items[idx].rendered,
                            &items[idx].raw_chunk,
                            p,
                            items[idx].is_new,
                            chunk_budget,
                        ) {
                            Some(truncated) => (truncated, ChunkState::Truncated),
                            None => (
                                summarize_budget_omitted_chunk(&items[idx].raw_chunk, p),
                                ChunkState::Omitted,
                            ),
                        }
                    } else {
                        (
                            summarize_budget_omitted_chunk(&items[idx].raw_chunk, p),
                            ChunkState::Omitted,
                        )
                    };
                    if replacement.len() < items[idx].rendered.len() {
                        items[idx].rendered = replacement;
                        items[idx].is_omitted = true;
                        states[idx] = new_state;
                    }
                }
            }
        }
    }

    let degraded_u3 = states
        .iter()
        .filter(|&&s| s == ChunkState::DegradedU3)
        .count();
    let degraded_u0 = states
        .iter()
        .filter(|&&s| s == ChunkState::DegradedU0)
        .count();
    let truncated_count = states
        .iter()
        .filter(|&&s| s == ChunkState::Truncated)
        .count();
    let omitted_count = states.iter().filter(|&&s| s == ChunkState::Omitted).count();
    let compacted_count = degraded_u3 + degraded_u0 + truncated_count + omitted_count;

    let mut banner = String::new();
    if compacted_count > 0 {
        let mut detail_parts = Vec::new();
        if degraded_u3 > 0 {
            detail_parts.push(format!("{degraded_u3} degraded to -U3"));
        }
        if degraded_u0 > 0 {
            detail_parts.push(format!("{degraded_u0} degraded to -U0"));
        }
        if truncated_count > 0 {
            detail_parts.push(format!("{truncated_count} truncated"));
        }
        if omitted_count > 0 {
            detail_parts.push(format!("{omitted_count} omitted"));
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
            "diff output exceeded patch budget ({max_bytes} bytes); compacted {compacted_count} file chunk(s) ({})",
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
                if let Some((target_path, _, _, is_new, _)) = info {
                    let path_is_test = is_test_file_path(&target_path);
                    if is_lockfile_or_generated(&target_path) {
                        let omitted = summarize_omitted_chunk(&chunk, &target_path);
                        return RenderedChunk {
                            path: Some(target_path),
                            is_test: path_is_test,
                            is_new,
                            is_omitted: true,
                            raw_chunk: chunk,
                            rendered: omitted,
                        };
                    }
                    return RenderedChunk {
                        path: Some(target_path),
                        is_test: path_is_test,
                        is_new,
                        is_omitted: false,
                        raw_chunk: chunk.clone(),
                        rendered: chunk,
                    };
                }
                RenderedChunk {
                    path: None,
                    is_test: false,
                    is_new: false,
                    is_omitted: false,
                    raw_chunk: chunk.clone(),
                    rendered: chunk,
                }
            })
            .collect();

        Ok(apply_patch_budget(
            &prefix,
            rendered,
            DEFAULT_PATCH_BUDGET_BYTES,
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
                                    is_omitted: false,
                                    raw_chunk: chunk.clone(),
                                    rendered: chunk.clone(),
                                },
                                file_warnings,
                            ))
                        }
                    };

                let path_is_test = is_test_file_path(&target_path);

                if is_lockfile_or_generated(&target_path) {
                    let omitted = summarize_omitted_chunk(chunk, &target_path);
                    return Ok((
                        RenderedChunk {
                            path: Some(target_path),
                            is_test: path_is_test,
                            is_new,
                            is_omitted: true,
                            raw_chunk: chunk.clone(),
                            rendered: omitted,
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
                                is_omitted: true,
                                raw_chunk: chunk.clone(),
                                rendered: test_summary,
                            },
                            file_warnings,
                        ));
                    }
                    if let Some(moved_summary) =
                        summarize_moved_new_file_chunk(chunk, &target_path, &moved_deleted_map)
                    {
                        return Ok((
                            RenderedChunk {
                                path: Some(target_path),
                                is_test: path_is_test,
                                is_new: true,
                                is_omitted: false,
                                raw_chunk: chunk.clone(),
                                rendered: moved_summary,
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
                            is_omitted: false,
                            raw_chunk: chunk.clone(),
                            rendered: chunk.clone(),
                        },
                        file_warnings,
                    ));
                }

                let diff_str = match SupportedLanguage::from_path(&target_path) {
                    Some(lang) => {
                        match expand_chunk_with_treesitter(
                            &repo_root,
                            &target_path,
                            chunk,
                            source,
                            lang,
                            &moved_deleted_map,
                        ) {
                            Ok(Some(expanded)) if !expanded.trim().is_empty() => expanded,
                            Ok(Some(_)) | Ok(None) => {
                                file_warnings.push(format!(
                                    "tree-sitter: could not expand '{}'; falling back to git diff -W",
                                    target_path.display()
                                ));
                                git_diff_w_for_file(
                                    &repo_root,
                                    &target_path,
                                    old_path.as_deref(),
                                    source,
                                )?
                            }
                            Err(e) => {
                                file_warnings.push(format!(
                                    "tree-sitter: failed for '{}' ({e}); falling back to git diff -W",
                                    target_path.display()
                                ));
                                git_diff_w_for_file(
                                    &repo_root,
                                    &target_path,
                                    old_path.as_deref(),
                                    source,
                                )?
                            }
                        }
                    }
                    None => {
                        file_warnings.push(format!(
                            "tree-sitter: unsupported language for '{}'; falling back to git diff -W",
                            target_path.display()
                        ));
                        git_diff_w_for_file(&repo_root, &target_path, old_path.as_deref(), source)?
                    }
                };
                Ok((
                    RenderedChunk {
                        path: Some(target_path),
                        is_test: path_is_test,
                        is_new: false,
                        is_omitted: false,
                        raw_chunk: chunk.clone(),
                        rendered: diff_str,
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
            DEFAULT_PATCH_BUDGET_BYTES,
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
        ] {
            assert!(
                is_lockfile_or_generated(Path::new(p)),
                "Expected generated/lockfile detection for {p}"
            );
        }
        for p in ["src/main.rs", "src/app.ts", "src/order.cs", "README.md"] {
            assert!(
                !is_lockfile_or_generated(Path::new(p)),
                "Unexpected generated/lockfile false positive for {p}"
            );
        }
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

        // Tier 1: Expanded test chunk shrinks to its raw `-U0` diff before being omitted
        let test_raw = make_chunk("tests/foo_test.rs", 8);
        let test_expanded = make_chunk("tests/foo_test.rs", 25);
        let prod_raw = make_chunk("src/prod.rs", 10);
        let prod_expanded = make_chunk("src/prod.rs", 25);
        let items_t1 = vec![
            RenderedChunk {
                path: Some(PathBuf::from("tests/foo_test.rs")),
                is_test: true,
                is_new: false,
                is_omitted: false,
                raw_chunk: test_raw.clone(),
                rendered: test_expanded,
            },
            RenderedChunk {
                path: Some(PathBuf::from("src/prod.rs")),
                is_test: false,
                is_new: false,
                is_omitted: false,
                raw_chunk: prod_raw.clone(),
                rendered: prod_expanded.clone(),
            },
        ];
        let mut warnings = Vec::new();
        let out_t1 = apply_patch_budget(
            "",
            items_t1,
            test_raw.len() + prod_expanded.len() + 20,
            &mut warnings,
        );
        assert!(
            out_t1.contains("+let line_7"),
            "Tier 1 must preserve raw -U0 test diff lines instead of omitting test file!"
        );
        assert!(!out_t1.contains("[omitted due to patch budget"));
        assert!(out_t1.contains("+let line_24"));
        assert!(warnings.iter().any(|w| w.contains("exceeded patch budget")));

        // Tier 2 & 3: When even raw `-U0` exceeds budget, Tier 3 omits test chunk while preserving production chunk
        let items_t3 = vec![
            RenderedChunk {
                path: Some(PathBuf::from("tests/foo_test.rs")),
                is_test: true,
                is_new: false,
                is_omitted: false,
                raw_chunk: test_raw.clone(),
                rendered: test_raw,
            },
            RenderedChunk {
                path: Some(PathBuf::from("src/prod.rs")),
                is_test: false,
                is_new: false,
                is_omitted: false,
                raw_chunk: prod_raw.clone(),
                rendered: prod_expanded.clone(),
            },
        ];
        let mut warnings3 = Vec::new();
        let out_t3 = apply_patch_budget("", items_t3, prod_raw.len() + 300, &mut warnings3);
        assert!(out_t3.contains("[omitted due to patch budget: tests/foo_test.rs"));
        assert!(out_t3.contains("+let line_9"));

        // Tier 4: Multiple equal-sized oversized production chunks are water-fill head/tail truncated
        // rather than completely omitting the largest production files
        let p1 = make_chunk("src/core_a.rs", 80);
        let p2 = make_chunk("src/core_b.rs", 80);
        let items_t4 = vec![
            RenderedChunk {
                path: Some(PathBuf::from("src/core_a.rs")),
                is_test: false,
                is_new: true,
                is_omitted: false,
                raw_chunk: p1.clone(),
                rendered: p1,
            },
            RenderedChunk {
                path: Some(PathBuf::from("src/core_b.rs")),
                is_test: false,
                is_new: true,
                is_omitted: false,
                raw_chunk: p2.clone(),
                rendered: p2,
            },
        ];
        let mut warnings4 = Vec::new();
        let out_t4 = apply_patch_budget("", items_t4, 2400, &mut warnings4);
        assert!(
            out_t4.len() <= 2400,
            "Tier 4 exceeded budget: {}",
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
        assert!(out_t4.contains("+let line_79"));
        assert!(
            out_t4.contains("[patch-budget: 2400 B hit; 2 truncated]"),
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

        // Build a tiny modified file (~350 bytes) that old proportional Tier 4 could drop below 512 B and omit
        let tiny_mod = String::from(
            "diff --git a/src/tiny.rs b/src/tiny.rs\nindex 1111111..2222222 100644\n--- a/src/tiny.rs\n+++ b/src/tiny.rs\n@@ -1,4 +1,4 @@ fn tiny() {\n fn tiny() {\n-    let a = 1;\n+    let a = 2;\n     a\n }\n",
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
            RenderedChunk {
                path: Some(PathBuf::from("src/small_mod.rs")),
                is_test: false,
                is_new: false,
                is_omitted: false,
                raw_chunk: raw_mod_u0,
                rendered: expanded_mod,
            },
            RenderedChunk {
                path: Some(PathBuf::from("src/tiny.rs")),
                is_test: false,
                is_new: false,
                is_omitted: false,
                raw_chunk: tiny_mod.clone(),
                rendered: tiny_mod,
            },
            RenderedChunk {
                path: Some(PathBuf::from("src/huge_new.rs")),
                is_test: false,
                is_new: true,
                is_omitted: false,
                raw_chunk: huge_new.clone(),
                rendered: huge_new,
            },
        ];

        let mut warnings = Vec::new();
        let budget = 16 * 1024;
        let out = apply_patch_budget(
            "commit abc123\n\n---\n 3 files changed\n\n",
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
        // Banner appears right below prefix and never double-counts
        assert!(
            out.contains("[patch-budget: 16 KiB hit; 1 degraded to -U3, 1 truncated]"),
            "Expected accurate patch-budget banner, got:\n{}",
            &out[..out.len().min(600)]
        );
        // Modified file keeps -U3 context + AST signature instead of regressing to -U0
        assert!(
            out.contains("@@ -18,7 +18,7 @@ impl Router > fn dispatch(req: &Req) {"),
            "Modified file must retain -U3 context and AST signature:\n{out}"
        );
        assert!(out.contains("     let pre_18 = compute_step(18);"));
        assert!(out.contains("     let post_24 = finish_step(24);"));
        assert!(
            !out.contains("     let pre_1 = compute_step(1);"),
            "Unchanged outer AST context beyond -U3 should have been trimmed"
        );
        // Tiny modified file is 100% intact (never truncated or omitted by water-filling)
        assert!(out.contains("-    let a = 1;\n+    let a = 2;\n"));
        assert!(!out.contains("truncated due to patch budget: src/tiny.rs"));
        assert!(!out.contains("omitted due to patch budget: src/tiny.rs"));
        // Large new file is truncated cleanly (and when tree-sitter is enabled, lists omitted defs)
        assert!(out.contains("[truncated due to patch budget: src/huge_new.rs"));
        #[cfg(feature = "tree-sitter")]
        assert!(
            out.contains("defs: fn generated_helper_"),
            "Expected AST mini-TOC of omitted defs in truncation marker:\n{out}"
        );

        // Mid-function truncation on a giant single function annotates `resuming in fn mega_engine`
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
                truncate_chunk_head_tail(&mega, &mega, Path::new("src/mega.rs"), true, 4096)
                    .expect("expected mega.rs to truncate within 4096 B");
            assert!(truncated.len() <= 4096);
            assert!(
                truncated.contains("resuming in fn mega_engine"),
                "Mid-function truncation must report resuming scope, got:\n{truncated}"
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
}
