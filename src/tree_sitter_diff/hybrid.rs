//! 3-Layer Hybrid diff pipeline components for 1-turn LLM code review:
//! - `HybridConfig`: configuration flags (default: `HybridConfig::full()`, with ablation presets for benchmarks)
//! - Layer 1: Compact upfront commit symbol index (+ large new-file index)
//! - Layer 3: Lightweight LSP-free Tree-sitter external AST context (same-file + 1-hop cross-file)

#[cfg(feature = "tree-sitter")]
use super::{
    ast_compact, check_primary_definition,
    diff_expand::{is_lockfile_or_generated, parse_with_timeout},
    git_cmd, is_test_file_path, signature, GitDiffSource, SupportedLanguage,
    MAX_TREESITTER_FILE_BYTES,
};
#[cfg(feature = "tree-sitter")]
use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(feature = "tree-sitter")]
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(feature = "tree-sitter")]
use std::path::{Path, PathBuf};
#[cfg(feature = "tree-sitter")]
use std::process::Stdio;
#[cfg(feature = "tree-sitter")]
use std::time::{Duration, Instant};

/// Configuration flags controlling the 3-Layer Hybrid diff pipeline.
/// All layers default to `true` (`HybridConfig::full()`); individual layers can be toggled
/// programmatically via `generate_expanded_diff_with_config` for ablation benchmarks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HybridConfig {
    /// Layer 1: compact per-file changed symbol index at the top of the diff.
    pub layer1: bool,
    /// Layer 2: signature-only suppression, clamped param preservation, expanded `(+N more)`,
    /// unambiguous scope separator, and selective key decorator preservation.
    pub layer2: bool,
    /// Layer 3-①: unmodified `struct`/`enum`/`interface`/`type`/`const` referenced in the same file.
    pub layer3_local: bool,
    /// Layer 3-②: 1-hop cross-file type definitions and body-stripped function signatures.
    pub layer3_cross: bool,
}

impl Default for HybridConfig {
    fn default() -> Self {
        Self::full()
    }
}

impl HybridConfig {
    pub const fn off() -> Self {
        Self {
            layer1: false,
            layer2: false,
            layer3_local: false,
            layer3_cross: false,
        }
    }

    pub const fn full() -> Self {
        Self {
            layer1: true,
            layer2: true,
            layer3_local: true,
            layer3_cross: true,
        }
    }

    pub fn is_any_enabled(self) -> bool {
        self.layer1 || self.layer2 || self.layer3_local || self.layer3_cross
    }

    pub fn parse(raw: Option<&str>) -> Self {
        let Some(raw) = raw else {
            return Self::full();
        };
        let trimmed = raw.trim().to_ascii_lowercase();
        if trimmed.is_empty() || matches!(trimmed.as_str(), "1" | "true" | "all" | "full" | "on") {
            return Self::full();
        }
        if matches!(trimmed.as_str(), "0" | "false" | "off" | "none") {
            return Self::off();
        }
        let mut cfg = Self::off();
        for tok in trimmed.split([',', '+', ' ']).filter(|t| !t.is_empty()) {
            match tok {
                "l1" | "layer1" | "toc" => cfg.layer1 = true,
                "l2" | "layer2" => cfg.layer2 = true,
                "l3_local" | "l3-local" | "local" => cfg.layer3_local = true,
                "l3_cross" | "l3-cross" | "cross" => cfg.layer3_cross = true,
                "l3" | "layer3" => {
                    cfg.layer3_local = true;
                    cfg.layer3_cross = true;
                }
                "all" | "full" => return Self::full(),
                _ => {}
            }
        }
        cfg
    }

    pub fn scope_sep(self) -> &'static str {
        if self.layer2 {
            " › "
        } else {
            " > "
        }
    }
}

/// Safely truncates `s` to at most `max_bytes` UTF-8 bytes without splitting multi-byte chars.
#[cfg(any(feature = "tree-sitter", test))]
pub(super) fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Summary metadata for a single file chunk used to build Layer 1 Symbol Index.
#[derive(Clone, Debug)]
pub(super) struct Layer1FileEntry {
    pub(super) path: String,
    pub(super) status: char, // 'A', 'M', 'D'
    pub(super) added: usize,
    pub(super) deleted: usize,
    pub(super) is_large_new: bool,
    pub(super) tags: Vec<&'static str>,
    pub(super) symbols: Vec<String>,
}

/// Computes the dynamic byte budget for Layer 1: `min(3% of patch budget, 6 KiB)`.
pub(super) fn layer1_budget_bytes(max_patch_bytes: usize) -> usize {
    ((max_patch_bytes * 3) / 100).min(6 * 1024)
}

/// Renders Layer 1 compact commit symbol index, bounded strictly to `max_bytes`.
/// Returns `None` for trivial single-file small diffs where a TOC adds pure overhead.
pub(super) fn render_layer1_index(
    entries: &[Layer1FileEntry],
    max_bytes: usize,
    total_rendered_bytes: usize,
) -> Option<String> {
    if entries.is_empty() || max_bytes < 160 {
        return None;
    }
    let total_churn: usize = entries.iter().map(|e| e.added + e.deleted).sum();
    let has_large_new = entries.iter().any(|e| e.is_large_new);
    if entries.len() <= 1 && !has_large_new && total_churn < 120 && total_rendered_bytes < 4096 {
        return None;
    }

    let total_added: usize = entries.iter().map(|e| e.added).sum();
    let total_deleted: usize = entries.iter().map(|e| e.deleted).sum();
    let file_noun = if entries.len() == 1 { "file" } else { "files" };
    let mut out = format!(
        "[Symbol Index: {} {file_noun}, +{total_added}/-{total_deleted}]\n",
        entries.len()
    );
    let footer_reserve = 36usize;
    let mut emitted = 0usize;
    // Under tight budgets with many files, emit compact file/tag-only rows so Layer 1 stays
    // concise and never crowds out Layer 2 production diffs.
    let omit_symbols_for_density = entries.len() > 25 && max_bytes <= 3 * 1024;

    for (idx, e) in entries.iter().enumerate() {
        let mut line = format!("  {} {} (+{}/-{})", e.status, e.path, e.added, e.deleted);
        if !e.tags.is_empty() {
            line.push_str(&format!(" [{}]", e.tags.join(", ")));
        }
        if !e.symbols.is_empty() && !omit_symbols_for_density {
            line.push_str(" — ");
            let sym_cap = if e.is_large_new {
                140
            } else if entries.len() > 20 {
                72
            } else {
                120
            };
            let mut sym_buf = String::new();
            for (s_idx, sym) in e.symbols.iter().enumerate() {
                let sep = if s_idx == 0 { "" } else { ", " };
                let rem = e.symbols.len().saturating_sub(s_idx + 1);
                let tail_res = if rem > 0 { 12 } else { 0 };
                if s_idx > 0 && sym_buf.len() + sep.len() + sym.len() + tail_res > sym_cap {
                    sym_buf.push_str(&format!(", +{} more", e.symbols.len() - s_idx));
                    break;
                }
                sym_buf.push_str(sep);
                sym_buf.push_str(sym);
            }
            line.push_str(&sym_buf);
        }
        line.push('\n');

        let has_more = idx + 1 < entries.len();
        let needed = line.len() + if has_more { footer_reserve } else { 1 };
        if out.len() + needed > max_bytes {
            let remaining = entries.len().saturating_sub(emitted);
            if remaining > 0 {
                let rem_noun = if remaining == 1 { "file" } else { "files" };
                let tail = format!("  ... (+{remaining} more {rem_noun})\n");
                if out.len() + tail.len() < max_bytes {
                    out.push_str(&tail);
                }
            }
            break;
        }
        out.push_str(&line);
        emitted += 1;
    }

    if emitted == 0 {
        return None;
    }
    out.push('\n');
    Some(out)
}

/// Strips ONLY the trailing file-by-file `--stat` block from the end of `prefix` when Layer 1
/// replaces it, never touching indented commit message body lines (even if a commit message
/// contains markdown tables with `" | "` or prose mentioning `"files changed"`).
pub(super) fn strip_redundant_stat_lines(prefix: &str) -> String {
    let lines: Vec<&str> = prefix.split_inclusive('\n').collect();
    if lines.is_empty() {
        return String::new();
    }
    // Scan backwards past trailing blank lines to locate the git `--stat` summary line
    let mut end_idx = lines.len();
    while end_idx > 0 && lines[end_idx - 1].trim().is_empty() {
        end_idx -= 1;
    }
    if end_idx == 0 {
        return prefix.to_string();
    }
    let last_trimmed = lines[end_idx - 1].trim_end_matches(['\r', '\n']);
    let is_stat_summary = last_trimmed.starts_with(' ')
        && (last_trimmed.contains(" file changed") || last_trimmed.contains(" files changed"))
        && (last_trimmed.contains(" insertion")
            || last_trimmed.contains(" deletion")
            || last_trimmed.ends_with("changed"));
    if !is_stat_summary {
        return prefix.to_string();
    }
    let mut stat_start = end_idx - 1;
    while stat_start > 0 {
        let prev = lines[stat_start - 1].trim_end_matches(['\r', '\n']);
        let is_stat_file_line = prev.starts_with(' ')
            && prev.contains(" | ")
            && !prev.trim_start().starts_with("diff ");
        if is_stat_file_line {
            stat_start -= 1;
        } else {
            break;
        }
    }
    let mut out = String::with_capacity(prefix.len());
    for l in &lines[..stat_start] {
        out.push_str(l);
    }
    for l in &lines[end_idx..] {
        out.push_str(l);
    }
    while out.ends_with("\n\n\n") {
        out.pop();
    }
    out
}

#[cfg(feature = "tree-sitter")]
#[derive(Clone, Debug)]
pub(super) struct ContextEntry {
    pub(super) file_path: PathBuf,
    pub(super) symbol_name: String,
    pub(super) start_line: usize,
    pub(super) end_line: usize,
    pub(super) is_local: bool,
    pub(super) is_type_def: bool,
    pub(super) pinned_by_import: bool,
    pub(super) snippet: String,
}

#[cfg(feature = "tree-sitter")]
#[derive(Clone, Debug)]
pub(super) struct FileRefQuery {
    pub(super) file_path: PathBuf,
    pub(super) lang: SupportedLanguage,
    /// Type / PascalCase / constant identifiers referenced by changed hunks (high priority).
    pub(super) type_or_const_refs: HashSet<String>,
    /// Qualified call/path targets `(qualifier, symbol)` e.g. `("signature", "extract_signature")`.
    pub(super) qualified_refs: Vec<(String, String)>,
    /// Bare function call identifiers referenced by changed hunks (lower priority).
    pub(super) call_refs: HashSet<String>,
    /// Extracted import / use / include / require specifiers from the file.
    pub(super) import_specs: Vec<String>,
    /// Explicit imported symbol -> hint specifier (e.g. `use crate::foo::Bar;` -> `Bar` -> `crate::foo::Bar`).
    pub(super) imported_symbols: HashMap<String, Vec<String>>,
}

#[cfg(feature = "tree-sitter")]
const MAX_CROSS_FILE_CANDIDATES: usize = 24;
#[cfg(feature = "tree-sitter")]
const MAX_CROSS_FILE_WALL_MS: u64 = 400;
#[cfg(feature = "tree-sitter")]
const MAX_TYPE_SNIPPET_LINES: usize = 35;

/// Ubiquitous stdlib / language-builtin / overly generic identifiers to skip in Layer 3 matching.
#[cfg(feature = "tree-sitter")]
fn is_noise_or_builtin_ident(name: &str) -> bool {
    if name.len() < 3 {
        return true;
    }
    matches!(
        name,
        // Rust / common types & traits
        "Self" | "Option" | "Result" | "Some" | "None" | "Ok" | "Err"
            | "String" | "Vec" | "Box" | "Arc" | "Rc" | "Cell" | "RefCell"
            | "Mutex" | "RwLock" | "Cow" | "Pin" | "Future" | "Iterator"
            | "HashMap" | "HashSet" | "BTreeMap" | "BTreeSet" | "VecDeque"
            | "Path" | "PathBuf" | "OsStr" | "OsString" | "Command" | "File"
            | "Duration" | "Instant" | "Error" | "Debug" | "Display" | "Clone"
            | "Copy" | "Default" | "PartialEq" | "Eq" | "PartialOrd" | "Ord"
            | "Hash" | "Send" | "Sync" | "Sized" | "Drop" | "From" | "Into"
            | "TryFrom" | "TryInto" | "AsRef" | "AsMut" | "Deref" | "DerefMut"
            | "Fn" | "FnMut" | "FnOnce" | "PhantomData" | "Ordering"
            | "bool" | "char" | "str" | "usize" | "isize"
            | "u8" | "u16" | "u32" | "u64" | "u128"
            | "i8" | "i16" | "i32" | "i64" | "i128" | "f32" | "f64"
            // Python / JS / TS / Java / C# / Go / C++ / Ruby builtins
            | "int" | "float" | "void" | "auto" | "const" | "let" | "var"
            | "true" | "false" | "null" | "nil" | "True" | "False" | "undefined"
            | "any" | "unknown" | "never" | "object" | "number" | "string" | "boolean"
            | "dict" | "list" | "set" | "tuple" | "bytes" | "bytearray"
            | "Array" | "Object" | "Map" | "Set" | "Promise" | "Date" | "RegExp"
            | "Record" | "Partial" | "Required" | "Readonly" | "Pick" | "Omit"
            | "Function" | "Symbol" | "Number" | "Boolean" | "BigInt" | "Buffer"
            | "List" | "Dict" | "Tuple" | "Optional" | "Union" | "Any" | "Callable"
            | "Iterable" | "Sequence" | "Mapping" | "Protocol" | "TypeVar" | "ClassVar"
            | "Integer" | "Long" | "Double" | "Float" | "Short" | "Byte" | "Character"
            | "Collection" | "ArrayList" | "LinkedList" | "ConcurrentHashMap"
            | "Task" | "ValueTask" | "IEnumerable" | "IList" | "IDictionary" | "Action" | "Func"
            | "Exception" | "RuntimeException" | "IllegalArgumentException" | "IOException"
            | "ValueError" | "TypeError" | "KeyError" | "RuntimeError" | "AttributeError"
            | "error" | "byte" | "rune" | "uintptr" | "complex64" | "complex128"
            | "size_t" | "ssize_t" | "uint8_t" | "uint16_t" | "uint32_t" | "uint64_t"
            | "int8_t" | "int16_t" | "int32_t" | "int64_t" | "nullptr" | "std"
            // Ubiquitous generic method / function / iterator names
            | "new" | "default" | "from" | "into" | "clone" | "fmt" | "drop"
            | "get" | "len" | "is_empty" | "push" | "pop" | "clear"
            | "iter" | "into_iter" | "iter_mut" | "collect" | "map" | "filter" | "find"
            | "all" | "next" | "count" | "take" | "skip" | "retain" | "dedup"
            | "truncate" | "flat_map" | "filter_map" | "and_then" | "or_else" | "map_err"
            | "ok_or" | "ok_or_else" | "copied" | "cloned" | "rev" | "chain" | "fold"
            | "position" | "rposition" | "max_by_key" | "min_by_key" | "sort_by" | "sort_by_key"
            | "sort_unstable" | "entry" | "or_default" | "or_insert" | "or_insert_with"
            | "is_some" | "is_none" | "is_ok" | "is_err" | "is_some_and" | "is_ok_and"
            | "unwrap" | "expect" | "unwrap_or" | "unwrap_or_else" | "unwrap_or_default"
            | "ok" | "err" | "as_ref" | "as_mut" | "as_deref" | "as_str" | "as_bytes"
            | "as_slice" | "to_string" | "to_owned" | "into_owned" | "to_vec"
            | "trim" | "trim_start" | "trim_end" | "trim_matches" | "trim_start_matches"
            | "trim_end_matches" | "strip_prefix" | "strip_suffix"
            | "split" | "split_once" | "rsplit" | "rsplit_once" | "split_whitespace"
            | "split_inclusive" | "lines" | "chars" | "char_indices"
            | "join" | "contains" | "starts_with" | "ends_with" | "replace"
            | "saturating_sub" | "saturating_add" | "saturating_mul"
            | "format" | "println" | "eprintln" | "writeln" | "eprint"
            | "write" | "read" | "open" | "close" | "flush" | "lock" | "unlock"
            | "run" | "init" | "main" | "start" | "stop" | "build" | "create" | "delete"
            | "update" | "insert" | "remove" | "append" | "extend" | "sort" | "reverse"
            | "print" | "printf" | "fprintf" | "sprintf" | "snprintf" | "malloc" | "free"
            | "assert" | "assert_eq" | "assert_ne" | "panic" | "todo" | "unreachable"
            | "repr" | "range" | "enumerate" | "zip" | "isinstance"
            | "hasattr" | "getattr" | "setattr" | "super" | "type" | "min" | "max" | "sum"
            | "log" | "info" | "warn" | "debug" | "trace" | "keys" | "values" | "items"
            | "entries" | "forEach" | "reduce" | "slice" | "splice" | "indexOf" | "includes"
            | "stringify" | "parse" | "resolve" | "reject" | "then" | "catch" | "finally"
            | "equals" | "hashCode" | "toString" | "compareTo" | "size" | "isEmpty"
            | "add" | "put" | "containsKey" | "stream" | "of" | "valueOf"
            | "Context" | "Config" | "State" | "Options" | "Params"
            | "Make" | "make" | "cap" | "copy" | "recover" | "defer"
    )
}

/// Checks whether a token is a valid identifier (rejecting punctuation fragments like `")"`
/// from chained calls `foo.iter().all(...)`).
#[cfg(feature = "tree-sitter")]
fn is_valid_ident(s: &str) -> bool {
    let bytes = s.as_bytes();
    !bytes.is_empty()
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'?' || b == b'!')
}

#[cfg(feature = "tree-sitter")]
fn looks_like_type_or_const(name: &str) -> bool {
    if name.len() < 3 {
        return false;
    }
    let first = name.chars().next().unwrap_or('_');
    if first.is_ascii_uppercase() {
        return true;
    }
    false
}

/// Checks if a primary definition AST node is a type/constant/interface definition suitable for
/// Layer 3-① (same-file unmodified context) and Layer 3-② (cross-file type definitions).
#[cfg(feature = "tree-sitter")]
fn is_type_or_const_def_node(lang: SupportedLanguage, node: &tree_sitter::Node) -> bool {
    let target = match node.kind() {
        "decorated_definition" => node.child_by_field_name("definition").unwrap_or(*node),
        "export_statement" => node.child_by_field_name("declaration").unwrap_or(*node),
        "template_declaration" => {
            let mut found = *node;
            let mut cursor = node.walk();
            for ch in node.named_children(&mut cursor) {
                if check_primary_definition(lang, &ch).is_some() {
                    found = ch;
                    break;
                }
            }
            found
        }
        _ => *node,
    };
    let kind = target.kind();
    match lang {
        SupportedLanguage::Rust => matches!(
            kind,
            "struct_item" | "enum_item" | "trait_item" | "type_item" | "const_item" | "static_item"
        ),
        SupportedLanguage::Python => kind == "class_definition",
        SupportedLanguage::JavaScript | SupportedLanguage::TypeScript | SupportedLanguage::Tsx => {
            matches!(
                kind,
                "interface_declaration"
                    | "type_alias_declaration"
                    | "enum_declaration"
                    | "class_declaration"
                    | "abstract_class_declaration"
            )
        }
        SupportedLanguage::Go => matches!(kind, "type_declaration" | "type_spec"),
        SupportedLanguage::C | SupportedLanguage::Cpp => matches!(
            kind,
            "struct_specifier"
                | "union_specifier"
                | "class_specifier"
                | "enum_specifier"
                | "type_definition"
        ),
        SupportedLanguage::Java => matches!(
            kind,
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "annotation_type_declaration"
        ),
        SupportedLanguage::CSharp => matches!(
            kind,
            "class_declaration"
                | "struct_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
        ),
        SupportedLanguage::Ruby => matches!(kind, "class" | "module"),
        SupportedLanguage::Bash | SupportedLanguage::Json => false,
    }
}

/// Checks whether a node represents a class-like container whose method bodies should be stripped
/// when rendered in Layer 3 (keeping fields, properties, and method signatures).
#[cfg(feature = "tree-sitter")]
fn is_class_like_type_node(node: &tree_sitter::Node) -> bool {
    let target = match node.kind() {
        "decorated_definition" => node.child_by_field_name("definition").unwrap_or(*node),
        "export_statement" => node.child_by_field_name("declaration").unwrap_or(*node),
        _ => *node,
    };
    matches!(
        target.kind(),
        "class_definition"
            | "class_declaration"
            | "abstract_class_declaration"
            | "class_specifier"
            | "class"
    )
}

/// Returns true if a comment text line is a pure divider or linter directive rather than semantic docs.
#[cfg(feature = "tree-sitter")]
fn is_noise_comment_line(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return true;
    }
    if t.chars()
        .all(|c| matches!(c, '-' | '=' | '*' | '_' | '/' | '#' | '~' | ' '))
    {
        return true;
    }
    let lower = t.to_ascii_lowercase();
    lower.starts_with("eslint-")
        || lower.starts_with("@ts-")
        || lower.starts_with("nolint")
        || lower.starts_with("rubocop:")
        || lower.starts_with("frozen_string_literal:")
        || lower.starts_with("noqa")
        || lower.starts_with("pylint:")
        || lower.starts_with("type: ignore")
        || lower.starts_with("fmt:")
        || lower.starts_with("prettier-ignore")
        || lower.starts_with("clang-format")
}

/// Cleans common C#/JavaDoc/JSDoc XML/tag noise from a single doc comment line.
#[cfg(feature = "tree-sitter")]
fn clean_doc_line(raw: &str) -> String {
    let s = raw
        .replace("<summary>", "")
        .replace("</summary>", "")
        .replace("<remarks>", "")
        .replace("</remarks>", "")
        .replace("<returns>", "returns: ")
        .replace("</returns>", "");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Strips a single-line comment marker for the given language, returning the comment body.
#[cfg(feature = "tree-sitter")]
fn strip_line_doc_prefix(lang: SupportedLanguage, trimmed: &str) -> Option<&str> {
    match lang {
        SupportedLanguage::Python | SupportedLanguage::Ruby => {
            trimmed.strip_prefix('#').map(str::trim)
        }
        SupportedLanguage::Rust | SupportedLanguage::CSharp => trimmed
            .strip_prefix("///")
            .or_else(|| trimmed.strip_prefix("//"))
            .map(str::trim),
        SupportedLanguage::Go
        | SupportedLanguage::JavaScript
        | SupportedLanguage::TypeScript
        | SupportedLanguage::Tsx
        | SupportedLanguage::Java
        | SupportedLanguage::C
        | SupportedLanguage::Cpp => trimmed
            .strip_prefix("///")
            .or_else(|| trimmed.strip_prefix("//"))
            .map(str::trim),
        SupportedLanguage::Bash | SupportedLanguage::Json => None,
    }
}

/// Finds an adjusted start row that includes up to `max_doc_lines` contiguous leading doc-comment
/// lines immediately above `raw_s_row` (allowing up to 2 adjacent Rust `#[...]` attribute lines).
#[cfg(feature = "tree-sitter")]
fn leading_doc_comment_start_row(
    lang: SupportedLanguage,
    lines: &[&str],
    raw_s_row: usize,
    max_doc_lines: usize,
) -> usize {
    if raw_s_row == 0 || max_doc_lines == 0 {
        return raw_s_row;
    }
    let mut probe = raw_s_row;
    // Allow stepping over up to 2 immediately adjacent attribute lines (e.g. `#[derive(...)]`)
    let mut skipped_attrs = 0usize;
    while probe > 0 && skipped_attrs < 2 {
        let prev = lines[probe - 1].trim();
        if prev.starts_with("#[") && prev.ends_with(']') {
            probe -= 1;
            skipped_attrs += 1;
        } else {
            break;
        }
    }
    let mut doc_start = probe;
    let mut doc_count = 0usize;
    while doc_start > 0 && doc_count < max_doc_lines {
        let prev = lines[doc_start - 1].trim();
        let Some(body) = strip_line_doc_prefix(lang, prev) else {
            break;
        };
        if is_noise_comment_line(body) {
            break;
        }
        doc_start -= 1;
        doc_count += 1;
    }
    if doc_count > 0 {
        doc_start
    } else {
        raw_s_row
    }
}

/// Extracts a concise (<= 160 bytes) semantic doc summary for a function/method definition:
/// - Python: first-statement body docstring (`"""..."""` / `'''...'''`), falling back to `#` comments
/// - Rust/C#/Go/TS/JS/Java/C/C++/Ruby: immediately preceding `///`, `//`, `#`, or `/** ... */` doc comment
#[cfg(feature = "tree-sitter")]
fn extract_short_doc_comment(
    lang: SupportedLanguage,
    node: &tree_sitter::Node,
    lines: &[&str],
    source: &str,
) -> Option<String> {
    const MAX_DOC_BYTES: usize = 160;
    let finalize = |parts: Vec<String>| -> Option<String> {
        let joined = parts.join(" ");
        let normalized = joined.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.len() < 4 {
            return None;
        }
        if normalized.len() <= MAX_DOC_BYTES {
            Some(normalized)
        } else {
            let cut = truncate_utf8_bytes(&normalized, MAX_DOC_BYTES.saturating_sub(3)).trim_end();
            Some(format!("{cut}..."))
        }
    };

    // 1) Python inline docstring (first statement of function body)
    if lang == SupportedLanguage::Python {
        let target = if node.kind() == "decorated_definition" {
            node.child_by_field_name("definition").unwrap_or(*node)
        } else {
            *node
        };
        if let Some(body) = signature::find_ast_body_node(lang, &target) {
            if let Some(first_stmt) = body.named_child(0) {
                if let Ok(raw_stmt) = first_stmt.utf8_text(source.as_bytes()) {
                    let trimmed = raw_stmt.trim();
                    let unprefixed = trimmed
                        .strip_prefix("r\"\"\"")
                        .or_else(|| trimmed.strip_prefix("u\"\"\""))
                        .or_else(|| trimmed.strip_prefix("\"\"\""))
                        .and_then(|s| s.strip_suffix("\"\"\""))
                        .or_else(|| {
                            trimmed
                                .strip_prefix("r'''")
                                .or_else(|| trimmed.strip_prefix("u'''"))
                                .or_else(|| trimmed.strip_prefix("'''"))
                                .and_then(|s| s.strip_suffix("'''"))
                        })
                        .or_else(|| {
                            trimmed
                                .strip_prefix('"')
                                .and_then(|s| s.strip_suffix('"'))
                                .filter(|s| !s.contains('\n'))
                        })
                        .or_else(|| {
                            trimmed
                                .strip_prefix('\'')
                                .and_then(|s| s.strip_suffix('\''))
                                .filter(|s| !s.contains('\n'))
                        });
                    if let Some(doc_inner) = unprefixed {
                        let doc_lines: Vec<String> = doc_inner
                            .lines()
                            .map(str::trim)
                            .filter(|l| !l.is_empty())
                            .take(2)
                            .map(str::to_string)
                            .collect();
                        if let Some(res) = finalize(doc_lines) {
                            return Some(res);
                        }
                    }
                }
            }
        }
    }

    // 2) Immediately preceding leading doc comment (skipping up to 3 adjacent attributes/decorators)
    let mut probe = node.start_position().row;
    let mut skipped_attrs = 0usize;
    while probe > 0 && skipped_attrs < 3 {
        let prev = lines[probe - 1].trim();
        let is_attr = (prev.starts_with("#[") && prev.ends_with(']'))
            || prev.starts_with('@')
            || (lang == SupportedLanguage::CSharp && prev.starts_with('[') && prev.ends_with(']'));
        if is_attr {
            probe -= 1;
            skipped_attrs += 1;
        } else {
            break;
        }
    }
    if probe == 0 {
        return None;
    }

    let above = lines[probe - 1].trim();
    // 2a) Block doc comment (`/** ... */` or `/* ... */`) ending right above the symbol
    if above.ends_with("*/") && !matches!(lang, SupportedLanguage::Python | SupportedLanguage::Ruby)
    {
        let mut start = probe - 1;
        let mut found_open = false;
        let limit = probe.saturating_sub(12);
        loop {
            let t = lines[start].trim();
            if t.starts_with("/*") {
                found_open = true;
                break;
            }
            if start == limit || start == 0 {
                break;
            }
            start -= 1;
        }
        if found_open {
            let mut summary: Vec<String> = Vec::new();
            let mut return_note: Option<String> = None;
            for raw_line in &lines[start..probe] {
                let cleaned_raw = raw_line
                    .trim()
                    .trim_start_matches("/**")
                    .trim_start_matches("/*")
                    .trim_end_matches("*/")
                    .trim_start_matches('*')
                    .trim();
                if is_noise_comment_line(cleaned_raw) {
                    continue;
                }
                if cleaned_raw.starts_with("@return") || cleaned_raw.starts_with("@returns") {
                    if return_note.is_none() {
                        return_note = Some(clean_doc_line(cleaned_raw));
                    }
                    continue;
                }
                if cleaned_raw.starts_with('@') {
                    continue;
                }
                let cleaned = clean_doc_line(cleaned_raw);
                if !cleaned.is_empty() && summary.len() < 2 {
                    summary.push(cleaned);
                }
            }
            if let Some(ret) = return_note {
                if summary.len() < 2 {
                    summary.push(ret);
                }
            }
            return finalize(summary);
        }
    }

    // 2b) Contiguous single-line doc comments (`///`, `//`, `#`)
    let mut collected_rev: Vec<String> = Vec::new();
    let mut row = probe;
    let mut scanned = 0usize;
    while row > 0 && scanned < 64 {
        let prev = lines[row - 1].trim();
        let Some(body) = strip_line_doc_prefix(lang, prev) else {
            break;
        };
        row -= 1;
        scanned += 1;
        if is_noise_comment_line(body) {
            continue;
        }
        let cleaned = clean_doc_line(body);
        if !cleaned.is_empty() {
            collected_rev.push(cleaned);
        }
    }
    if collected_rev.is_empty() {
        return None;
    }
    collected_rev.reverse();
    collected_rev.truncate(2);
    finalize(collected_rev)
}

/// Renders a type/const definition snippet (capped at `MAX_TYPE_SNIPPET_LINES`), returning
/// `(actual_1based_start_line, snippet)` and stripping method bodies for large class definitions.
#[cfg(feature = "tree-sitter")]
fn render_type_def_snippet(
    lang: SupportedLanguage,
    node: &tree_sitter::Node,
    lines: &[&str],
    source: &str,
) -> Option<(usize, String)> {
    let raw_s_row = node.start_position().row;
    let s_row = leading_doc_comment_start_row(lang, lines, raw_s_row, 2);
    let e_row = node.end_position().row.min(lines.len().saturating_sub(1));
    if s_row > e_row {
        return None;
    }
    let actual_start_line = s_row + 1;
    let span = e_row - s_row + 1;

    // For large classes (> 18 lines), strip method bodies to show class header + fields + method signatures
    if is_class_like_type_node(node) && span > 18 {
        let mut fold_ranges: Vec<(usize, usize)> = Vec::new();
        fn collect_method_bodies(
            lang: SupportedLanguage,
            n: tree_sitter::Node,
            out: &mut Vec<(usize, usize)>,
        ) {
            if ast_compact::is_foldable_function_node(lang, &n) {
                if let Some(body) = signature::find_ast_body_node(lang, &n) {
                    let bs = body.start_position().row;
                    let be = body.end_position().row;
                    if be > bs {
                        out.push((bs, be));
                    }
                }
                return;
            }
            let mut cursor = n.walk();
            for ch in n.named_children(&mut cursor) {
                collect_method_bodies(lang, ch, out);
            }
        }
        collect_method_bodies(lang, *node, &mut fold_ranges);
        fold_ranges.sort_unstable();

        let mut out_lines = Vec::new();
        let mut row = s_row;
        while row <= e_row && out_lines.len() < MAX_TYPE_SNIPPET_LINES {
            if let Some(&(bs, be)) = fold_ranges.iter().find(|&&(bs, be)| row >= bs && row <= be) {
                if row == bs {
                    let line = lines[row];
                    if let Some(brace_pos) = line.find('{') {
                        out_lines.push(format!("{} {{ ... }}", line[..brace_pos].trim_end()));
                    } else if lang == SupportedLanguage::Python || lang == SupportedLanguage::Ruby {
                        let indent = &line[..line.len() - line.trim_start().len()];
                        out_lines.push(format!("{indent}..."));
                    } else {
                        out_lines.push(format!("{} ...", line.trim_end()));
                    }
                }
                row = be + 1;
                continue;
            }
            out_lines.push(lines[row].to_string());
            row += 1;
        }
        if row <= e_row {
            out_lines.push(format!("// ... (+{} lines)", e_row - row + 1));
        }
        return Some((actual_start_line, out_lines.join("\n")));
    }

    if span <= MAX_TYPE_SNIPPET_LINES {
        let _ = source;
        return Some((actual_start_line, lines[s_row..=e_row].join("\n")));
    }

    let mut out_lines: Vec<String> = lines[s_row..s_row + MAX_TYPE_SNIPPET_LINES - 1]
        .iter()
        .map(|l| (*l).to_string())
        .collect();
    out_lines.push(format!(
        "// ... (+{} lines)",
        span - (MAX_TYPE_SNIPPET_LINES - 1)
    ));
    Some((actual_start_line, out_lines.join("\n")))
}

#[cfg(feature = "tree-sitter")]
fn derive_js_module_stem(spec: &str) -> Option<String> {
    let clean = spec
        .split(['?', '#'])
        .next()
        .unwrap_or(spec)
        .trim_end_matches('/');
    let mut stripped = clean;
    for ext in [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"] {
        if let Some(s) = stripped.strip_suffix(ext) {
            stripped = s;
            break;
        }
    }
    stripped = stripped.strip_suffix("/index").unwrap_or(stripped);
    let last = stripped.rsplit('/').next().unwrap_or(stripped).trim();
    if last.is_empty() || last == "." || last == ".." {
        None
    } else {
        Some(last.to_string())
    }
}

/// Extracts imports (`import_specs`, `imported_symbols`, and `alias_map`) from a file's AST across all 8 languages.
#[cfg(feature = "tree-sitter")]
fn collect_file_imports(
    lang: SupportedLanguage,
    root: &tree_sitter::Node,
    source: &str,
    import_specs: &mut Vec<String>,
    imported_symbols: &mut HashMap<String, Vec<String>>,
    alias_map: &mut HashMap<String, String>,
) {
    fn is_valid_import_alias(s: &str) -> bool {
        is_valid_ident(s)
            && s.len() >= 2
            && !matches!(
                s,
                "of" | "in"
                    | "to"
                    | "as"
                    | "if"
                    | "do"
                    | "or"
                    | "on"
                    | "by"
                    | "at"
                    | "is"
                    | "ok"
                    | "id"
                    | "fn"
            )
    }
    fn record_import_entry(
        spec: String,
        sym: Option<String>,
        allow_short_alias: bool,
        import_specs: &mut Vec<String>,
        imported_symbols: &mut HashMap<String, Vec<String>>,
    ) -> bool {
        let clean = spec.trim().to_string();
        if clean.is_empty() {
            return false;
        }
        match sym {
            None => {
                if !import_specs.contains(&clean) {
                    import_specs.push(clean);
                }
                true
            }
            Some(s) => {
                let s_clean = s.trim().to_string();
                if s_clean == "*" {
                    if !import_specs.contains(&clean) {
                        import_specs.push(clean);
                    }
                    return true;
                }
                let sym_ok = if allow_short_alias {
                    is_valid_import_alias(&s_clean)
                } else {
                    !s_clean.is_empty()
                        && is_valid_ident(&s_clean)
                        && !is_noise_or_builtin_ident(&s_clean)
                };
                if sym_ok {
                    if !import_specs.contains(&clean) {
                        import_specs.push(clean.clone());
                    }
                    let list = imported_symbols.entry(s_clean).or_default();
                    if !list.contains(&clean) {
                        list.push(clean);
                    }
                    true
                } else {
                    false
                }
            }
        }
    }

    let mut cursor = root.walk();
    // Inspect top-level children (and one level inside Rust `mod` / C# `namespace`)
    let mut stack: Vec<tree_sitter::Node> = root.named_children(&mut cursor).collect();
    let mut visited = 0usize;
    while let Some(node) = stack.pop() {
        visited += 1;
        if visited > 600 {
            break;
        }
        let kind = node.kind();
        let text = source
            .get(node.start_byte()..node.end_byte())
            .unwrap_or("")
            .trim();
        if text.is_empty() || text.len() > 2048 {
            continue;
        }

        match lang {
            SupportedLanguage::Rust => match kind {
                "use_declaration" => {
                    let body = text
                        .trim_start_matches("pub(crate)")
                        .trim_start_matches("pub(super)")
                        .trim_start_matches("pub")
                        .trim()
                        .trim_start_matches("use ")
                        .trim_end_matches(';')
                        .trim();
                    let mut rust_rec = |spec: String, sym: Option<String>| {
                        record_import_entry(spec, sym, false, import_specs, imported_symbols);
                    };
                    expand_rust_use_tree(body, &mut rust_rec, alias_map);
                }
                "mod_item"
                    // External module declaration `mod foo;` (no body)
                    if node.child_by_field_name("body").is_none() =>
                {
                    if let Some(name_node) = node.child_by_field_name("name") {
                        if let Some(mod_name) =
                            source.get(name_node.start_byte()..name_node.end_byte())
                        {
                            record_import_entry(
                                format!("mod::{mod_name}"),
                                None,
                                false,
                                import_specs,
                                imported_symbols,
                            );
                        }
                    }
                }
                _ => {}
            },
            SupportedLanguage::Python => match kind {
                "import_from_statement" => {
                    // `from pkg.mod import A, B as C` or `from pkg import (\n A, # note\n B\n)`
                    if let Some((from_part, imp_part)) = text.split_once(" import ") {
                        let module = from_part.trim_start_matches("from ").trim();
                        let no_comments: String = imp_part
                            .lines()
                            .map(|l| l.split('#').next().unwrap_or(""))
                            .collect::<Vec<_>>()
                            .join(" ");
                        let cleaned = no_comments.trim().trim_matches(['(', ')']);
                        for item in cleaned.split(',') {
                            let item_t = item.trim();
                            if item_t.is_empty() {
                                continue;
                            }
                            if item_t == "*" {
                                record_import_entry(
                                    module.to_string(),
                                    Some("*".to_string()),
                                    false,
                                    import_specs,
                                    imported_symbols,
                                );
                                continue;
                            }
                            let (orig, alias_opt) = match item_t.split_once(" as ") {
                                Some((o, a)) => (o.trim(), Some(a.trim())),
                                None => (item_t.split_whitespace().next().unwrap_or(""), None),
                            };
                            if !orig.is_empty() {
                                record_import_entry(
                                    module.to_string(),
                                    Some(orig.to_string()),
                                    false,
                                    import_specs,
                                    imported_symbols,
                                );
                                if let Some(al) = alias_opt.filter(|a| !a.is_empty()) {
                                    record_import_entry(
                                        module.to_string(),
                                        Some(al.to_string()),
                                        true,
                                        import_specs,
                                        imported_symbols,
                                    );
                                    alias_map.insert(al.to_string(), orig.to_string());
                                }
                            }
                        }
                    }
                }
                "import_statement" => {
                    let no_comments: String = text
                        .lines()
                        .map(|l| l.split('#').next().unwrap_or(""))
                        .collect::<Vec<_>>()
                        .join(" ");
                    if let Some(rest) = no_comments.trim().strip_prefix("import ") {
                        for item in rest.split(',') {
                            let item_t = item.trim();
                            if item_t.is_empty() {
                                continue;
                            }
                            let (mod_path, alias_opt) = match item_t.split_once(" as ") {
                                Some((m, a)) => (m.trim(), Some(a.trim())),
                                None => (item_t.split_whitespace().next().unwrap_or(""), None),
                            };
                            if !mod_path.is_empty() {
                                let last = mod_path.rsplit('.').next().unwrap_or(mod_path);
                                record_import_entry(
                                    mod_path.to_string(),
                                    Some(last.to_string()),
                                    false,
                                    import_specs,
                                    imported_symbols,
                                );
                                if let Some(al) = alias_opt.filter(|a| !a.is_empty()) {
                                    record_import_entry(
                                        mod_path.to_string(),
                                        Some(al.to_string()),
                                        true,
                                        import_specs,
                                        imported_symbols,
                                    );
                                    alias_map.insert(al.to_string(), last.to_string());
                                }
                            }
                        }
                    }
                }
                _ => {}
            },
            SupportedLanguage::JavaScript
            | SupportedLanguage::TypeScript
            | SupportedLanguage::Tsx => {
                if kind == "import_statement" {
                    if let Some(src_node) = node.child_by_field_name("source") {
                        if let Some(raw_src) =
                            source.get(src_node.start_byte()..src_node.end_byte())
                        {
                            let spec = raw_src.trim().trim_matches(['"', '\'', '`']).to_string();
                            let rel_src_start = src_node
                                .start_byte()
                                .saturating_sub(node.start_byte())
                                .min(text.len());
                            let before_src = text[..rel_src_start].trim();
                            let clause_raw = before_src
                                .strip_prefix("import")
                                .unwrap_or(before_src)
                                .trim();
                            let clause = clause_raw
                                .strip_suffix("from")
                                .map(str::trim_end)
                                .unwrap_or(clause_raw);
                            let mut recorded_any = false;

                            // 1) Named imports `{ A, B as C }`
                            if let Some(brace_start) = clause.find('{') {
                                if let Some(brace_end) = clause[brace_start..].find('}') {
                                    let inner = &clause[brace_start + 1..brace_start + brace_end];
                                    for part in inner.split(',') {
                                        let p = part.trim().trim_start_matches("type ").trim();
                                        let (orig, alias_opt) = match p.split_once(" as ") {
                                            Some((o, a)) => (o.trim(), Some(a.trim())),
                                            None => {
                                                (p.split_whitespace().next().unwrap_or(""), None)
                                            }
                                        };
                                        if !orig.is_empty() {
                                            record_import_entry(
                                                spec.clone(),
                                                Some(orig.to_string()),
                                                false,
                                                import_specs,
                                                imported_symbols,
                                            );
                                            recorded_any = true;
                                            if let Some(al) = alias_opt.filter(|a| !a.is_empty()) {
                                                record_import_entry(
                                                    spec.clone(),
                                                    Some(al.to_string()),
                                                    true,
                                                    import_specs,
                                                    imported_symbols,
                                                );
                                                alias_map.insert(al.to_string(), orig.to_string());
                                            }
                                        }
                                    }
                                }
                            }

                            // 2) Default and/or namespace import before `{` (supports `import Default, { Named }` and `import * as ns`)
                            let head_part = match clause.find('{') {
                                Some(b) => clause[..b].trim(),
                                None => clause.trim(),
                            };
                            let head_no_type = head_part.strip_prefix("type ").unwrap_or(head_part);
                            let head_spaced = head_no_type.replace(',', " , ");
                            let toks: Vec<&str> = head_spaced.split_whitespace().collect();
                            let mut idx = 0usize;
                            while idx < toks.len() {
                                let t = toks[idx];
                                if t == "," {
                                    idx += 1;
                                    continue;
                                }
                                if t == "*"
                                    && idx + 2 < toks.len()
                                    && toks[idx + 1] == "as"
                                    && is_valid_ident(toks[idx + 2])
                                {
                                    let ns = toks[idx + 2];
                                    record_import_entry(
                                        spec.clone(),
                                        Some(ns.to_string()),
                                        true,
                                        import_specs,
                                        imported_symbols,
                                    );
                                    recorded_any = true;
                                    if (spec.starts_with('.')
                                        || spec.starts_with("@/")
                                        || spec.starts_with("~/"))
                                        && is_valid_import_alias(ns)
                                    {
                                        if let Some(stem) = derive_js_module_stem(&spec) {
                                            if stem.chars().any(|c| c.is_ascii_alphanumeric())
                                                && ns != stem
                                            {
                                                alias_map.insert(ns.to_string(), stem);
                                            }
                                        }
                                    }
                                    idx += 3;
                                    continue;
                                }
                                if t != "*" && t != "as" && is_valid_ident(t) {
                                    record_import_entry(
                                        spec.clone(),
                                        Some(t.to_string()),
                                        false,
                                        import_specs,
                                        imported_symbols,
                                    );
                                    recorded_any = true;
                                }
                                idx += 1;
                            }

                            if !recorded_any {
                                record_import_entry(
                                    spec,
                                    None,
                                    false,
                                    import_specs,
                                    imported_symbols,
                                );
                            }
                        }
                    }
                } else if matches!(
                    kind,
                    "lexical_declaration" | "variable_declaration" | "expression_statement"
                ) && text.contains("require(")
                {
                    if let Some(req_pos) = text.find("require(") {
                        let after_req = text[req_pos + "require(".len()..].trim_start();
                        if let Some(&quote) = after_req
                            .as_bytes()
                            .first()
                            .filter(|&&b| b == b'\'' || b == b'"' || b == b'`')
                        {
                            let qch = quote as char;
                            if let Some(end_q) = after_req[1..].find(qch) {
                                let spec = after_req[1..1 + end_q].trim().to_string();
                                let lhs = text[..req_pos].trim();
                                let mut recorded_any = false;
                                if let (Some(bs), Some(be)) = (lhs.find('{'), lhs.find('}')) {
                                    if bs < be {
                                        for part in lhs[bs + 1..be].split(',') {
                                            let p = part.trim();
                                            let (orig, alias_opt) = match p.split_once(':') {
                                                Some((o, a)) => (o.trim(), Some(a.trim())),
                                                None => {
                                                    (p.split('=').next().unwrap_or("").trim(), None)
                                                }
                                            };
                                            if !orig.is_empty() {
                                                record_import_entry(
                                                    spec.clone(),
                                                    Some(orig.to_string()),
                                                    false,
                                                    import_specs,
                                                    imported_symbols,
                                                );
                                                recorded_any = true;
                                                if let Some(al) =
                                                    alias_opt.filter(|a| !a.is_empty())
                                                {
                                                    record_import_entry(
                                                        spec.clone(),
                                                        Some(al.to_string()),
                                                        true,
                                                        import_specs,
                                                        imported_symbols,
                                                    );
                                                    alias_map
                                                        .insert(al.to_string(), orig.to_string());
                                                }
                                            }
                                        }
                                    }
                                } else if let Some(eq_pos) = lhs.find('=') {
                                    let var_name =
                                        lhs[..eq_pos].split_whitespace().last().unwrap_or("");
                                    if !var_name.is_empty() {
                                        record_import_entry(
                                            spec.clone(),
                                            Some(var_name.to_string()),
                                            true,
                                            import_specs,
                                            imported_symbols,
                                        );
                                        recorded_any = true;
                                        if (spec.starts_with('.')
                                            || spec.starts_with("@/")
                                            || spec.starts_with("~/"))
                                            && is_valid_import_alias(var_name)
                                        {
                                            if let Some(stem) = derive_js_module_stem(&spec) {
                                                if stem.chars().any(|c| c.is_ascii_alphanumeric())
                                                    && var_name != stem
                                                {
                                                    alias_map.insert(var_name.to_string(), stem);
                                                }
                                            }
                                        }
                                    }
                                }
                                if !recorded_any {
                                    record_import_entry(
                                        spec,
                                        None,
                                        false,
                                        import_specs,
                                        imported_symbols,
                                    );
                                }
                            }
                        }
                    }
                }
            }
            SupportedLanguage::Go => {
                if kind == "import_declaration" {
                    for line in text.lines() {
                        if let Some(q1) = line.find('"') {
                            if let Some(q2) = line[q1 + 1..].find('"') {
                                let pkg = &line[q1 + 1..q1 + 1 + q2];
                                let pkg_name = pkg.rsplit('/').next().unwrap_or(pkg);
                                let before_q =
                                    line[..q1].trim().trim_start_matches("import").trim();
                                record_import_entry(
                                    pkg.to_string(),
                                    Some(pkg_name.to_string()),
                                    false,
                                    import_specs,
                                    imported_symbols,
                                );
                                if !before_q.is_empty() && before_q != "(" && before_q != "_" {
                                    if before_q == "." {
                                        record_import_entry(
                                            pkg.to_string(),
                                            Some("*".to_string()),
                                            false,
                                            import_specs,
                                            imported_symbols,
                                        );
                                    } else if is_valid_ident(before_q) {
                                        record_import_entry(
                                            pkg.to_string(),
                                            Some(before_q.to_string()),
                                            true,
                                            import_specs,
                                            imported_symbols,
                                        );
                                        alias_map
                                            .insert(before_q.to_string(), pkg_name.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            SupportedLanguage::Java => {
                if kind == "import_declaration" {
                    let body = text
                        .trim_start_matches("import ")
                        .trim_start_matches("static ")
                        .trim_end_matches(';')
                        .trim();
                    if body.starts_with("java.")
                        || body.starts_with("javax.")
                        || body.starts_with("jdk.")
                        || body.starts_with("org.junit.")
                    {
                        continue;
                    }
                    let sym = body.rsplit('.').next().map(str::to_string);
                    record_import_entry(
                        body.to_string(),
                        sym,
                        false,
                        import_specs,
                        imported_symbols,
                    );
                }
            }
            SupportedLanguage::CSharp => {
                if kind == "using_directive" {
                    let body = text
                        .trim_start_matches("global ")
                        .trim_start_matches("using ")
                        .trim_start_matches("static ")
                        .trim_end_matches(';')
                        .trim();
                    let target_ns = if let Some((al, rhs)) = body.split_once('=') {
                        let al_t = al.trim();
                        let rhs_t = rhs.trim();
                        let last = rhs_t.rsplit('.').next().unwrap_or(rhs_t);
                        if is_valid_ident(al_t) && is_valid_ident(last) {
                            alias_map.insert(al_t.to_string(), last.to_string());
                        }
                        rhs_t
                    } else {
                        body
                    };
                    if target_ns == "System"
                        || target_ns.starts_with("System.")
                        || target_ns == "Microsoft"
                        || target_ns.starts_with("Microsoft.")
                    {
                        continue;
                    }
                    record_import_entry(
                        target_ns.to_string(),
                        None,
                        false,
                        import_specs,
                        imported_symbols,
                    );
                } else if matches!(
                    kind,
                    "namespace_declaration" | "file_scoped_namespace_declaration"
                ) {
                    let mut c = node.walk();
                    for ch in node.named_children(&mut c) {
                        if ch.kind() == "using_directive" || ch.kind() == "declaration_list" {
                            stack.push(ch);
                        }
                    }
                } else if kind == "declaration_list" {
                    let mut c = node.walk();
                    for ch in node.named_children(&mut c) {
                        if ch.kind() == "using_directive" {
                            stack.push(ch);
                        }
                    }
                }
            }
            SupportedLanguage::C | SupportedLanguage::Cpp => {
                if kind == "preproc_include" {
                    if let Some(q1) = text.find('"') {
                        if let Some(q2) = text[q1 + 1..].find('"') {
                            record_import_entry(
                                text[q1 + 1..q1 + 1 + q2].to_string(),
                                None,
                                false,
                                import_specs,
                                imported_symbols,
                            );
                        }
                    } else if let Some(a1) = text.find('<') {
                        if let Some(a2) = text[a1 + 1..].find('>') {
                            let hdr = &text[a1 + 1..a1 + 1 + a2];
                            if hdr.contains('/') || hdr.ends_with(".h") || hdr.ends_with(".hpp") {
                                record_import_entry(
                                    hdr.to_string(),
                                    None,
                                    false,
                                    import_specs,
                                    imported_symbols,
                                );
                            }
                        }
                    }
                }
            }
            SupportedLanguage::Ruby => {
                if kind == "call" {
                    if let Some(m) = node.child_by_field_name("method") {
                        let m_name = source.get(m.start_byte()..m.end_byte()).unwrap_or("");
                        if matches!(m_name, "require" | "require_relative") {
                            for quote in ['\'', '"'] {
                                if let Some(q1) = text.find(quote) {
                                    if let Some(q2) = text[q1 + 1..].find(quote) {
                                        let prefix = if m_name == "require_relative" {
                                            "./"
                                        } else {
                                            ""
                                        };
                                        record_import_entry(
                                            format!("{prefix}{}", &text[q1 + 1..q1 + 1 + q2]),
                                            None,
                                            false,
                                            import_specs,
                                            imported_symbols,
                                        );
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            SupportedLanguage::Bash | SupportedLanguage::Json => {}
        }
    }
}

#[cfg(feature = "tree-sitter")]
fn expand_rust_use_tree(
    body: &str,
    record: &mut impl FnMut(String, Option<String>),
    alias_map: &mut HashMap<String, String>,
) {
    fn walk_tree(
        prefix: &str,
        rest: &str,
        record: &mut impl FnMut(String, Option<String>),
        alias_map: &mut HashMap<String, String>,
    ) {
        let r = rest.trim();
        if r.is_empty() {
            return;
        }
        if let Some(open) = r.find('{') {
            let head = r[..open].trim().trim_end_matches("::");
            let new_prefix = if prefix.is_empty() {
                head.to_string()
            } else if head.is_empty() {
                prefix.to_string()
            } else {
                format!("{prefix}::{head}")
            };
            if let Some(close) = r.rfind('}') {
                let inner = &r[open + 1..close];
                // Split by top-level commas respecting nested `{}`
                let mut depth = 0usize;
                let mut start = 0usize;
                for (i, ch) in inner.char_indices() {
                    match ch {
                        '{' => depth += 1,
                        '}' => depth = depth.saturating_sub(1),
                        ',' if depth == 0 => {
                            walk_tree(&new_prefix, &inner[start..i], record, alias_map);
                            start = i + 1;
                        }
                        _ => {}
                    }
                }
                walk_tree(&new_prefix, &inner[start..], record, alias_map);
            }
            return;
        }
        let (no_alias, alias_opt) = match r.split_once(" as ") {
            Some((orig, al)) => (orig.trim(), Some(al.trim())),
            None => (r, None),
        };
        let full = if no_alias == "self" {
            prefix.to_string()
        } else if prefix.is_empty() {
            no_alias.to_string()
        } else {
            format!("{prefix}::{no_alias}")
        };
        if full.starts_with("std::") || full.starts_with("core::") || full.starts_with("alloc::") {
            return;
        }
        let last = full.rsplit("::").next().unwrap_or("").to_string();
        if last == "*" {
            record(full.trim_end_matches("::*").to_string(), None);
        } else if !last.is_empty() {
            record(full.clone(), Some(last.clone()));
            if let Some(al) = alias_opt.filter(|a| !a.is_empty() && *a != "_") {
                record(full, Some(al.to_string()));
                alias_map.insert(al.to_string(), last);
            }
        }
    }
    walk_tree("", body, record, alias_map);
}

/// Splits a call target expression at its rightmost top-level separator (`"::"`, `"."`, or `"->"`),
/// ignoring delimiters inside `(...)`, `[...]`, `<...>`, strings/chars, and turbofish `::<`.
#[cfg(feature = "tree-sitter")]
fn split_rightmost_call_sep(lang: SupportedLanguage, raw: &str) -> Option<(&str, &str)> {
    let bytes = raw.as_bytes();
    let mut i = 0usize;
    let mut paren = 0usize;
    let mut bracket = 0usize;
    let mut angle = 0usize;
    let mut best: Option<(usize, usize)> = None;
    let allow_char_or_lifetime = matches!(
        lang,
        SupportedLanguage::Rust
            | SupportedLanguage::C
            | SupportedLanguage::Cpp
            | SupportedLanguage::Java
            | SupportedLanguage::CSharp
            | SupportedLanguage::Go
    );

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' || b == b'`' {
            let q = b;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                } else if bytes[i] == q {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if b == b'\'' {
            if allow_char_or_lifetime {
                if raw.is_char_boundary(i) {
                    if let Some(lit_len) = signature::char_literal_len(&raw[i..]) {
                        i = (i + lit_len).min(bytes.len());
                        continue;
                    }
                }
                i += 1;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                continue;
            } else {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i = (i + 2).min(bytes.len());
                    } else if bytes[i] == b'\'' {
                        i += 1;
                        break;
                    } else {
                        i += 1;
                    }
                }
                continue;
            }
        }

        if paren == 0 && bracket == 0 && angle == 0 {
            if i + 1 < bytes.len() && bytes[i] == b':' && bytes[i + 1] == b':' {
                // Skip turbofish `::<...>` so `<...>` is attached to the method/fn on the RHS
                if bytes.get(i + 2) == Some(&b'<') {
                    i += 2;
                    continue;
                }
                best = Some((i, i + 2));
                i += 2;
                continue;
            }
            if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'>' {
                best = Some((i, i + 2));
                i += 2;
                continue;
            }
            if b == b'.' {
                best = Some((i, i + 1));
                i += 1;
                continue;
            }
        }

        match b {
            b'(' => paren += 1,
            b')' => paren = paren.saturating_sub(1),
            b'[' => bracket += 1,
            b']' => bracket = bracket.saturating_sub(1),
            b'<' => angle += 1,
            b'>' => {
                let prev = i.checked_sub(1).map(|p| bytes[p]);
                if prev != Some(b'-') && prev != Some(b'=') {
                    angle = angle.saturating_sub(1);
                }
            }
            _ => {}
        }
        i += 1;
    }

    best.map(|(sep_start, sep_end)| (&raw[..sep_start], &raw[sep_end..]))
}

/// Extracts receiver qualifier identifier(s) from the LHS of a call separator.
/// For direct chains (`a.b.c`), yields only the immediate receiver `["b"]`.
/// For chained call/index receivers (`crate::foo::Client::default()` or `foo::bar()`),
/// extracts valid non-keyword top-level chain identifiers along the outer receiver chain.
#[cfg(feature = "tree-sitter")]
fn extract_lhs_qualifiers(lang: SupportedLanguage, lhs: &str) -> Vec<&str> {
    let mut trimmed = lhs.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    loop {
        let before = trimmed;
        if lang == SupportedLanguage::Rust {
            if let Some(rest) = trimmed.strip_suffix(".await") {
                trimmed = rest.trim_end();
                continue;
            }
        }
        if !matches!(lang, SupportedLanguage::Ruby | SupportedLanguage::Python)
            && trimmed.ends_with('?')
        {
            trimmed = trimmed[..trimmed.len() - 1].trim_end();
            continue;
        }
        if matches!(
            lang,
            SupportedLanguage::TypeScript
                | SupportedLanguage::Tsx
                | SupportedLanguage::JavaScript
                | SupportedLanguage::CSharp
        ) && trimmed.ends_with('!')
        {
            trimmed = trimmed[..trimmed.len() - 1].trim_end();
            continue;
        }
        if trimmed == before {
            break;
        }
    }
    if trimmed.is_empty() {
        return Vec::new();
    }

    let last_ch = trimmed.as_bytes()[trimmed.len() - 1];
    if !matches!(last_ch, b')' | b']' | b'>') && !trimmed.contains(['(', '[', '<']) {
        let q = trimmed
            .rsplit([':', '.', '>', '(', ' ', '[', ']'])
            .next()
            .unwrap_or("")
            .trim();
        if is_valid_ident(q) && !matches!(q, "self" | "this" | "super" | "crate" | "new" | "await")
        {
            return vec![q];
        }
        return Vec::new();
    }

    let allow_char_or_lifetime = matches!(
        lang,
        SupportedLanguage::Rust
            | SupportedLanguage::C
            | SupportedLanguage::Cpp
            | SupportedLanguage::Java
            | SupportedLanguage::CSharp
            | SupportedLanguage::Go
    );

    let bytes = trimmed.as_bytes();
    let mut i = 0usize;
    let mut paren = 0usize;
    let mut bracket = 0usize;
    let mut angle = 0usize;
    let mut out = Vec::new();

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' || b == b'`' {
            let q = b;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                } else if bytes[i] == q {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if b == b'\'' {
            if allow_char_or_lifetime {
                if trimmed.is_char_boundary(i) {
                    if let Some(lit_len) = signature::char_literal_len(&trimmed[i..]) {
                        i = (i + lit_len).min(bytes.len());
                        continue;
                    }
                }
                i += 1;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                continue;
            } else {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i = (i + 2).min(bytes.len());
                    } else if bytes[i] == b'\'' {
                        i += 1;
                        break;
                    } else {
                        i += 1;
                    }
                }
                continue;
            }
        }
        if paren == 0 && bracket == 0 && angle == 0 && (b.is_ascii_alphabetic() || b == b'_') {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric()
                    || bytes[i] == b'_'
                    || (lang == SupportedLanguage::Ruby && matches!(bytes[i], b'?' | b'!')))
            {
                i += 1;
            }
            let tok = &trimmed[start..i];
            if is_valid_ident(tok)
                && !matches!(tok, "self" | "this" | "super" | "crate" | "new" | "await")
                && !out.contains(&tok)
            {
                out.push(tok);
            }
            continue;
        }
        match b {
            b'(' => paren += 1,
            b')' => paren = paren.saturating_sub(1),
            b'[' => bracket += 1,
            b']' => bracket = bracket.saturating_sub(1),
            b'<' => angle += 1,
            b'>' => {
                let prev = i.checked_sub(1).map(|p| bytes[p]);
                if prev != Some(b'-') && prev != Some(b'=') {
                    angle = angle.saturating_sub(1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Scans deleted (`-`) diff lines in changed hunks for PascalCase/SCREAMING_CASE constants, types,
/// qualified calls (`Foo::bar`, `Foo.bar`), and direct function calls so removed invariants
/// (e.g. `BPS_DENOMINATOR`, `RefundLimits.CanAutoRefund`) are also surfaced in Layer 3.
#[cfg(feature = "tree-sitter")]
fn collect_refs_from_deleted_lines(
    lang: SupportedLanguage,
    deleted_lines: &[&str],
    type_refs: &mut HashSet<String>,
    qual_refs: &mut Vec<(String, String)>,
    call_refs: &mut HashSet<String>,
) {
    for raw in deleted_lines {
        let trimmed = raw.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
        {
            continue;
        }
        let bytes = trimmed.as_bytes();
        let mut i = 0usize;
        let mut prev_ident: Option<(&str, bool)> = None; // (ident, followed_by_sep)
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'"' {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i = (i + 2).min(bytes.len());
                    } else if bytes[i] == b'"' {
                        i += 1;
                        break;
                    } else {
                        i += 1;
                    }
                }
                prev_ident = None;
                continue;
            }
            if b == b'\'' {
                if matches!(lang, SupportedLanguage::C | SupportedLanguage::Cpp)
                    && i > 0
                    && bytes[i - 1].is_ascii_digit()
                {
                    // C++14 digit separator (`1'000`)
                    i += 1;
                    prev_ident = None;
                    continue;
                }
                if matches!(
                    lang,
                    SupportedLanguage::Rust
                        | SupportedLanguage::C
                        | SupportedLanguage::Cpp
                        | SupportedLanguage::Java
                        | SupportedLanguage::CSharp
                        | SupportedLanguage::Go
                ) {
                    if trimmed.is_char_boundary(i) {
                        if let Some(lit_len) = signature::char_literal_len(&trimmed[i..]) {
                            i = (i + lit_len).min(bytes.len());
                            prev_ident = None;
                            continue;
                        }
                    }
                    // Rust lifetime (`'a`, `'static`, `'_`) or unmatched char quote
                    i += 1;
                    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                    {
                        i += 1;
                    }
                    prev_ident = None;
                    continue;
                }
                // Python / JS / TS / Ruby single-quoted string literal
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i = (i + 2).min(bytes.len());
                    } else if bytes[i] == b'\'' {
                        i += 1;
                        break;
                    } else {
                        i += 1;
                    }
                }
                prev_ident = None;
                continue;
            }
            if b.is_ascii_alphabetic() || b == b'_' {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let tok = &trimmed[start..i];
                let after = trimmed[i..].trim_start();
                let is_call = after.starts_with('(');
                let has_sep = trimmed[i..].starts_with("::")
                    || trimmed[i..].starts_with('.')
                    || trimmed[i..].starts_with("->");

                if let Some((q, true)) = prev_ident {
                    if !is_noise_or_builtin_ident(tok) && q != "self" && q != "this" {
                        qual_refs.push((q.to_string(), tok.to_string()));
                    }
                }
                if !is_noise_or_builtin_ident(tok) {
                    if looks_like_type_or_const(tok) {
                        type_refs.insert(tok.to_string());
                        if is_call {
                            call_refs.insert(tok.to_string());
                        }
                    } else if is_call {
                        call_refs.insert(tok.to_string());
                    }
                }
                prev_ident = Some((tok, has_sep));
                if trimmed[i..].starts_with("::") || trimmed[i..].starts_with("->") {
                    i += 2;
                } else if trimmed[i..].starts_with('.') {
                    i += 1;
                }
                continue;
            }
            prev_ident = None;
            i += 1;
        }
    }
}

/// Extracts referenced symbols (types, qualified calls, bare calls) from AST nodes intersecting
/// `changed_line_ranges` (1-based inclusive) plus deleted `-` lines and enclosing scope headers
/// (`impl Type`, class bases), and simultaneously extracts Layer 3-① (same-file unmodified
/// definitions) from the already-parsed AST!
#[cfg(feature = "tree-sitter")]
#[allow(clippy::too_many_arguments)]
pub(super) fn extract_local_and_query_from_ast(
    lang: SupportedLanguage,
    path: &Path,
    root: &tree_sitter::Node,
    source: &str,
    plain_lines: &[&str],
    changed_line_ranges: &[(usize, usize)],
    deleted_lines: &[&str],
    exposed_line_ranges: &[(usize, usize)],
    inline_test_blocks: &[ast_compact::InlineTestBlock],
    collect_local: bool,
    collect_cross_query: bool,
) -> (Vec<ContextEntry>, Option<FileRefQuery>, Vec<String>) {
    if !collect_local && !collect_cross_query {
        return (Vec::new(), None, Vec::new());
    }
    if matches!(lang, SupportedLanguage::Json | SupportedLanguage::Bash) {
        return (Vec::new(), None, Vec::new());
    }

    // Exclude changes that lie wholly inside inline test blocks (`#[cfg(test)] mod tests`, etc.)
    // so test-only edits inside production files don't pull test helpers or unrelated types into Layer 3.
    let prod_changed_ranges: Vec<(usize, usize)> = changed_line_ranges
        .iter()
        .copied()
        .filter(|&(cs, ce)| {
            !inline_test_blocks
                .iter()
                .any(|b| cs >= b.start_line && ce <= b.end_line)
        })
        .collect();
    if prod_changed_ranges.is_empty() && deleted_lines.is_empty() {
        return (Vec::new(), None, Vec::new());
    }

    let mut type_or_const_refs: HashSet<String> = HashSet::new();
    let mut qualified_refs: Vec<(String, String)> = Vec::new();
    let mut call_refs: HashSet<String> = HashSet::new();

    // Helper: check if an AST node overlaps any changed/context range
    let overlaps_ranges = |s_line: usize, e_line: usize, ranges: &[(usize, usize)]| -> bool {
        ranges.iter().any(|&(rs, re)| e_line >= rs && s_line <= re)
    };

    // Walk AST nodes overlapping changed line ranges (or expanded hunk ranges, capped to changed + signature)
    fn walk_refs(
        lang: SupportedLanguage,
        node: tree_sitter::Node,
        source: &str,
        ranges: &[(usize, usize)],
        type_refs: &mut HashSet<String>,
        qual_refs: &mut Vec<(String, String)>,
        call_refs: &mut HashSet<String>,
    ) {
        let s_line = node.start_position().row + 1;
        let e_line = node.end_position().row + 1;
        if !ranges.iter().any(|&(rs, re)| e_line >= rs && s_line <= re) {
            return;
        }
        let kind = node.kind();

        // Capture enclosing Rust `impl Type` / `impl Trait for Type` header types even when only a method inside changed!
        if lang == SupportedLanguage::Rust && kind == "impl_item" {
            for field in ["type", "trait"] {
                if let Some(t) = node.child_by_field_name(field) {
                    collect_type_idents_in_subtree(t, source, type_refs);
                }
            }
        }
        // Capture enclosing class superclasses/interfaces
        if matches!(
            kind,
            "class_definition" | "class_declaration" | "class_specifier" | "class"
        ) {
            for field in ["superclasses", "superclass", "interfaces", "heritage"] {
                if let Some(t) = node.child_by_field_name(field) {
                    collect_type_idents_in_subtree(t, source, type_refs);
                }
            }
        }
        // Capture enclosing function signature types when body changed (crucial for clamped hunks!)
        if check_primary_definition(lang, &node).is_some() {
            if let Some(p) = signature::find_ast_parameters_node(lang, &node) {
                collect_type_idents_in_subtree(p, source, type_refs);
            }
            for ret_field in ["return_type", "result", "type"] {
                if let Some(r) = node.child_by_field_name(ret_field) {
                    collect_type_idents_in_subtree(r, source, type_refs);
                }
            }
        }

        // Capture direct type references
        if matches!(
            kind,
            "type_identifier"
                | "type_name"
                | "predefined_type"
                | "qualified_type"
                | "scoped_type_identifier"
        ) {
            if let Some(txt) = source.get(node.start_byte()..node.end_byte()) {
                let last = txt.rsplit([':', '.']).next().unwrap_or(txt).trim();
                if is_valid_ident(last) && !is_noise_or_builtin_ident(last) {
                    type_refs.insert(last.to_string());
                }
                if let Some((q, sym)) = txt.rsplit_once("::").or_else(|| txt.rsplit_once('.')) {
                    let q_last = q.rsplit([':', '.']).next().unwrap_or(q).trim();
                    let sym_clean = sym.trim();
                    if is_valid_ident(q_last)
                        && is_valid_ident(sym_clean)
                        && !is_noise_or_builtin_ident(sym_clean)
                    {
                        qual_refs.push((q_last.to_string(), sym_clean.to_string()));
                    }
                }
            }
        }

        // Capture scoped paths (Rust `foo::bar`, C++ `ns::bar`)
        if matches!(kind, "scoped_identifier" | "qualified_identifier") {
            if let Some(txt) = source.get(node.start_byte()..node.end_byte()) {
                if let Some((q, sym)) = txt.rsplit_once("::") {
                    let q_last = q.rsplit("::").next().unwrap_or(q).trim();
                    let sym_clean = sym.trim();
                    if is_valid_ident(sym_clean) && !is_noise_or_builtin_ident(sym_clean) {
                        if looks_like_type_or_const(sym_clean) {
                            type_refs.insert(sym_clean.to_string());
                        }
                        if is_valid_ident(q_last) {
                            qual_refs.push((q_last.to_string(), sym_clean.to_string()));
                        }
                    }
                    if is_valid_ident(q_last)
                        && looks_like_type_or_const(q_last)
                        && !is_noise_or_builtin_ident(q_last)
                    {
                        type_refs.insert(q_last.to_string());
                    }
                }
            }
        }

        // Capture function/method calls across languages
        if matches!(
            kind,
            "call_expression" | "call" | "method_invocation" | "invocation_expression"
        ) {
            if kind == "method_invocation" || (lang == SupportedLanguage::Ruby && kind == "call") {
                // Java `obj.method(...)` (`object`/`name`) and Ruby `recv.method(...)` (`receiver`/`method`)
                // keep the receiver out of the method name node; handle them exclusively here without
                // falling through to the bare-function branch below.
                let (obj_field, name_field) = if kind == "method_invocation" {
                    ("object", "name")
                } else {
                    ("receiver", "method")
                };
                let obj = node
                    .child_by_field_name(obj_field)
                    .and_then(|n| source.get(n.start_byte()..n.end_byte()))
                    .map(str::trim)
                    .unwrap_or("");
                let name = node
                    .child_by_field_name(name_field)
                    .and_then(|n| source.get(n.start_byte()..n.end_byte()))
                    .map(str::trim)
                    .unwrap_or("");
                if is_valid_ident(name) && !is_noise_or_builtin_ident(name) {
                    let quals = extract_lhs_qualifiers(lang, obj);
                    if !quals.is_empty() {
                        for q in quals {
                            qual_refs.push((q.to_string(), name.to_string()));
                            if looks_like_type_or_const(q) && !is_noise_or_builtin_ident(q) {
                                type_refs.insert(q.to_string());
                            }
                        }
                    } else if obj.is_empty() || obj == "this" || obj == "super" || obj == "self" {
                        if looks_like_type_or_const(name) {
                            type_refs.insert(name.to_string());
                        }
                        call_refs.insert(name.to_string());
                    }
                }
            } else if let Some(fn_n) = node
                .child_by_field_name("function")
                .or_else(|| node.child_by_field_name("name"))
                .or_else(|| node.child_by_field_name("method"))
            {
                let raw = source
                    .get(fn_n.start_byte()..fn_n.end_byte())
                    .unwrap_or("")
                    .trim();
                if let Some((lhs, rhs)) = split_rightmost_call_sep(lang, raw) {
                    let rhs_no_turbo = rhs.split("::<").next().unwrap_or(rhs);
                    let sym = rhs_no_turbo.split(['<', '(']).next().unwrap_or("").trim();
                    if is_valid_ident(sym) && !is_noise_or_builtin_ident(sym) {
                        let quals = extract_lhs_qualifiers(lang, lhs);
                        for q in quals {
                            qual_refs.push((q.to_string(), sym.to_string()));
                            if looks_like_type_or_const(q) && !is_noise_or_builtin_ident(q) {
                                type_refs.insert(q.to_string());
                            }
                        }
                    }
                } else {
                    let raw_no_turbo = raw.split("::<").next().unwrap_or(raw);
                    let sym = raw_no_turbo.split(['<', '(']).next().unwrap_or("").trim();
                    if is_valid_ident(sym) && !is_noise_or_builtin_ident(sym) {
                        if looks_like_type_or_const(sym) {
                            // PascalCase call (e.g. Go exported `EnqueueJob`, Python/JS constructor `Foo()`)
                            type_refs.insert(sym.to_string());
                            call_refs.insert(sym.to_string());
                        } else {
                            call_refs.insert(sym.to_string());
                        }
                    }
                }
            }
        }

        // Capture uppercase identifiers (types, enums, SCREAMING_CASE constants)
        if matches!(kind, "identifier" | "constant" | "field_identifier") {
            if let Some(txt) = source.get(node.start_byte()..node.end_byte()) {
                let t = txt.trim();
                if looks_like_type_or_const(t) && !is_noise_or_builtin_ident(t) {
                    type_refs.insert(t.to_string());
                }
            }
        }

        let mut cursor = node.walk();
        for ch in node.named_children(&mut cursor) {
            walk_refs(lang, ch, source, ranges, type_refs, qual_refs, call_refs);
        }
    }

    fn collect_type_idents_in_subtree(
        node: tree_sitter::Node,
        source: &str,
        out: &mut HashSet<String>,
    ) {
        let kind = node.kind();
        if matches!(
            kind,
            "type_identifier" | "identifier" | "constant" | "type_name"
        ) {
            if let Some(txt) = source.get(node.start_byte()..node.end_byte()) {
                let t = txt.trim();
                if looks_like_type_or_const(t) && !is_noise_or_builtin_ident(t) {
                    out.insert(t.to_string());
                }
            }
        }
        let mut cursor = node.walk();
        for ch in node.named_children(&mut cursor) {
            collect_type_idents_in_subtree(ch, source, out);
        }
    }

    walk_refs(
        lang,
        *root,
        source,
        &prod_changed_ranges,
        &mut type_or_const_refs,
        &mut qualified_refs,
        &mut call_refs,
    );

    collect_refs_from_deleted_lines(
        lang,
        deleted_lines,
        &mut type_or_const_refs,
        &mut qualified_refs,
        &mut call_refs,
    );

    // Deduplicate qualified_refs preserving order
    let mut seen_qual = HashSet::new();
    qualified_refs.retain(|pair| seen_qual.insert(pair.clone()));

    // Collect same-file definitions: both exposed (for dedup) and unmodified local matches (Layer 3-①)
    let mut exposed_def_names: Vec<String> = Vec::new();
    let mut local_entries: Vec<ContextEntry> = Vec::new();
    let mut local_seen: HashSet<String> = HashSet::new();

    #[allow(clippy::too_many_arguments)]
    fn walk_local_defs(
        lang: SupportedLanguage,
        path: &Path,
        node: tree_sitter::Node,
        plain_lines: &[&str],
        source: &str,
        exposed_ranges: &[(usize, usize)],
        inline_test_blocks: &[ast_compact::InlineTestBlock],
        type_refs: &HashSet<String>,
        call_refs: &HashSet<String>,
        collect_local: bool,
        exposed_names: &mut Vec<String>,
        local_entries: &mut Vec<ContextEntry>,
        local_seen: &mut HashSet<String>,
    ) {
        let n_start = node.start_position().row + 1;
        let n_end = node.end_position().row + 1;
        // Skip inline test blocks (`#[cfg(test)] mod tests`, etc.) inside production files
        if inline_test_blocks
            .iter()
            .any(|b| n_start >= b.start_line && n_end <= b.end_line)
        {
            return;
        }
        if let Some(matched) = check_primary_definition(lang, &node) {
            let s_line = matched.start_position().row + 1;
            let e_line = (matched.end_position().row + 1).min(plain_lines.len().max(1));
            if let Some(name) = ast_compact::bare_def_name(lang, &matched, plain_lines, source) {
                let is_exposed = exposed_ranges
                    .iter()
                    .any(|&(rs, re)| s_line >= rs && e_line <= re);
                if is_exposed {
                    if !exposed_names.contains(&name) {
                        exposed_names.push(name.clone());
                    }
                } else if collect_local
                    && !local_seen.contains(&name)
                    && is_type_or_const_def_node(lang, &matched)
                    && (type_refs.contains(&name) || call_refs.contains(&name))
                {
                    if let Some((actual_s_line, snippet)) =
                        render_type_def_snippet(lang, &matched, plain_lines, source)
                    {
                        local_seen.insert(name.clone());
                        local_entries.push(ContextEntry {
                            file_path: path.to_path_buf(),
                            symbol_name: name,
                            start_line: actual_s_line,
                            end_line: e_line,
                            is_local: true,
                            is_type_def: true,
                            pinned_by_import: true,
                            snippet,
                        });
                    }
                }
            }
            // Do not recurse into function/method bodies (prevents local `const` inside fn bodies
            // from leaking into file-level Layer 3-①).
            if ast_compact::is_foldable_function_node(lang, &matched)
                || !super::diff_expand::is_container_definition_node(&matched)
            {
                return;
            }
        }
        let mut cursor = node.walk();
        for ch in node.named_children(&mut cursor) {
            walk_local_defs(
                lang,
                path,
                ch,
                plain_lines,
                source,
                exposed_ranges,
                inline_test_blocks,
                type_refs,
                call_refs,
                collect_local,
                exposed_names,
                local_entries,
                local_seen,
            );
        }
    }

    let _ = overlaps_ranges;
    walk_local_defs(
        lang,
        path,
        *root,
        plain_lines,
        source,
        exposed_line_ranges,
        inline_test_blocks,
        &type_or_const_refs,
        &call_refs,
        collect_local,
        &mut exposed_def_names,
        &mut local_entries,
        &mut local_seen,
    );

    let query = if collect_cross_query {
        let mut import_specs = Vec::new();
        let mut imported_symbols = HashMap::new();
        let mut alias_map = HashMap::new();
        collect_file_imports(
            lang,
            root,
            source,
            &mut import_specs,
            &mut imported_symbols,
            &mut alias_map,
        );
        // Propagate alias references back to their original exported names
        for (alias, orig) in &alias_map {
            if type_or_const_refs.contains(alias) {
                type_or_const_refs.insert(orig.clone());
            }
            if call_refs.contains(alias) {
                call_refs.insert(orig.clone());
            }
            let extra_quals: Vec<(String, String)> = qualified_refs
                .iter()
                .filter(|(q, _)| q == alias)
                .map(|(_, s)| (orig.clone(), s.clone()))
                .collect();
            for pair in extra_quals {
                if !qualified_refs.contains(&pair) {
                    qualified_refs.push(pair);
                }
            }
        }
        // Remove symbols already resolved locally or exposed in this file
        for name in &local_seen {
            type_or_const_refs.remove(name);
            call_refs.remove(name);
        }
        for name in &exposed_def_names {
            type_or_const_refs.remove(name);
            call_refs.remove(name);
        }
        if type_or_const_refs.is_empty() && qualified_refs.is_empty() && call_refs.is_empty() {
            None
        } else {
            Some(FileRefQuery {
                file_path: path.to_path_buf(),
                lang,
                type_or_const_refs,
                qualified_refs,
                call_refs,
                import_specs,
                imported_symbols,
            })
        }
    } else {
        None
    };

    (local_entries, query, exposed_def_names)
}

/// Lists tracked repository source files for the exact `GitDiffSource` snapshot using `-z`
/// NUL-delimited output so non-ASCII paths are never octal-escaped by `core.quotepath`.
#[cfg(feature = "tree-sitter")]
fn list_tracked_source_files(repo_root: &Path, source: GitDiffSource<'_>) -> Vec<PathBuf> {
    let mut cmd = git_cmd(repo_root);
    match source {
        GitDiffSource::Commit(sha) => {
            cmd.args(["ls-tree", "-r", "--name-only", "-z", sha]);
        }
        GitDiffSource::Staged | GitDiffSource::Unstaged => {
            cmd.args(["ls-files", "-z"]);
        }
    }
    let Ok(output) = cmd.output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    output
        .stdout
        .split(|&b| b == 0)
        .filter(|slice| !slice.is_empty())
        .map(|slice| PathBuf::from(String::from_utf8_lossy(slice).trim()))
        .filter(|p| {
            let s = p.to_string_lossy();
            !s.is_empty()
                && !s.starts_with("vendor/")
                && !s.contains("/vendor/")
                && !s.starts_with("node_modules/")
                && !s.contains("/node_modules/")
                && !s.starts_with("dist/")
                && !s.starts_with("target/")
                && !is_lockfile_or_generated(p)
                && !is_test_file_path(p)
                && SupportedLanguage::from_path(p).is_some_and(|l| {
                    !matches!(l, SupportedLanguage::Json | SupportedLanguage::Bash)
                })
        })
        .collect()
}

#[cfg(feature = "tree-sitter")]
struct TrackedFileEntry {
    path: PathBuf,
    norm: String,
    lang: Option<SupportedLanguage>,
    file_name: String,
    ext: Option<String>,
}

/// Precomputed repository index built once per `resolve_cross_file_context` invocation so
/// per-query candidate resolution runs in O(|imports| + |refs|) instead of O(|imports| * |repo|).
#[cfg(feature = "tree-sitter")]
struct TrackedRepoIndex {
    entries: Vec<TrackedFileEntry>,
    norm_to_idx: HashMap<String, usize>,
    dir_to_indices: HashMap<String, Vec<usize>>,
    dir_suffix_to_dirs: HashMap<String, Vec<String>>,
    file_name_to_indices: HashMap<String, Vec<usize>>,
    lang_stem_to_indices: HashMap<SupportedLanguage, HashMap<String, Vec<usize>>>,
    cs_seg_to_indices: HashMap<String, Vec<usize>>,
}

#[cfg(feature = "tree-sitter")]
impl TrackedRepoIndex {
    fn build(tracked_files: &[PathBuf]) -> Self {
        let mut entries = Vec::with_capacity(tracked_files.len());
        let mut norm_to_idx = HashMap::with_capacity(tracked_files.len());
        let mut dir_to_indices: HashMap<String, Vec<usize>> = HashMap::new();
        let mut file_name_to_indices: HashMap<String, Vec<usize>> = HashMap::new();
        let mut lang_stem_to_indices: HashMap<SupportedLanguage, HashMap<String, Vec<usize>>> =
            HashMap::new();
        let mut cs_seg_to_indices: HashMap<String, Vec<usize>> = HashMap::new();

        for p in tracked_files {
            let idx = entries.len();
            let norm = p.to_string_lossy().replace('\\', "/");
            let lang = SupportedLanguage::from_path(p);
            let file_name = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let dir_norm = match norm.rsplit_once('/') {
                Some((d, _)) => d.to_string(),
                None => String::new(),
            };
            let ext = p.extension().and_then(|e| e.to_str()).map(str::to_string);

            norm_to_idx.insert(norm.clone(), idx);
            dir_to_indices
                .entry(dir_norm.clone())
                .or_default()
                .push(idx);
            if !file_name.is_empty() {
                file_name_to_indices
                    .entry(file_name.clone())
                    .or_default()
                    .push(idx);
            }
            if let Some(l) = lang {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    let norm_stem = normalize_alnum_lower(stem);
                    if !norm_stem.is_empty() {
                        lang_stem_to_indices
                            .entry(l)
                            .or_default()
                            .entry(norm_stem)
                            .or_default()
                            .push(idx);
                    }
                }
            }
            if ext.as_deref() == Some("cs") && !dir_norm.is_empty() {
                let mut seen_segs = HashSet::new();
                for seg in dir_norm.split('/').filter(|s| !s.is_empty()) {
                    if seen_segs.insert(seg) {
                        cs_seg_to_indices
                            .entry(seg.to_string())
                            .or_default()
                            .push(idx);
                    }
                }
            }
            entries.push(TrackedFileEntry {
                path: p.clone(),
                norm,
                lang,
                file_name,
                ext,
            });
        }

        let mut dir_suffix_to_dirs: HashMap<String, Vec<String>> = HashMap::new();
        for dir in dir_to_indices.keys() {
            if dir.is_empty() {
                continue;
            }
            dir_suffix_to_dirs
                .entry(dir.clone())
                .or_default()
                .push(dir.clone());
            for (pos, _) in dir.match_indices('/') {
                let tail = &dir[pos + 1..];
                if !tail.is_empty() {
                    dir_suffix_to_dirs
                        .entry(tail.to_string())
                        .or_default()
                        .push(dir.clone());
                }
            }
        }

        Self {
            entries,
            norm_to_idx,
            dir_to_indices,
            dir_suffix_to_dirs,
            file_name_to_indices,
            lang_stem_to_indices,
            cs_seg_to_indices,
        }
    }

    fn find_by_suffix(&self, suffix: &str) -> Vec<usize> {
        let clean = suffix.replace('\\', "/");
        if clean.is_empty() {
            return Vec::new();
        }
        let Some((dir_tail, fname)) = clean.rsplit_once('/') else {
            return self
                .file_name_to_indices
                .get(&clean)
                .cloned()
                .unwrap_or_default();
        };
        let Some(fn_idxs) = self.file_name_to_indices.get(fname) else {
            return Vec::new();
        };
        let Some(dirs) = self.dir_suffix_to_dirs.get(dir_tail) else {
            return Vec::new();
        };
        if fn_idxs.len() <= dirs.len().saturating_mul(4) {
            return fn_idxs
                .iter()
                .copied()
                .filter(|&idx| {
                    let norm = &self.entries[idx].norm;
                    norm == &clean
                        || (norm.len() > clean.len()
                            && norm.ends_with(&clean)
                            && norm.as_bytes()[norm.len() - clean.len() - 1] == b'/')
                })
                .collect();
        }
        let mut matched = Vec::new();
        for d in dirs {
            if let Some(d_idxs) = self.dir_to_indices.get(d) {
                for &idx in d_idxs {
                    if self.entries[idx].file_name == fname {
                        matched.push(idx);
                    }
                }
            }
        }
        matched
    }
}

#[cfg(feature = "tree-sitter")]
struct ResolvedCandidate {
    path: PathBuf,
    pinned_by_import: bool,
    same_pkg: bool,
    hints: HashSet<String>,
}

/// Resolves candidate external repo files from a file's imports and same-directory package siblings.
#[cfg(feature = "tree-sitter")]
fn resolve_candidate_files_for_query(
    q: &FileRefQuery,
    index: &TrackedRepoIndex,
) -> Vec<ResolvedCandidate> {
    // Map: entry idx -> (pinned_by_import, same_pkg, hint_symbols)
    let mut scored: HashMap<usize, (bool, bool, HashSet<String>)> = HashMap::new();
    let src_dir = q.file_path.parent().unwrap_or_else(|| Path::new(""));
    let src_path_norm = q.file_path.to_string_lossy().replace('\\', "/");
    let src_dir_norm = src_dir.to_string_lossy().replace('\\', "/");

    let add_idx = |scored: &mut HashMap<usize, (bool, bool, HashSet<String>)>,
                   idx: usize,
                   pinned_by_import: bool,
                   same_pkg: bool,
                   sym_hint: Option<&str>| {
        if index.entries[idx].norm == src_path_norm {
            return;
        }
        let entry = scored
            .entry(idx)
            .or_insert_with(|| (false, false, HashSet::new()));
        entry.0 |= pinned_by_import;
        entry.1 |= same_pkg;
        if let Some(s) = sym_hint {
            entry.2.insert(s.to_string());
        }
    };

    let add_rel_path = |scored: &mut HashMap<usize, (bool, bool, HashSet<String>)>,
                        p: PathBuf,
                        pinned_by_import: bool,
                        same_pkg: bool,
                        sym_hint: Option<&str>| {
        let norm = p.to_string_lossy().replace('\\', "/");
        if let Some(&idx) = index.norm_to_idx.get(&norm) {
            add_idx(scored, idx, pinned_by_import, same_pkg, sym_hint);
        }
    };

    // Helper: normalize relative path segments (`a/b/../c` -> `a/c`)
    let normalize_rel = |base: &Path, rel: &str| -> PathBuf {
        let mut parts: Vec<String> = Vec::new();
        for comp in base.join(rel).components() {
            match comp {
                std::path::Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
                std::path::Component::ParentDir => {
                    parts.pop();
                }
                _ => {}
            }
        }
        parts.into_iter().collect()
    };

    // Build fast lookup of all identifiers actually referenced by this file's changed hunks,
    // and invert `imported_symbols` (`spec -> [sym, ...]`) so untouched top-of-file imports
    // never waste candidate slots or leak unrelated symbols.
    let mut all_wanted_refs: HashSet<&str> = HashSet::new();
    for t in &q.type_or_const_refs {
        all_wanted_refs.insert(t.as_str());
    }
    for c in &q.call_refs {
        all_wanted_refs.insert(c.as_str());
    }
    for (qual, sym) in &q.qualified_refs {
        all_wanted_refs.insert(qual.as_str());
        all_wanted_refs.insert(sym.as_str());
    }
    let mut spec_explicit_syms: HashMap<&str, Vec<&str>> = HashMap::new();
    for (sym, specs) in &q.imported_symbols {
        for s in specs {
            spec_explicit_syms
                .entry(s.as_str())
                .or_default()
                .push(sym.as_str());
        }
    }

    // 1. Direct import / use / include / require resolution (filtered to diff-referenced imports!)
    for spec in &q.import_specs {
        if let Some(mod_name) = spec.strip_prefix("mod::") {
            let mod_prefix = format!("{mod_name}::");
            let used_in_diff = all_wanted_refs.contains(mod_name)
                || q.imported_symbols.iter().any(|(sym, specs)| {
                    all_wanted_refs.contains(sym.as_str())
                        && specs.iter().any(|s| s.starts_with(&mod_prefix))
                });
            if !used_in_diff {
                continue;
            }
        } else if let Some(explicit_syms) = spec_explicit_syms.get(spec.as_str()) {
            if !explicit_syms.iter().any(|s| all_wanted_refs.contains(s)) {
                continue;
            }
        }
        match q.lang {
            SupportedLanguage::Rust => {
                // `crate::a::b::Sym`, `super::foo::Bar`, `self::sub::Baz`, `mod::foo`
                let parts: Vec<&str> = spec.split("::").filter(|s| !s.is_empty()).collect();
                if parts.is_empty() {
                    continue;
                }
                let sym_hint = parts
                    .last()
                    .copied()
                    .filter(|s| !is_noise_or_builtin_ident(s) && all_wanted_refs.contains(*s));
                let mut rel_bases: Vec<PathBuf> = Vec::new();
                if parts[0] == "mod" && parts.len() >= 2 {
                    let m = parts[1];
                    rel_bases.push(src_dir.join(format!("{m}.rs")));
                    rel_bases.push(src_dir.join(m).join("mod.rs"));
                    if let Some(stem) = q.file_path.file_stem().and_then(|s| s.to_str()) {
                        if stem != "mod" && stem != "lib" && stem != "main" {
                            rel_bases.push(src_dir.join(stem).join(format!("{m}.rs")));
                            rel_bases.push(src_dir.join(stem).join(m).join("mod.rs"));
                        }
                    }
                } else if parts[0] == "crate" {
                    // Find nearest `src/` root for this file
                    let crate_src = find_rust_crate_src_dir(&q.file_path);
                    let segs = &parts[1..];
                    for take in [segs.len(), segs.len().saturating_sub(1)] {
                        if take == 0 {
                            continue;
                        }
                        let sub: PathBuf = segs[..take].iter().collect();
                        rel_bases.push(crate_src.join(&sub).with_extension("rs"));
                        rel_bases.push(crate_src.join(&sub).join("mod.rs"));
                    }
                } else if parts[0] == "super" {
                    let mut base = src_dir.to_path_buf();
                    let mut idx = 0;
                    // In non-mod.rs files (e.g. `src/foo/bar.rs`), `super` is `src/foo.rs` or `src/foo/`
                    if let Some(stem) = q.file_path.file_stem().and_then(|s| s.to_str()) {
                        if stem != "mod" && stem != "lib" && stem != "main" {
                            // `super::X` from `src/foo/bar.rs` can live in `src/foo.rs` or `src/foo/X.rs`
                            if let Some(dir_name) = base.file_name().and_then(|n| n.to_str()) {
                                if let Some(parent) = base.parent() {
                                    rel_bases.push(parent.join(format!("{dir_name}.rs")));
                                }
                            }
                            rel_bases.push(base.join("mod.rs"));
                            idx = 1;
                        }
                    }
                    while idx < parts.len() && parts[idx] == "super" {
                        base.pop();
                        idx += 1;
                    }
                    let segs = &parts[idx..];
                    for take in [segs.len(), segs.len().saturating_sub(1)] {
                        if take == 0 {
                            rel_bases.push(base.join("mod.rs"));
                            if let Some(dir_name) = base.file_name().and_then(|n| n.to_str()) {
                                if let Some(parent) = base.parent() {
                                    rel_bases.push(parent.join(format!("{dir_name}.rs")));
                                }
                            }
                            continue;
                        }
                        let sub: PathBuf = segs[..take].iter().collect();
                        rel_bases.push(base.join(&sub).with_extension("rs"));
                        rel_bases.push(base.join(&sub).join("mod.rs"));
                    }
                } else if parts[0] == "self" {
                    let segs = &parts[1..];
                    for take in [segs.len(), segs.len().saturating_sub(1)] {
                        if take == 0 {
                            continue;
                        }
                        let sub: PathBuf = segs[..take].iter().collect();
                        rel_bases.push(src_dir.join(&sub).with_extension("rs"));
                        rel_bases.push(src_dir.join(&sub).join("mod.rs"));
                    }
                }
                for cand in rel_bases {
                    add_rel_path(&mut scored, cand, true, false, sym_hint);
                }
            }
            SupportedLanguage::Python => {
                if spec.starts_with('.') {
                    let dots = spec.chars().take_while(|&c| c == '.').count();
                    let rest = &spec[dots..];
                    let mut base = src_dir.to_path_buf();
                    for _ in 1..dots {
                        base.pop();
                    }
                    if rest.is_empty() {
                        // `from . import foo, bar`: resolve each diff-referenced symbol as a sibling module
                        if let Some(syms) = spec_explicit_syms.get(spec.as_str()) {
                            for &sym in syms {
                                if all_wanted_refs.contains(sym) {
                                    add_rel_path(
                                        &mut scored,
                                        base.join(sym).with_extension("py"),
                                        true,
                                        false,
                                        None,
                                    );
                                    add_rel_path(
                                        &mut scored,
                                        base.join(sym).join("__init__.py"),
                                        true,
                                        false,
                                        None,
                                    );
                                }
                            }
                        }
                        add_rel_path(&mut scored, base.join("__init__.py"), true, false, None);
                    } else {
                        let sub: PathBuf = rest.split('.').filter(|s| !s.is_empty()).collect();
                        add_rel_path(
                            &mut scored,
                            base.join(&sub).with_extension("py"),
                            true,
                            false,
                            None,
                        );
                        add_rel_path(
                            &mut scored,
                            base.join(&sub).join("__init__.py"),
                            true,
                            false,
                            None,
                        );
                    }
                } else {
                    let slash_spec = spec.replace('.', "/");
                    for idx in index.find_by_suffix(&format!("{slash_spec}.py")) {
                        add_idx(&mut scored, idx, true, false, None);
                    }
                    for idx in index.find_by_suffix(&format!("{slash_spec}/__init__.py")) {
                        add_idx(&mut scored, idx, true, false, None);
                    }
                }
            }
            SupportedLanguage::JavaScript
            | SupportedLanguage::TypeScript
            | SupportedLanguage::Tsx => {
                if spec.starts_with('.') {
                    let base = normalize_rel(src_dir, spec);
                    let exts = ["ts", "tsx", "mts", "js", "jsx", "mjs"];
                    add_rel_path(&mut scored, base.clone(), true, false, None);
                    // Strip trailing .js/.mjs if TS imports compiled extension
                    let stripped = base
                        .to_string_lossy()
                        .trim_end_matches(".js")
                        .trim_end_matches(".mjs")
                        .trim_end_matches(".cjs")
                        .to_string();
                    for ext in exts {
                        add_rel_path(
                            &mut scored,
                            PathBuf::from(format!("{stripped}.{ext}")),
                            true,
                            false,
                            None,
                        );
                        add_rel_path(
                            &mut scored,
                            base.join(format!("index.{ext}")),
                            true,
                            false,
                            None,
                        );
                    }
                } else if let Some(alias_rest) =
                    spec.strip_prefix("@/").or_else(|| spec.strip_prefix("~/"))
                {
                    for ext in ["ts", "tsx", "js", "jsx"] {
                        for idx in index.find_by_suffix(&format!("{alias_rest}.{ext}")) {
                            add_idx(&mut scored, idx, true, false, None);
                        }
                        for idx in index.find_by_suffix(&format!("{alias_rest}/index.{ext}")) {
                            add_idx(&mut scored, idx, true, false, None);
                        }
                    }
                }
            }
            SupportedLanguage::Go => {
                // Match repo-internal package directory by trailing path components
                let parts: Vec<&str> = spec.split('/').filter(|s| !s.is_empty()).collect();
                for skip in 0..parts.len().min(3) {
                    let tail = parts[skip..].join("/");
                    if let Some(dirs) = index.dir_suffix_to_dirs.get(&tail) {
                        for d in dirs {
                            if let Some(d_idxs) = index.dir_to_indices.get(d) {
                                for &idx in d_idxs {
                                    if index.entries[idx].ext.as_deref() == Some("go") {
                                        add_idx(&mut scored, idx, true, false, None);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            SupportedLanguage::Java => {
                let rel_java = format!("{}.java", spec.replace('.', "/"));
                let sym = spec.rsplit('.').next();
                for idx in index.find_by_suffix(&rel_java) {
                    add_idx(&mut scored, idx, true, false, sym);
                }
            }
            SupportedLanguage::CSharp => {
                let last_ns = spec.rsplit('.').next().unwrap_or(spec);
                if let Some(idxs) = index.cs_seg_to_indices.get(last_ns) {
                    for &idx in idxs {
                        add_idx(&mut scored, idx, true, false, None);
                    }
                }
            }
            SupportedLanguage::C | SupportedLanguage::Cpp => {
                let norm = normalize_rel(src_dir, spec);
                add_rel_path(&mut scored, norm, true, false, None);
                for idx in index.find_by_suffix(spec) {
                    add_idx(&mut scored, idx, true, false, None);
                }
            }
            SupportedLanguage::Ruby => {
                if let Some(rel) = spec.strip_prefix("./") {
                    let norm = normalize_rel(src_dir, rel);
                    add_rel_path(&mut scored, norm.clone(), true, false, None);
                    add_rel_path(&mut scored, norm.with_extension("rb"), true, false, None);
                } else {
                    let rb_suffix = if spec.ends_with(".rb") {
                        spec.clone()
                    } else {
                        format!("{spec}.rb")
                    };
                    for idx in index.find_by_suffix(&rb_suffix) {
                        add_idx(&mut scored, idx, true, false, None);
                    }
                }
            }
            SupportedLanguage::Bash | SupportedLanguage::Json => {}
        }
    }

    // 2. Same-directory package siblings (essential for Go, Java, C# same-package types and C/C++ paired header).
    // Note: Go/Java/C# directory siblings mark `same_pkg = true` rather than `pinned_by_import = true`,
    // so an unrelated sibling class (`OtherService.java`) cannot match arbitrary lowercase receiver calls.
    let auto_pkg = matches!(
        q.lang,
        SupportedLanguage::Go | SupportedLanguage::Java | SupportedLanguage::CSharp
    );
    let is_c_cpp = matches!(q.lang, SupportedLanguage::C | SupportedLanguage::Cpp);
    if auto_pkg || is_c_cpp {
        if let Some(dir_idxs) = index.dir_to_indices.get(&src_dir_norm) {
            for &idx in dir_idxs {
                let entry = &index.entries[idx];
                let same_lang_family = match (q.lang, entry.lang) {
                    (a, Some(b)) if a == b => true,
                    (SupportedLanguage::C, Some(SupportedLanguage::Cpp))
                    | (SupportedLanguage::Cpp, Some(SupportedLanguage::C)) => true,
                    _ => false,
                };
                if !same_lang_family {
                    continue;
                }
                if auto_pkg {
                    add_idx(&mut scored, idx, false, true, None);
                } else if entry.path.file_stem() == q.file_path.file_stem() {
                    // Paired header `foo.h` / `foo.hpp` for `foo.c` / `foo.cpp`
                    add_idx(&mut scored, idx, true, true, None);
                }
            }
        }
    }

    // 3. Filename-stem match for unresolved PascalCase types (e.g. `OrderBook` -> `order_book.rs` / `OrderBook.cs`)
    if let Some(stem_map) = index.lang_stem_to_indices.get(&q.lang) {
        for type_name in &q.type_or_const_refs {
            let norm_t = normalize_alnum_lower(type_name);
            if let Some(matches) = stem_map.get(&norm_t) {
                // Only add if unambiguous (<= 2 files in the repo share this stem)
                if matches.len() <= 2 {
                    for &idx in matches {
                        add_idx(&mut scored, idx, false, false, Some(type_name));
                    }
                }
            }
        }
        for (qual, sym) in &q.qualified_refs {
            let norm_q = normalize_alnum_lower(qual);
            if let Some(matches) = stem_map.get(&norm_q) {
                if matches.len() <= 2 {
                    for &idx in matches {
                        add_idx(&mut scored, idx, false, false, Some(sym));
                    }
                }
            }
        }
    }

    scored
        .into_iter()
        .map(
            |(idx, (pinned_by_import, same_pkg, hints))| ResolvedCandidate {
                path: index.entries[idx].path.clone(),
                pinned_by_import,
                same_pkg,
                hints,
            },
        )
        .collect()
}

#[cfg(feature = "tree-sitter")]
fn find_rust_crate_src_dir(path: &Path) -> PathBuf {
    let mut cur = PathBuf::new();
    for comp in path.components() {
        cur.push(comp);
        if comp.as_os_str() == "src" {
            return cur;
        }
    }
    PathBuf::from("src")
}

/// Reads multiple file blobs at the exact `GitDiffSource` snapshot:
/// - `Commit(sha)` / `Staged`: single batched `git cat-file --batch` pipe (zero per-file subprocess overhead!)
/// - `Unstaged`: bounded `std::fs::read` (skipping symlinks and > 1 MiB files).
#[cfg(feature = "tree-sitter")]
fn read_snapshot_files_batch(
    repo_root: &Path,
    source: GitDiffSource<'_>,
    paths: &[PathBuf],
) -> HashMap<PathBuf, String> {
    let mut out = HashMap::new();
    if paths.is_empty() {
        return out;
    }

    match source {
        GitDiffSource::Unstaged => {
            for p in paths {
                let full = repo_root.join(p);
                let Ok(meta) = std::fs::symlink_metadata(&full) else {
                    continue;
                };
                if meta.file_type().is_symlink()
                    || meta.is_dir()
                    || meta.len() > MAX_TREESITTER_FILE_BYTES as u64
                {
                    continue;
                }
                if let Ok(bytes) = std::fs::read(&full) {
                    if bytes.len() <= MAX_TREESITTER_FILE_BYTES && !bytes.contains(&0) {
                        out.insert(p.clone(), String::from_utf8_lossy(&bytes).into_owned());
                    }
                }
            }
        }
        GitDiffSource::Staged | GitDiffSource::Commit(_) => {
            let prefix = match source {
                GitDiffSource::Staged => ":0:".to_string(),
                GitDiffSource::Commit(sha) => format!("{sha}:"),
                GitDiffSource::Unstaged => unreachable!(),
            };
            let mut cmd = git_cmd(repo_root);
            cmd.args(["cat-file", "--batch"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            let Ok(mut child) = cmd.spawn() else {
                return out;
            };

            let stdin_opt = child.stdin.take();
            let stdout_opt = child.stdout.take();
            std::thread::scope(|s| {
                if let Some(mut stdin) = stdin_opt {
                    let prefix_ref = &prefix;
                    s.spawn(move || {
                        for p in paths {
                            let norm = p.to_string_lossy().replace('\\', "/");
                            if writeln!(stdin, "{prefix_ref}{norm}").is_err() {
                                break;
                            }
                        }
                    });
                }

                if let Some(stdout) = stdout_opt {
                    let mut reader = BufReader::new(stdout);
                    let mut header = String::new();
                    for p in paths {
                        header.clear();
                        let Ok(n) = reader.read_line(&mut header) else {
                            break;
                        };
                        if n == 0 {
                            break;
                        };
                        let trimmed = header.trim_end_matches(['\r', '\n']);
                        if trimmed.ends_with(" missing") || trimmed.ends_with(" ambiguous") {
                            continue;
                        }
                        let mut parts = trimmed.split_whitespace();
                        let _oid = parts.next();
                        let obj_type = parts.next();
                        let size: usize = match parts.next().and_then(|s| s.parse().ok()) {
                            Some(sz) => sz,
                            None => break,
                        };
                        if obj_type != Some("blob") || size > MAX_TREESITTER_FILE_BYTES {
                            // Drain exact payload + trailing '\n'
                            let mut drain = (&mut reader).take((size + 1) as u64);
                            let _ = std::io::copy(&mut drain, &mut std::io::sink());
                            continue;
                        }
                        let mut buf = vec![0u8; size];
                        if reader.read_exact(&mut buf).is_err() {
                            break;
                        }
                        // Consume trailing LF written by git cat-file --batch
                        let mut lf = [0u8; 1];
                        let _ = reader.read_exact(&mut lf);
                        if !buf.contains(&0) {
                            out.insert(p.clone(), String::from_utf8_lossy(&buf).into_owned());
                        }
                    }
                }
            });
            let _ = child.wait();
        }
    }

    out
}

#[cfg(feature = "tree-sitter")]
#[derive(Default, Clone)]
struct CandidateWanted {
    score: usize,
    pinned: bool,
    same_pkg: bool,
    types: HashSet<String>,
    direct_calls: HashSet<String>,
    pinned_calls: HashSet<String>,
    /// Qualified call constraints `sym -> {qual_1, qual_2}` (so a method/associated call `q.sym()`
    /// in one file never falsely matches an unrelated type's `Other::sym()` in another file).
    qual_calls: HashMap<String, HashSet<String>>,
}

#[cfg(feature = "tree-sitter")]
fn normalize_alnum_lower(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Resolves Layer 3-② cross-file 1-hop AST type definitions and body-stripped function signatures.
#[cfg(feature = "tree-sitter")]
pub(super) fn resolve_cross_file_context(
    repo_root: &Path,
    source: GitDiffSource<'_>,
    queries: &[FileRefQuery],
    already_exposed: &HashSet<String>,
    scope_sep: &'static str,
) -> Vec<ContextEntry> {
    if queries.is_empty() {
        return Vec::new();
    }
    let started = Instant::now();
    let deadline = Duration::from_millis(MAX_CROSS_FILE_WALL_MS);

    let tracked_files = list_tracked_source_files(repo_root, source);
    if tracked_files.is_empty() {
        return Vec::new();
    }
    let repo_index = TrackedRepoIndex::build(&tracked_files);

    // Scope wanted symbols per candidate file so references in file A never cross-contaminate
    // candidate files imported only by file B.
    let mut candidate_rank: HashMap<PathBuf, CandidateWanted> = HashMap::new();

    for q in queries {
        if started.elapsed() >= deadline {
            break;
        }
        // Only Go shares package-level unqualified functions across sibling files without imports;
        // Java/C# methods require explicit receiver/type/static-import qualification.
        let allow_same_pkg_bare_calls = q.lang == SupportedLanguage::Go;
        let mut q_types: HashSet<String> = HashSet::new();
        let mut q_direct_calls: HashSet<String> = HashSet::new();
        let mut q_pinned_calls: HashSet<String> = HashSet::new();
        let mut q_qual_calls: HashMap<String, HashSet<String>> = HashMap::new();

        for t in &q.type_or_const_refs {
            if !already_exposed.contains(t) {
                q_types.insert(t.clone());
            }
        }
        for (qual, sym) in &q.qualified_refs {
            if !already_exposed.contains(sym) {
                if looks_like_type_or_const(sym) {
                    q_types.insert(sym.clone());
                }
                q_qual_calls
                    .entry(sym.clone())
                    .or_default()
                    .insert(qual.clone());
            }
        }
        for c in &q.call_refs {
            if !already_exposed.contains(c) {
                if q.imported_symbols.contains_key(c) {
                    q_direct_calls.insert(c.clone());
                } else if allow_same_pkg_bare_calls {
                    q_pinned_calls.insert(c.clone());
                }
            }
        }
        if q_types.is_empty()
            && q_direct_calls.is_empty()
            && q_pinned_calls.is_empty()
            && q_qual_calls.is_empty()
        {
            continue;
        }
        for cand in resolve_candidate_files_for_query(q, &repo_index) {
            let entry = candidate_rank.entry(cand.path).or_default();
            entry.score += if cand.pinned_by_import {
                3
            } else if cand.same_pkg {
                2
            } else {
                1
            } + cand.hints.len();
            entry.pinned |= cand.pinned_by_import;
            entry.same_pkg |= cand.same_pkg;
            entry.types.extend(q_types.iter().cloned());
            entry.direct_calls.extend(q_direct_calls.iter().cloned());
            if cand.pinned_by_import || (allow_same_pkg_bare_calls && cand.same_pkg) {
                entry.pinned_calls.extend(q_pinned_calls.iter().cloned());
            }
            for (sym, quals) in &q_qual_calls {
                entry
                    .qual_calls
                    .entry(sym.clone())
                    .or_default()
                    .extend(quals.iter().cloned());
            }
        }
    }

    if candidate_rank.is_empty() {
        return Vec::new();
    }

    let mut ranked_paths: Vec<(PathBuf, CandidateWanted)> = candidate_rank.into_iter().collect();
    ranked_paths.sort_by(|a, b| b.1.score.cmp(&a.1.score).then_with(|| a.0.cmp(&b.0)));
    ranked_paths.truncate(MAX_CROSS_FILE_CANDIDATES);

    let to_read: Vec<PathBuf> = ranked_paths.iter().map(|(p, _)| p.clone()).collect();
    let blobs = read_snapshot_files_batch(repo_root, source, &to_read);

    // Collect candidate matches grouped by symbol name for ambiguity filtering
    let mut matches_by_symbol: HashMap<String, Vec<ContextEntry>> = HashMap::new();

    for (cand_path, cw) in ranked_paths {
        if started.elapsed() >= deadline {
            break;
        }
        let Some(lang) = SupportedLanguage::from_path(&cand_path) else {
            continue;
        };
        let Some(content) = blobs.get(&cand_path) else {
            continue;
        };
        let Ok(tree) = parse_with_timeout(lang, content) else {
            continue;
        };
        let lines: Vec<&str> = content.lines().collect();
        if lines.is_empty() {
            continue;
        }

        let inline_tests = ast_compact::find_inline_test_blocks(lang, &tree.root_node(), content);
        let raw_stem = cand_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let cand_stem_norm = if matches!(raw_stem, "mod" | "__init__" | "index") {
            cand_path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .map(normalize_alnum_lower)
                .unwrap_or_else(|| normalize_alnum_lower(raw_stem))
        } else {
            normalize_alnum_lower(raw_stem)
        };

        #[allow(clippy::too_many_arguments)]
        fn walk_exports(
            lang: SupportedLanguage,
            cand_path: &Path,
            cand_stem_norm: &str,
            node: tree_sitter::Node,
            lines: &[&str],
            source: &str,
            inline_tests: &[ast_compact::InlineTestBlock],
            cw: &CandidateWanted,
            already_exposed: &HashSet<String>,
            scope_sep: &'static str,
            out: &mut HashMap<String, Vec<ContextEntry>>,
        ) {
            let n_start = node.start_position().row + 1;
            let n_end = node.end_position().row + 1;
            // Skip inline test blocks (`#[cfg(test)] mod tests`, etc.) inside production files
            if inline_tests
                .iter()
                .any(|b| n_start >= b.start_line && n_end <= b.end_line)
            {
                return;
            }
            if let Some(matched) = check_primary_definition(lang, &node) {
                if let Some(name) = ast_compact::bare_def_name(lang, &matched, lines, source) {
                    if !already_exposed.contains(&name) && !is_noise_or_builtin_ident(&name) {
                        let is_type = is_type_or_const_def_node(lang, &matched);
                        let parent =
                            signature::extract_parent_scope_prefix(lang, &matched, lines, source);
                        let parent_tokens: Vec<String> = parent
                            .as_deref()
                            .map(|p| {
                                p.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                                    .filter(|tok| {
                                        !tok.is_empty()
                                            && !matches!(
                                                *tok,
                                                "class"
                                                    | "struct"
                                                    | "enum"
                                                    | "interface"
                                                    | "trait"
                                                    | "record"
                                                    | "impl"
                                                    | "for"
                                                    | "where"
                                                    | "mod"
                                                    | "module"
                                                    | "namespace"
                                                    | "abstract"
                                                    | "pub"
                                            )
                                    })
                                    .map(normalize_alnum_lower)
                                    .filter(|t| !t.is_empty())
                                    .collect()
                            })
                            .unwrap_or_default();

                        let qual_matched = !is_type
                            && cw.qual_calls.get(&name).is_some_and(|quals| {
                                quals.iter().any(|q| {
                                    let q_norm = normalize_alnum_lower(q);
                                    if q_norm.is_empty() {
                                        return false;
                                    }
                                    // 1) Qualifier matches candidate file stem (e.g. `archive::extract_archive` -> `archive.rs`,
                                    //    or imported TS/JS namespace `fmt` -> `./format.ts`). Not applied to unimported Java/C# classes
                                    //    where files do not act as importable module namespaces.
                                    if !cand_stem_norm.is_empty()
                                        && (!matches!(
                                            lang,
                                            SupportedLanguage::Java | SupportedLanguage::CSharp
                                        ) || cw.pinned)
                                        && q_norm == *cand_stem_norm
                                    {
                                        return true;
                                    }
                                    // 2) Qualifier matches enclosing parent type/class/impl token (e.g. `RateLimiter.allow?`)
                                    if !parent_tokens.is_empty() && parent_tokens.contains(&q_norm)
                                    {
                                        return true;
                                    }
                                    // 3) Receiver variable call (`inventory.reserveStock`, `rb.write`):
                                    //    allow when the enclosing class/type is itself referenced in `cw.types`
                                    //    OR (when explicitly imported) matches the candidate file stem.
                                    let is_lowercase_receiver =
                                        q.chars().next().is_some_and(|c| c.is_ascii_lowercase());
                                    if is_lowercase_receiver && !parent_tokens.is_empty() {
                                        let parent_in_types = cw.types.iter().any(|t| {
                                            let norm_t = normalize_alnum_lower(t);
                                            !norm_t.is_empty() && parent_tokens.contains(&norm_t)
                                        });
                                        let parent_matches_file = !cand_stem_norm.is_empty()
                                            && parent_tokens.iter().any(|pt| *pt == cand_stem_norm);
                                        if parent_in_types || (cw.pinned && parent_matches_file) {
                                            return true;
                                        }
                                    }
                                    false
                                })
                            });

                        let is_wanted = (is_type && cw.types.contains(&name))
                            || (!is_type && cw.direct_calls.contains(&name))
                            || (!is_type
                                && (cw.pinned || cw.same_pkg)
                                && cw.pinned_calls.contains(&name))
                            || qual_matched;
                        if is_wanted {
                            let s_line = matched.start_position().row + 1;
                            let e_line = (matched.end_position().row + 1).min(lines.len().max(1));
                            let rendered = if is_type {
                                render_type_def_snippet(lang, &matched, lines, source)
                            } else {
                                let raw_sig =
                                    signature::extract_signature(lang, &matched, lines, source);
                                let sig_opt =
                                    signature::compact_definition_signature_with_ast_opts(
                                        lang,
                                        &matched,
                                        parent.as_deref(),
                                        raw_sig.as_deref(),
                                        source,
                                        signature::SigFormatOpts {
                                            max_chars: 200,
                                            scope_sep,
                                            preserve_key_decorators: true,
                                            greedy_params: true,
                                        },
                                    )
                                    .map(|s| s.trim_end_matches('{').trim_end().to_string());
                                sig_opt.map(|sig| {
                                    let text = if let Some(doc) =
                                        extract_short_doc_comment(lang, &matched, lines, source)
                                    {
                                        format!("{sig}\n  // doc: {doc}")
                                    } else {
                                        sig
                                    };
                                    (s_line, text)
                                })
                            };
                            if let Some((actual_s_line, snip)) = rendered {
                                let entry_list = out.entry(name.clone()).or_default();
                                let dup = entry_list.iter().any(|e| {
                                    e.file_path == cand_path && e.start_line == actual_s_line
                                });
                                if !dup {
                                    entry_list.push(ContextEntry {
                                        file_path: cand_path.to_path_buf(),
                                        symbol_name: name,
                                        start_line: actual_s_line,
                                        end_line: e_line,
                                        is_local: false,
                                        is_type_def: is_type,
                                        pinned_by_import: cw.pinned || cw.same_pkg,
                                        snippet: snip,
                                    });
                                }
                            }
                        }
                    }
                }
                if !super::diff_expand::is_container_definition_node(&matched) {
                    return;
                }
            }
            let mut cursor = node.walk();
            for ch in node.named_children(&mut cursor) {
                walk_exports(
                    lang,
                    cand_path,
                    cand_stem_norm,
                    ch,
                    lines,
                    source,
                    inline_tests,
                    cw,
                    already_exposed,
                    scope_sep,
                    out,
                );
            }
        }

        walk_exports(
            lang,
            &cand_path,
            &cand_stem_norm,
            tree.root_node(),
            &lines,
            content,
            &inline_tests,
            &cw,
            already_exposed,
            scope_sep,
            &mut matches_by_symbol,
        );
    }

    // Ambiguity discipline:
    // - If pinned matches exist, prefer pinned matches only.
    // - If > 2 distinct files still match an unpinned symbol name, DROP it rather than guessing!
    let mut final_entries = Vec::new();
    for (_sym, mut cands) in matches_by_symbol {
        let pinned_cands: Vec<ContextEntry> = cands
            .iter()
            .filter(|c| c.pinned_by_import)
            .cloned()
            .collect();
        if !pinned_cands.is_empty() {
            cands = pinned_cands;
        }
        let distinct_files: HashSet<&Path> = cands.iter().map(|c| c.file_path.as_path()).collect();
        if distinct_files.len() > 2 {
            continue;
        }
        // Keep at most 2 definitions per symbol name (e.g. trait + struct or 2 overloads)
        cands.sort_by(|a, b| {
            b.is_type_def
                .cmp(&a.is_type_def)
                .then_with(|| a.file_path.cmp(&b.file_path))
                .then_with(|| a.start_line.cmp(&b.start_line))
        });
        cands.truncate(2);
        final_entries.extend(cands);
    }

    final_entries
}

/// Computes Layer 3 dynamic budget:
/// - Under normal conditions: `min(15% of max_bytes, max(4 KiB, 25% of layer2_bytes), remaining_bytes)`
/// - Under production compaction pressure (when Layer 2 had to degrade/truncate):
///   shrinks to a tiny local-only floor so Layer 3 never starves modified production code.
#[cfg(feature = "tree-sitter")]
pub(super) fn compute_layer3_budget(
    max_patch_bytes: usize,
    layer2_bytes: usize,
    remaining_bytes: usize,
    prod_compacted: bool,
) -> usize {
    if remaining_bytes < 256 {
        return 0;
    }
    let pct_cap = (max_patch_bytes * 15) / 100;
    let rel_cap = (layer2_bytes / 4).max(4 * 1024);
    let cap = if prod_compacted {
        // Keep only a small floor for same-file types if production code was compacted
        (max_patch_bytes / 25).min(12 * 1024)
    } else {
        pct_cap.min(rel_cap)
    };
    cap.min(remaining_bytes)
}

/// Renders Layer 3 external AST context block strictly within `max_bytes`, prioritizing:
/// 1) Same-file unmodified type/const definitions (`is_local && is_type_def`)
/// 2) Cross-file pinned type definitions (`!is_local && is_type_def && pinned_by_import`)
/// 3) Cross-file function/method signatures (`!is_local && !is_type_def`)
#[cfg(feature = "tree-sitter")]
pub(super) fn render_layer3_context(
    mut entries: Vec<ContextEntry>,
    max_bytes: usize,
    local_only: bool,
) -> Option<(String, usize)> {
    if entries.is_empty() || max_bytes < 160 {
        return None;
    }
    if local_only {
        entries.retain(|e| e.is_local);
        if entries.is_empty() {
            return None;
        }
    }

    // Deduplicate by (file_path, symbol_name, start_line)
    entries.sort_by(|a, b| {
        b.is_local
            .cmp(&a.is_local)
            .then_with(|| b.is_type_def.cmp(&a.is_type_def))
            .then_with(|| b.pinned_by_import.cmp(&a.pinned_by_import))
            .then_with(|| a.file_path.cmp(&b.file_path))
            .then_with(|| a.start_line.cmp(&b.start_line))
    });
    let mut seen_keys: HashSet<(PathBuf, String, usize)> = HashSet::new();
    entries
        .retain(|e| seen_keys.insert((e.file_path.clone(), e.symbol_name.clone(), e.start_line)));

    // Pack highest-priority entries first within `max_bytes`, then group selected entries by file
    let header = "\n=== External AST Context (1-hop, unmodified) ===\n";
    let mut used = header.len();
    let mut selected: Vec<ContextEntry> = Vec::new();
    let mut selected_full_type_ranges: Vec<(PathBuf, usize, usize)> = Vec::new();
    let mut file_headers_seen: HashSet<(bool, PathBuf)> = HashSet::new();

    for e in entries {
        // Deduplicate method signatures whose full enclosing class/struct (<= 18 lines, unstripped)
        // is already included verbatim in Layer 3 for the same file.
        if !e.is_type_def
            && selected_full_type_ranges
                .iter()
                .any(|(fp, s, en)| *fp == e.file_path && e.start_line >= *s && e.end_line <= *en)
        {
            continue;
        }
        let fh_cost = if file_headers_seen.contains(&(e.is_local, e.file_path.clone())) {
            0
        } else {
            let scope_tag = if e.is_local {
                "same-file"
            } else {
                "cross-file"
            };
            format!("--- {scope_tag}: {} ---\n", e.file_path.display()).len()
        };
        let item_text = format_context_entry(&e);
        if used + fh_cost + item_text.len() > max_bytes {
            continue;
        }
        used += fh_cost + item_text.len();
        file_headers_seen.insert((e.is_local, e.file_path.clone()));
        if e.is_type_def && e.end_line.saturating_sub(e.start_line) < 18 {
            selected_full_type_ranges.push((e.file_path.clone(), e.start_line, e.end_line));
        }
        selected.push(e);
    }

    if selected.is_empty() {
        return None;
    }

    let count = selected.len();
    // Group by (same-file first, file_path) for clean, readable output
    let mut grouped: BTreeMap<(u8, String), Vec<ContextEntry>> = BTreeMap::new();
    for e in selected {
        let rank = if e.is_local { 0 } else { 1 };
        let key = e.file_path.to_string_lossy().replace('\\', "/");
        grouped.entry((rank, key)).or_default().push(e);
    }

    let mut out = String::with_capacity(used + 32);
    out.push_str(header);
    for ((rank, path_str), mut list) in grouped {
        let scope_tag = if rank == 0 { "same-file" } else { "cross-file" };
        out.push_str(&format!("--- {scope_tag}: {path_str} ---\n"));
        list.sort_by_key(|e| e.start_line);
        for e in list {
            out.push_str(&format_context_entry(&e));
        }
    }

    if out.len() <= max_bytes {
        Some((out, count))
    } else {
        // Hard safety guard (UTF-8 boundary safe)
        let cut = truncate_utf8_bytes(&out, max_bytes.saturating_sub(1));
        Some((format!("{cut}\n"), count))
    }
}

#[cfg(feature = "tree-sitter")]
fn format_context_entry(e: &ContextEntry) -> String {
    if e.is_type_def {
        format!("// L{}-L{}\n{}\n", e.start_line, e.end_line, e.snippet)
    } else {
        format!("L{}: {}\n", e.start_line, e.snippet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hybrid_config_parse_and_ablation_modes() {
        assert_eq!(HybridConfig::default(), HybridConfig::full());
        assert_eq!(HybridConfig::parse(None), HybridConfig::full());
        assert_eq!(HybridConfig::parse(Some("")), HybridConfig::full());
        assert_eq!(HybridConfig::parse(Some("off")), HybridConfig::off());
        assert_eq!(HybridConfig::parse(Some("0")), HybridConfig::off());
        assert_eq!(HybridConfig::parse(Some("1")), HybridConfig::full());
        assert_eq!(HybridConfig::parse(Some("all")), HybridConfig::full());

        let b1 = HybridConfig::parse(Some("l1,l2"));
        assert!(b1.layer1 && b1.layer2 && !b1.layer3_local && !b1.layer3_cross);
        assert_eq!(b1.scope_sep(), " › ");
        assert_eq!(HybridConfig::off().scope_sep(), " > ");

        let b2 = HybridConfig::parse(Some("l1+l2+l3_local"));
        assert!(b2.layer1 && b2.layer2 && b2.layer3_local && !b2.layer3_cross);

        let l3 = HybridConfig::parse(Some("l3"));
        assert!(!l3.layer1 && !l3.layer2 && l3.layer3_local && l3.layer3_cross);
    }

    #[test]
    fn test_truncate_utf8_bytes_multibyte_safety() {
        let s = "한글_심볼_이름_테스트_🚀_end";
        for cap in 0..=s.len() {
            let cut = truncate_utf8_bytes(s, cap);
            assert!(cut.len() <= cap);
            assert!(s.starts_with(cut));
        }
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn test_hybrid_end_to_end_sig_only_clamped_params_decorators_and_layer3() {
        use crate::tree_sitter_diff::generate_expanded_diff_with_config;
        use std::process::Command;

        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let run = |args: &[&str]| {
            let st = Command::new("git")
                .args(args)
                .current_dir(repo)
                .status()
                .unwrap();
            assert!(st.success(), "git {:?} failed", args);
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "core.autocrlf", "false"]);

        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(
            repo.join("src/dep.rs"),
            "pub struct ExternalToken { pub ttl_ms: u64 }\n\
             /// Verifies token expiry in milliseconds.\n\
             pub fn verify_token(tok: &ExternalToken, strict: bool) -> bool { strict && tok.ttl_ms > 0 }\n",
        )
        .unwrap();

        let body: String = (0..35)
            .map(|i| format!("    let x_{i} = v + {i};\n"))
            .collect();
        let type_body: String = (0..35)
            .map(|i| format!("    let t_{i} = v as usize + {i};\n"))
            .collect();
        let main_base = format!(
            "use crate::dep::{{ExternalToken, verify_token}};\n\
             pub const MAX_RETRIES: u8 = 3;\n\
             pub struct LocalCfg {{ pub retries: u8 }}\n\
             fn long_helper(v: usize) -> usize {{\n{body}    v\n}}\n\
             pub fn type_changed_fn(v: u8) -> usize {{\n{type_body}    v as usize\n}}\n\
             pub fn check_req(cfg: &LocalCfg, tok: &ExternalToken) -> bool {{\n\
                 verify_token(tok, cfg.retries < MAX_RETRIES)\n\
             }}\n"
        );
        std::fs::write(repo.join("src/main.rs"), &main_base).unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);

        let main_mod = main_base
            .replace(
                "fn long_helper(v: usize)",
                "pub(crate) fn long_helper(v: usize)",
            )
            .replace(
                "pub fn type_changed_fn(v: u8) -> usize",
                "pub fn type_changed_fn(v: i32) -> usize",
            )
            .replace(
                "verify_token(tok, cfg.retries < MAX_RETRIES)",
                "verify_token(tok, false)",
            );
        std::fs::write(repo.join("src/main.rs"), &main_mod).unwrap();

        let mut w_a = Vec::new();
        let base_out = generate_expanded_diff_with_config(
            repo,
            GitDiffSource::Unstaged,
            512 * 1024,
            HybridConfig::off(),
            &mut w_a,
        )
        .unwrap();

        let mut w_b = Vec::new();
        let hybrid_out = generate_expanded_diff_with_config(
            repo,
            GitDiffSource::Unstaged,
            512 * 1024,
            HybridConfig::full(),
            &mut w_b,
        )
        .unwrap();

        // Baseline expands full 35-line body and has no Layer 3
        assert!(base_out.contains("let x_34 = v + 34;"));
        assert!(!base_out.contains("=== External AST Context"));

        // Hybrid suppresses pure visibility-only unchanged body...
        assert!(hybrid_out.contains("[sig-only; body unchanged:"));
        assert!(!hybrid_out.contains("let x_34 = v + 34;"));
        // ...but NEVER folds a parameter-type signature change (body invariants stay visible)
        assert!(hybrid_out.contains("let t_34 = v as usize + 34;"));
        // Layer 3 extracts local types, deleted-line constants, cross-file types & signatures + doc comment
        assert!(hybrid_out.contains("pub struct LocalCfg"));
        assert!(hybrid_out.contains("pub const MAX_RETRIES: u8 = 3;"));
        assert!(hybrid_out.contains("pub struct ExternalToken"));
        assert!(
            hybrid_out.contains("pub fn verify_token(tok: &ExternalToken, strict: bool) -> bool")
        );
        assert!(hybrid_out.contains("// doc: Verifies token expiry in milliseconds."));
    }

    #[test]
    fn test_strip_redundant_stat_lines_preserves_commit_message_tables_and_prose() {
        let prefix = concat!(
            "commit 1234567890abcdef\n",
            "Author: dev <dev@example.com>\n\n",
            "    feat: summary table\n\n",
            "    | Col A | Col B |\n",
            "    Note: 3 files changed in the previous design.\n\n",
            "---\n",
            " src/foo.rs | 12 +++++++-----\n",
            " src/bar.rs |  4 ++--\n",
            " 2 files changed, 9 insertions(+), 7 deletions(-)\n\n"
        );
        let stripped = strip_redundant_stat_lines(prefix);
        assert!(stripped.contains("    | Col A | Col B |"));
        assert!(stripped.contains("    Note: 3 files changed in the previous design."));
        assert!(!stripped.contains(" src/foo.rs | 12"));
        assert!(!stripped.contains("2 files changed, 9 insertions"));
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn test_builtin_import_and_iterator_all_do_not_leak_cross_file_and_alias_works() {
        use crate::tree_sitter_diff::generate_expanded_diff_with_config;
        use std::process::Command;

        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let run = |args: &[&str]| {
            let st = Command::new("git")
                .args(args)
                .current_dir(repo)
                .status()
                .unwrap();
            assert!(st.success(), "git {:?} failed", args);
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "core.autocrlf", "false"]);

        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(
            repo.join("src/domain.rs"),
            "pub type Result<T> = std::result::Result<T, String>;\n\
             pub struct ToolPermissions;\n\
             impl ToolPermissions {\n\
                 pub const fn all() -> Self { ToolPermissions }\n\
             }\n\
             pub fn real_target(x: u32) -> bool { x > 0 }\n",
        )
        .unwrap();

        let body: String = (0..30)
            .map(|i| format!("    let v_{i} = x + {i};\n"))
            .collect();
        // `src/a.rs` only imports `Result` (a builtin-filtered name) and calls `.iter().all(...)`
        std::fs::write(
            repo.join("src/a.rs"),
            "use crate::domain::Result;\n\
             pub fn check_list(items: &[u32]) -> Result<bool> {\n\
                 Ok(items.iter().all(|&x| x > 0))\n\
             }\n",
        )
        .unwrap();
        // `src/b.rs` imports `real_target as aliased_target` and also has an `extern \"C\" fn`
        std::fs::write(
            repo.join("src/b.rs"),
            format!(
                "use crate::domain::real_target as aliased_target;\n\
                 fn abi_fn(x: u32) -> u32 {{\n{body}    x\n}}\n\
                 pub fn call_alias(x: u32) -> bool {{\n\
                     aliased_target(x)\n\
                 }}\n"
            ),
        )
        .unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);

        std::fs::write(
            repo.join("src/a.rs"),
            "use crate::domain::Result;\n\
             pub fn check_list(items: &[u32]) -> Result<bool> {\n\
                 Ok(items.iter().all(|&x| x >= 1))\n\
             }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/b.rs"),
            format!(
                "use crate::domain::real_target as aliased_target;\n\
                 extern \"C\" fn abi_fn(x: u32) -> u32 {{\n{body}    x\n}}\n\
                 pub fn call_alias(x: u32) -> bool {{\n\
                     aliased_target(x + 1)\n\
                 }}\n"
            ),
        )
        .unwrap();

        let mut warnings = Vec::new();
        let out = generate_expanded_diff_with_config(
            repo,
            GitDiffSource::Unstaged,
            512 * 1024,
            HybridConfig::full(),
            &mut warnings,
        )
        .unwrap();

        // 1) `ToolPermissions::all()` must NOT leak from `.iter().all(...)` + `use crate::domain::Result`
        assert!(
            !out.contains("ToolPermissions"),
            "Unrelated ToolPermissions::all() leaked into Layer 3:\n{out}"
        );
        // 2) Aliased import `real_target as aliased_target` MUST resolve `real_target`
        assert!(
            out.contains("pub fn real_target(x: u32) -> bool"),
            "Aliased cross-file function `real_target` was not resolved:\n{out}"
        );
        // 3) Changing `fn` -> `extern \"C\" fn` must NOT be folded as `[sig-only]`
        assert!(
            out.contains("let v_29 = x + 29;"),
            "ABI change `extern \"C\" fn` must not be folded as sig-only:\n{out}"
        );
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn test_extract_short_doc_comment_python_docstring() {
        let src = "def compute_vat(amount_cents: int, rate_bps: int) -> tuple[int, int]:\n\
                   \x20\x20\x20\x20\"\"\"Returns (net_cents, tax_cents) tuple.\"\"\"\n\
                   \x20\x20\x20\x20tax = (amount_cents * rate_bps) // 10_000\n\
                   \x20\x20\x20\x20return amount_cents - tax, tax\n";
        let tree = parse_with_timeout(SupportedLanguage::Python, src).unwrap();
        let lines: Vec<&str> = src.lines().collect();
        let fn_node = tree.root_node().named_child(0).unwrap();
        let doc = extract_short_doc_comment(SupportedLanguage::Python, &fn_node, &lines, src);
        assert_eq!(
            doc.as_deref(),
            Some("Returns (net_cents, tax_cents) tuple.")
        );
    }

    #[test]
    fn test_render_layer1_index_singular_vs_plural_files() {
        let e1 = Layer1FileEntry {
            path: "src/services/billing/checkout_order_validator.rs".to_string(),
            status: 'M',
            added: 150,
            deleted: 10,
            is_large_new: false,
            tags: Vec::new(),
            symbols: vec![
                "validate_order_payload".to_string(),
                "apply_tax_rules".to_string(),
            ],
        };
        let out_one = render_layer1_index(std::slice::from_ref(&e1), 4096, 8192).unwrap();
        assert!(
            out_one.contains("[Symbol Index: 1 file, +150/-10]"),
            "Expected singular '1 file':\n{out_one}"
        );
        let mut e2 = e1.clone();
        e2.path = "src/services/billing/secondary_helper_module.rs".to_string();
        let out_two = render_layer1_index(&[e1.clone(), e2.clone()], 4096, 8192).unwrap();
        assert!(
            out_two.contains("[Symbol Index: 2 files, +300/-20]"),
            "Expected plural '2 files':\n{out_two}"
        );
        // Tight budget (>= 160 floor) that fits e1 + footer reserve but drops e2 -> `(+1 more file)` singular
        let header_and_e1 = concat!(
            "[Symbol Index: 2 files, +300/-20]\n",
            "  M src/services/billing/checkout_order_validator.rs (+150/-10) — validate_order_payload, apply_tax_rules\n"
        )
        .len();
        let out_tail = render_layer1_index(&[e1, e2], header_and_e1 + 40, 8192).unwrap();
        assert!(
            out_tail.contains("(+1 more file)\n"),
            "Expected singular '(+1 more file)':\n{out_tail}"
        );
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn test_deleted_line_rust_lifetimes_and_python_single_quoted_strings() {
        let mut rs_types = HashSet::new();
        let mut rs_quals = Vec::new();
        let mut rs_calls = HashSet::new();
        collect_refs_from_deleted_lines(
            SupportedLanguage::Rust,
            &["fn check<'a, 'b>(s: &'a str, t: &'b str, p: &LedgerPolicy, ch: '\\'') -> bool {"],
            &mut rs_types,
            &mut rs_quals,
            &mut rs_calls,
        );
        assert!(
            rs_types.contains("LedgerPolicy"),
            "Rust lifetime must NOT skip rest of deleted line: types={rs_types:?}"
        );

        let mut py_types = HashSet::new();
        let mut py_quals = Vec::new();
        let mut py_calls = HashSet::new();
        collect_refs_from_deleted_lines(
            SupportedLanguage::Python,
            &["msg = 'Missing LedgerPolicy configuration'"],
            &mut py_types,
            &mut py_quals,
            &mut py_calls,
        );
        assert!(
            !py_types.contains("LedgerPolicy"),
            "Python single-quoted string prose must still be skipped: types={py_types:?}"
        );
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn test_long_single_line_doc_comment_extracts_top_summary() {
        let src = "\
/// Top summary first sentence.
/// Top summary second sentence.
///
/// # Examples
///
/// ```
/// let example = run_thing(1);
/// assert!(example > 0);
/// ```
pub fn run_thing(x: i32) -> i32 { x + 1 }
";
        let tree = parse_with_timeout(SupportedLanguage::Rust, src).unwrap();
        let lines: Vec<&str> = src.lines().collect();
        let mut cursor = tree.root_node().walk();
        let fn_node = tree
            .root_node()
            .named_children(&mut cursor)
            .find(|n| n.kind() == "function_item")
            .expect("expected function_item");
        let doc = extract_short_doc_comment(SupportedLanguage::Rust, &fn_node, &lines, src);
        assert_eq!(
            doc.as_deref(),
            Some("Top summary first sentence. Top summary second sentence.")
        );
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn test_chained_calls_imports_doc_line_numbers_and_cat_file_snapshots() {
        use crate::tree_sitter_diff::generate_expanded_diff_with_config;
        use std::process::Command;

        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let run = |args: &[&str]| {
            let st = Command::new("git")
                .args(args)
                .current_dir(repo)
                .status()
                .unwrap();
            assert!(st.success(), "git {:?} failed", args);
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "core.autocrlf", "false"]);

        // 1. Rust chained calls + leading doc line number alignment + Staged vs Unstaged snapshot isolation
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(
            repo.join("src/client.rs"),
            "/// Order gateway configuration.\n\
             #[derive(Debug, Clone)]\n\
             pub struct GatewayCfg { pub retries: u32 }\n\
             pub struct OrderClient { cfg: GatewayCfg }\n\
             impl OrderClient {\n\
                 pub fn new() -> Self { Self { cfg: GatewayCfg { retries: 1 } } }\n\
                 pub fn send_order(&self, id: u64) -> bool { id > 0 }\n\
             }\n\
             pub fn call_api<T>(v: u64) -> bool { v > 0 }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/caller.rs"),
            "use crate::client::{OrderClient, call_api};\n\
             pub fn submit(id: u64) -> bool {\n\
                 let _ok = OrderClient::new().send_order(id);\n\
                 call_api::<crate::client::GatewayCfg>(id)\n\
             }\n",
        )
        .unwrap();

        // 2. Python multiline `#` import & TS mixed/namespace imports
        std::fs::write(
            repo.join("src/py_policy.py"),
            "class RefundRule:\n    limit: int = 100\n\nclass AuditLog:\n    enabled: bool = True\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/py_app.py"),
            "from py_policy import (\n\
                 RefundRule, # primary rule\n\
                 AuditLog,\n\
             )\n\
             def apply_policy(x: int) -> bool:\n\
                 return x > 0\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/ts_service.ts"),
            "export class DefaultRouter {\n    route(p: string): boolean { return p.length > 0; }\n}\n\
             export interface NamedOpt { timeoutMs: number; }\n\
             export function dispatchNs(x: number): number { return x * 2; }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/ts_ns.ts"),
            "export function nsHelper(v: number): number { return v + 10; }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/ts_app.ts"),
            "import DefaultRouter, { NamedOpt } from './ts_service';\n\
             import * as ns from './ts_ns';\n\
             export function handle(opt: NamedOpt): number {\n\
                 const _r = new DefaultRouter();\n\
                 return ns.nsHelper(opt.timeoutMs);\n\
             }\n",
        )
        .unwrap();

        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);

        // Stage changes for Staged & Commit test
        std::fs::write(
            repo.join("src/caller.rs"),
            "use crate::client::{OrderClient, call_api};\n\
             pub fn submit(id: u64) -> bool {\n\
                 let _ok = crate::client::OrderClient::new().send_order(id + 1);\n\
                 crate::client::call_api::<crate::client::GatewayCfg>(id + 1)\n\
             }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/py_app.py"),
            "from py_policy import (\n\
                 RefundRule, # primary rule\n\
                 AuditLog,\n\
             )\n\
             def apply_policy(x: int) -> bool:\n\
                 _ = AuditLog()\n\
                 return x > 0\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/ts_app.ts"),
            "import DefaultRouter, { NamedOpt } from './ts_service';\n\
             import * as ns from './ts_ns';\n\
             export function handle(opt: NamedOpt): number {\n\
                 const router = new DefaultRouter();\n\
                 return router.route('ok') ? ns.nsHelper(opt.timeoutMs + 1) : 0;\n\
             }\n",
        )
        .unwrap();
        run(&["add", "src/caller.rs", "src/py_app.py", "src/ts_app.ts"]);

        // Add DIRTY worktree-only marker to src/client.rs AFTER `git add` so Staged/Commit must NOT see it!
        std::fs::write(
            repo.join("src/client.rs"),
            "/// DIRTY_WORKTREE_ONLY_DOC\n\
             pub struct GatewayCfg { pub retries: u32 }\n\
             pub struct OrderClient { cfg: GatewayCfg }\n\
             impl OrderClient {\n\
                 pub fn new() -> Self { Self { cfg: GatewayCfg { retries: 999 } } }\n\
                 pub fn send_order(&self, id: u64) -> bool { id > 999 }\n\
             }\n\
             pub fn call_api<T>(v: u64) -> bool { v > 999 }\n",
        )
        .unwrap();

        let mut w_staged = Vec::new();
        let staged_out = generate_expanded_diff_with_config(
            repo,
            GitDiffSource::Staged,
            512 * 1024,
            HybridConfig::full(),
            &mut w_staged,
        )
        .unwrap();

        // Snapshot isolation: git cat-file --batch read index (:0:src/client.rs), NOT dirty worktree
        assert!(
            !staged_out.contains("DIRTY_WORKTREE_ONLY_DOC") && !staged_out.contains("999"),
            "GitDiffSource::Staged must read from git cat-file --batch index, not worktree:\n{staged_out}"
        );
        // Chained call & turbofish resolution
        assert!(
            staged_out.contains("send_order(&self, id: u64) -> bool"),
            "Chained `OrderClient::new().send_order(...)` must resolve into Layer 3:\n{staged_out}"
        );
        assert!(
            staged_out.contains("pub fn call_api<T>(v: u64) -> bool"),
            "Turbofish `call_api::<GatewayCfg>(...)` must resolve into Layer 3:\n{staged_out}"
        );
        // Layer 3 start line number must align with leading doc/attrs (L1-L3, not L3-L3)
        assert!(
            staged_out.contains("// L1-L3\n/// Order gateway configuration."),
            "Layer 3 snippet line range must start at actual leading doc row L1:\n{staged_out}"
        );
        // Python multiline `#` comment import resolved `AuditLog`
        assert!(
            staged_out.contains("class AuditLog:"),
            "Python multiline import after `# comment` must resolve `AuditLog`:\n{staged_out}"
        );
        // TS mixed `import DefaultRouter, { NamedOpt }` + `import * as ns`
        assert!(
            staged_out.contains("class DefaultRouter"),
            "TS `import DefaultRouter, {{ NamedOpt }}` must resolve DefaultRouter:\n{staged_out}"
        );
        assert!(
            staged_out.contains("interface NamedOpt"),
            "TS `import DefaultRouter, {{ NamedOpt }}` must resolve NamedOpt:\n{staged_out}"
        );
        assert!(
            staged_out.contains("function nsHelper(v: number): number"),
            "TS `import * as ns` alias must resolve `ns.nsHelper`:\n{staged_out}"
        );

        // Commit mode snapshot test (`GitDiffSource::Commit`)
        run(&["commit", "-q", "-m", "feat: staged update"]);
        let head_sha = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(repo)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let head_sha = head_sha.trim();
        let mut w_commit = Vec::new();
        let commit_out = generate_expanded_diff_with_config(
            repo,
            GitDiffSource::Commit(head_sha),
            512 * 1024,
            HybridConfig::full(),
            &mut w_commit,
        )
        .unwrap();
        assert!(
            !commit_out.contains("DIRTY_WORKTREE_ONLY_DOC") && !commit_out.contains("999"),
            "GitDiffSource::Commit must read git cat-file --batch at {head_sha}, not dirty worktree:\n{commit_out}"
        );
        assert!(
            commit_out.contains("send_order(&self, id: u64) -> bool"),
            "GitDiffSource::Commit must resolve cross-file Layer 3 via git cat-file --batch:\n{commit_out}"
        );

        // Compaction shedding test: tight budget forces Tier 3 `-U3` production compaction -> sheds cross-file Layer 3
        let mut big_body = String::new();
        for i in 0..80 {
            big_body.push_str(&format!("    let step_{i} = id + {i};\n"));
        }
        std::fs::write(
            repo.join("src/client.rs"),
            "pub struct GatewayCfg { pub retries: u32 }\n\
             pub struct OrderClient;\n\
             impl OrderClient {\n\
                 pub fn new() -> Self { Self }\n\
                 pub fn send_order(&self, id: u64) -> bool { id > 0 }\n\
             }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/heavy.rs"),
            format!(
                "use crate::client::OrderClient;\n\
                 pub fn heavy_work(id: u64) -> u64 {{\n\
                 {big_body}    let _ = OrderClient::new().send_order(id);\n\
                     step_79\n\
                 }}\n"
            ),
        )
        .unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "heavy"]);
        std::fs::write(
            repo.join("src/heavy.rs"),
            format!(
                "use crate::client::OrderClient;\n\
                 pub fn heavy_work(id: u64) -> u64 {{\n\
                 {big_body}    let _ = OrderClient::new().send_order(id + 2);\n\
                     step_79\n\
                 }}\n"
            ),
        )
        .unwrap();
        let mut w_compact = Vec::new();
        // ~1200 B fits compacted -U3 + Layer 1, forces Tier 3 prod compaction & sheds cross-file Layer 3
        let compact_out = generate_expanded_diff_with_config(
            repo,
            GitDiffSource::Unstaged,
            1200,
            HybridConfig::full(),
            &mut w_compact,
        )
        .unwrap();
        assert!(
            compact_out.len() <= 1200,
            "Compacted output {} exceeded 1200 B",
            compact_out.len()
        );
        assert!(
            !compact_out.contains("--- cross-file:"),
            "Production compaction (Tier 3) must automatically shed cross-file Layer 3:\n{compact_out}"
        );
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn test_extract_lhs_qualifiers_chaining_await_optional_and_quotes() {
        assert_eq!(
            extract_lhs_qualifiers(SupportedLanguage::Rust, "OrderClient::connect(cfg)?"),
            vec!["OrderClient", "connect"]
        );
        assert_eq!(
            extract_lhs_qualifiers(SupportedLanguage::Rust, "OrderClient::connect(cfg).await"),
            vec!["OrderClient", "connect"]
        );
        assert_eq!(
            extract_lhs_qualifiers(SupportedLanguage::Rust, "OrderClient::connect(cfg).await?"),
            vec!["OrderClient", "connect"]
        );
        assert_eq!(
            extract_lhs_qualifiers(SupportedLanguage::Rust, "self.client.await?"),
            vec!["client"]
        );
        assert_eq!(
            extract_lhs_qualifiers(SupportedLanguage::TypeScript, "getClient()?"),
            vec!["getClient"]
        );
        assert_eq!(
            extract_lhs_qualifiers(SupportedLanguage::TypeScript, "getClient()!"),
            vec!["getClient"]
        );
        assert_eq!(
            extract_lhs_qualifiers(SupportedLanguage::TypeScript, "client?"),
            vec!["client"]
        );
        assert_eq!(
            extract_lhs_qualifiers(SupportedLanguage::JavaScript, "getSvc('a)b')"),
            vec!["getSvc"]
        );
        assert_eq!(
            extract_lhs_qualifiers(SupportedLanguage::Python, "get_svc('x(y').client"),
            vec!["get_svc", "client"]
        );
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn test_ts_noise_stem_commonjs_require_wildcard_leak_and_java_method_invocation() {
        use crate::tree_sitter_diff::generate_expanded_diff_with_config;
        use std::process::Command;

        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let run = |args: &[&str]| {
            let st = Command::new("git")
                .args(args)
                .current_dir(repo)
                .status()
                .unwrap();
            assert!(st.success(), "git {:?} failed", args);
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "core.autocrlf", "false"]);

        std::fs::create_dir_all(repo.join("src/pkg")).unwrap();
        // 1. TS noise stem `./format`, CommonJS `require('./ts_ns')`, and unused 1-char `* as x`
        std::fs::write(
            repo.join("src/format.ts"),
            "export function fmtMoney(cents: number): string { return `${cents}`; }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/ts_ns.js"),
            "function nsHelper(v) { return v + 10; }\nmodule.exports = { nsHelper };\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/unused_mod.ts"),
            "export class UnusedSecretPolicy { enabled: boolean = true; }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/web.js"),
            "import * as fmt from './format';\n\
             import * as x from './unused_mod';\n\
             const api = require('./ts_ns');\n\
             export function renderTotal(cents) {\n\
                 const _unused = UnusedSecretPolicy;\n\
                 return fmt.fmtMoney(api.nsHelper(cents));\n\
             }\n",
        )
        .unwrap();
        // Remove UnusedSecretPolicy ref from web.js (only unused import remains)
        std::fs::write(
            repo.join("src/web.js"),
            "import * as fmt from './format';\n\
             import * as x from './unused_mod';\n\
             const api = require('./ts_ns');\n\
             export function renderTotal(cents) {\n\
                 return fmt.fmtMoney(api.nsHelper(cents));\n\
             }\n",
        )
        .unwrap();
        // Keep a matching file that refers to UnusedSecretPolicy? No — verify `* as x from './unused_mod'`
        // doesn't trigger pinning of `./unused_mod` even when a type named UnusedSecretPolicy exists elsewhere.

        // 2. Java `inventory.reserveStock(...)` must NOT match unrelated `OtherService.reserveStock` in same pkg
        std::fs::write(
            repo.join("src/pkg/InventoryService.java"),
            "package pkg;\n\
             public class InventoryService {\n\
                 public boolean reserveStock(String sku, int qty) { return qty > 0; }\n\
             }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/pkg/OtherService.java"),
            "package pkg;\n\
             public class OtherService {\n\
                 public boolean reserveStock(String unrelated, long token) { return token > 0; }\n\
             }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/pkg/OrderFlow.java"),
            "package pkg;\n\
             public class OrderFlow {\n\
                 public boolean submit(InventoryService inventory, String sku) {\n\
                     return inventory.reserveStock(sku, 1);\n\
                 }\n\
             }\n",
        )
        .unwrap();

        // 3. Rust `.await?.` and TS `?.`/`!.` chained calls
        std::fs::write(
            repo.join("src/rust_client.rs"),
            "pub struct AsyncClient;\n\
             impl AsyncClient {\n\
                 pub async fn connect() -> Result<Self, ()> { Ok(Self) }\n\
                 pub async fn send_order(&self, id: u64) -> Result<bool, ()> { Ok(id > 0) }\n\
             }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/rust_app.rs"),
            "use crate::rust_client::AsyncClient;\n\
             pub async fn run_submit(id: u64) -> Result<bool, ()> {\n\
                 AsyncClient::connect().await?.send_order(id).await\n\
             }\n",
        )
        .unwrap();

        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);

        std::fs::write(
            repo.join("src/web.js"),
            "import * as fmt from './format';\n\
             import * as x from './unused_mod';\n\
             const api = require('./ts_ns');\n\
             export function renderTotal(cents) {\n\
                 return fmt.fmtMoney(api.nsHelper(cents + 1));\n\
             }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/pkg/OrderFlow.java"),
            "package pkg;\n\
             public class OrderFlow {\n\
                 public boolean submit(InventoryService inventory, String sku) {\n\
                     return inventory.reserveStock(sku, 2);\n\
                 }\n\
             }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/rust_app.rs"),
            "use crate::rust_client::AsyncClient;\n\
             pub async fn run_submit(id: u64) -> Result<bool, ()> {\n\
                 AsyncClient::connect().await?.send_order(id + 1).await\n\
             }\n",
        )
        .unwrap();

        let mut warnings = Vec::new();
        let out = generate_expanded_diff_with_config(
            repo,
            GitDiffSource::Unstaged,
            512 * 1024,
            HybridConfig::full(),
            &mut warnings,
        )
        .unwrap();

        assert!(
            out.contains("function fmtMoney(cents: number): string"),
            "TS `import * as fmt from './format'` must resolve despite `format` being in builtin list:\n{out}"
        );
        assert!(
            out.contains("function nsHelper(v)"),
            "CommonJS `const api = require('./ts_ns')` must register alias for `api.nsHelper`:\n{out}"
        );
        assert!(
            !out.contains("UnusedSecretPolicy"),
            "Unused `import * as x from './unused_mod'` must not leak wildcard candidate:\n{out}"
        );
        assert!(
            out.contains("InventoryService") && out.contains("reserveStock(String sku, int qty)"),
            "Java `inventory.reserveStock` must resolve InventoryService.reserveStock:\n{out}"
        );
        assert!(
            !out.contains("OtherService"),
            "Java `inventory.reserveStock` must NOT fall through into call_refs matching OtherService:\n{out}"
        );
        assert!(
            out.contains("send_order(&self, id: u64)"),
            "Rust `.await?.` chain must extract AsyncClient/connect qualifiers for send_order:\n{out}"
        );
    }
}
