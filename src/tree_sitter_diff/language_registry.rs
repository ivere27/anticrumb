//! Declarative file-extension -> Tree-sitter language registry with priority-ordered
//! candidate resolution and AST `ERROR`-node gating.
//!
//! Mappings live in `extensions.json`:
//! - `"exact"`: unambiguous 1:1 extension -> language mappings (parsed directly).
//! - `"priority"`: ambiguous extensions mapped to an ordered list of candidate languages
//!   (e.g. `"inc": ["cpp"]`, extensible to `["cpp", "php", ...]`). Candidates are tried
//!   in priority order and accepted only when Tree-sitter AST validation passes.

use super::SupportedLanguage;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::OnceLock,
};

#[cfg(feature = "tree-sitter")]
use super::{check_primary_definition, diff_expand::parse_with_timeout};

const BUNDLED_EXTENSIONS_JSON: &str = include_str!("extensions.json");

/// Maximum percentage of non-whitespace source bytes allowed inside `ERROR` nodes
/// when accepting a `Priority` candidate that has localized parse errors (e.g. unknown
/// preprocessor qualifier macros or `#if/#else` split braces in C/C++).
#[cfg(feature = "tree-sitter")]
const MAX_PRIORITY_ERROR_BYTE_PCT: usize = 10;

/// Maximum ratio of `MISSING` tokens to non-empty lines (`missing * 10 <= non_empty_lines`, i.e. 10%).
#[cfg(feature = "tree-sitter")]
const PRIORITY_MISSING_LINE_DIVISOR: usize = 10;

/// Extension resolution result from [`LanguageRegistry`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LanguageCandidates<'a> {
    /// Unambiguous 1:1 extension mapping (parsed directly without AST error-rate rejection).
    Exact(SupportedLanguage),
    /// Priority-ordered candidate list validated via Tree-sitter AST `ERROR`/`MISSING` gating.
    Priority(&'a [SupportedLanguage]),
}

impl<'a> LanguageCandidates<'a> {
    /// Returns the primary (exact or highest-priority) language for this extension.
    pub fn primary(self) -> SupportedLanguage {
        match self {
            Self::Exact(lang) => lang,
            Self::Priority(langs) => langs[0],
        }
    }

    /// Returns true if resolution requires Tree-sitter AST error gating.
    pub fn requires_parse_validation(self) -> bool {
        matches!(self, Self::Priority(_))
    }

    /// Calls `f` for each candidate language in priority order (or once for `Exact`).
    pub fn iter(self) -> LanguageCandidatesIter<'a> {
        match self {
            Self::Exact(lang) => LanguageCandidatesIter::Exact(Some(lang)),
            Self::Priority(langs) => LanguageCandidatesIter::Priority(langs.iter()),
        }
    }

    /// Formats candidate language names in priority order for diagnostic warnings (e.g. `"cpp, php"`).
    pub fn format_names(self) -> String {
        let mut out = String::new();
        for (i, lang) in self.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(lang.as_str());
        }
        out
    }
}

pub enum LanguageCandidatesIter<'a> {
    Exact(Option<SupportedLanguage>),
    Priority(std::slice::Iter<'a, SupportedLanguage>),
}

impl Iterator for LanguageCandidatesIter<'_> {
    type Item = SupportedLanguage;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Exact(slot) => slot.take(),
            Self::Priority(iter) => iter.next().copied(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExtensionsConfig {
    #[serde(default)]
    exact: BTreeMap<String, String>,
    #[serde(default)]
    priority: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    plain_diff: Vec<String>,
}

/// Immutable lookup table loaded from `extensions.json` (or custom JSON via [`Self::from_json_str`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanguageRegistry {
    exact: HashMap<String, SupportedLanguage>,
    priority: HashMap<String, Vec<SupportedLanguage>>,
    plain_diff: HashSet<String>,
}

impl LanguageRegistry {
    /// Returns the built-in registry embedded from `src/tree_sitter_diff/extensions.json`.
    pub fn builtin() -> &'static Self {
        static BUILTIN: OnceLock<LanguageRegistry> = OnceLock::new();
        BUILTIN.get_or_init(|| {
            Self::from_json_str(BUNDLED_EXTENSIONS_JSON)
                .expect("bundled tree_sitter_diff/extensions.json must be valid")
        })
    }

    /// Parses and validates an extensions mapping JSON document.
    pub fn from_json_str(raw: &str) -> std::result::Result<Self, String> {
        let parsed: RawExtensionsConfig =
            serde_json::from_str(raw).map_err(|e| format!("invalid extensions JSON: {e}"))?;

        let mut exact = HashMap::with_capacity(parsed.exact.len());
        for (raw_ext, raw_lang) in parsed.exact {
            let ext = normalize_ext(&raw_ext)?;
            let lang = SupportedLanguage::from_name(&raw_lang).ok_or_else(|| {
                format!("unknown language '{raw_lang}' for exact extension '{ext}'")
            })?;
            if exact.insert(ext.clone(), lang).is_some() {
                return Err(format!("duplicate exact extension '{ext}'"));
            }
        }

        let mut priority = HashMap::with_capacity(parsed.priority.len());
        for (raw_ext, raw_langs) in parsed.priority {
            let ext = normalize_ext(&raw_ext)?;
            if exact.contains_key(&ext) {
                return Err(format!(
                    "extension '{ext}' cannot appear in both 'exact' and 'priority'"
                ));
            }
            if raw_langs.is_empty() {
                return Err(format!(
                    "priority list for extension '{ext}' must not be empty"
                ));
            }
            let mut langs = Vec::with_capacity(raw_langs.len());
            for raw_lang in raw_langs {
                let lang = SupportedLanguage::from_name(&raw_lang).ok_or_else(|| {
                    format!("unknown language '{raw_lang}' in priority list for extension '{ext}'")
                })?;
                if langs.contains(&lang) {
                    return Err(format!(
                        "duplicate language '{raw_lang}' in priority list for extension '{ext}'"
                    ));
                }
                langs.push(lang);
            }
            if priority.insert(ext.clone(), langs).is_some() {
                return Err(format!("duplicate priority extension '{ext}'"));
            }
        }

        let mut plain_diff = HashSet::with_capacity(parsed.plain_diff.len());
        for raw_ext in parsed.plain_diff {
            let ext = normalize_ext(&raw_ext)?;
            if exact.contains_key(&ext) || priority.contains_key(&ext) {
                return Err(format!(
                    "extension '{ext}' cannot appear in 'plain_diff' and also in 'exact'/'priority'"
                ));
            }
            if !plain_diff.insert(ext.clone()) {
                return Err(format!("duplicate plain_diff extension '{ext}'"));
            }
        }

        Ok(Self {
            exact,
            priority,
            plain_diff,
        })
    }

    /// Resolves extension candidates for `path` (case-insensitive extension match).
    pub fn candidates_for_path(&self, path: &Path) -> Option<LanguageCandidates<'_>> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        self.candidates_for_ext(&ext)
    }

    /// Resolves extension candidates for an already-normalized lowercase extension without leading dot.
    pub fn candidates_for_ext(&self, ext: &str) -> Option<LanguageCandidates<'_>> {
        if let Some(&lang) = self.exact.get(ext) {
            return Some(LanguageCandidates::Exact(lang));
        }
        self.priority
            .get(ext)
            .map(|langs| LanguageCandidates::Priority(langs.as_slice()))
    }

    /// Returns true if `path` is configured in `"plain_diff"` (attach clean `-U3` diff without warnings).
    pub fn is_plain_diff_path(&self, path: &Path) -> bool {
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| self.is_plain_diff_ext(&e.to_ascii_lowercase()))
        {
            return true;
        }
        // Rust's `Path::extension()` returns `None` for dotfiles like `.env` and `"local"`
        // for `.env.local`. Match `.env` / `.env.*` when `"env"` is configured in `plain_diff`
        // unless the path has an explicit Tree-sitter grammar extension (e.g. `.env.json`).
        if self.is_plain_diff_ext("env") && self.candidates_for_path(path).is_none() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                let lower = name.to_ascii_lowercase();
                if lower == ".env" || lower.starts_with(".env.") {
                    return true;
                }
            }
        }
        false
    }

    /// Returns true if the normalized lowercase extension is in `"plain_diff"`.
    pub fn is_plain_diff_ext(&self, ext: &str) -> bool {
        self.plain_diff.contains(ext)
    }
}

fn normalize_ext(raw: &str) -> std::result::Result<String, String> {
    let trimmed = raw.trim().trim_start_matches('.');
    if trimmed.is_empty() {
        return Err(format!("invalid empty extension '{raw}'"));
    }
    Ok(trimmed.to_ascii_lowercase())
}

#[cfg(feature = "tree-sitter")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ParseQuality {
    /// Zero `ERROR` and zero `MISSING` nodes anywhere in the tree.
    Clean,
    /// Localized parse errors within strict bounds, zero errors overlapping changed hunks,
    /// and at least one error-free primary definition in the file.
    Tolerated {
        error_non_ws_bytes: usize,
        missing_count: usize,
    },
    /// Too many parse errors, missing tokens, error overlapping a changed hunk, or no
    /// valid primary definitions alongside errors.
    Rejected,
}

#[cfg(feature = "tree-sitter")]
pub(super) enum CandidateParseOutcome {
    Parsed {
        lang: SupportedLanguage,
        tree: tree_sitter::Tree,
    },
    ExactParseFailed(&'static str),
    NoMatchingCandidate,
}

/// Evaluates whether a Tree-sitter parse tree is structurally valid for a `Priority` candidate
/// using pure Tree-sitter AST signals (`has_error`, `is_error`, `is_missing`, primary definitions).
/// `changed_line_ranges` contains 1-based inclusive `(start_line, end_line)` spans for modified hunks.
#[cfg(feature = "tree-sitter")]
pub(super) fn assess_priority_parse(
    lang: SupportedLanguage,
    tree: &tree_sitter::Tree,
    source: &str,
    changed_line_ranges: &[(usize, usize)],
) -> ParseQuality {
    let root = tree.root_node();
    if !root.has_error() {
        return ParseQuality::Clean;
    }

    let source_non_ws_bytes = source.bytes().filter(|b| !b.is_ascii_whitespace()).count();
    if source_non_ws_bytes == 0 {
        return ParseQuality::Clean;
    }
    let non_empty_lines = source
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
        .max(1);

    let mut error_non_ws_bytes = 0usize;
    let mut missing_count = 0usize;
    let mut valid_primary_defs = 0usize;
    let mut changed_hunk_has_error = false;

    let overlaps_changed = |node: &tree_sitter::Node| -> bool {
        if changed_line_ranges.is_empty() {
            return false;
        }
        let s_row = node.start_position().row;
        let end = node.end_position();
        let e_row = if end.column == 0 && end.row > s_row {
            end.row - 1
        } else {
            end.row
        };
        changed_line_ranges.iter().any(|&(start_line, end_line)| {
            let cs = start_line.saturating_sub(1);
            let ce = end_line.saturating_sub(1);
            s_row <= ce && e_row >= cs
        })
    };

    // Iterative DFS via TreeCursor (avoids recursion depth issues on deep trees and never
    // double-counts bytes inside nested ERROR subtrees).
    let mut cursor = root.walk();
    'walk: loop {
        let node = cursor.node();
        let mut descend = true;

        if node.is_error() {
            if overlaps_changed(&node) {
                changed_hunk_has_error = true;
                break 'walk;
            }
            let s = node.start_byte().min(source.len());
            let e = node.end_byte().min(source.len());
            if s < e {
                error_non_ws_bytes += source.as_bytes()[s..e]
                    .iter()
                    .filter(|b| !b.is_ascii_whitespace())
                    .count();
            }
            descend = false;
        } else if node.is_missing() {
            missing_count += 1;
            if overlaps_changed(&node) {
                changed_hunk_has_error = true;
                break 'walk;
            }
            descend = false;
        } else if !node.has_error() {
            if check_primary_definition(lang, &node).is_some() {
                valid_primary_defs += 1;
            }
            // No error nodes can exist inside a `!has_error()` subtree; if we already found
            // at least one valid primary definition, skip descending this clean subtree.
            if valid_primary_defs > 0 {
                descend = false;
            }
        }

        if descend && cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }

    if changed_hunk_has_error {
        return ParseQuality::Rejected;
    }
    if valid_primary_defs == 0 {
        return ParseQuality::Rejected;
    }
    if error_non_ws_bytes.saturating_mul(100)
        > source_non_ws_bytes.saturating_mul(MAX_PRIORITY_ERROR_BYTE_PCT)
    {
        return ParseQuality::Rejected;
    }
    if missing_count.saturating_mul(PRIORITY_MISSING_LINE_DIVISOR) > non_empty_lines {
        return ParseQuality::Rejected;
    }

    ParseQuality::Tolerated {
        error_non_ws_bytes,
        missing_count,
    }
}

/// Parses `source` using `candidates`.
/// - `Exact(lang)`: parses unconditionally with `lang` (existing behavior).
/// - `Priority(langs)`: tries each candidate in priority order, accepting the first `Clean`
///   parse immediately, or falling back to the cleanest `Tolerated` parse across all candidates;
///   returns `NoMatchingCandidate` if every candidate fails AST validation.
#[cfg(feature = "tree-sitter")]
pub(super) fn parse_for_candidates(
    candidates: LanguageCandidates<'_>,
    source: &str,
    changed_line_ranges: &[(usize, usize)],
) -> CandidateParseOutcome {
    match candidates {
        LanguageCandidates::Exact(lang) => match parse_with_timeout(lang, source) {
            Ok(tree) => CandidateParseOutcome::Parsed { lang, tree },
            Err(e) => CandidateParseOutcome::ExactParseFailed(e),
        },
        LanguageCandidates::Priority(langs) => {
            let mut best_tolerated: Option<(usize, usize, SupportedLanguage, tree_sitter::Tree)> =
                None;

            for &lang in langs {
                let Ok(tree) = parse_with_timeout(lang, source) else {
                    continue;
                };
                match assess_priority_parse(lang, &tree, source, changed_line_ranges) {
                    ParseQuality::Clean => {
                        return CandidateParseOutcome::Parsed { lang, tree };
                    }
                    ParseQuality::Tolerated {
                        error_non_ws_bytes,
                        missing_count,
                    } => {
                        let is_better = match &best_tolerated {
                            None => true,
                            Some((best_err, best_miss, _, _)) => {
                                (error_non_ws_bytes, missing_count) < (*best_err, *best_miss)
                            }
                        };
                        if is_better {
                            best_tolerated = Some((error_non_ws_bytes, missing_count, lang, tree));
                        }
                    }
                    ParseQuality::Rejected => {}
                }
            }

            if let Some((_, _, lang, tree)) = best_tolerated {
                CandidateParseOutcome::Parsed { lang, tree }
            } else {
                CandidateParseOutcome::NoMatchingCandidate
            }
        }
    }
}

/// Resolves and parses `source` for `path` using the built-in [`LanguageRegistry`].
/// Returns `None` if the extension is unsupported, parsing fails, or no priority candidate passes.
#[cfg(feature = "tree-sitter")]
pub(super) fn parse_for_path(
    path: &Path,
    source: &str,
) -> Option<(SupportedLanguage, tree_sitter::Tree)> {
    let candidates = SupportedLanguage::candidates_for_path(path)?;
    match parse_for_candidates(candidates, source, &[]) {
        CandidateParseOutcome::Parsed { lang, tree } => Some((lang, tree)),
        CandidateParseOutcome::ExactParseFailed(_) | CandidateParseOutcome::NoMatchingCandidate => {
            None
        }
    }
}
