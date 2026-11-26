//! 3-Layer Hybrid diff pipeline components for 1-turn LLM code review:
//! - `HybridConfig`: configuration flags (default: `HybridConfig::full()`, with ablation presets for benchmarks)
//! - Layer 1: Compact upfront commit symbol index (+ large new-file index)
//! - Layer 3: Lightweight LSP-free Tree-sitter external AST context (same-file + 1-hop forward & reverse)

#[cfg(feature = "tree-sitter")]
use super::{
    ast_compact, check_primary_definition,
    diff_expand::{has_generated_source_header, is_lockfile_or_generated, parse_with_timeout},
    git_cmd, is_test_file_path,
    language_registry::{self, CandidateParseOutcome},
    run_cmd_with_timeout, signature, test_scope, GitDiffSource, LanguageCandidates,
    SupportedLanguage, MAX_TREESITTER_FILE_BYTES,
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
use std::sync::mpsc;
#[cfg(feature = "tree-sitter")]
use std::time::Duration;

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
    /// Layer 3-③: 1-hop reverse callers referencing modified or deleted production symbols.
    pub layer3_callers: bool,
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
            layer3_callers: false,
        }
    }

    pub const fn full() -> Self {
        Self {
            layer1: true,
            layer2: true,
            layer3_local: true,
            layer3_cross: true,
            layer3_callers: true,
        }
    }

    pub fn is_any_enabled(self) -> bool {
        self.layer1 || self.layer2 || self.layer3_local || self.layer3_cross || self.layer3_callers
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
                "l3_callers" | "l3-callers" | "callers" => cfg.layer3_callers = true,
                "l3" | "layer3" => {
                    cfg.layer3_local = true;
                    cfg.layer3_cross = true;
                    cfg.layer3_callers = true;
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
    (max_patch_bytes.saturating_mul(3) / 100).min(6 * 1024)
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
    pub(super) is_caller: bool,
    pub(super) snippet: String,
}

#[cfg(feature = "tree-sitter")]
#[derive(Clone, Debug)]
pub(super) struct ReverseTarget {
    pub(super) symbol: String,
    pub(super) def_file: PathBuf,
    pub(super) lang: SupportedLanguage,
    pub(super) is_type: bool,
    pub(super) sig_changed: bool,
    pub(super) parent_hints: Vec<String>,
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
const MAX_CROSS_FILE_PARSE_BYTES: usize = 2 * 1024 * 1024;
#[cfg(feature = "tree-sitter")]
const MAX_TYPE_SNIPPET_LINES: usize = 35;
#[cfg(feature = "tree-sitter")]
const MAX_GREP_LINE_BYTES: usize = 4 * 1024;
#[cfg(feature = "tree-sitter")]
const MAX_GREP_RAW_BYTES: usize = 16 * 1024 * 1024;
#[cfg(feature = "tree-sitter")]
const MAX_GREP_RECORDS_PER_FILE: usize = 12;
#[cfg(feature = "tree-sitter")]
const MAX_GREP_RECORDS: usize = 400;
#[cfg(feature = "tree-sitter")]
const MAX_REVERSE_TARGET_SYMBOLS: usize = 32;
#[cfg(feature = "tree-sitter")]
const MAX_CALLER_CANDIDATE_FILES: usize = 24;
#[cfg(feature = "tree-sitter")]
const MAX_CALLERS_PER_TARGET: usize = 4;
#[cfg(feature = "tree-sitter")]
const BATCH_CAT_FILE_TIMEOUT: Duration = Duration::from_secs(8);

#[cfg(feature = "tree-sitter")]
pub(super) fn normalize_path_key(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

#[cfg(feature = "tree-sitter")]
pub(super) fn extract_parent_hint_tokens(parent: Option<&str>) -> Vec<String> {
    parent
        .map(|p| {
            p.split(|c: char| !c.is_alphanumeric() && c != '_')
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
        .unwrap_or_default()
}

/// Ubiquitous stdlib / language-builtin / overly generic identifiers to skip in Layer 3 matching.
#[cfg(feature = "tree-sitter")]
fn is_noise_or_builtin_ident(name: &str) -> bool {
    let long_enough = if name.is_ascii() {
        name.len() >= 3
    } else {
        name.chars().count() >= 2
    };
    if !long_enough {
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

/// Checks whether a token is a valid UTF-8 identifier (rejecting punctuation fragments like `")"`
/// from chained calls `foo.iter().all(...)`, while supporting Unicode/non-ASCII symbols).
#[cfg(feature = "tree-sitter")]
pub(super) fn is_valid_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_alphabetic() || first == '_') {
        return false;
    }
    let core = s.strip_suffix(['?', '!']).unwrap_or(s);
    core.chars().all(|c| c.is_alphanumeric() || c == '_')
}

#[cfg(feature = "tree-sitter")]
fn looks_like_type_or_const(name: &str) -> bool {
    let long_enough = if name.is_ascii() {
        name.len() >= 3
    } else {
        name.chars().count() >= 2
    };
    long_enough && name.chars().next().is_some_and(|c| c.is_uppercase())
}

/// Checks if a primary definition AST node is a type/constant/interface definition suitable for
/// Layer 3-① (same-file unmodified context) and Layer 3-② (cross-file type definitions).
#[cfg(feature = "tree-sitter")]
pub(super) fn is_type_or_const_def_node(lang: SupportedLanguage, node: &tree_sitter::Node) -> bool {
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
        SupportedLanguage::Dart => {
            matches!(
                kind,
                "class_declaration"
                    | "mixin_declaration"
                    | "mixin_application_class"
                    | "enum_declaration"
                    | "extension_declaration"
                    | "extension_type_declaration"
                    | "type_alias"
            ) || super::is_dart_const_or_final_top_level_var(&target)
        }
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
            | "mixin_declaration"
            | "extension_declaration"
            | "extension_type_declaration"
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
        | SupportedLanguage::Cpp
        | SupportedLanguage::Dart => trimmed
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
pub(super) fn extract_short_doc_comment(
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

#[cfg(feature = "tree-sitter")]
fn derive_dart_module_stem(spec: &str) -> Option<String> {
    let clean = spec
        .split(['?', '#'])
        .next()
        .unwrap_or(spec)
        .trim()
        .trim_end_matches('/');
    let clean = clean.strip_prefix("package:").unwrap_or(clean);
    let stripped = clean.strip_suffix(".dart").unwrap_or(clean);
    let last = stripped.rsplit('/').next().unwrap_or(stripped).trim();
    if last.is_empty()
        || last == "."
        || last == ".."
        || last.contains(':')
        || !last.chars().any(|c| c.is_alphanumeric())
    {
        None
    } else {
        Some(last.to_string())
    }
}

/// Extracts imports (`import_specs`, `imported_symbols`, and `alias_map`) from a file's AST across supported languages.
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
            && s.chars().count() >= 2
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
                                            if stem.chars().any(|c| c.is_alphanumeric())
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
                                                if stem.chars().any(|c| c.is_alphanumeric())
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
            SupportedLanguage::Dart => {
                if kind == "import_or_export" {
                    let mut c = node.walk();
                    for ch in node.named_children(&mut c) {
                        if ch.kind() == "library_import" {
                            let mut ic = ch.walk();
                            for imp in ch.named_children(&mut ic) {
                                if imp.kind() != "import_specification" {
                                    continue;
                                }
                                let uri_raw = imp
                                    .child_by_field_name("uri")
                                    .and_then(|u| source.get(u.start_byte()..u.end_byte()))
                                    .map(str::trim)
                                    .unwrap_or("");
                                let Some(spec) = extract_first_quoted_str(uri_raw) else {
                                    continue;
                                };
                                if spec.starts_with("dart:") {
                                    continue;
                                }
                                let alias = imp
                                    .child_by_field_name("alias")
                                    .and_then(|a| source.get(a.start_byte()..a.end_byte()))
                                    .map(str::trim)
                                    .filter(|a| is_valid_ident(a));

                                let mut show_syms: Vec<String> = Vec::new();
                                let mut sc = imp.walk();
                                for sub in imp.named_children(&mut sc) {
                                    if sub.kind() == "combinator"
                                        && sub.child(0).is_some_and(|c| c.kind() == "show")
                                    {
                                        let mut cc = sub.walk();
                                        for id_n in sub.named_children(&mut cc) {
                                            if id_n.kind() == "identifier" {
                                                if let Some(id_s) =
                                                    source.get(id_n.start_byte()..id_n.end_byte())
                                                {
                                                    let t = id_s.trim();
                                                    if !t.is_empty() {
                                                        show_syms.push(t.to_string());
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }

                                if !show_syms.is_empty() {
                                    for sym in show_syms {
                                        record_import_entry(
                                            spec.clone(),
                                            Some(sym),
                                            false,
                                            import_specs,
                                            imported_symbols,
                                        );
                                    }
                                } else {
                                    record_import_entry(
                                        spec.clone(),
                                        None,
                                        false,
                                        import_specs,
                                        imported_symbols,
                                    );
                                }
                                if let Some(al) = alias {
                                    if let Some(stem) = derive_dart_module_stem(&spec) {
                                        alias_map.insert(al.to_string(), stem);
                                    }
                                    record_import_entry(
                                        spec,
                                        Some(al.to_string()),
                                        true,
                                        import_specs,
                                        imported_symbols,
                                    );
                                }
                            }
                        } else if ch.kind() == "library_export" {
                            if let Some(u) = ch.child_by_field_name("uri") {
                                let uri_raw =
                                    source.get(u.start_byte()..u.end_byte()).unwrap_or("");
                                if let Some(spec) = extract_first_quoted_str(uri_raw) {
                                    if !spec.starts_with("dart:") {
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
                } else if matches!(kind, "part_directive" | "part_of_directive") {
                    let uri_node = node.child_by_field_name("uri").or_else(|| {
                        let mut pc = node.walk();
                        let found = node.named_children(&mut pc).find(|ch| ch.kind() == "uri");
                        found
                    });
                    if let Some(u) = uri_node {
                        let uri_raw = source.get(u.start_byte()..u.end_byte()).unwrap_or("");
                        if let Some(spec) = extract_first_quoted_str(uri_raw) {
                            if !spec.starts_with("dart:") {
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
            SupportedLanguage::Bash | SupportedLanguage::Json => {}
        }
    }
}

#[cfg(feature = "tree-sitter")]
fn extract_first_quoted_str(raw: &str) -> Option<String> {
    let mut best: Option<(usize, char)> = None;
    for q in ['\'', '"'] {
        if let Some(idx) = raw.find(q) {
            if best.is_none_or(|(b, _)| idx < b) {
                best = Some((idx, q));
            }
        }
    }
    let (start, q) = best?;
    let rest = &raw[start + 1..];
    let end = rest.find(q)?;
    let inner = rest[..end].trim();
    if inner.is_empty() {
        None
    } else {
        Some(inner.to_string())
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

#[cfg(feature = "tree-sitter")]
fn advance_escape(s: &str, backslash_idx: usize) -> usize {
    let next_start = backslash_idx + 1;
    let next_len = s
        .get(next_start..)
        .and_then(|rest| rest.chars().next())
        .map_or(0, char::len_utf8);
    (next_start + next_len).min(s.len())
}

/// Splits a call target expression at its rightmost top-level separator (`"::"`, `"."`, or `"->"`),
/// ignoring delimiters inside `(...)`, `[...]`, `<...>`, strings/chars, and turbofish `::<`.
/// Returns `(lhs, rhs, is_static_colon_sep)`.
#[cfg(feature = "tree-sitter")]
fn split_rightmost_call_sep(lang: SupportedLanguage, raw: &str) -> Option<(&str, &str, bool)> {
    let bytes = raw.as_bytes();
    let mut i = 0usize;
    let mut paren = 0usize;
    let mut bracket = 0usize;
    let mut angle = 0usize;
    let mut best: Option<(usize, usize, bool)> = None;
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
        let ch = raw[i..].chars().next()?;
        let ch_len = ch.len_utf8();
        if ch == '"' || ch == '`' {
            let q = ch;
            i += ch_len;
            while i < bytes.len() {
                let sc = raw[i..].chars().next()?;
                if sc == '\\' {
                    i = advance_escape(raw, i);
                } else if sc == q {
                    i += sc.len_utf8();
                    break;
                } else {
                    i += sc.len_utf8();
                }
            }
            continue;
        }
        if ch == '\'' {
            if allow_char_or_lifetime {
                if let Some(lit_len) = signature::char_literal_len(&raw[i..]) {
                    i = (i + lit_len).min(bytes.len());
                    continue;
                }
                i += 1;
                while let Some(c) = raw[i..].chars().next() {
                    if c.is_alphanumeric() || c == '_' {
                        i += c.len_utf8();
                    } else {
                        break;
                    }
                }
                continue;
            } else {
                i += 1;
                while i < bytes.len() {
                    let sc = raw[i..].chars().next()?;
                    if sc == '\\' {
                        i = advance_escape(raw, i);
                    } else if sc == '\'' {
                        i += 1;
                        break;
                    } else {
                        i += sc.len_utf8();
                    }
                }
                continue;
            }
        }

        if paren == 0 && bracket == 0 && angle == 0 {
            if raw[i..].starts_with("::") {
                // Skip turbofish `::<...>` so `<...>` is attached to the method/fn on the RHS
                if bytes.get(i + 2) == Some(&b'<') {
                    i += 2;
                    continue;
                }
                best = Some((i, i + 2, true));
                i += 2;
                continue;
            }
            if raw[i..].starts_with("->") {
                best = Some((i, i + 2, false));
                i += 2;
                continue;
            }
            if ch == '.' {
                best = Some((i, i + 1, false));
                i += 1;
                continue;
            }
        }

        match ch {
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            '[' => bracket += 1,
            ']' => bracket = bracket.saturating_sub(1),
            '<' => angle += 1,
            '>' => {
                let prev = i.checked_sub(1).map(|p| bytes[p]);
                if prev != Some(b'-') && prev != Some(b'=') {
                    angle = angle.saturating_sub(1);
                }
            }
            _ => {}
        }
        i += ch_len;
    }

    best.map(|(sep_start, sep_end, is_colon)| (&raw[..sep_start], &raw[sep_end..], is_colon))
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
        if !matches!(lang, SupportedLanguage::Ruby | SupportedLanguage::Python) {
            if let Some(rest) = trimmed.strip_suffix('?') {
                trimmed = rest.trim_end();
                continue;
            }
        }
        if matches!(
            lang,
            SupportedLanguage::TypeScript
                | SupportedLanguage::Tsx
                | SupportedLanguage::JavaScript
                | SupportedLanguage::CSharp
                | SupportedLanguage::Dart
        ) {
            if let Some(rest) = trimmed.strip_suffix('!') {
                trimmed = rest.trim_end();
                continue;
            }
        }
        if trimmed == before {
            break;
        }
    }
    if trimmed.is_empty() {
        return Vec::new();
    }

    let last_ch = trimmed.chars().next_back();
    if !matches!(last_ch, Some(')') | Some(']') | Some('>')) && !trimmed.contains(['(', '[', '<']) {
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
        let Some(ch) = trimmed[i..].chars().next() else {
            break;
        };
        let ch_len = ch.len_utf8();
        if ch == '"' || ch == '`' {
            let q = ch;
            i += ch_len;
            while i < bytes.len() {
                let Some(sc) = trimmed[i..].chars().next() else {
                    break;
                };
                if sc == '\\' {
                    i = advance_escape(trimmed, i);
                } else if sc == q {
                    i += sc.len_utf8();
                    break;
                } else {
                    i += sc.len_utf8();
                }
            }
            continue;
        }
        if ch == '\'' {
            if allow_char_or_lifetime {
                if let Some(lit_len) = signature::char_literal_len(&trimmed[i..]) {
                    i = (i + lit_len).min(bytes.len());
                    continue;
                }
                i += 1;
                while let Some(c) = trimmed[i..].chars().next() {
                    if c.is_alphanumeric() || c == '_' {
                        i += c.len_utf8();
                    } else {
                        break;
                    }
                }
                continue;
            } else {
                i += 1;
                while i < bytes.len() {
                    let Some(sc) = trimmed[i..].chars().next() else {
                        break;
                    };
                    if sc == '\\' {
                        i = advance_escape(trimmed, i);
                    } else if sc == '\'' {
                        i += 1;
                        break;
                    } else {
                        i += sc.len_utf8();
                    }
                }
                continue;
            }
        }
        if paren == 0 && bracket == 0 && angle == 0 && (ch.is_alphabetic() || ch == '_') {
            let start = i;
            i += ch_len;
            while let Some(ic) = trimmed[i..].chars().next() {
                if ic.is_alphanumeric()
                    || ic == '_'
                    || (lang == SupportedLanguage::Ruby && matches!(ic, '?' | '!'))
                {
                    i += ic.len_utf8();
                } else {
                    break;
                }
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
        match ch {
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            '[' => bracket += 1,
            ']' => bracket = bracket.saturating_sub(1),
            '<' => angle += 1,
            '>' => {
                let prev = i.checked_sub(1).map(|p| bytes[p]);
                if prev != Some(b'-') && prev != Some(b'=') {
                    angle = angle.saturating_sub(1);
                }
            }
            _ => {}
        }
        i += ch_len;
    }
    out
}

/// Finds the receiver expression text of the nearest enclosing Dart `cascade` node
/// for a `cascade_call_expression` (`obj..foo()`, `obj?..foo()`).
#[cfg(feature = "tree-sitter")]
fn find_enclosing_cascade_receiver<'a>(node: &tree_sitter::Node<'a>, source: &'a str) -> &'a str {
    let mut cur = node.parent();
    while let Some(p) = cur {
        if p.kind() == "cascade" {
            return p
                .named_child(0)
                .and_then(|r| source.get(r.start_byte()..r.end_byte()))
                .map(str::trim)
                .unwrap_or("");
        }
        cur = p.parent();
    }
    ""
}

/// Scans deleted (`-`) diff lines in changed hunks for PascalCase/SCREAMING_CASE constants, types,
/// Unicode identifiers, qualified calls (`Foo::bar`, `Foo.bar`), and direct function calls so removed
/// invariants (e.g. `BPS_DENOMINATOR`, `RefundLimits.CanAutoRefund`) are also surfaced in Layer 3.
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
            let Some(ch) = trimmed[i..].chars().next() else {
                break;
            };
            let ch_len = ch.len_utf8();
            if ch == '"' || (ch == '`' && !matches!(lang, SupportedLanguage::Python)) {
                let q = ch;
                i += ch_len;
                while i < bytes.len() {
                    let Some(sc) = trimmed[i..].chars().next() else {
                        break;
                    };
                    if sc == '\\' && !(q == '`' && lang == SupportedLanguage::Go) {
                        i = advance_escape(trimmed, i);
                    } else if sc == q {
                        i += sc.len_utf8();
                        break;
                    } else {
                        i += sc.len_utf8();
                    }
                }
                prev_ident = None;
                continue;
            }
            if ch == '\'' {
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
                    if let Some(lit_len) = signature::char_literal_len(&trimmed[i..]) {
                        i = (i + lit_len).min(bytes.len());
                        prev_ident = None;
                        continue;
                    }
                    // Rust lifetime (`'a`, `'static`, `'_`) or unmatched char quote
                    i += 1;
                    while let Some(lc) = trimmed[i..].chars().next() {
                        if lc.is_alphanumeric() || lc == '_' {
                            i += lc.len_utf8();
                        } else {
                            break;
                        }
                    }
                    prev_ident = None;
                    continue;
                }
                // Python / JS / TS / Ruby single-quoted string literal
                i += 1;
                while i < bytes.len() {
                    let Some(sc) = trimmed[i..].chars().next() else {
                        break;
                    };
                    if sc == '\\' {
                        i = advance_escape(trimmed, i);
                    } else if sc == '\'' {
                        i += 1;
                        break;
                    } else {
                        i += sc.len_utf8();
                    }
                }
                prev_ident = None;
                continue;
            }
            if ch.is_alphabetic() || ch == '_' {
                let start = i;
                i += ch_len;
                while let Some(ic) = trimmed[i..].chars().next() {
                    if ic.is_alphanumeric()
                        || ic == '_'
                        || (lang == SupportedLanguage::Ruby && matches!(ic, '?' | '!'))
                    {
                        i += ic.len_utf8();
                    } else {
                        break;
                    }
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
                    if looks_like_type_or_const(tok) || (!tok.is_ascii() && !is_call) {
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
            i += ch_len;
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
                        if looks_like_type_or_const(sym_clean) || !sym_clean.is_ascii() {
                            type_refs.insert(sym_clean.to_string());
                        }
                        if is_valid_ident(q_last) {
                            qual_refs.push((q_last.to_string(), sym_clean.to_string()));
                        }
                    }
                    if is_valid_ident(q_last)
                        && (looks_like_type_or_const(q_last) || !q_last.is_ascii())
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
                if let Some((lhs, rhs, _)) = split_rightmost_call_sep(lang, raw) {
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
        } else if kind == "cascade_call_expression" && lang == SupportedLanguage::Dart {
            if let Some(prop_n) = node.child_by_field_name("property") {
                let sym = source
                    .get(prop_n.start_byte()..prop_n.end_byte())
                    .map(str::trim)
                    .unwrap_or("");
                if is_valid_ident(sym) && !is_noise_or_builtin_ident(sym) {
                    let recv = find_enclosing_cascade_receiver(&node, source);
                    let quals = extract_lhs_qualifiers(lang, recv);
                    if !quals.is_empty() {
                        for q in quals {
                            qual_refs.push((q.to_string(), sym.to_string()));
                            if looks_like_type_or_const(q) && !is_noise_or_builtin_ident(q) {
                                type_refs.insert(q.to_string());
                            }
                        }
                    } else {
                        if looks_like_type_or_const(sym) {
                            type_refs.insert(sym.to_string());
                        }
                        call_refs.insert(sym.to_string());
                    }
                }
            } else if let Some(fn_n) = node.child_by_field_name("function") {
                let raw = source
                    .get(fn_n.start_byte()..fn_n.end_byte())
                    .unwrap_or("")
                    .trim();
                if let Some((lhs, rhs, _)) = split_rightmost_call_sep(lang, raw) {
                    let sym = rhs
                        .split(['<', '('])
                        .next()
                        .unwrap_or("")
                        .trim()
                        .trim_end_matches(['?', '!']);
                    if is_valid_ident(sym) && !is_noise_or_builtin_ident(sym) {
                        let quals = extract_lhs_qualifiers(lang, lhs);
                        if !quals.is_empty() {
                            for q in quals {
                                qual_refs.push((q.to_string(), sym.to_string()));
                                if looks_like_type_or_const(q) && !is_noise_or_builtin_ident(q) {
                                    type_refs.insert(q.to_string());
                                }
                            }
                        } else {
                            call_refs.insert(sym.to_string());
                        }
                    }
                }
            }
        }

        // Capture uppercase / caseless non-ASCII identifiers (types, enums, SCREAMING_CASE constants)
        if matches!(kind, "identifier" | "constant" | "field_identifier") {
            if let Some(txt) = source.get(node.start_byte()..node.end_byte()) {
                let t = txt.trim();
                let is_non_ascii_type_or_const =
                    !t.is_ascii() && kind != "field_identifier" && is_valid_ident(t);
                if (looks_like_type_or_const(t) || is_non_ascii_type_or_const)
                    && !is_noise_or_builtin_ident(t)
                {
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
                let allow_explicit_type =
                    matches!(kind, "type_identifier" | "type_name") && is_valid_ident(t);
                if (allow_explicit_type
                    || looks_like_type_or_const(t)
                    || (!t.is_ascii() && is_valid_ident(t)))
                    && !is_noise_or_builtin_ident(t)
                {
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
                            is_caller: false,
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

/// Returns true if `p` is an eligible non-generated, non-vendor, non-test code file
/// for cross-file forward or reverse Layer 3 resolution.
#[cfg(feature = "tree-sitter")]
fn is_eligible_repo_source_path(p: &Path) -> bool {
    let s = normalize_path_key(p);
    !s.is_empty()
        && !s.starts_with("vendor/")
        && !s.contains("/vendor/")
        && !s.starts_with("node_modules/")
        && !s.contains("/node_modules/")
        && !s.starts_with("dist/")
        && !s.contains("/dist/")
        && !s.starts_with("target/")
        && !s.contains("/target/")
        && !is_lockfile_or_generated(p)
        && !is_test_file_path(p)
        && SupportedLanguage::candidates_for_path(p).is_some_and(|c| {
            c.iter()
                .any(|l| !matches!(l, SupportedLanguage::Json | SupportedLanguage::Bash))
        })
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
    let Ok(output) = run_cmd_with_timeout(cmd, Duration::from_secs(8), None) else {
        return Vec::new();
    };
    if output.timed_out || !output.status.is_some_and(|s| s.success()) {
        return Vec::new();
    }
    output
        .stdout
        .split(|&b| b == 0)
        .filter(|slice| !slice.is_empty())
        .map(|slice| {
            PathBuf::from(
                String::from_utf8_lossy(slice)
                    .trim_matches(['\r', '\n'])
                    .trim(),
            )
        })
        .filter(|p| is_eligible_repo_source_path(p))
        .collect()
}

#[cfg(feature = "tree-sitter")]
struct TrackedFileEntry {
    path: PathBuf,
    norm: String,
    candidates: Option<LanguageCandidates<'static>>,
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
            let candidates = SupportedLanguage::candidates_for_path(p);
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
            if let Some(cands) = candidates {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    let norm_stem = normalize_alnum_lower(stem);
                    if !norm_stem.is_empty() {
                        for l in cands.iter() {
                            lang_stem_to_indices
                                .entry(l)
                                .or_default()
                                .entry(norm_stem.clone())
                                .or_default()
                                .push(idx);
                        }
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
                candidates,
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
                    // Strip trailing .js/.mjs/.cjs if TS imports compiled extension
                    let base_str = base.to_string_lossy();
                    let stripped = base_str
                        .strip_suffix(".js")
                        .or_else(|| base_str.strip_suffix(".mjs"))
                        .or_else(|| base_str.strip_suffix(".cjs"))
                        .unwrap_or(&base_str)
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
            SupportedLanguage::Dart => {
                if let Some(pkg_rest) = spec.strip_prefix("package:") {
                    if let Some((pkg_name, rel_path)) = pkg_rest.split_once('/') {
                        let pkg_lib_suffix = format!("{pkg_name}/lib/{rel_path}");
                        let mut matched = false;
                        for idx in index.find_by_suffix(&pkg_lib_suffix) {
                            add_idx(&mut scored, idx, true, false, None);
                            matched = true;
                        }
                        if !matched {
                            let lib_suffix = format!("lib/{rel_path}");
                            let slash_lib_suffix = format!("/{lib_suffix}");
                            for idx in index.find_by_suffix(&lib_suffix) {
                                let cand_norm = &index.entries[idx].norm;
                                // Match root `lib/<rel_path>` or same-package subdirectory
                                // `<pkg_root>/lib/<rel_path>` (mirroring reverse matching) so
                                // third-party or differently-named pubspec imports never collide
                                // across sibling monorepo packages.
                                let allowed = cand_norm == &lib_suffix
                                    || cand_norm.strip_suffix(&slash_lib_suffix).is_some_and(
                                        |pkg_root| {
                                            src_path_norm.starts_with(&format!("{pkg_root}/"))
                                        },
                                    );
                                if allowed {
                                    add_idx(&mut scored, idx, true, false, None);
                                }
                            }
                        }
                    }
                } else {
                    let norm = normalize_rel(src_dir, spec);
                    add_rel_path(&mut scored, norm.clone(), true, false, None);
                    if norm.extension().is_none() {
                        add_rel_path(&mut scored, norm.with_extension("dart"), true, false, None);
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
                let same_lang_family = entry.candidates.is_some_and(|cands| {
                    cands.iter().any(|b| {
                        q.lang == b
                            || matches!(
                                (q.lang, b),
                                (SupportedLanguage::C, SupportedLanguage::Cpp)
                                    | (SupportedLanguage::Cpp, SupportedLanguage::C)
                            )
                    })
                });
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
/// - `Commit(sha)` / `Staged`: single batched `git cat-file --batch` pipe bounded by `BATCH_CAT_FILE_TIMEOUT`
///   and protected against `\n`/`\r` protocol injection in paths.
/// - `Unstaged`: bounded `std::fs::read` (skipping symlinks and > 1 MiB files).
#[cfg(feature = "tree-sitter")]
pub(super) fn read_snapshot_files_batch(
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
            // Filter out any paths containing `\n`/`\r` so stdin and stdout responses remain strictly 1:1.
            let valid_specs: Vec<(PathBuf, String)> = paths
                .iter()
                .filter_map(|p| {
                    let norm = normalize_path_key(p);
                    if norm.is_empty() || norm.contains('\n') || norm.contains('\r') {
                        None
                    } else {
                        Some((p.clone(), format!("{prefix}{norm}")))
                    }
                })
                .collect();
            if valid_specs.is_empty() {
                return out;
            }

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
            let specs_for_writer: Vec<String> =
                valid_specs.iter().map(|(_, spec)| spec.clone()).collect();
            let paths_for_reader: Vec<PathBuf> = valid_specs.into_iter().map(|(p, _)| p).collect();

            let writer_handle = stdin_opt.map(|mut stdin| {
                std::thread::spawn(move || {
                    for spec in specs_for_writer {
                        if writeln!(stdin, "{spec}").is_err() {
                            break;
                        }
                    }
                })
            });

            let (tx, rx) = mpsc::sync_channel(1);
            let reader_handle = stdout_opt.map(|stdout| {
                std::thread::spawn(move || {
                    let mut map = HashMap::new();
                    let mut reader = BufReader::new(stdout);
                    let mut header = String::new();
                    for p in paths_for_reader {
                        header.clear();
                        let Ok(n) = reader.read_line(&mut header) else {
                            break;
                        };
                        if n == 0 {
                            break;
                        }
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
                            let mut drain = (&mut reader).take((size + 1) as u64);
                            let _ = std::io::copy(&mut drain, &mut std::io::sink());
                            continue;
                        }
                        let mut buf = vec![0u8; size];
                        if reader.read_exact(&mut buf).is_err() {
                            break;
                        }
                        let mut lf = [0u8; 1];
                        let _ = reader.read_exact(&mut lf);
                        if !buf.contains(&0) {
                            map.insert(p, String::from_utf8_lossy(&buf).into_owned());
                        }
                    }
                    let _ = tx.send(map);
                })
            });

            match rx.recv_timeout(BATCH_CAT_FILE_TIMEOUT) {
                Ok(map) => {
                    out = map;
                    let _ = child.wait();
                    if let Some(h) = writer_handle {
                        let _ = h.join();
                    }
                    if let Some(h) = reader_handle {
                        let _ = h.join();
                    }
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    if let Some(h) = writer_handle {
                        let _ = h.join();
                    }
                    if let Some(h) = reader_handle {
                        let _ = h.join();
                    }
                }
            }
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
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Resolves Layer 3-② cross-file 1-hop AST type definitions and body-stripped function signatures.
/// Uses deterministic candidate/byte bounds (`MAX_CROSS_FILE_CANDIDATES`, `MAX_CROSS_FILE_PARSE_BYTES`)
/// and file-scoped `already_exposed` keys `(norm_path, symbol_name)`.
#[cfg(feature = "tree-sitter")]
pub(super) fn resolve_cross_file_context(
    repo_root: &Path,
    source: GitDiffSource<'_>,
    queries: &[FileRefQuery],
    already_exposed: &HashSet<(String, String)>,
    scope_sep: &'static str,
) -> Vec<ContextEntry> {
    if queries.is_empty() {
        return Vec::new();
    }

    let tracked_files = list_tracked_source_files(repo_root, source);
    if tracked_files.is_empty() {
        return Vec::new();
    }
    let repo_index = TrackedRepoIndex::build(&tracked_files);

    // Scope wanted symbols per candidate file so references in file A never cross-contaminate
    // candidate files imported only by file B.
    let mut candidate_rank: HashMap<PathBuf, CandidateWanted> = HashMap::new();

    for q in queries {
        // Only Go shares package-level unqualified functions across sibling files without imports;
        // Java/C# methods require explicit receiver/type/static-import qualification.
        let allow_same_pkg_bare_calls = q.lang == SupportedLanguage::Go;
        let mut q_types: HashSet<String> = HashSet::new();
        let mut q_direct_calls: HashSet<String> = HashSet::new();
        let mut q_pinned_calls: HashSet<String> = HashSet::new();
        let mut q_qual_calls: HashMap<String, HashSet<String>> = HashMap::new();

        for t in &q.type_or_const_refs {
            q_types.insert(t.clone());
        }
        for (qual, sym) in &q.qualified_refs {
            if looks_like_type_or_const(sym) || !sym.is_ascii() {
                q_types.insert(sym.clone());
            }
            q_qual_calls
                .entry(sym.clone())
                .or_default()
                .insert(qual.clone());
        }
        for c in &q.call_refs {
            if q.imported_symbols.contains_key(c) {
                q_direct_calls.insert(c.clone());
            } else if allow_same_pkg_bare_calls {
                q_pinned_calls.insert(c.clone());
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

    // Collect candidate matches grouped deterministically by symbol name for ambiguity filtering
    let mut matches_by_symbol: BTreeMap<String, Vec<ContextEntry>> = BTreeMap::new();
    let mut parsed_bytes = 0usize;

    for (cand_path, cw) in ranked_paths {
        let Some(content) = blobs.get(&cand_path) else {
            continue;
        };
        if parsed_bytes + content.len() > MAX_CROSS_FILE_PARSE_BYTES {
            break;
        }
        if has_generated_source_header(content, Some(&cand_path)) {
            continue;
        }
        parsed_bytes += content.len();
        let Some((lang, tree)) = language_registry::parse_for_path(&cand_path, content) else {
            continue;
        };
        if matches!(lang, SupportedLanguage::Json | SupportedLanguage::Bash) {
            continue;
        }
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
        let cand_norm = normalize_path_key(&cand_path);

        #[allow(clippy::too_many_arguments)]
        fn walk_exports(
            lang: SupportedLanguage,
            cand_path: &Path,
            cand_norm: &str,
            cand_stem_norm: &str,
            node: tree_sitter::Node,
            lines: &[&str],
            source: &str,
            inline_tests: &[ast_compact::InlineTestBlock],
            cw: &CandidateWanted,
            already_exposed: &HashSet<(String, String)>,
            scope_sep: &'static str,
            out: &mut BTreeMap<String, Vec<ContextEntry>>,
        ) {
            let n_start = node.start_position().row + 1;
            let n_end = node.end_position().row + 1;
            if inline_tests
                .iter()
                .any(|b| n_start >= b.start_line && n_end <= b.end_line)
            {
                return;
            }
            if let Some(matched) = check_primary_definition(lang, &node) {
                if let Some(name) = ast_compact::bare_def_name(lang, &matched, lines, source) {
                    if !already_exposed.contains(&(cand_norm.to_string(), name.clone()))
                        && !is_noise_or_builtin_ident(&name)
                    {
                        let is_type = is_type_or_const_def_node(lang, &matched);
                        let parent =
                            signature::extract_parent_scope_prefix(lang, &matched, lines, source);
                        let parent_tokens = extract_parent_hint_tokens(parent.as_deref());

                        let qual_matched = !is_type
                            && cw.qual_calls.get(&name).is_some_and(|quals| {
                                quals.iter().any(|q| {
                                    let q_norm = normalize_alnum_lower(q);
                                    if q_norm.is_empty() {
                                        return false;
                                    }
                                    if !cand_stem_norm.is_empty()
                                        && (!matches!(
                                            lang,
                                            SupportedLanguage::Java | SupportedLanguage::CSharp
                                        ) || cw.pinned)
                                        && q_norm == *cand_stem_norm
                                    {
                                        return true;
                                    }
                                    if !parent_tokens.is_empty() && parent_tokens.contains(&q_norm)
                                    {
                                        return true;
                                    }
                                    let is_lowercase_receiver =
                                        q.chars().next().is_some_and(|c| c.is_lowercase());
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
                                        is_caller: false,
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
                    cand_norm,
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
            &cand_norm,
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

/// Extracts non-test top-level/class-member definitions from a whole deleted file chunk (`is_deleted`)
/// as `ReverseTarget`s so unmodified external callers breaking due to a deletion are surfaced.
#[cfg(feature = "tree-sitter")]
fn complete_deleted_header_syntax(reconstructed: &str, lang: SupportedLanguage) -> Option<String> {
    let trimmed = reconstructed.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    match lang {
        SupportedLanguage::Python => {
            let last_code = trimmed
                .lines()
                .rev()
                .map(|l| l.split('#').next().unwrap_or("").trim())
                .find(|l| !l.is_empty())
                .unwrap_or("");
            if last_code.ends_with(':') {
                return Some(format!("{trimmed}\n    pass\n"));
            }
            let open_p = trimmed.matches('(').count();
            let close_p = trimmed.matches(')').count();
            if open_p > close_p {
                return Some(format!("{trimmed}):\n    pass\n"));
            }
            None
        }
        SupportedLanguage::Ruby => {
            let has_header = trimmed.lines().any(|l| {
                let t = l.trim_start();
                t.starts_with("def ") || t.starts_with("class ") || t.starts_with("module ")
            });
            if has_header {
                Some(format!("{trimmed}\nend\n"))
            } else {
                None
            }
        }
        SupportedLanguage::Rust
        | SupportedLanguage::Go
        | SupportedLanguage::C
        | SupportedLanguage::Cpp
        | SupportedLanguage::Java
        | SupportedLanguage::CSharp
        | SupportedLanguage::JavaScript
        | SupportedLanguage::TypeScript
        | SupportedLanguage::Tsx
        | SupportedLanguage::Dart => {
            let mut open_curly = 0usize;
            let mut close_curly = 0usize;
            let mut open_paren = 0usize;
            let mut close_paren = 0usize;
            for ch in trimmed.chars() {
                match ch {
                    '{' => open_curly += 1,
                    '}' => close_curly += 1,
                    '(' => open_paren += 1,
                    ')' => close_paren += 1,
                    _ => {}
                }
            }
            if open_curly > close_curly {
                let closes = "}".repeat(open_curly - close_curly);
                return Some(format!("{trimmed}\n{closes}\n"));
            }
            if open_paren > close_paren && open_curly == 0 && close_curly == 0 {
                let closes = ")".repeat(open_paren - close_paren);
                return Some(format!("{trimmed}{closes} {{\n}}\n"));
            }
            if !trimmed.ends_with([';', '}', ':']) {
                return Some(format!("{trimmed} {{\n}}\n"));
            }
            None
        }
        SupportedLanguage::Bash | SupportedLanguage::Json => None,
    }
}

#[cfg(feature = "tree-sitter")]
fn extract_deleted_targets_with_fallbacks(
    reconstructed: &str,
    initial_tree: Option<&tree_sitter::Tree>,
    path: &Path,
    lang: SupportedLanguage,
) -> Vec<ReverseTarget> {
    let mut targets = match initial_tree {
        Some(tree) => extract_reverse_targets_from_tree(reconstructed, tree, path, lang),
        None => extract_reverse_targets_from_source(reconstructed, path, lang),
    };
    if !targets.is_empty() {
        return targets;
    }
    let completed = complete_deleted_header_syntax(reconstructed, lang);
    if let Some(ref comp) = completed {
        targets = extract_reverse_targets_from_source(comp, path, lang);
        if !targets.is_empty() {
            return targets;
        }
    }
    if matches!(
        lang,
        SupportedLanguage::Java
            | SupportedLanguage::CSharp
            | SupportedLanguage::TypeScript
            | SupportedLanguage::Tsx
            | SupportedLanguage::JavaScript
            | SupportedLanguage::Dart
    ) {
        let inner = completed.as_deref().unwrap_or(reconstructed);
        let wrapped = format!("class __AnticrumbDelWrap {{\n{inner}\n}}\n");
        targets = extract_reverse_targets_from_source(&wrapped, path, lang)
            .into_iter()
            .filter(|t| t.symbol != "__AnticrumbDelWrap")
            .map(|mut t| {
                t.parent_hints.retain(|h| h != "__anticrumbdelwrap");
                t
            })
            .collect();
    }
    targets
}

#[cfg(feature = "tree-sitter")]
pub(super) fn collect_deleted_reverse_targets(
    chunk: &str,
    path: &Path,
    candidates: LanguageCandidates<'_>,
) -> Vec<ReverseTarget> {
    if is_test_file_path(path)
        || !candidates
            .iter()
            .any(|l| !matches!(l, SupportedLanguage::Json | SupportedLanguage::Bash))
    {
        return Vec::new();
    }
    let mut in_hunk = false;
    let mut reconstructed = String::new();
    for raw_line in chunk.split_inclusive('\n') {
        if raw_line.starts_with("@@ ") {
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if let Some(rest) = raw_line.strip_prefix('-') {
            if reconstructed.len() + rest.len() > MAX_TREESITTER_FILE_BYTES {
                break;
            }
            reconstructed.push_str(rest);
            if !rest.ends_with('\n') {
                reconstructed.push('\n');
            }
        } else if let Some(rest) = raw_line.strip_prefix(' ') {
            if reconstructed.len() + rest.len() > MAX_TREESITTER_FILE_BYTES {
                break;
            }
            reconstructed.push_str(rest);
            if !rest.ends_with('\n') {
                reconstructed.push('\n');
            }
        }
    }
    match candidates {
        LanguageCandidates::Exact(lang) => {
            extract_deleted_targets_with_fallbacks(&reconstructed, None, path, lang)
        }
        LanguageCandidates::Priority(_) => {
            match language_registry::parse_for_candidates(candidates, &reconstructed, &[]) {
                CandidateParseOutcome::Parsed { lang, tree }
                    if !matches!(lang, SupportedLanguage::Json | SupportedLanguage::Bash) =>
                {
                    extract_deleted_targets_with_fallbacks(&reconstructed, Some(&tree), path, lang)
                }
                _ => Vec::new(),
            }
        }
    }
}

/// Extracts non-test top-level/member definitions removed in a pure-deletion or renamed-header hunk
/// of a modified file (dedenting uniform common indentation so deleted/renamed class/module methods
/// and single-line signature headers parse cleanly).
#[cfg(feature = "tree-sitter")]
pub(super) fn collect_deleted_lines_reverse_targets(
    deleted_lines: &[&str],
    path: &Path,
    lang: SupportedLanguage,
) -> Vec<ReverseTarget> {
    if deleted_lines.is_empty()
        || is_test_file_path(path)
        || matches!(lang, SupportedLanguage::Json | SupportedLanguage::Bash)
    {
        return Vec::new();
    }
    let min_indent = deleted_lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start_matches([' ', '\t']).len())
        .min()
        .unwrap_or(0);
    let mut reconstructed = String::new();
    for l in deleted_lines {
        let stripped = l.get(min_indent..).unwrap_or(l);
        if reconstructed.len() + stripped.len() > MAX_TREESITTER_FILE_BYTES {
            break;
        }
        reconstructed.push_str(stripped);
        if !stripped.ends_with('\n') {
            reconstructed.push('\n');
        }
    }
    extract_deleted_targets_with_fallbacks(&reconstructed, None, path, lang)
}

#[cfg(feature = "tree-sitter")]
fn extract_reverse_targets_from_source(
    source: &str,
    path: &Path,
    lang: SupportedLanguage,
) -> Vec<ReverseTarget> {
    if source.trim().is_empty() {
        return Vec::new();
    }
    let Ok(tree) = parse_with_timeout(lang, source) else {
        return Vec::new();
    };
    extract_reverse_targets_from_tree(source, &tree, path, lang)
}

#[cfg(feature = "tree-sitter")]
fn extract_reverse_targets_from_tree(
    source: &str,
    tree: &tree_sitter::Tree,
    path: &Path,
    lang: SupportedLanguage,
) -> Vec<ReverseTarget> {
    if source.trim().is_empty() {
        return Vec::new();
    }
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let inline_tests = ast_compact::find_inline_test_blocks(lang, &tree.root_node(), source);
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    #[allow(clippy::too_many_arguments)]
    fn walk_deleted(
        lang: SupportedLanguage,
        path: &Path,
        node: tree_sitter::Node,
        lines: &[&str],
        source: &str,
        inline_tests: &[ast_compact::InlineTestBlock],
        seen: &mut HashSet<String>,
        out: &mut Vec<ReverseTarget>,
    ) {
        let n_start = node.start_position().row + 1;
        let n_end = node.end_position().row + 1;
        if inline_tests
            .iter()
            .any(|b| n_start >= b.start_line && n_end <= b.end_line)
        {
            return;
        }
        if let Some(matched) = check_primary_definition(lang, &node) {
            if !test_scope::is_test_scope(lang, &matched, Some(path), lines) {
                if let Some(name) = ast_compact::bare_def_name(lang, &matched, lines, source) {
                    if name != "__AnticrumbDelWrap"
                        && is_valid_ident(&name)
                        && !is_noise_or_builtin_ident(&name)
                        && seen.insert(name.clone())
                    {
                        let is_type = is_type_or_const_def_node(lang, &matched);
                        let parent =
                            signature::extract_parent_scope_prefix(lang, &matched, lines, source);
                        let parent_hints = extract_parent_hint_tokens(parent.as_deref());
                        out.push(ReverseTarget {
                            symbol: name,
                            def_file: path.to_path_buf(),
                            lang,
                            is_type,
                            sig_changed: true,
                            parent_hints,
                        });
                    }
                }
            }
            if ast_compact::is_foldable_function_node(lang, &matched)
                || !super::diff_expand::is_container_definition_node(&matched)
            {
                return;
            }
        }
        let mut cursor = node.walk();
        for ch in node.named_children(&mut cursor) {
            walk_deleted(lang, path, ch, lines, source, inline_tests, seen, out);
        }
    }

    walk_deleted(
        lang,
        path,
        tree.root_node(),
        &lines,
        source,
        &inline_tests,
        &mut seen,
        &mut out,
    );
    out
}

#[cfg(feature = "tree-sitter")]
fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// UTF-8-aware whole-identifier check (handles Korean/Unicode identifiers and Ruby `?`/`!` suffixes).
#[cfg(feature = "tree-sitter")]
pub(super) fn contains_utf8_ident(haystack: &str, needle: &str, ruby_suffix_mode: bool) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    let needle_has_suffix_punct = ruby_suffix_mode && needle.ends_with(['?', '!']);
    let mut offset = 0usize;
    while let Some(rel) = haystack[offset..].find(needle) {
        let pos = offset + rel;
        let end = pos + needle.len();
        let before_ok = haystack[..pos]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident_char(c));
        let next_ch = haystack[end..].chars().next();
        let after_ok = next_ch.is_none_or(|c| {
            if !ruby_suffix_mode || needle_has_suffix_punct {
                !is_ident_char(c)
            } else {
                !is_ident_char(c) && !matches!(c, '?' | '!')
            }
        });
        if before_ok && after_ok {
            return true;
        }
        if let Some(ch) = haystack[pos..].chars().next() {
            offset = pos + ch.len_utf8();
        } else {
            break;
        }
    }
    false
}

#[cfg(feature = "tree-sitter")]
fn stem_for_path_norm(p: &Path) -> String {
    let raw_stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    if matches!(raw_stem, "mod" | "__init__" | "index") {
        p.parent()
            .and_then(|pr| pr.file_name())
            .and_then(|s| s.to_str())
            .map(normalize_alnum_lower)
            .unwrap_or_else(|| normalize_alnum_lower(raw_stem))
    } else {
        normalize_alnum_lower(raw_stem)
    }
}

#[cfg(feature = "tree-sitter")]
fn import_spec_matches_target_file(
    cand_lang: SupportedLanguage,
    cand_path: &Path,
    spec: &str,
    target_def_file: &Path,
    target_def_norm: &str,
    target_stem_norm: &str,
) -> bool {
    let cand_dir = cand_path.parent().unwrap_or_else(|| Path::new(""));
    let normalize_rel = |base: &Path, rel: &str| -> String {
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
        parts.join("/")
    };

    match cand_lang {
        SupportedLanguage::Rust => {
            let parts: Vec<&str> = spec.split("::").filter(|s| !s.is_empty()).collect();
            if parts.is_empty() {
                return false;
            }
            if parts[0] == "mod" && parts.len() >= 2 {
                let m = parts[1];
                let p1 = normalize_rel(cand_dir, &format!("{m}.rs"));
                let p2 = normalize_rel(cand_dir, &format!("{m}/mod.rs"));
                if target_def_norm == p1 || target_def_norm == p2 {
                    return true;
                }
                if let Some(cstem) = cand_path.file_stem().and_then(|s| s.to_str()) {
                    if cstem != "mod" && cstem != "lib" && cstem != "main" {
                        let p3 = normalize_rel(cand_dir, &format!("{cstem}/{m}.rs"));
                        let p4 = normalize_rel(cand_dir, &format!("{cstem}/{m}/mod.rs"));
                        if target_def_norm == p3 || target_def_norm == p4 {
                            return true;
                        }
                    }
                }
                return false;
            }
            if parts[0] == "crate" {
                let crate_src = find_rust_crate_src_dir(cand_path);
                let segs = &parts[1..];
                for take in [segs.len(), segs.len().saturating_sub(1)] {
                    if take == 0 {
                        continue;
                    }
                    let sub: PathBuf = segs[..take].iter().collect();
                    let c1 = normalize_path_key(&crate_src.join(&sub).with_extension("rs"));
                    let c2 = normalize_path_key(&crate_src.join(&sub).join("mod.rs"));
                    if target_def_norm == c1 || target_def_norm == c2 {
                        return true;
                    }
                }
                return false;
            }
            if parts[0] == "super" {
                let mut base = cand_dir.to_path_buf();
                let mut idx = 0usize;
                if let Some(stem) = cand_path.file_stem().and_then(|s| s.to_str()) {
                    if stem != "mod" && stem != "lib" && stem != "main" {
                        if let Some(dir_name) = base.file_name().and_then(|n| n.to_str()) {
                            if let Some(parent) = base.parent() {
                                if target_def_norm
                                    == normalize_path_key(&parent.join(format!("{dir_name}.rs")))
                                {
                                    return true;
                                }
                            }
                        }
                        if target_def_norm == normalize_path_key(&base.join("mod.rs")) {
                            return true;
                        }
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
                        continue;
                    }
                    let sub: PathBuf = segs[..take].iter().collect();
                    let c1 = normalize_path_key(&base.join(&sub).with_extension("rs"));
                    let c2 = normalize_path_key(&base.join(&sub).join("mod.rs"));
                    if target_def_norm == c1 || target_def_norm == c2 {
                        return true;
                    }
                }
                return false;
            }
            if parts[0] == "self" {
                let segs = &parts[1..];
                for take in [segs.len(), segs.len().saturating_sub(1)] {
                    if take == 0 {
                        continue;
                    }
                    let sub: PathBuf = segs[..take].iter().collect();
                    let c1 = normalize_path_key(&cand_dir.join(&sub).with_extension("rs"));
                    let c2 = normalize_path_key(&cand_dir.join(&sub).join("mod.rs"));
                    if target_def_norm == c1 || target_def_norm == c2 {
                        return true;
                    }
                }
                return false;
            }
            if matches!(parts[0], "std" | "core" | "alloc" | "proc_macro" | "test")
                || target_stem_norm.is_empty()
                || parts.len() < 2
            {
                return false;
            }
            // Fallback for multi-crate paths (`subcrate::mod::Sym`), avoiding false pinning
            // on stdlib/external imports or ubiquitous file stems (`io`, `sync`, `error`, etc.)
            let is_generic_stem = target_stem_norm.len() < 4
                || matches!(
                    target_stem_norm,
                    "mod"
                        | "lib"
                        | "main"
                        | "error"
                        | "errors"
                        | "types"
                        | "utils"
                        | "util"
                        | "helpers"
                        | "helper"
                        | "common"
                        | "config"
                        | "sync"
                        | "fmt"
                        | "time"
                        | "task"
                        | "test"
                );
            let mod_segs = if parts
                .last()
                .is_some_and(|p| p.chars().next().is_some_and(|c| c.is_uppercase()))
            {
                &parts[..parts.len() - 1]
            } else {
                &parts[..]
            };
            let target_dir_norms: Vec<String> = target_def_file
                .parent()
                .map(|p| {
                    p.components()
                        .filter_map(|c| match c {
                            std::path::Component::Normal(s) => {
                                let n = normalize_alnum_lower(&s.to_string_lossy());
                                (!n.is_empty() && n != "src").then_some(n)
                            }
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            let crate_root_norm = normalize_alnum_lower(parts[0]);
            let has_crate_or_parent_dir_match =
                !crate_root_norm.is_empty() && target_dir_norms.contains(&crate_root_norm);
            let is_crate_lib = target_def_file.file_stem().and_then(|s| s.to_str()) == Some("lib")
                && has_crate_or_parent_dir_match;
            if is_crate_lib {
                return true;
            }
            if is_generic_stem {
                has_crate_or_parent_dir_match
                    && mod_segs
                        .iter()
                        .any(|p| normalize_alnum_lower(p) == target_stem_norm)
            } else {
                mod_segs[1..]
                    .iter()
                    .any(|p| normalize_alnum_lower(p) == target_stem_norm)
                    || (has_crate_or_parent_dir_match
                        && mod_segs
                            .iter()
                            .any(|p| normalize_alnum_lower(p) == target_stem_norm))
            }
        }
        SupportedLanguage::Python => {
            if spec.starts_with('.') {
                let dots = spec.chars().take_while(|&c| c == '.').count();
                let rest = &spec[dots..];
                let mut base = cand_dir.to_path_buf();
                for _ in 1..dots {
                    base.pop();
                }
                if rest.is_empty() {
                    if !target_stem_norm.is_empty() {
                        let target_dir = target_def_file.parent().unwrap_or_else(|| Path::new(""));
                        return normalize_path_key(target_dir) == normalize_path_key(&base);
                    }
                    return target_def_norm == normalize_path_key(&base.join("__init__.py"));
                }
                let sub: PathBuf = rest.split('.').filter(|s| !s.is_empty()).collect();
                let c1 = normalize_path_key(&base.join(&sub).with_extension("py"));
                let c2 = normalize_path_key(&base.join(&sub).join("__init__.py"));
                target_def_norm == c1 || target_def_norm == c2
            } else {
                let slash = spec.replace('.', "/");
                target_def_norm.ends_with(&format!("{slash}.py"))
                    || target_def_norm.ends_with(&format!("{slash}/__init__.py"))
            }
        }
        SupportedLanguage::JavaScript | SupportedLanguage::TypeScript | SupportedLanguage::Tsx => {
            if spec.starts_with('.') {
                let rel = normalize_rel(cand_dir, spec);
                if target_def_norm == rel {
                    return true;
                }
                let stripped = rel
                    .strip_suffix(".js")
                    .or_else(|| rel.strip_suffix(".mjs"))
                    .or_else(|| rel.strip_suffix(".cjs"))
                    .unwrap_or(&rel);
                for ext in ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"] {
                    if target_def_norm == format!("{stripped}.{ext}")
                        || target_def_norm == format!("{rel}/index.{ext}")
                    {
                        return true;
                    }
                }
                false
            } else if let Some(alias_rest) =
                spec.strip_prefix("@/").or_else(|| spec.strip_prefix("~/"))
            {
                ["ts", "tsx", "js", "jsx"].iter().any(|ext| {
                    target_def_norm.ends_with(&format!("{alias_rest}.{ext}"))
                        || target_def_norm.ends_with(&format!("{alias_rest}/index.{ext}"))
                })
            } else {
                false
            }
        }
        SupportedLanguage::Go => {
            let target_dir = target_def_file
                .parent()
                .map(normalize_path_key)
                .unwrap_or_default();
            if target_dir.is_empty() {
                return false;
            }
            let parts: Vec<&str> = spec.split('/').filter(|s| !s.is_empty()).collect();
            (0..parts.len().min(3)).any(|skip| {
                let tail = parts[skip..].join("/");
                target_dir == tail || target_dir.ends_with(&format!("/{tail}"))
            })
        }
        SupportedLanguage::Java => {
            let clean = spec.strip_suffix(".*").unwrap_or(spec);
            let slash = clean.replace('.', "/");
            target_def_norm.ends_with(&format!("{slash}.java"))
                || (spec.ends_with(".*") && target_def_norm.contains(&format!("{slash}/")))
        }
        SupportedLanguage::CSharp => {
            let target_dir = target_def_file
                .parent()
                .map(normalize_path_key)
                .unwrap_or_default();
            let last_ns = spec.rsplit('.').next().unwrap_or(spec);
            !last_ns.is_empty()
                && target_dir
                    .split('/')
                    .any(|seg| seg.eq_ignore_ascii_case(last_ns))
        }
        SupportedLanguage::C | SupportedLanguage::Cpp => {
            let rel = normalize_rel(cand_dir, spec);
            if target_def_norm == rel || target_def_norm.ends_with(&format!("/{spec}")) {
                return true;
            }
            // In C/C++, callers `#include "foo.h"` when implementation changes happen in `foo.c`/`foo.cpp`
            let spec_stem = Path::new(spec)
                .file_stem()
                .and_then(|s| s.to_str())
                .map(normalize_alnum_lower)
                .unwrap_or_default();
            !spec_stem.is_empty() && spec_stem == target_stem_norm
        }
        SupportedLanguage::Ruby => {
            if let Some(rel) = spec.strip_prefix("./") {
                let n = normalize_rel(cand_dir, rel);
                target_def_norm == n || target_def_norm == format!("{n}.rb")
            } else {
                let rb = if spec.ends_with(".rb") {
                    spec.to_string()
                } else {
                    format!("{spec}.rb")
                };
                target_def_norm == rb || target_def_norm.ends_with(&format!("/{rb}"))
            }
        }
        SupportedLanguage::Dart => {
            if let Some(pkg_rest) = spec.strip_prefix("package:") {
                if let Some((pkg_name, rel_path)) = pkg_rest.split_once('/') {
                    let pkg_lib = format!("{pkg_name}/lib/{rel_path}");
                    if target_def_norm == pkg_lib
                        || target_def_norm.ends_with(&format!("/{pkg_lib}"))
                    {
                        return true;
                    }
                    let lib_rel = format!("lib/{rel_path}");
                    if target_def_norm == lib_rel {
                        return true;
                    }
                    // For a single-package app living inside a subdirectory whose directory name
                    // differs from its pubspec package name (e.g. `mobile/lib/...`), still allow
                    // intra-package matches while rejecting cross-package monorepo collisions.
                    if let Some(target_pkg_root) =
                        target_def_norm.strip_suffix(&format!("/{lib_rel}"))
                    {
                        let cand_norm = normalize_path_key(cand_path);
                        return cand_norm.starts_with(&format!("{target_pkg_root}/"));
                    }
                    false
                } else {
                    false
                }
            } else {
                let rel = normalize_rel(cand_dir, spec);
                target_def_norm == rel || target_def_norm == format!("{rel}.dart")
            }
        }
        SupportedLanguage::Bash | SupportedLanguage::Json => false,
    }
}

#[cfg(feature = "tree-sitter")]
struct ReverseGrepRecord {
    path: PathBuf,
    matched_syms: Vec<String>,
}

#[cfg(feature = "tree-sitter")]
struct GrepBatchResult {
    records: Vec<ReverseGrepRecord>,
    capped_early: bool,
}

#[cfg(feature = "tree-sitter")]
enum GrepLineRead {
    Line(usize),
    Eof(usize),
    RawLimitExceeded,
}

#[cfg(feature = "tree-sitter")]
fn read_grep_line_bounded<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_line_bytes: usize,
    max_raw_bytes: usize,
) -> std::io::Result<GrepLineRead> {
    buf.clear();
    let mut consumed_total = 0usize;
    let mut nul_count = 0usize;
    let mut text_kept = 0usize;
    // Allow up to 4 KiB for `path\0line_no\0` metadata plus up to `max_line_bytes` of matched source text.
    let hard_buf_cap = max_line_bytes.saturating_add(4096);

    loop {
        let avail = match reader.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if avail.is_empty() {
            return Ok(GrepLineRead::Eof(consumed_total));
        }
        let nl_opt = avail.iter().position(|&b| b == b'\n');
        let step_len = nl_opt.map_or(avail.len(), |p| p + 1);
        if consumed_total.saturating_add(step_len) > max_raw_bytes {
            return Ok(GrepLineRead::RawLimitExceeded);
        }
        let copy_slice = match nl_opt {
            Some(p) => &avail[..p],
            None => avail,
        };
        for &b in copy_slice {
            if b == 0 && nul_count < 2 {
                nul_count += 1;
                if buf.len() < hard_buf_cap {
                    buf.push(b);
                }
                continue;
            }
            if nul_count < 2 {
                if buf.len() < 4096 {
                    buf.push(b);
                }
            } else if text_kept < max_line_bytes && buf.len() < hard_buf_cap {
                buf.push(b);
                text_kept += 1;
            }
        }
        consumed_total += step_len;
        reader.consume(step_len);
        if nl_opt.is_some() {
            return Ok(GrepLineRead::Line(consumed_total));
        }
    }
}

#[cfg(feature = "tree-sitter")]
fn run_git_grep_for_symbols(
    repo_root: &Path,
    source: GitDiffSource<'_>,
    syms: &[&str],
    word_boundary: bool,
    modified_files_norm: &HashSet<String>,
) -> GrepBatchResult {
    if syms.is_empty() {
        return GrepBatchResult {
            records: Vec::new(),
            capped_early: false,
        };
    }
    let mut cmd = git_cmd(repo_root);
    cmd.args(["grep", "-n", "-z", "-F", "-I"]);
    if word_boundary {
        cmd.arg("-w");
    }
    if matches!(source, GitDiffSource::Staged) {
        cmd.arg("--cached");
    }
    for s in syms {
        cmd.arg("-e").arg(s);
    }
    if let GitDiffSource::Commit(sha) = source {
        cmd.arg(sha);
    }
    cmd.arg("--")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let Ok(mut child) = cmd.spawn() else {
        return GrepBatchResult {
            records: Vec::new(),
            capped_early: false,
        };
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return GrepBatchResult {
            records: Vec::new(),
            capped_early: false,
        };
    };

    let sha_prefix = match source {
        GitDiffSource::Commit(sha) => Some(format!("{sha}:")),
        GitDiffSource::Staged | GitDiffSource::Unstaged => None,
    };
    let owned_syms: Vec<String> = syms.iter().map(|s| (*s).to_string()).collect();
    let mod_norm_cloned = modified_files_norm.clone();

    let (tx, rx) = mpsc::sync_channel(1);
    let reader_handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line_buf = Vec::with_capacity(1024);
        let mut records = Vec::new();
        let mut raw_bytes_read = 0usize;
        let mut capped_early = false;
        let mut per_file_counts: HashMap<String, usize> = HashMap::new();
        let mut per_file_syms: HashMap<String, HashSet<String>> = HashMap::new();

        loop {
            let rem_raw = MAX_GREP_RAW_BYTES.saturating_sub(raw_bytes_read);
            if rem_raw == 0 {
                capped_early = true;
                break;
            }
            match read_grep_line_bounded(&mut reader, &mut line_buf, MAX_GREP_LINE_BYTES, rem_raw) {
                Ok(GrepLineRead::Line(n)) => {
                    raw_bytes_read += n;
                }
                Ok(GrepLineRead::Eof(n)) => {
                    raw_bytes_read += n;
                    if line_buf.is_empty() {
                        break;
                    }
                }
                Ok(GrepLineRead::RawLimitExceeded) => {
                    capped_early = true;
                    break;
                }
                Err(_) => break,
            }

            if line_buf.is_empty() {
                continue;
            }
            if line_buf.last() == Some(&b'\r') {
                line_buf.pop();
            }

            let mut fields = line_buf.splitn(3, |&b| b == 0);
            let Some(path_b) = fields.next() else {
                continue;
            };
            let Some(lno_b) = fields.next() else {
                continue;
            };
            let Some(text_b) = fields.next() else {
                continue;
            };
            let path_raw = String::from_utf8_lossy(path_b);
            let rel_str = match &sha_prefix {
                Some(pref) => path_raw.strip_prefix(pref).unwrap_or(&path_raw),
                None => &path_raw,
            };
            if rel_str.is_empty() {
                continue;
            }
            let Ok(line_no) = std::str::from_utf8(lno_b).unwrap_or("").parse::<usize>() else {
                continue;
            };
            if line_no == 0 {
                continue;
            }

            // Pre-filter modified/test/docs/vendor/generated paths and non-matching lines BEFORE
            // counting against `MAX_GREP_RECORDS` or per-file quotas.
            let path = PathBuf::from(rel_str);
            let p_norm = normalize_path_key(&path);
            if mod_norm_cloned.contains(&p_norm) || !is_eligible_repo_source_path(&path) {
                continue;
            }

            let is_ruby = SupportedLanguage::candidates_for_path(&path)
                .is_some_and(|c| c.iter().any(|l| l == SupportedLanguage::Ruby));
            let line_text = String::from_utf8_lossy(text_b);
            let mut matched_syms = Vec::new();
            for sym in &owned_syms {
                if contains_utf8_ident(&line_text, sym, is_ruby) {
                    matched_syms.push(sym.clone());
                }
            }
            if matched_syms.is_empty() {
                continue;
            }

            let f_count = per_file_counts.entry(p_norm.clone()).or_insert(0);
            let seen_syms = per_file_syms.entry(p_norm).or_default();
            let introduces_new_sym = matched_syms.iter().any(|s| !seen_syms.contains(s));
            if *f_count >= MAX_GREP_RECORDS_PER_FILE
                && (!introduces_new_sym || *f_count >= MAX_GREP_RECORDS_PER_FILE + 8)
            {
                continue;
            }
            *f_count += 1;
            for s in &matched_syms {
                seen_syms.insert(s.clone());
            }

            records.push(ReverseGrepRecord { path, matched_syms });
            if records.len() >= MAX_GREP_RECORDS {
                capped_early = true;
                break;
            }
        }
        let _ = tx.send(GrepBatchResult {
            records,
            capped_early,
        });
    });

    match rx.recv_timeout(Duration::from_secs(6)) {
        Ok(res) => {
            if res.capped_early {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader_handle.join();
                res
            } else {
                let exit_ok = child
                    .wait()
                    .ok()
                    .is_some_and(|st| matches!(st.code(), Some(0) | Some(1)));
                let _ = reader_handle.join();
                if exit_ok {
                    res
                } else {
                    GrepBatchResult {
                        records: Vec::new(),
                        capped_early: false,
                    }
                }
            }
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader_handle.join();
            GrepBatchResult {
                records: Vec::new(),
                capped_early: false,
            }
        }
    }
}

/// Resolves Layer 3-③ 1-hop reverse callers referencing modified or deleted production symbols
/// across all 10 Tree-sitter code languages using snapshot-safe `git grep` + `git cat-file --batch`
/// + AST call/type-ref verification (never worktree leakage, never comments/strings/imports/tests).
#[cfg(feature = "tree-sitter")]
pub(super) fn resolve_reverse_callers_context(
    repo_root: &Path,
    source: GitDiffSource<'_>,
    targets: &[ReverseTarget],
    modified_files_norm: &HashSet<String>,
    scope_sep: &'static str,
) -> Vec<ContextEntry> {
    if targets.is_empty() {
        return Vec::new();
    }

    // Group & rank unique target symbols, prioritizing signature changes and type/const changes.
    let mut grouped: HashMap<String, Vec<ReverseTarget>> = HashMap::new();
    for t in targets {
        if is_valid_ident(&t.symbol) && !is_noise_or_builtin_ident(&t.symbol) {
            grouped.entry(t.symbol.clone()).or_default().push(t.clone());
        }
    }
    if grouped.is_empty() {
        return Vec::new();
    }

    let mut ranked_syms: Vec<(String, bool, bool)> = grouped
        .iter()
        .map(|(sym, ts)| {
            let best_sig = ts.iter().any(|t| t.sig_changed);
            let best_type = ts.iter().any(|t| t.is_type);
            (sym.clone(), best_sig, best_type)
        })
        .collect();
    ranked_syms.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked_syms.truncate(MAX_REVERSE_TARGET_SYMBOLS);

    let sym_flags: HashMap<String, (bool, bool)> = ranked_syms
        .iter()
        .map(|(s, sc, it)| (s.clone(), (*sc, *it)))
        .collect();
    let selected_syms: Vec<String> = ranked_syms.into_iter().map(|(s, _, _)| s).collect();
    let mut word_syms: Vec<&str> = Vec::new();
    let mut literal_syms: Vec<&str> = Vec::new();
    for s in &selected_syms {
        if s.is_ascii() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            word_syms.push(s.as_str());
        } else {
            literal_syms.push(s.as_str());
        }
    }

    let word_batch =
        run_git_grep_for_symbols(repo_root, source, &word_syms, true, modified_files_norm);
    let mut all_records = word_batch.records;
    if word_batch.capped_early {
        let seen_word: HashSet<String> = all_records
            .iter()
            .flat_map(|r| r.matched_syms.iter().cloned())
            .collect();
        let missed_prio: Vec<&str> = word_syms
            .iter()
            .copied()
            .filter(|s| {
                !seen_word.contains(*s) && sym_flags.get(*s).is_some_and(|&(sc, it)| sc || it)
            })
            .collect();
        if !missed_prio.is_empty() {
            all_records.extend(
                run_git_grep_for_symbols(
                    repo_root,
                    source,
                    &missed_prio,
                    true,
                    modified_files_norm,
                )
                .records,
            );
        }
    }

    let literal_batch =
        run_git_grep_for_symbols(repo_root, source, &literal_syms, false, modified_files_norm);
    let lit_offset = all_records.len();
    all_records.extend(literal_batch.records);
    if literal_batch.capped_early {
        let seen_lit: HashSet<String> = all_records[lit_offset..]
            .iter()
            .flat_map(|r| r.matched_syms.iter().cloned())
            .collect();
        let missed_prio: Vec<&str> = literal_syms
            .iter()
            .copied()
            .filter(|s| {
                !seen_lit.contains(*s) && sym_flags.get(*s).is_some_and(|&(sc, it)| sc || it)
            })
            .collect();
        if !missed_prio.is_empty() {
            all_records.extend(
                run_git_grep_for_symbols(
                    repo_root,
                    source,
                    &missed_prio,
                    false,
                    modified_files_norm,
                )
                .records,
            );
        }
    }

    if all_records.is_empty() {
        return Vec::new();
    }

    // Aggregate eligible candidate files and preserve `(has_sig_changed, has_type_target)` priority.
    struct CandScore {
        path: PathBuf,
        matched_syms: HashSet<String>,
        hit_count: usize,
        has_sig_changed: bool,
        has_type_target: bool,
    }
    let mut cand_map: HashMap<String, CandScore> = HashMap::new();
    for rec in all_records {
        let p_norm = normalize_path_key(&rec.path);
        let entry = cand_map.entry(p_norm).or_insert_with(|| CandScore {
            path: rec.path,
            matched_syms: HashSet::new(),
            hit_count: 0,
            has_sig_changed: false,
            has_type_target: false,
        });
        entry.hit_count += 1;
        for h in rec.matched_syms {
            if let Some(&(sc, it)) = sym_flags.get(&h) {
                entry.has_sig_changed |= sc;
                entry.has_type_target |= it;
            }
            entry.matched_syms.insert(h);
        }
    }
    if cand_map.is_empty() {
        return Vec::new();
    }

    let mut cand_list: Vec<CandScore> = cand_map.into_values().collect();
    cand_list.sort_by(|a, b| {
        b.has_sig_changed
            .cmp(&a.has_sig_changed)
            .then_with(|| b.has_type_target.cmp(&a.has_type_target))
            .then_with(|| b.matched_syms.len().cmp(&a.matched_syms.len()))
            .then_with(|| b.hit_count.cmp(&a.hit_count))
            .then_with(|| a.path.cmp(&b.path))
    });
    cand_list.truncate(MAX_CALLER_CANDIDATE_FILES);

    let to_read: Vec<PathBuf> = cand_list.iter().map(|c| c.path.clone()).collect();
    let blobs = read_snapshot_files_batch(repo_root, source, &to_read);

    // Precompute normalized metadata per target
    struct TargetMeta<'a> {
        target: &'a ReverseTarget,
        def_norm: String,
        def_dir_norm: String,
        pkg_dir_norm: String,
        stem_norm: String,
    }
    let mut target_meta_map: HashMap<&str, Vec<TargetMeta<'_>>> = HashMap::new();
    for sym in &selected_syms {
        if let Some(ts) = grouped.get(sym) {
            let metas = ts
                .iter()
                .map(|t| TargetMeta {
                    target: t,
                    def_norm: normalize_path_key(&t.def_file),
                    def_dir_norm: t
                        .def_file
                        .parent()
                        .map(normalize_path_key)
                        .unwrap_or_default(),
                    pkg_dir_norm: t
                        .def_file
                        .parent()
                        .and_then(|p| p.file_name())
                        .and_then(|s| s.to_str())
                        .map(normalize_alnum_lower)
                        .unwrap_or_default(),
                    stem_norm: stem_for_path_norm(&t.def_file),
                })
                .collect();
            target_meta_map.insert(sym.as_str(), metas);
        }
    }

    #[derive(Clone)]
    struct CallerMatch {
        entry: ContextEntry,
        enc_line: usize,
        pinned: bool,
    }
    let mut callers_by_target: BTreeMap<String, Vec<CallerMatch>> = BTreeMap::new();
    let mut parsed_bytes = 0usize;

    for cand in cand_list {
        let Some(content) = blobs.get(&cand.path) else {
            continue;
        };
        if parsed_bytes + content.len() > MAX_CROSS_FILE_PARSE_BYTES {
            break;
        }
        if has_generated_source_header(content, Some(&cand.path)) {
            continue;
        }
        parsed_bytes += content.len();
        let Some((cand_lang, tree)) = language_registry::parse_for_path(&cand.path, content) else {
            continue;
        };
        if matches!(cand_lang, SupportedLanguage::Json | SupportedLanguage::Bash) {
            continue;
        }
        let lines: Vec<&str> = content.lines().collect();
        if lines.is_empty() {
            continue;
        }
        let inline_tests =
            ast_compact::find_inline_test_blocks(cand_lang, &tree.root_node(), content);

        let mut import_specs = Vec::new();
        let mut imported_symbols = HashMap::new();
        let mut alias_map = HashMap::new();
        collect_file_imports(
            cand_lang,
            &tree.root_node(),
            content,
            &mut import_specs,
            &mut imported_symbols,
            &mut alias_map,
        );

        let cand_dir_norm = cand
            .path
            .parent()
            .map(normalize_path_key)
            .unwrap_or_default();

        // Check import/same-pkg pinning per wanted symbol present in this candidate file
        let mut sym_pinned: HashMap<String, bool> = HashMap::new();
        let mut local_name_to_targets: HashMap<String, Vec<String>> = HashMap::new();

        for sym in &cand.matched_syms {
            let Some(metas) = target_meta_map.get(sym.as_str()) else {
                continue;
            };
            let pinned = metas.iter().any(|tm| {
                if let Some(specs) = imported_symbols.get(sym) {
                    if specs.iter().any(|sp| {
                        import_spec_matches_target_file(
                            cand_lang,
                            &cand.path,
                            sp,
                            &tm.target.def_file,
                            &tm.def_norm,
                            &tm.stem_norm,
                        )
                    }) {
                        return true;
                    }
                }
                if import_specs.iter().any(|sp| {
                    import_spec_matches_target_file(
                        cand_lang,
                        &cand.path,
                        sp,
                        &tm.target.def_file,
                        &tm.def_norm,
                        &tm.stem_norm,
                    )
                }) {
                    return true;
                }
                // Same-package Go sibling or same-directory C/C++/Java/C# sibling for matching family
                let same_family = cand_lang == tm.target.lang
                    || matches!(
                        (cand_lang, tm.target.lang),
                        (SupportedLanguage::C, SupportedLanguage::Cpp)
                            | (SupportedLanguage::Cpp, SupportedLanguage::C)
                    );
                same_family
                    && cand_dir_norm == tm.def_dir_norm
                    && matches!(
                        cand_lang,
                        SupportedLanguage::Go
                            | SupportedLanguage::Java
                            | SupportedLanguage::CSharp
                            | SupportedLanguage::C
                            | SupportedLanguage::Cpp
                    )
                    && (cand_lang == SupportedLanguage::Go
                        || tm.target.is_type
                        || !matches!(
                            cand_lang,
                            SupportedLanguage::Java | SupportedLanguage::CSharp
                        ))
            });
            sym_pinned.insert(sym.clone(), pinned);
            local_name_to_targets
                .entry(sym.clone())
                .or_default()
                .push(sym.clone());
        }
        // Propagate aliases (`real as alias` in candidate file) if alias itself appears in candidate!
        let is_ruby_cand = cand_lang == SupportedLanguage::Ruby;
        for (alias, orig) in &alias_map {
            if let Some(metas) = target_meta_map.get(orig.as_str()) {
                if contains_utf8_ident(content, alias, is_ruby_cand) {
                    let pinned = metas.iter().any(|tm| {
                        imported_symbols.get(alias).is_some_and(|specs| {
                            specs.iter().any(|sp| {
                                import_spec_matches_target_file(
                                    cand_lang,
                                    &cand.path,
                                    sp,
                                    &tm.target.def_file,
                                    &tm.def_norm,
                                    &tm.stem_norm,
                                )
                            })
                        }) || import_specs.iter().any(|sp| {
                            import_spec_matches_target_file(
                                cand_lang,
                                &cand.path,
                                sp,
                                &tm.target.def_file,
                                &tm.def_norm,
                                &tm.stem_norm,
                            )
                        })
                    });
                    sym_pinned
                        .entry(orig.clone())
                        .and_modify(|p| *p |= pinned)
                        .or_insert(pinned);
                    local_name_to_targets
                        .entry(alias.clone())
                        .or_default()
                        .push(orig.clone());
                }
            }
        }

        if local_name_to_targets.is_empty() {
            continue;
        }

        #[derive(Clone)]
        struct VerifiedHit {
            target_sym: String,
            call_line: usize,
            enc_line: usize,
            enc_sig: Option<String>,
            pinned: bool,
        }

        let is_qualifier_compatible = |qual_opt: Option<&str>,
                                       target_sym: &str,
                                       is_static_sep: bool|
         -> bool {
            let Some(qual) = qual_opt.map(str::trim).filter(|q| !q.is_empty()) else {
                return true;
            };
            if matches!(qual, "self" | "this" | "super" | "crate") {
                return true;
            }
            let Some(metas) = target_meta_map.get(target_sym) else {
                return false;
            };
            let alias_target = alias_map.get(qual).map(String::as_str);
            let q_resolved = alias_target.unwrap_or(qual);
            let q_norm = normalize_alnum_lower(q_resolved);
            if q_norm.is_empty() {
                return true;
            }
            for tm in metas {
                if !tm.stem_norm.is_empty() && q_norm == tm.stem_norm {
                    return true;
                }
                if !tm.target.parent_hints.is_empty() && tm.target.parent_hints.contains(&q_norm) {
                    return true;
                }
                if cand_lang == SupportedLanguage::Go
                    && !tm.pkg_dir_norm.is_empty()
                    && q_norm == tm.pkg_dir_norm
                {
                    return true;
                }
            }
            // Reject:
            // 1) uppercase static type/class qualifiers (`OtherType::foo()`, `OtherService.reserve(...)`)
            // 2) explicit import/namespace aliases resolving to a different module (`other_ns.foo()`)
            // 3) Rust static `mod_other::foo()` / `crate::mod_other::foo` where module does not match stem/alias/parent
            let is_upper_static = q_resolved.chars().next().is_some_and(|c| c.is_uppercase());
            if is_upper_static
                || alias_target.is_some()
                || (cand_lang == SupportedLanguage::Rust && is_static_sep)
            {
                return false;
            }
            true
        };

        let is_skip_ast_kind = |kind: &str| -> bool {
            matches!(
                kind,
                "comment"
                    | "line_comment"
                    | "block_comment"
                    | "documentation_comment"
                    | "use_declaration"
                    | "import_statement"
                    | "import_from_statement"
                    | "import_declaration"
                    | "using_directive"
                    | "preproc_include"
                    | "import_or_export"
                    | "library_import"
                    | "library_export"
                    | "part_directive"
                    | "part_of_directive"
                    | "jsx_closing_element"
                    | "string_literal"
                    | "raw_string_literal"
                    | "char_literal"
                    | "character_literal"
                    | "interpreted_string_literal"
                    | "rune_literal"
                    | "string_fragment"
                    | "string_content"
                    | "text_block"
                    | "verbatim_string_literal"
                    | "regex"
                    | "simple_symbol"
                    | "symbol"
            )
        };

        let is_binding_or_decl_name_pos = |node: &tree_sitter::Node| -> bool {
            let Some(parent) = node.parent() else {
                return false;
            };
            let pk = parent.kind();
            if parent
                .child_by_field_name("name")
                .is_some_and(|c| c.id() == node.id())
                && matches!(
                    pk,
                    "parameter"
                        | "required_parameter"
                        | "optional_parameter"
                        | "formal_parameter"
                        | "parameter_declaration"
                        | "default_parameter"
                        | "typed_default_parameter"
                        | "keyword_parameter"
                        | "splat_parameter"
                        | "hash_splat_parameter"
                        | "block_parameter"
                        | "variable_declarator"
                        | "function_item"
                        | "function_declaration"
                        | "function_definition"
                        | "method_declaration"
                        | "method_definition"
                        | "method"
                        | "singleton_method"
                        | "struct_item"
                        | "enum_item"
                        | "trait_item"
                        | "type_item"
                        | "const_item"
                        | "static_item"
                        | "class_definition"
                        | "class_declaration"
                        | "interface_declaration"
                        | "enum_declaration"
                        | "record_declaration"
                        | "type_alias_declaration"
                        | "type_spec"
                        | "field_declaration"
                        | "property_declaration"
                        | "function_signature"
                        | "getter_signature"
                        | "setter_signature"
                        | "constructor_signature"
                        | "constant_constructor_signature"
                        | "factory_constructor_signature"
                        | "redirecting_factory_constructor_signature"
                        | "mixin_declaration"
                        | "extension_declaration"
                        | "extension_type_declaration"
                        | "initialized_identifier"
                        | "initialized_variable_declaration"
                        | "static_final_declaration"
                        | "field_formal_parameter"
                        | "typed_identifier"
                        | "enum_entry"
                )
            {
                return true;
            }
            if matches!(
                pk,
                "parameter"
                    | "let_declaration"
                    | "short_var_declaration"
                    | "var_spec"
                    | "const_spec"
            ) && parent
                .child_by_field_name("pattern")
                .is_some_and(|c| c.id() == node.id())
            {
                return true;
            }
            if matches!(
                pk,
                "function_declarator"
                    | "init_declarator"
                    | "parameter_declaration"
                    | "optional_parameter_declaration"
                    | "pointer_declarator"
                    | "reference_declarator"
                    | "array_declarator"
            ) && parent
                .child_by_field_name("declarator")
                .is_some_and(|c| c.id() == node.id())
            {
                return true;
            }
            if pk == "assignment"
                && parent
                    .child_by_field_name("left")
                    .is_some_and(|c| c.id() == node.id())
            {
                return true;
            }
            if pk == "typed_parameter"
                && node.kind() == "identifier"
                && parent
                    .child_by_field_name("type")
                    .is_none_or(|t| t.id() != node.id())
            {
                return true;
            }
            if pk == "parameter_declaration" && node.kind() == "identifier" {
                return true;
            }
            if pk == "identifier_pattern" {
                return true;
            }
            if cand_lang == SupportedLanguage::Dart
                && ((pk == "type_alias" && node.kind() == "type_identifier")
                    || (matches!(pk, "mixin_application_class" | "extension_type_name")
                        && node.kind() == "identifier"))
            {
                return true;
            }
            false
        };

        let mut raw_hits: Vec<VerifiedHit> = Vec::new();

        #[allow(clippy::too_many_arguments)]
        fn walk_caller_ast<'a>(
            lang: SupportedLanguage,
            cand_path: &Path,
            node: tree_sitter::Node<'a>,
            lines: &[&str],
            source: &str,
            inline_tests: &[ast_compact::InlineTestBlock],
            local_name_to_targets: &HashMap<String, Vec<String>>,
            sym_pinned: &HashMap<String, bool>,
            is_qualifier_compatible: &dyn Fn(Option<&str>, &str, bool) -> bool,
            is_skip_ast_kind: &dyn Fn(&str) -> bool,
            is_binding_or_decl_name_pos: &dyn Fn(&tree_sitter::Node) -> bool,
            enclosing_stack: &mut Vec<tree_sitter::Node<'a>>,
            target_meta_map: &HashMap<&str, Vec<TargetMeta<'_>>>,
            scope_sep: &'static str,
            out_hits: &mut Vec<VerifiedHit>,
        ) {
            let kind = node.kind();
            if is_skip_ast_kind(kind) {
                return;
            }
            let n_start = node.start_position().row + 1;
            let n_end = node.end_position().row + 1;
            if inline_tests
                .iter()
                .any(|b| n_start >= b.start_line && n_end <= b.end_line)
            {
                return;
            }

            // Skip plain Ruby/Python `"string"` and JS/TS `template_string` and C# `interpolated_string_expression`
            // bodies unless descending into explicit interpolation nodes.
            if (kind == "string"
                && matches!(
                    lang,
                    SupportedLanguage::Python
                        | SupportedLanguage::Ruby
                        | SupportedLanguage::JavaScript
                        | SupportedLanguage::TypeScript
                        | SupportedLanguage::Tsx
                ))
                || kind == "template_string"
                || kind == "interpolated_string_expression"
            {
                let mut cursor = node.walk();
                for ch in node.named_children(&mut cursor) {
                    if matches!(ch.kind(), "interpolation" | "template_substitution") {
                        walk_caller_ast(
                            lang,
                            cand_path,
                            ch,
                            lines,
                            source,
                            inline_tests,
                            local_name_to_targets,
                            sym_pinned,
                            is_qualifier_compatible,
                            is_skip_ast_kind,
                            is_binding_or_decl_name_pos,
                            enclosing_stack,
                            target_meta_map,
                            scope_sep,
                            out_hits,
                        );
                    }
                }
                return;
            }

            // Skip Ruby `require` / `require_relative`
            if lang == SupportedLanguage::Ruby && kind == "call" {
                if let Some(m) = node.child_by_field_name("method") {
                    if matches!(
                        source.get(m.start_byte()..m.end_byte()),
                        Some("require" | "require_relative")
                    ) {
                        return;
                    }
                }
            }

            // Skip test-attributed functions/blocks in production files
            let pushed_def = if let Some(matched) = check_primary_definition(lang, &node) {
                if matched.id() == node.id() {
                    if test_scope::is_test_scope(lang, &matched, Some(cand_path), lines) {
                        return;
                    }
                    enclosing_stack.push(matched);
                    true
                } else {
                    false
                }
            } else {
                false
            };

            let record_hit = |local_sym: &str,
                              qual_opt: Option<&str>,
                              hit_line: usize,
                              is_static_sep: bool,
                              stack: &[tree_sitter::Node<'a>],
                              out: &mut Vec<VerifiedHit>| {
                let Some(orig_syms) = local_name_to_targets.get(local_sym) else {
                    return;
                };
                for orig_sym in orig_syms {
                    let pinned = *sym_pinned.get(orig_sym).unwrap_or(&false);
                    if !is_qualifier_compatible(qual_opt, orig_sym, is_static_sep) {
                        continue;
                    }
                    // Pick innermost enclosing function/method (fallback to class/type if at class level)
                    let enc_node = stack
                        .iter()
                        .rev()
                        .find(|n| {
                            ast_compact::is_foldable_function_node(lang, n)
                                || matches!(n.kind(), "decorated_definition" | "export_statement")
                        })
                        .or_else(|| stack.last());
                    let (enc_line, enc_sig) = if let Some(en) = enc_node {
                        let parent =
                            signature::extract_parent_scope_prefix(lang, en, lines, source);
                        let raw_sig = signature::extract_signature(lang, en, lines, source);
                        let compact = signature::compact_definition_signature_with_ast_opts(
                            lang,
                            en,
                            parent.as_deref(),
                            raw_sig.as_deref(),
                            source,
                            signature::SigFormatOpts {
                                max_chars: 120,
                                scope_sep,
                                preserve_key_decorators: false,
                                greedy_params: true,
                            },
                        )
                        .map(|s| s.trim_end_matches('{').trim_end().to_string());
                        (en.start_position().row + 1, compact)
                    } else {
                        (hit_line, None)
                    };
                    out.push(VerifiedHit {
                        target_sym: orig_sym.clone(),
                        call_line: hit_line,
                        enc_line,
                        enc_sig,
                        pinned,
                    });
                }
            };

            // Detect direct callee / property / attribute / trailing member positions already evaluated
            // (or governed) by an enclosing qualified expression, so a rejected `OtherClass.foo()`
            // or `<OtherNS.PriceTag />` never leaks through as an unqualified `foo` on child recursion.
            let is_direct_callee_or_member_leaf = node.parent().is_some_and(|p| {
                let pk = p.kind();
                p.child_by_field_name("function")
                    .is_some_and(|c| c.id() == node.id())
                    || p.child_by_field_name("name")
                        .is_some_and(|c| c.id() == node.id())
                    || p.child_by_field_name("method")
                        .is_some_and(|c| c.id() == node.id())
                    || p.child_by_field_name("macro")
                        .is_some_and(|c| c.id() == node.id())
                    || p.child_by_field_name("constructor")
                        .is_some_and(|c| c.id() == node.id())
                    || (matches!(pk, "object_creation_expression" | "generic_type")
                        && p.child_by_field_name("type")
                            .is_some_and(|c| c.id() == node.id()))
                    || (pk == "attribute"
                        && p.child_by_field_name("attribute")
                            .is_some_and(|c| c.id() == node.id()))
                    || (matches!(
                        pk,
                        "member_expression"
                            | "null_aware_member_expression"
                            | "cascade_call_expression"
                            | "cascade_member_expression"
                            | "cascade_null_aware_member_expression"
                            | "jsx_member_expression"
                            | "nested_identifier"
                    ) && p
                        .child_by_field_name("property")
                        .is_some_and(|c| c.id() == node.id()))
                    || (matches!(pk, "field_expression" | "selector_expression")
                        && p.child_by_field_name("field")
                            .is_some_and(|c| c.id() == node.id()))
                    || (matches!(
                        pk,
                        "method_reference" | "jsx_namespace_name" | "scoped_type_identifier"
                    ) && p.named_child(0).is_some_and(|c| c.id() != node.id()))
            });

            // 1. Call expressions & invocations across all 10 languages
            if matches!(
                kind,
                "call_expression" | "call" | "method_invocation" | "invocation_expression"
            ) {
                if kind == "method_invocation"
                    || (lang == SupportedLanguage::Ruby && kind == "call")
                {
                    let (obj_f, name_f) = if kind == "method_invocation" {
                        ("object", "name")
                    } else {
                        ("receiver", "method")
                    };
                    let obj = node
                        .child_by_field_name(obj_f)
                        .and_then(|n| source.get(n.start_byte()..n.end_byte()))
                        .map(str::trim)
                        .unwrap_or("");
                    if let Some(name_n) = node.child_by_field_name(name_f) {
                        let name = source
                            .get(name_n.start_byte()..name_n.end_byte())
                            .map(str::trim)
                            .unwrap_or("");
                        let hit_line = name_n.start_position().row + 1;
                        let quals = extract_lhs_qualifiers(lang, obj);
                        if let Some(&q) = quals.last() {
                            record_hit(name, Some(q), hit_line, false, enclosing_stack, out_hits);
                        } else if obj.is_empty() || matches!(obj, "this" | "self" | "super") {
                            record_hit(name, None, hit_line, false, enclosing_stack, out_hits);
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
                    let hit_line = fn_n
                        .child_by_field_name("field")
                        .or_else(|| fn_n.child_by_field_name("property"))
                        .or_else(|| fn_n.child_by_field_name("attribute"))
                        .or_else(|| fn_n.child_by_field_name("name"))
                        .map_or_else(
                            || fn_n.start_position().row + 1,
                            |n| n.start_position().row + 1,
                        );
                    if let Some((lhs, rhs, is_static_sep)) = split_rightmost_call_sep(lang, raw) {
                        let rhs_no_turbo = rhs.split("::<").next().unwrap_or(rhs);
                        let raw_sym = rhs_no_turbo.split(['<', '(']).next().unwrap_or("").trim();
                        let sym = if lang == SupportedLanguage::Ruby {
                            raw_sym
                        } else {
                            raw_sym.trim_end_matches(['?', '!'])
                        };
                        let quals = extract_lhs_qualifiers(lang, lhs);
                        let qual_opt = quals.last().copied();
                        record_hit(
                            sym,
                            qual_opt,
                            hit_line,
                            is_static_sep,
                            enclosing_stack,
                            out_hits,
                        );
                    } else {
                        let raw_no_turbo = raw.split("::<").next().unwrap_or(raw);
                        let raw_sym = raw_no_turbo.split(['<', '(']).next().unwrap_or("").trim();
                        let sym = if lang == SupportedLanguage::Ruby {
                            raw_sym
                        } else {
                            raw_sym.trim_end_matches(['?', '!'])
                        };
                        record_hit(sym, None, hit_line, false, enclosing_stack, out_hits);
                    }
                }
            } else if kind == "cascade_call_expression" && lang == SupportedLanguage::Dart {
                if let Some(prop_n) = node.child_by_field_name("property") {
                    let sym = source
                        .get(prop_n.start_byte()..prop_n.end_byte())
                        .map(str::trim)
                        .unwrap_or("");
                    let hit_line = prop_n.start_position().row + 1;
                    let recv = find_enclosing_cascade_receiver(&node, source);
                    let quals = extract_lhs_qualifiers(lang, recv);
                    record_hit(
                        sym,
                        quals.last().copied(),
                        hit_line,
                        false,
                        enclosing_stack,
                        out_hits,
                    );
                } else if let Some(fn_n) = node.child_by_field_name("function") {
                    let raw = source
                        .get(fn_n.start_byte()..fn_n.end_byte())
                        .unwrap_or("")
                        .trim();
                    let hit_line = fn_n.child_by_field_name("property").map_or_else(
                        || fn_n.start_position().row + 1,
                        |n| n.start_position().row + 1,
                    );
                    if let Some((lhs, rhs, is_static_sep)) = split_rightmost_call_sep(lang, raw) {
                        let sym = rhs
                            .split(['<', '('])
                            .next()
                            .unwrap_or("")
                            .trim()
                            .trim_end_matches(['?', '!']);
                        let quals = extract_lhs_qualifiers(lang, lhs);
                        record_hit(
                            sym,
                            quals.last().copied(),
                            hit_line,
                            is_static_sep,
                            enclosing_stack,
                            out_hits,
                        );
                    }
                }
            } else if kind == "macro_invocation" {
                if let Some(m_n) = node.child_by_field_name("macro") {
                    let raw = source
                        .get(m_n.start_byte()..m_n.end_byte())
                        .unwrap_or("")
                        .trim();
                    let (qual, sym, is_static_sep) = match raw.rsplit_once("::") {
                        Some((l, r)) => (
                            Some(l.rsplit("::").next().unwrap_or(l).trim()),
                            r.trim(),
                            true,
                        ),
                        None => (None, raw, false),
                    };
                    record_hit(
                        sym,
                        qual,
                        m_n.start_position().row + 1,
                        is_static_sep,
                        enclosing_stack,
                        out_hits,
                    );
                }
            } else if matches!(
                kind,
                "new_expression"
                    | "object_creation_expression"
                    | "const_object_expression"
                    | "constructor_invocation"
            ) {
                if let Some(t_n) = node
                    .child_by_field_name("constructor")
                    .or_else(|| node.child_by_field_name("type"))
                {
                    let raw = source
                        .get(t_n.start_byte()..t_n.end_byte())
                        .unwrap_or("")
                        .trim();
                    let no_gen = raw.split('<').next().unwrap_or(raw).trim();
                    if let Some((lhs, rhs, is_static_sep)) = split_rightmost_call_sep(lang, no_gen)
                    {
                        let quals = extract_lhs_qualifiers(lang, lhs);
                        record_hit(
                            rhs.trim(),
                            quals.last().copied(),
                            t_n.start_position().row + 1,
                            is_static_sep,
                            enclosing_stack,
                            out_hits,
                        );
                    } else {
                        record_hit(
                            no_gen,
                            None,
                            t_n.start_position().row + 1,
                            false,
                            enclosing_stack,
                            out_hits,
                        );
                    }
                }
            } else if kind == "method_reference" {
                if let Some(raw) = source.get(node.start_byte()..node.end_byte()) {
                    if let Some((l, r)) = raw.trim().rsplit_once("::") {
                        let qual = l.rsplit(['.', ':']).next().map(str::trim);
                        record_hit(r.trim(), qual, n_start, true, enclosing_stack, out_hits);
                    }
                }
            } else if matches!(kind, "jsx_opening_element" | "jsx_self_closing_element") {
                if let Some(name_n) = node.child_by_field_name("name") {
                    let raw = source
                        .get(name_n.start_byte()..name_n.end_byte())
                        .unwrap_or("")
                        .trim();
                    let (qual, sym) = if let Some((l, r)) = raw.rsplit_once(['.', ':']) {
                        (
                            Some(l.rsplit(['.', ':']).next().unwrap_or(l).trim()),
                            r.trim(),
                        )
                    } else {
                        (None, raw)
                    };
                    record_hit(
                        sym,
                        qual,
                        name_n.start_position().row + 1,
                        false,
                        enclosing_stack,
                        out_hits,
                    );
                }
            } else if kind == "attribute"
                && lang == SupportedLanguage::Python
                && !is_direct_callee_or_member_leaf
                && !is_binding_or_decl_name_pos(&node)
            {
                if let (Some(obj_n), Some(attr_n)) = (
                    node.child_by_field_name("object"),
                    node.child_by_field_name("attribute"),
                ) {
                    let obj = source
                        .get(obj_n.start_byte()..obj_n.end_byte())
                        .map(str::trim)
                        .unwrap_or("");
                    let attr = source
                        .get(attr_n.start_byte()..attr_n.end_byte())
                        .map(str::trim)
                        .unwrap_or("");
                    if let Some(orig_syms) = local_name_to_targets.get(attr) {
                        let has_type_target = orig_syms.iter().any(|os| {
                            target_meta_map
                                .get(os.as_str())
                                .is_some_and(|ms| ms.iter().any(|m| m.target.is_type))
                        });
                        let pinned = orig_syms
                            .iter()
                            .any(|os| *sym_pinned.get(os).unwrap_or(&false));
                        if has_type_target || pinned {
                            let quals = extract_lhs_qualifiers(lang, obj);
                            record_hit(
                                attr,
                                quals.last().copied(),
                                attr_n.start_position().row + 1,
                                false,
                                enclosing_stack,
                                out_hits,
                            );
                        }
                    }
                }
            } else if matches!(
                kind,
                "type_identifier"
                    | "type_name"
                    | "qualified_type"
                    | "scoped_type_identifier"
                    | "generic_name"
            ) && !is_direct_callee_or_member_leaf
                && !is_binding_or_decl_name_pos(&node)
            {
                if let Some(raw) = source.get(node.start_byte()..node.end_byte()) {
                    let clean = raw.split('<').next().unwrap_or(raw).trim();
                    if let Some((lhs, rhs, is_static_sep)) = split_rightmost_call_sep(lang, clean) {
                        let quals = extract_lhs_qualifiers(lang, lhs);
                        record_hit(
                            rhs.trim(),
                            quals.last().copied(),
                            n_start,
                            is_static_sep,
                            enclosing_stack,
                            out_hits,
                        );
                    } else {
                        record_hit(clean, None, n_start, false, enclosing_stack, out_hits);
                    }
                }
            } else if matches!(
                kind,
                "scoped_identifier" | "qualified_identifier" | "identifier" | "constant"
            ) && !is_direct_callee_or_member_leaf
                && !is_binding_or_decl_name_pos(&node)
            {
                if let Some(raw) = source.get(node.start_byte()..node.end_byte()) {
                    let t = raw.trim();
                    if kind == "scoped_identifier" || kind == "qualified_identifier" {
                        if let Some((lhs, rhs, is_static_sep)) = split_rightmost_call_sep(lang, t) {
                            let quals = extract_lhs_qualifiers(lang, lhs);
                            record_hit(
                                rhs.trim(),
                                quals.last().copied(),
                                n_start,
                                is_static_sep,
                                enclosing_stack,
                                out_hits,
                            );
                        }
                    } else if let Some(orig_syms) = local_name_to_targets.get(t) {
                        let has_type_target = orig_syms.iter().any(|os| {
                            target_meta_map
                                .get(os.as_str())
                                .is_some_and(|ms| ms.iter().any(|m| m.target.is_type))
                        });
                        let pinned = orig_syms
                            .iter()
                            .any(|os| *sym_pinned.get(os).unwrap_or(&false));
                        if has_type_target || pinned {
                            record_hit(t, None, n_start, false, enclosing_stack, out_hits);
                        }
                    }
                }
            }

            let mut cursor = node.walk();
            for ch in node.named_children(&mut cursor) {
                walk_caller_ast(
                    lang,
                    cand_path,
                    ch,
                    lines,
                    source,
                    inline_tests,
                    local_name_to_targets,
                    sym_pinned,
                    is_qualifier_compatible,
                    is_skip_ast_kind,
                    is_binding_or_decl_name_pos,
                    enclosing_stack,
                    target_meta_map,
                    scope_sep,
                    out_hits,
                );
            }
            if pushed_def {
                enclosing_stack.pop();
            }
        }

        let mut enclosing_stack = Vec::new();
        walk_caller_ast(
            cand_lang,
            &cand.path,
            tree.root_node(),
            &lines,
            content,
            &inline_tests,
            &local_name_to_targets,
            &sym_pinned,
            &is_qualifier_compatible,
            &is_skip_ast_kind,
            &is_binding_or_decl_name_pos,
            &mut enclosing_stack,
            &target_meta_map,
            scope_sep,
            &mut raw_hits,
        );

        // Deduplicate per `(target_sym, enc_line)` within this file so one caller with 5 calls
        // uses 1 slot instead of exhausting `MAX_CALLERS_PER_TARGET`.
        let mut per_sym_enc: HashSet<(String, usize)> = HashSet::new();
        for hit in raw_hits {
            if !per_sym_enc.insert((hit.target_sym.clone(), hit.enc_line)) {
                continue;
            }
            let call_code_raw = lines
                .get(hit.call_line.saturating_sub(1))
                .map(|l| l.trim())
                .unwrap_or("");
            if call_code_raw.is_empty() {
                continue;
            }
            let call_code = if call_code_raw.len() > 120 {
                format!("{}...", truncate_utf8_bytes(call_code_raw, 117).trim_end())
            } else {
                call_code_raw.to_string()
            };
            let snippet = match hit.enc_sig {
                Some(ref esig) if esig != &call_code => {
                    format!("[caller of {}] {esig} -> {call_code}", hit.target_sym)
                }
                _ => format!("[caller of {}] {call_code}", hit.target_sym),
            };
            callers_by_target
                .entry(hit.target_sym.clone())
                .or_default()
                .push(CallerMatch {
                    entry: ContextEntry {
                        file_path: cand.path.clone(),
                        symbol_name: hit.target_sym,
                        start_line: hit.call_line,
                        end_line: hit.call_line,
                        is_local: false,
                        is_type_def: false,
                        pinned_by_import: hit.pinned,
                        is_caller: true,
                        snippet,
                    },
                    enc_line: hit.enc_line,
                    pinned: hit.pinned,
                });
        }
    }

    let mut final_entries = Vec::new();
    for (_sym, mut cands) in callers_by_target {
        cands.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then_with(|| a.entry.file_path.cmp(&b.entry.file_path))
                .then_with(|| a.enc_line.cmp(&b.enc_line))
                .then_with(|| a.entry.start_line.cmp(&b.entry.start_line))
        });
        cands.truncate(MAX_CALLERS_PER_TARGET);
        final_entries.extend(cands.into_iter().map(|c| c.entry));
    }

    final_entries
}

/// Absolute Layer 3 byte ceiling when `max_patch_bytes` is unbounded (`usize::MAX`).
#[cfg(feature = "tree-sitter")]
pub(super) const UNBOUNDED_LAYER3_MAX_BYTES: usize = 128 * 1024;

/// Computes Layer 3 dynamic budget:
/// - Under normal finite budgets: `min(15% of max_bytes, max(4 KiB, 25% of layer2_bytes), remaining_bytes)`
/// - Under unbounded mode (`max_patch_bytes == usize::MAX`): `min(128 KiB, max(4 KiB, 25% of layer2_bytes), remaining_bytes)`
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
    let pct_cap = if max_patch_bytes == usize::MAX {
        UNBOUNDED_LAYER3_MAX_BYTES
    } else {
        max_patch_bytes.saturating_mul(15) / 100
    };
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
/// 0) Same-file unmodified type/const definitions (`is_local`)
/// 1) Cross-file pinned type definitions (`!is_caller && is_type_def && pinned_by_import`)
/// 2) Cross-file pinned function/method signatures (`!is_caller && !is_type_def && pinned_by_import`)
/// 3) Pinned reverse 1-hop callers (`is_caller && pinned_by_import`)
/// 4) Unpinned cross-file forward types/signatures (`!is_caller && !pinned_by_import`)
/// 5) Unpinned reverse 1-hop callers (`is_caller && !pinned_by_import`)
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

    let entry_priority = |e: &ContextEntry| -> u8 {
        if e.is_local {
            0
        } else if !e.is_caller && e.is_type_def && e.pinned_by_import {
            1
        } else if !e.is_caller && !e.is_type_def && e.pinned_by_import {
            2
        } else if e.is_caller && e.pinned_by_import {
            3
        } else if !e.is_caller && !e.pinned_by_import {
            4
        } else {
            5
        }
    };

    entries.sort_by(|a, b| {
        entry_priority(a)
            .cmp(&entry_priority(b))
            .then_with(|| b.is_type_def.cmp(&a.is_type_def))
            .then_with(|| a.file_path.cmp(&b.file_path))
            .then_with(|| a.start_line.cmp(&b.start_line))
            .then_with(|| a.is_caller.cmp(&b.is_caller))
            .then_with(|| a.symbol_name.cmp(&b.symbol_name))
    });
    let mut seen_keys: HashSet<(PathBuf, String, usize, bool)> = HashSet::new();
    entries.retain(|e| {
        seen_keys.insert((
            e.file_path.clone(),
            e.symbol_name.clone(),
            e.start_line,
            e.is_caller,
        ))
    });

    // Pack highest-priority entries first within `max_bytes`, then group selected entries by file
    let header = "\n=== External AST Context (1-hop, unmodified) ===\n";
    let mut used = header.len();
    let mut selected: Vec<ContextEntry> = Vec::new();
    let mut selected_full_type_ranges: Vec<(PathBuf, usize, usize)> = Vec::new();
    let mut file_headers_seen: HashSet<(bool, PathBuf)> = HashSet::new();

    for e in entries {
        // Deduplicate method signatures whose full enclosing class/struct (<= 18 lines, unstripped)
        // is already included verbatim in Layer 3 for the same file (never drop reverse callers).
        if !e.is_type_def
            && !e.is_caller
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
        let key = normalize_path_key(&e.file_path);
        grouped.entry((rank, key)).or_default().push(e);
    }

    let mut out = String::with_capacity(used + 32);
    out.push_str(header);
    for ((rank, path_str), mut list) in grouped {
        let scope_tag = if rank == 0 { "same-file" } else { "cross-file" };
        out.push_str(&format!("--- {scope_tag}: {path_str} ---\n"));
        list.sort_by_key(|e| (e.is_caller, e.start_line));
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
        assert!(
            b1.layer1 && b1.layer2 && !b1.layer3_local && !b1.layer3_cross && !b1.layer3_callers
        );
        assert_eq!(b1.scope_sep(), " › ");
        assert_eq!(HybridConfig::off().scope_sep(), " > ");

        let b2 = HybridConfig::parse(Some("l1+l2+l3_local"));
        assert!(
            b2.layer1 && b2.layer2 && b2.layer3_local && !b2.layer3_cross && !b2.layer3_callers
        );

        let b3 = HybridConfig::parse(Some("l1+l2+l3_local+l3_cross"));
        assert!(b3.layer1 && b3.layer2 && b3.layer3_local && b3.layer3_cross && !b3.layer3_callers);

        let callers_only = HybridConfig::parse(Some("callers"));
        assert!(
            !callers_only.layer1
                && !callers_only.layer2
                && !callers_only.layer3_local
                && !callers_only.layer3_cross
                && callers_only.layer3_callers
        );

        let l3 = HybridConfig::parse(Some("l3"));
        assert!(
            !l3.layer1 && !l3.layer2 && l3.layer3_local && l3.layer3_cross && l3.layer3_callers
        );
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

        // Add DIRTY worktree-only markers to src/client.rs AFTER `git add` so Staged/Commit must NOT see them!
        // Avoid short pure-hex numeric sentinels like `"999"` because git commit/blob SHAs are `[0-9a-f]+`.
        std::fs::write(
            repo.join("src/client.rs"),
            "/// DIRTY_WORKTREE_ONLY_DOC\n\
             pub struct GatewayCfg { pub dirty_retries: u32 }\n\
             pub struct OrderClient { dirty_cfg: GatewayCfg }\n\
             impl OrderClient {\n\
                 pub fn new() -> Self { Self { dirty_cfg: GatewayCfg { dirty_retries: 9_999_888 } } }\n\
                 pub fn send_order(&self, dirty_id: u64) -> bool { dirty_id > 9_999_888 }\n\
             }\n\
             pub fn call_api<T>(dirty_v: u64) -> bool { dirty_v > 9_999_888 }\n",
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
            !staged_out.contains("DIRTY_WORKTREE_ONLY_DOC")
                && !staged_out.contains("dirty_retries")
                && !staged_out.contains("dirty_cfg")
                && !staged_out.contains("dirty_id")
                && !staged_out.contains("dirty_v")
                && !staged_out.contains("9_999_888"),
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
            !commit_out.contains("DIRTY_WORKTREE_ONLY_DOC")
                && !commit_out.contains("dirty_retries")
                && !commit_out.contains("dirty_cfg")
                && !commit_out.contains("dirty_id")
                && !commit_out.contains("dirty_v")
                && !commit_out.contains("9_999_888"),
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

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn test_file_scoped_exposed_set_and_cat_file_newline_path_safety() {
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
            repo.join("src/ext.rs"),
            "pub struct CustomLedgerRule { pub max_hops: u8 }\n\
             /// External validator rule.\n\
             pub fn verify_rule(v: u64) -> bool { v > 10 }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/local.rs"),
            "pub fn verify_rule(v: u64) -> bool { v > 0 }\n\
             pub fn run_check(x: u64) -> bool {\n\
                 crate::ext::verify_rule(x)\n\
             }\n",
        )
        .unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);

        // Modify both `local::verify_rule` (so `verify_rule` is in `exposed_set` for `src/local.rs`)
        // AND `local::run_check` calling `crate::ext::verify_rule`.
        // File-scoped `(norm_path, symbol_name)` dedup MUST still resolve `src/ext.rs`'s `verify_rule`!
        std::fs::write(
            repo.join("src/local.rs"),
            "pub fn verify_rule(v: u64) -> bool { v > 1 }\n\
             pub fn run_check(x: u64) -> bool {\n\
                 crate::ext::verify_rule(x + 1)\n\
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
            out.contains("--- cross-file: src/ext.rs ---")
                && out.contains("// doc: External validator rule."),
            "Modifying `src/local.rs::verify_rule` must not suppress `src/ext.rs::verify_rule`:\n{out}"
        );

        // Ensure read_snapshot_files_batch safely skips `\n`/`\r` protocol injection paths without
        // misaligning subsequent valid paths in `git cat-file --batch`.
        let batch = read_snapshot_files_batch(
            repo,
            GitDiffSource::Commit("HEAD"),
            &[
                PathBuf::from("src/ext.rs"),
                PathBuf::from("bad\n:0:src/local.rs"),
                PathBuf::from("bad2\rpath"),
                PathBuf::from("src/local.rs"),
            ],
        );
        assert_eq!(
            batch.len(),
            2,
            "Only valid paths must be returned: {:?}",
            batch.keys()
        );
        assert!(batch[&PathBuf::from("src/ext.rs")].contains("CustomLedgerRule"));
        assert!(batch[&PathBuf::from("src/local.rs")].contains("run_check"));
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn test_multilingual_utf8_paths_identifiers_docs_and_reverse_callers() {
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
        run(&["config", "user.name", "테스터"]);
        run(&["config", "user.email", "tester@example.com"]);
        run(&["config", "core.autocrlf", "false"]);

        std::fs::create_dir_all(repo.join("src/한글_모듈")).unwrap();
        std::fs::write(
            repo.join("src/한글_모듈/설정.rs"),
            "/// 다국어 결제 한도 설정.\n\
             pub struct 한글_결제설정 { pub 최대금액: u64 }\n\
             /// 수수료를 원 단위로 계산합니다.\n\
             pub fn 수수료_계산(금액: u64, 설정: &한글_결제설정) -> u64 {\n\
                 금액.min(설정.최대금액) / 10\n\
             }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/한글_모듈/결제.rs"),
            "use super::설정::{한글_결제설정, 수수료_계산};\n\
             pub const 기본_할인률: u64 = 5;\n\
             /// 결제 검증 함수 (수정됨).\n\
             pub fn 결제_검증(금액: u64, 원화모드: bool) -> bool {\n\
                 금액 > 기본_할인률 && 원화모드\n\
             }\n\
             pub fn 승인_실행(설정: &한글_결제설정, 금액: u64) -> u64 {\n\
                 if 결제_검증(금액, true) { 수수료_계산(금액, 설정) } else { 0 }\n\
             }\n",
        )
        .unwrap();
        // Unmodified external UTF-8 caller of `결제_검증` (and comments/strings that must be ignored)
        std::fs::write(
            repo.join("src/주문_서비스.rs"),
            "use crate::한글_모듈::결제::결제_검증;\n\
             pub fn 주문_처리(금액: u64) -> bool {\n\
                 // 결제_검증 주석은 무시되어야 함\n\
                 let _msg = \"결제_검증 문자열도 무시\";\n\
                 결제_검증(금액, false)\n\
             }\n",
        )
        .unwrap();
        // External UTF-8 unit test caller in tests/ must be excluded
        std::fs::create_dir_all(repo.join("tests")).unwrap();
        std::fs::write(
            repo.join("tests/한글_테스트.rs"),
            "pub fn test_결제_검증_체크() -> bool {\n    결제_검증(100, true)\n}\n",
        )
        .unwrap();

        run(&["add", "."]);
        run(&["commit", "-q", "-m", "feat: 한글 다국어 초기 커밋"]);

        // Change signature of `결제_검증` and reference `기본_할인률` / `수수료_계산`
        std::fs::write(
            repo.join("src/한글_모듈/결제.rs"),
            "use super::설정::{한글_결제설정, 수수료_계산};\n\
             pub const 기본_할인률: u64 = 5;\n\
             /// 결제 검증 함수 (수정됨).\n\
             pub fn 결제_검증(금액: u64, 수수료_원: u64, 원화모드: bool) -> bool {\n\
                 금액 > 수수료_원 && 원화모드\n\
             }\n\
             pub fn 승인_실행(설정: &한글_결제설정, 금액: u64) -> u64 {\n\
                 if 결제_검증(금액, 10, true) { 수수료_계산(금액, 설정) } else { 0 }\n\
             }\n",
        )
        .unwrap();
        run(&["add", "src/한글_모듈/결제.rs"]);
        run(&["commit", "-q", "-m", "fix: 결제_검증 파라미터 추가"]);

        let sha = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(repo)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let sha = sha.trim();

        let mut warnings = Vec::new();
        let out = generate_expanded_diff_with_config(
            repo,
            GitDiffSource::Commit(sha),
            512 * 1024,
            HybridConfig::full(),
            &mut warnings,
        )
        .unwrap();

        // Layer 1 UTF-8 path + symbols
        assert!(
            out.contains("src/한글_모듈/결제.rs") && out.contains("결제_검증"),
            "Layer 1 must preserve UTF-8 paths and Korean symbols:\n{out}"
        );
        // Layer 3 local deleted const
        assert!(
            out.contains("pub const 기본_할인률: u64 = 5;"),
            "Layer 3-① must recover Korean deleted-line const `기본_할인률`:\n{out}"
        );
        // Layer 3 forward cross-file Korean fn signature + doc
        assert!(
            out.contains("pub struct 한글_결제설정")
                && out.contains("pub fn 수수료_계산(금액: u64, 설정: &한글_결제설정) -> u64")
                && out.contains("// doc: 수수료를 원 단위로 계산합니다."),
            "Layer 3-② must resolve Korean cross-file struct/fn/doc via super:: import:\n{out}"
        );
        // Layer 3 reverse caller (UTF-8 path + non-ASCII ident via git grep without `-w`)
        assert!(
            out.contains("--- cross-file: src/주문_서비스.rs ---")
                && out.contains("[caller of 결제_검증]")
                && out.contains("주문_처리")
                && out.contains("결제_검증(금액, false)"),
            "Layer 3-③ must resolve unmodified Korean reverse caller `주문_처리`:\n{out}"
        );
        // External test file must NOT appear as a reverse caller
        assert!(
            !out.contains("test_결제_검증_체크"),
            "Test file must be excluded from reverse callers:\n{out}"
        );
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn test_reverse_callers_all_10_languages_deleted_guards_snapshots_and_compaction() {
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

        std::fs::create_dir_all(repo.join("src/go_pkg")).unwrap();
        std::fs::create_dir_all(repo.join("src/java_pkg")).unwrap();
        std::fs::create_dir_all(repo.join("src/cs_pkg")).unwrap();

        // 1. Rust: modified fn + inline #[cfg(test)] mod + #[test] fn + deleted whole file `legacy_rs.rs`
        std::fs::write(
            repo.join("src/rs_api.rs"),
            "pub struct RsToken { pub exp: u64 }\n\
             pub fn rs_charge(cents: u64) -> u64 { cents }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/rs_legacy.rs"),
            "pub fn obsolete_rs_hook(id: u64) -> bool { id > 0 }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/rs_caller.rs"),
            "use crate::rs_api::rs_charge;\n\
             use crate::rs_legacy::obsolete_rs_hook;\n\
             pub fn rs_handle_order(v: u64) -> u64 {\n\
                 // rs_charge(999) in comment must be ignored\n\
                 let _s = \"rs_charge(999) in string must be ignored\";\n\
                 if obsolete_rs_hook(v) { rs_charge(v) } else { 0 }\n\
             }\n\
             #[test]\n\
             fn test_rs_charge_unit() { assert_eq!(rs_charge(1), 1); }\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 use super::*;\n\
                 fn helper_in_inline_tests() { let _ = rs_charge(2); }\n\
             }\n",
        )
        .unwrap();

        // 2. Python: modified function + alias caller
        std::fs::write(
            repo.join("src/py_math.py"),
            "def py_tax(amount: int) -> int:\n    return amount // 10\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/py_caller.py"),
            "from src.py_math import py_tax as compute_tax_alias\n\
             def py_invoice(subtotal: int) -> int:\n    return compute_tax_alias(subtotal)\n",
        )
        .unwrap();

        // 3. JavaScript: CommonJS caller
        std::fs::write(
            repo.join("src/js_util.js"),
            "function jsFormatPrice(cents) { return String(cents); }\n\
             module.exports = { jsFormatPrice };\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/js_caller.js"),
            "const { jsFormatPrice } = require('./js_util');\n\
             export function jsRenderRow(c) { return jsFormatPrice(c); }\n",
        )
        .unwrap();

        // 4. TypeScript + 5. TSX
        std::fs::write(
            repo.join("src/ts_calc.ts"),
            "export function tsComputeFee(cents: number): number { return cents * 2; }\n\
             export function tsLegacyDeleted(): void {}\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/tsx_badge.tsx"),
            "export function PriceTag({ label }: { label: string }) { return <span>{label}</span>; }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/ts_consumer.ts"),
            "import { tsComputeFee, tsLegacyDeleted } from './ts_calc';\n\
             export function tsCheckout(v: number): number {\n\
                 tsLegacyDeleted();\n\
                 return tsComputeFee(v);\n\
             }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/tsx_consumer.tsx"),
            "import { PriceTag } from './tsx_badge';\n\
             export function PageView() { return <PriceTag label=\"hi\" />; }\n",
        )
        .unwrap();

        // 6. Go
        std::fs::write(
            repo.join("src/go_pkg/tax.go"),
            "package go_pkg\nfunc GoCalcTax(cents int64) int64 { return cents / 10 }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/go_pkg/order.go"),
            "package go_pkg\nfunc GoBill(total int64) int64 { return GoCalcTax(total) }\n",
        )
        .unwrap();

        // 7. C
        std::fs::write(
            repo.join("src/c_hash.c"),
            "unsigned c_hash_buf(const char *s) { return (unsigned)*s; }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/c_hash.h"),
            "unsigned c_hash_buf(const char *s);\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/c_caller.c"),
            "#include \"c_hash.h\"\nunsigned c_index_key(const char *k) { return c_hash_buf(k); }\n",
        )
        .unwrap();

        // 8. C++
        std::fs::write(
            repo.join("src/cpp_ring.hpp"),
            "class CppRing {\npublic:\n    int cppWrite(int n) { return n; }\n};\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/cpp_caller.cpp"),
            "#include \"cpp_ring.hpp\"\nint cppFlush(CppRing &r, int n) { return r.cppWrite(n); }\n",
        )
        .unwrap();

        // 9. Java
        std::fs::write(
            repo.join("src/java_pkg/InvSvc.java"),
            "package java_pkg;\npublic class InvSvc {\n    public boolean javaReserve(String sku) { return true; }\n}\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/java_pkg/OrderSvc.java"),
            "package java_pkg;\npublic class OrderSvc {\n\
                 public boolean place(InvSvc svc, String s) { return svc.javaReserve(s); }\n\
             }\n",
        )
        .unwrap();

        // 10. C#
        std::fs::write(
            repo.join("src/cs_pkg/PolicySvc.cs"),
            "namespace CsPkg {\n    public class PolicySvc {\n        public bool CsApprove(int cents) { return cents > 0; }\n    }\n}\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/cs_pkg/RefundCtrl.cs"),
            "using CsPkg;\nnamespace CsPkg {\n    public class RefundCtrl {\n        public bool Submit(PolicySvc svc, int c) { return svc.CsApprove(c); }\n    }\n}\n",
        )
        .unwrap();

        // 11. Ruby
        std::fs::write(
            repo.join("src/rb_guard.rb"),
            "class RbGuard\n  def self.allow_req?(key)\n    !key.empty?\n  end\nend\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/rb_app.rb"),
            "require_relative 'rb_guard'\nclass RbApp\n  def check(k)\n    RbGuard.allow_req?(k)\n  end\nend\n",
        )
        .unwrap();

        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init all 10 languages"]);

        // Modify all 10 targets (+ delete src/rs_legacy.rs + delete tsLegacyDeleted from src/ts_calc.ts)
        std::fs::write(
            repo.join("src/rs_api.rs"),
            "pub struct RsToken { pub exp: u64, pub scope: u8 }\n\
             pub fn rs_charge(cents: u64, rate: u32) -> u64 { cents * rate as u64 }\n",
        )
        .unwrap();
        std::fs::remove_file(repo.join("src/rs_legacy.rs")).unwrap();
        std::fs::write(
            repo.join("src/py_math.py"),
            "def py_tax(amount: int, bps: int) -> int:\n    return (amount * bps) // 10000\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/js_util.js"),
            "function jsFormatPrice(cents, cur) { return `${cur}${cents}`; }\n\
             module.exports = { jsFormatPrice };\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/ts_calc.ts"),
            "export function tsComputeFee(cents: number, taxRate: number): number { return cents * taxRate; }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/tsx_badge.tsx"),
            "export function PriceTag({ label, currency }: { label: string; currency: string }) { return <span>{currency}{label}</span>; }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/go_pkg/tax.go"),
            "package go_pkg\nfunc GoCalcTax(cents int64, bps int64) int64 { return (cents * bps) / 10000 }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/c_hash.c"),
            "unsigned c_hash_buf(const char *s, unsigned len) { return len ? (unsigned)*s : 0; }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/cpp_ring.hpp"),
            "class CppRing {\npublic:\n    int cppWrite(int n, bool flush) { return flush ? n : 0; }\n};\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/java_pkg/InvSvc.java"),
            "package java_pkg;\npublic class InvSvc {\n    public boolean javaReserve(String sku, int qty) { return qty > 0; }\n}\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/cs_pkg/PolicySvc.cs"),
            "namespace CsPkg {\n    public class PolicySvc {\n        public bool CsApprove(int cents, bool strict) { return strict && cents > 0; }\n    }\n}\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/rb_guard.rb"),
            "class RbGuard\n  def self.allow_req?(key, cost: 1)\n    cost <= 10 && !key.empty?\n  end\nend\n",
        )
        .unwrap();

        // Stage everything for Staged snapshot test, then overwrite caller in worktree ONLY!
        run(&["add", "-A"]);
        std::fs::write(
            repo.join("src/rs_caller.rs"),
            "pub fn DIRTY_WORKTREE_CALLER() { rs_charge(9_777_666); }\n",
        )
        .unwrap();

        let mut warnings = Vec::new();
        // Test Staged snapshot accuracy + callers across all languages
        let staged_out = generate_expanded_diff_with_config(
            repo,
            GitDiffSource::Staged,
            512 * 1024,
            HybridConfig::full(),
            &mut warnings,
        )
        .unwrap();

        assert!(
            !staged_out.contains("DIRTY_WORKTREE_CALLER") && !staged_out.contains("9_777_666"),
            "Staged reverse callers must read staged index, never dirty worktree:\n{staged_out}"
        );
        // Verify exclusions in rs_caller.rs and C header forward declarations
        assert!(
            !staged_out.contains("test_rs_charge_unit")
                && !staged_out.contains("helper_in_inline_tests")
                && !staged_out.contains("rs_charge(999)")
                && !staged_out.contains("--- cross-file: src/c_hash.h ---"),
            "Comments, strings, #[test], #[cfg(test)] mod tests, and C header prototypes must NOT appear as callers:\n{staged_out}"
        );

        // Check callers resolved across all 10 languages (+ deleted file / deleted fn)
        for expected in [
            "[caller of rs_charge]",
            "rs_handle_order",
            "[caller of obsolete_rs_hook]",
            "[caller of py_tax]",
            "py_invoice",
            "[caller of jsFormatPrice]",
            "[caller of tsComputeFee]",
            "[caller of tsLegacyDeleted]",
            "[caller of PriceTag]",
            "[caller of GoCalcTax]",
            "[caller of c_hash_buf]",
            "[caller of cppWrite]",
            "[caller of javaReserve]",
            "[caller of CsApprove]",
            "[caller of allow_req?]",
        ] {
            assert!(
                staged_out.contains(expected),
                "Missing expected reverse caller marker `{expected}`:\n{staged_out}"
            );
        }

        // Test layer3_callers-only mode (`layer3_local: false, layer3_cross: false, layer3_callers: true`)
        let mut w_callers = Vec::new();
        let callers_only_cfg = HybridConfig::parse(Some("callers"));
        let callers_only_out = generate_expanded_diff_with_config(
            repo,
            GitDiffSource::Staged,
            512 * 1024,
            callers_only_cfg,
            &mut w_callers,
        )
        .unwrap();
        assert!(
            callers_only_out.contains("=== External AST Context (1-hop, unmodified) ===")
                && callers_only_out.contains("[caller of rs_charge]"),
            "Callers-only HybridConfig must render reverse callers even when layer3_cross=false:\n{callers_only_out}"
        );

        // Test compaction shedding: tiny budget forcing Tier 3+ must drop reverse callers
        let mut w_tight = Vec::new();
        let tight_out = generate_expanded_diff_with_config(
            repo,
            GitDiffSource::Staged,
            2048,
            HybridConfig::full(),
            &mut w_tight,
        )
        .unwrap();
        assert!(
            !tight_out.contains("[caller of "),
            "Tight budget forcing prod compaction must shed reverse callers:\n{tight_out}"
        );
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn test_sig_only_and_deleted_renamed_single_line_headers_emit_reverse_callers() {
        use crate::tree_sitter_diff::generate_expanded_diff_with_config;
        use std::process::Command;

        // 1. Direct incomplete header recovery tests without trailing newlines
        let del_rs = collect_deleted_lines_reverse_targets(
            &["    pub fn legacy_handler(x: u64, cfg: &Cfg) -> u64 {"],
            Path::new("src/svc.rs"),
            SupportedLanguage::Rust,
        );
        assert!(
            del_rs
                .iter()
                .any(|t| t.symbol == "legacy_handler" && t.sig_changed),
            "Incomplete Rust 1-line signature header must recover symbol: {del_rs:?}"
        );

        let del_py = collect_deleted_lines_reverse_targets(
            &["    def legacy_py_hook(self, amount: int) -> int:"],
            Path::new("src/svc.py"),
            SupportedLanguage::Python,
        );
        assert!(
            del_py.iter().any(|t| t.symbol == "legacy_py_hook"),
            "Incomplete Python 1-line def header must recover symbol: {del_py:?}"
        );

        let del_ts = collect_deleted_lines_reverse_targets(
            &["    async legacyTsMethod(id: string): Promise<number> {"],
            Path::new("src/svc.ts"),
            SupportedLanguage::TypeScript,
        );
        assert!(
            del_ts.iter().any(|t| t.symbol == "legacyTsMethod"),
            "Standalone deleted TS class method must recover symbol via class wrapper: {del_ts:?}"
        );
        assert!(
            !del_ts.iter().any(|t| t.symbol == "__AnticrumbDelWrap"),
            "Dummy wrapper class name must never leak: {del_ts:?}"
        );

        let del_rb = collect_deleted_lines_reverse_targets(
            &["  def legacy_allow?(key)"],
            Path::new("src/svc.rb"),
            SupportedLanguage::Ruby,
        );
        assert!(
            del_rb.iter().any(|t| t.symbol == "legacy_allow?"),
            "Incomplete Ruby def header must recover predicate symbol: {del_rb:?}"
        );

        let del_multi = collect_deleted_lines_reverse_targets(
            &[
                "pub struct OldCfg { pub limit: u64 }",
                "pub fn old_helper() -> u64 { 1 }",
            ],
            Path::new("src/cfg.rs"),
            SupportedLanguage::Rust,
        );
        let cfg_t = del_multi
            .iter()
            .find(|t| t.symbol == "OldCfg")
            .expect("OldCfg");
        let fn_t = del_multi
            .iter()
            .find(|t| t.symbol == "old_helper")
            .expect("old_helper");
        assert!(
            cfg_t.is_type,
            "Struct target in multi-def hunk must preserve is_type=true"
        );
        assert!(
            !fn_t.is_type,
            "Function target in multi-def hunk must have is_type=false"
        );

        // 2. End-to-end `[sig-only]` edit & `[sig-only]` rename must STILL emit reverse callers!
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

        let long_body: String = (0..30)
            .map(|i| format!("    let v_{i} = x + {i};\n"))
            .collect();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(
            repo.join("src/api.rs"),
            format!(
                "pub struct Cfg {{ pub n: u64 }}\n\
                 pub fn sig_only_svc(x: u64) -> u64 {{\n{long_body}    v_29\n}}\n\
                 pub fn rename_before(x: u64) -> u64 {{\n{long_body}    v_29\n}}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            repo.join("src/caller.rs"),
            "use crate::api::{sig_only_svc, rename_before};\n\
             pub fn invoke_both(x: u64) -> u64 {\n\
                 sig_only_svc(x) + rename_before(x)\n\
             }\n",
        )
        .unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);

        // Pure visibility/param-free rename/sig edit on large bodies so Layer 2 emits `[sig-only]`
        std::fs::write(
            repo.join("src/api.rs"),
            format!(
                "pub struct Cfg {{ pub n: u64 }}\n\
                 pub(crate) fn sig_only_svc(x: u64) -> u64 {{\n{long_body}    v_29\n}}\n\
                 pub fn rename_after(x: u64) -> u64 {{\n{long_body}    v_29\n}}\n"
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

        assert!(
            out.contains("[sig-only; body unchanged:"),
            "Expected [sig-only] suppression on large unchanged bodies:\n{out}"
        );
        assert!(
            out.contains("[caller of sig_only_svc]") && out.contains("invoke_both"),
            "[sig-only] edit MUST still surface reverse caller of sig_only_svc:\n{out}"
        );
        assert!(
            out.contains("[caller of rename_before]") && out.contains("invoke_both"),
            "[sig-only] rename MUST still recover old name and surface reverse caller:\n{out}"
        );
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn test_contains_utf8_ident_ruby_vs_macros_and_nullable_reverse_callers() {
        use crate::tree_sitter_diff::generate_expanded_diff_with_config;
        use std::process::Command;

        // Unit assertions for Ruby suffix isolation vs non-Ruby `?` / `!` operators
        assert!(contains_utf8_ident("my_macro!(x)", "my_macro", false));
        assert!(contains_utf8_ident("x: MyConfig?", "MyConfig", false));
        assert!(contains_utf8_ident("myFunc?.()", "myFunc", false));
        assert!(contains_utf8_ident("myVal!.run()", "myVal", false));
        assert!(contains_utf8_ident("MY_CONST?", "MY_CONST", false));
        assert!(!contains_utf8_ident("allow_req?(k)", "allow_req", true));
        assert!(contains_utf8_ident("allow_req?(k)", "allow_req?", true));
        assert!(!contains_utf8_ident("save!(rec)", "save", true));

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
        // Rust macro + constant
        std::fs::write(
            repo.join("src/macros.rs"),
            "#[macro_export]\nmacro_rules! my_macro {\n    ($x:expr) => { Some($x) };\n}\n\
             pub const MY_FLAG: u8 = 1;\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/macro_caller.rs"),
            "pub fn use_macro(v: u8) -> Option<u8> {\n    my_macro!(v)\n}\n",
        )
        .unwrap();
        // TypeScript nullable config + optional chaining call + non-null call
        std::fs::write(
            repo.join("src/ts_cfg.ts"),
            "export interface MyConfig { timeout: number; }\n\
             export function optCall(v: number): number { return v + 1; }\n\
             export function bangCall(v: number): number { return v + 2; }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/ts_caller.ts"),
            "import { MyConfig, optCall, bangCall } from './ts_cfg';\n\
             export function runTs(c: MyConfig?) {\n\
                 return (optCall?.(1) ?? 0) + bangCall!(2);\n\
             }\n",
        )
        .unwrap();
        // Ruby with predicate vs non-predicate isolation
        std::fs::write(
            repo.join("src/rb_svc.rb"),
            "class RbSvc\n  def self.allow_req(k)\n    k\n  end\nend\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/rb_only_pred.rb"),
            "class RbPredUser\n  def check(k)\n    RbGuard.allow_req?(k)\n  end\nend\n",
        )
        .unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);

        std::fs::write(
            repo.join("src/macros.rs"),
            "#[macro_export]\nmacro_rules! my_macro {\n    ($x:expr) => { Some($x + 1) };\n}\n\
             pub const MY_FLAG: u8 = 2;\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/ts_cfg.ts"),
            "export interface MyConfig { timeout: number; retries: number; }\n\
             export function optCall(v: number, strict: boolean): number { return strict ? v : 0; }\n\
             export function bangCall(v: number, tag: string): number { return v + tag.length; }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/rb_svc.rb"),
            "class RbSvc\n  def self.allow_req(k, strict: true)\n    strict ? k : nil\n  end\nend\n",
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
            out.contains("[caller of my_macro]") && out.contains("use_macro"),
            "Rust macro_invocation `my_macro!(v)` must resolve reverse caller:\n{out}"
        );
        assert!(
            out.contains("[caller of optCall]") && out.contains("runTs"),
            "TS optional call `optCall?.(1)` must resolve reverse caller:\n{out}"
        );
        assert!(
            out.contains("[caller of bangCall]") && out.contains("runTs"),
            "TS non-null call `bangCall!(2)` must resolve reverse caller:\n{out}"
        );
        assert!(
            !out.contains("RbPredUser") && !out.contains("rb_only_pred.rb"),
            "Ruby `allow_req?` must NOT falsely match modified `allow_req`:\n{out}"
        );
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn test_reverse_grep_not_starved_by_docs_tests_vendor_or_minified_bundle() {
        use crate::tree_sitter_diff::generate_expanded_diff_with_config;
        use std::io::Cursor;
        use std::process::Command;

        // 1. Unit check `read_grep_line_bounded` on >4 KiB single line followed by another line
        let big_line = format!("a/x.js\x001\x00{}{}\n", "x".repeat(8192), "TAIL_MARKER");
        let second_line = "b/y.rs\x007\x00prod_call()\n";
        let payload = format!("{big_line}{second_line}");
        let mut reader = std::io::BufReader::new(Cursor::new(payload.as_bytes()));
        let mut buf = Vec::new();
        let st1 = read_grep_line_bounded(&mut reader, &mut buf, 1024, 1024 * 1024).unwrap();
        assert!(matches!(st1, GrepLineRead::Line(_)));
        assert!(buf.len() <= 1024 + 4096);
        let st2 = read_grep_line_bounded(&mut reader, &mut buf, 1024, 1024 * 1024).unwrap();
        assert!(matches!(st2, GrepLineRead::Line(_)));
        assert_eq!(buf, b"b/y.rs\x007\x00prod_call()");

        // 2. End-to-end repo where docs/ + tests/ + vendor/ + 600 KiB minified bundle precede src/
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

        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::create_dir_all(repo.join("tests")).unwrap();
        std::fs::create_dir_all(repo.join("vendor")).unwrap();
        std::fs::create_dir_all(repo.join("assets")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();

        let docs_noise: String = (0..450)
            .map(|i| format!("Line {i}: see `target_prod_fn` for billing.\n"))
            .collect();
        std::fs::write(repo.join("docs/guide.md"), docs_noise).unwrap();
        let test_noise: String = (0..120)
            .map(|i| format!("pub fn test_case_{i}() {{ let _ = target_prod_fn({i}); }}\n"))
            .collect();
        std::fs::write(repo.join("tests/billing_test.rs"), test_noise).unwrap();
        let vendor_noise: String = (0..120)
            .map(|i| format!("pub fn v_{i}() {{ let _ = target_prod_fn({i}); }}\n"))
            .collect();
        std::fs::write(repo.join("vendor/third.rs"), vendor_noise).unwrap();
        let minified_line = format!(
            "/*minified*/var x='{}';target_prod_fn(1);{}\n",
            "a".repeat(320 * 1024),
            "b".repeat(320 * 1024)
        );
        std::fs::write(repo.join("assets/bundle.min.js"), &minified_line).unwrap();
        std::fs::write(repo.join("assets/big_inline.js"), &minified_line).unwrap();

        std::fs::write(
            repo.join("src/prod_api.rs"),
            "pub fn target_prod_fn(cents: u64) -> u64 { cents * 2 }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/z_real_caller.rs"),
            "use crate::prod_api::target_prod_fn;\n\
             pub fn execute_charge(c: u64) -> u64 { target_prod_fn(c) }\n",
        )
        .unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);

        std::fs::write(
            repo.join("src/prod_api.rs"),
            "pub fn target_prod_fn(cents: u64, tax_bps: u32) -> u64 { cents + tax_bps as u64 }\n",
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
            out.contains("[caller of target_prod_fn]") && out.contains("execute_charge"),
            "Production caller after >400 docs/tests/vendor lines and >600 KiB minified bundle must NOT be starved:\n{out}"
        );
        assert!(
            !out.contains("guide.md")
                && !out.contains("billing_test.rs")
                && !out.contains("third.rs")
                && !out.contains("bundle.min.js")
                && !out.contains("big_inline.js"),
            "Non-eligible files must never appear in reverse callers:\n{out}"
        );
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn test_budget_unpinned_forward_does_not_suppress_pinned_reverse_callers() {
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
        // 15 unimported files defining PascalCase types that will produce Tier 4 unpinned forward hits
        for i in 0..15 {
            std::fs::write(
                repo.join(format!("src/unpinned_{i:02}.rs")),
                format!("pub struct GhostRule{i:02} {{ pub f1: u64, pub f2: u64, pub f3: u64 }}\n"),
            )
            .unwrap();
        }
        let mut refs_body = String::new();
        for i in 0..15 {
            refs_body.push_str(&format!("    let _g{i}: Option<GhostRule{i:02}> = None;\n"));
        }
        std::fs::write(
            repo.join("src/modified.rs"),
            format!("pub fn critical_hook(x: u64) -> u64 {{\n{refs_body}    x\n}}\n"),
        )
        .unwrap();
        std::fs::write(
            repo.join("src/real_caller.rs"),
            "use crate::modified::critical_hook;\n\
             pub fn invoke_hook(v: u64) -> u64 { critical_hook(v) }\n",
        )
        .unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);

        std::fs::write(
            repo.join("src/modified.rs"),
            format!("pub fn critical_hook(x: u64, fee: u32) -> u64 {{\n{refs_body}    x + fee as u64\n}}\n"),
        )
        .unwrap();

        let mut warnings = Vec::new();
        // ~12 KiB budget => l3_budget ~ 1.8 KiB (< 15 unpinned forward structs, > Tier 3 pinned caller).
        // Tier 4 unpinned forward items MUST NOT prevent running Tier 3 pinned reverse caller lookup!
        let out = generate_expanded_diff_with_config(
            repo,
            GitDiffSource::Unstaged,
            12 * 1024,
            HybridConfig::full(),
            &mut warnings,
        )
        .unwrap();

        assert!(
            out.contains("[caller of critical_hook]") && out.contains("invoke_hook"),
            "Tier 4 unpinned forward entries must NOT block Tier 3 pinned reverse callers:\n{out}"
        );
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn test_reverse_caller_24_file_cap_prioritizes_sig_changed_and_type_over_body_only() {
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
        for i in 0..28 {
            std::fs::write(
                repo.join(format!("src/aa_body_caller_{i:02}.rs")),
                "use crate::lib_api::helper_body_only;\n\
                 pub fn use_body(v: u64) -> u64 { helper_body_only(v) }\n",
            )
            .unwrap();
        }
        std::fs::write(
            repo.join("src/zz_sig_caller.rs"),
            "use crate::lib_api::critical_api_sig;\n\
             pub fn call_sig(v: u64) -> u64 { critical_api_sig(v) }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/zz_type_caller.rs"),
            "use crate::lib_api::CriticalCfg;\n\
             pub fn read_cfg(c: &CriticalCfg) -> u64 { c.limit }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/lib_api.rs"),
            "pub struct CriticalCfg { pub limit: u64 }\n\
             pub fn critical_api_sig(x: u64) -> u64 { x + 1 }\n\
             pub fn helper_body_only(x: u64) -> u64 {\n\
                 let a = x + 1;\n\
                 let b = a + 2;\n\
                 let c = b + 3;\n\
                 let d = c + 4;\n\
                 let e = d + 5;\n\
                 e\n\
             }\n",
        )
        .unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);

        std::fs::write(
            repo.join("src/lib_api.rs"),
            "pub struct CriticalCfg { pub limit: u64, pub strict: bool }\n\
             pub fn critical_api_sig(x: u64, y: u64) -> u64 { x + y }\n\
             pub fn helper_body_only(x: u64) -> u64 {\n\
                 let a = x + 1;\n\
                 let b = a + 2;\n\
                 let c = b + 3;\n\
                 let d = c + 99;\n\
                 let e = d + 5;\n\
                 e\n\
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
            out.contains("[caller of critical_api_sig]") && out.contains("call_sig"),
            "28 body-only callers must NOT evict sig_changed caller past 24-file cap:\n{out}"
        );
        assert!(
            out.contains("[caller of CriticalCfg]") && out.contains("read_cfg"),
            "28 body-only callers must NOT evict type target caller past 24-file cap:\n{out}"
        );
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn test_qualifier_bypass_guards_rust_overpinning_and_multilingual_qualifiers() {
        use crate::tree_sitter_diff::generate_expanded_diff_with_config;
        use std::process::Command;

        // 1. Unit checks for `import_spec_matches_target_file` (Rust std/self/overpinning & TS `.js`)
        assert!(!import_spec_matches_target_file(
            SupportedLanguage::Rust,
            Path::new("src/consumer.rs"),
            "std::io::Read",
            Path::new("src/io.rs"),
            "src/io.rs",
            "io",
        ));
        assert!(!import_spec_matches_target_file(
            SupportedLanguage::Rust,
            Path::new("src/sub/consumer.rs"),
            "self::inner::Helper",
            Path::new("src/io.rs"),
            "src/io.rs",
            "io",
        ));
        assert!(!import_spec_matches_target_file(
            SupportedLanguage::Rust,
            Path::new("src/consumer.rs"),
            "tokio::sync::Mutex",
            Path::new("src/sync.rs"),
            "src/sync.rs",
            "sync",
        ));
        assert!(import_spec_matches_target_file(
            SupportedLanguage::Rust,
            Path::new("src/consumer.rs"),
            "crate::io::flush_io",
            Path::new("src/io.rs"),
            "src/io.rs",
            "io",
        ));
        assert!(!import_spec_matches_target_file(
            SupportedLanguage::TypeScript,
            Path::new("src/app.ts"),
            "./foo.js",
            Path::new("src/foo.js.ts"),
            "src/foo.js.ts",
            "foojs",
        ));

        // 2. End-to-end AST qualifier bypass checks across Rust, Python, and TSX (including Unicode)
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
        run(&["config", "user.name", "테스터"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "core.autocrlf", "false"]);

        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(
            repo.join("src/target_mod.rs"),
            "pub fn rs_target(x: u64) -> u64 { x }\n\
             pub fn 한글_검증(금액: u64) -> bool { 금액 > 0 }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/other_mod.rs"),
            "pub fn rs_target(x: u64) -> u64 { x + 1 }\n\
             pub fn 한글_검증(금액: u64) -> bool { 금액 > 10 }\n",
        )
        .unwrap();
        // Pinned import to target_mod, yet ONLY calls `other_mod::rs_target` / `.map(other_mod::한글_검증)` -> MUST reject!
        std::fs::write(
            repo.join("src/rs_false_caller.rs"),
            "use crate::target_mod::{rs_target, 한글_검증};\n\
             use crate::other_mod;\n\
             pub fn wrong_mod(xs: &[u64]) -> Vec<bool> {\n\
                 let _ = other_mod::rs_target(1);\n\
                 xs.iter().copied().map(other_mod::한글_검증).collect()\n\
             }\n",
        )
        .unwrap();
        // Valid aliased static path caller -> MUST accept!
        std::fs::write(
            repo.join("src/rs_valid_alias.rs"),
            "use crate::target_mod as tm;\n\
             pub fn valid_alias_call(x: u64) -> u64 { tm::rs_target(x) }\n",
        )
        .unwrap();

        // Python pinned import, yet ONLY calls `OtherClass.py_fn()` and `cb = OtherClass.py_fn` -> MUST reject!
        std::fs::write(
            repo.join("src/py_mod.py"),
            "def py_fn(x: int) -> int:\n    return x\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/py_false.py"),
            "from src.py_mod import py_fn\n\
             class OtherClass:\n    @staticmethod\n    def py_fn(x):\n        return x\n\
             def bad_py(x: int):\n\
                 cb = OtherClass.py_fn\n\
                 return OtherClass.py_fn(x)\n",
        )
        .unwrap();

        // TSX pinned import, yet ONLY renders `<OtherNS.BadgeView />` -> MUST reject!
        std::fs::write(
            repo.join("src/badge.tsx"),
            "export function BadgeView({ n }: { n: number }) { return <b>{n}</b>; }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/tsx_false.tsx"),
            "import { BadgeView } from './badge';\n\
             import * as other_ns from './other_badge';\n\
             export function FalsePage() {\n\
                 return <><OtherNS.BadgeView n={1} /><other_ns.BadgeView n={2} /></>;\n\
             }\n",
        )
        .unwrap();

        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);

        std::fs::write(
            repo.join("src/target_mod.rs"),
            "pub fn rs_target(x: u64, s: bool) -> u64 { if s { x } else { 0 } }\n\
             pub fn 한글_검증(금액: u64, 수수료: u64) -> bool { 금액 > 수수료 }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/py_mod.py"),
            "def py_fn(x: int, factor: int) -> int:\n    return x * factor\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/badge.tsx"),
            "export function BadgeView({ n, label }: { n: number; label: string }) { return <b>{label}{n}</b>; }\n",
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
            !out.contains("rs_false_caller.rs") && !out.contains("wrong_mod"),
            "Pinned Rust file calling only `other_mod::...` must NOT match target_mod:\n{out}"
        );
        assert!(
            out.contains("rs_valid_alias.rs") && out.contains("valid_alias_call"),
            "Aliased Rust `use crate::target_mod as tm; tm::rs_target` must still resolve:\n{out}"
        );
        assert!(
            !out.contains("py_false.py") && !out.contains("bad_py"),
            "Pinned Python file calling only `OtherClass.py_fn` must NOT bypass qualifier guard:\n{out}"
        );
        assert!(
            !out.contains("tsx_false.tsx") && !out.contains("FalsePage"),
            "Pinned TSX file rendering only `<OtherNS.BadgeView />` or `<other_ns.BadgeView />` must NOT bypass qualifier guard:\n{out}"
        );
    }

    #[test]
    fn test_read_stream_bounded_truncation_and_signal() {
        use crate::tree_sitter_diff::read_stream_bounded;
        use std::io::Cursor;
        use std::sync::atomic::{AtomicBool, Ordering};

        let sig = AtomicBool::new(false);
        let data = b"0123456789abcdef";
        let (buf, truncated) = read_stream_bounded(Cursor::new(&data[..]), Some(8), Some(&sig));
        assert_eq!(buf, b"01234567");
        assert!(truncated);
        assert!(sig.load(Ordering::Acquire));

        let sig_ok = AtomicBool::new(false);
        let (buf_ok, truncated_ok) =
            read_stream_bounded(Cursor::new(&data[..]), Some(16), Some(&sig_ok));
        assert_eq!(buf_ok, data);
        assert!(!truncated_ok);
        assert!(!sig_ok.load(Ordering::Acquire));
    }

    #[cfg(feature = "tree-sitter")]
    #[test]
    fn test_dart_imports_multiline_show_part_and_monorepo_packages() {
        let dart_src = "\
import 'package:pkg_a/models/user.dart'
    show
      UserRecord,
      UserRole;
import 'package:pkg_a/models/hidden.dart'
    hide
      InternalThing;
import 'package:pkg_a/utils/math_ops.dart' as math_alias;
import 'package:pkg_a/utils/calc.dart' as c;
@Deprecated('use v2')
part 'user.g.dart';
part of 'library_root.dart';
@Deprecated('legacy library')
part of my_legacy_lib;

const int kMaxRetries = 3;
final String kDefaultTag = 'v1';
var mutableCount = 0;
int mutableTyped = 1;
";
        let tree = parse_with_timeout(SupportedLanguage::Dart, dart_src).unwrap();
        let mut import_specs = Vec::new();
        let mut imported_symbols = HashMap::new();
        let mut alias_map = HashMap::new();
        collect_file_imports(
            SupportedLanguage::Dart,
            &tree.root_node(),
            dart_src,
            &mut import_specs,
            &mut imported_symbols,
            &mut alias_map,
        );

        assert!(
            imported_symbols
                .get("UserRecord")
                .is_some_and(|v| v.iter().any(|s| s == "package:pkg_a/models/user.dart")),
            "multiline show combinator must extract UserRecord: {imported_symbols:?}"
        );
        assert!(
            imported_symbols
                .get("UserRole")
                .is_some_and(|v| v.iter().any(|s| s == "package:pkg_a/models/user.dart")),
            "multiline show combinator must extract UserRole: {imported_symbols:?}"
        );
        assert!(
            !imported_symbols.contains_key("InternalThing"),
            "hide combinator must NOT register hidden symbols as shown: {imported_symbols:?}"
        );
        assert_eq!(
            alias_map.get("math_alias").map(String::as_str),
            Some("math_ops"),
            "multi-char Dart import alias must populate alias_map: {alias_map:?}"
        );
        assert_eq!(
            alias_map.get("c").map(String::as_str),
            Some("calc"),
            "single-char Dart import alias must populate alias_map: {alias_map:?}"
        );
        assert!(
            imported_symbols
                .get("math_alias")
                .is_some_and(|v| v.iter().any(|s| s == "package:pkg_a/utils/math_ops.dart")),
            "multi-char Dart alias must register in imported_symbols: {imported_symbols:?}"
        );
        assert!(
            import_specs.iter().any(|s| s == "user.g.dart"),
            "annotated part directive must extract URI node, not annotation string: {import_specs:?}"
        );
        assert!(
            import_specs.iter().any(|s| s == "library_root.dart"),
            "part of directive must be collected: {import_specs:?}"
        );
        assert!(
            !import_specs
                .iter()
                .any(|s| s == "use v2" || s == "legacy library"),
            "annotation string arguments on part/part-of directives must NOT leak into import_specs: {import_specs:?}"
        );

        // Verify `is_type_or_const_def_node` matches `is_dart_primary_definition` for top-level vars
        let mut var_results = Vec::new();
        let mut cursor = tree.root_node().walk();
        for ch in tree.root_node().named_children(&mut cursor) {
            if ch.kind() == "top_level_variable_declaration" {
                let text = &dart_src[ch.start_byte()..ch.end_byte()];
                var_results.push((
                    text.trim().to_string(),
                    is_type_or_const_def_node(SupportedLanguage::Dart, &ch),
                ));
            }
        }
        assert_eq!(var_results.len(), 4);
        assert!(var_results[0].1, "const top-level var must be true");
        assert!(var_results[1].1, "final top-level var must be true");
        assert!(!var_results[2].1, "mutable var must be false");
        assert!(
            !var_results[3].1,
            "mutable typed top-level var must be false"
        );

        // Monorepo candidate resolution: `package:pkg_a/models/user.dart` must prefer `packages/pkg_a/lib/...`
        let mono_files = vec![
            PathBuf::from("packages/pkg_a/lib/models/user.dart"),
            PathBuf::from("packages/pkg_b/lib/models/user.dart"),
            PathBuf::from("packages/pkg_a/lib/app.dart"),
            PathBuf::from("packages/pkg_a/test/models/user.dart"),
        ];
        let mono_idx = TrackedRepoIndex::build(&mono_files);
        let q_mono = FileRefQuery {
            file_path: PathBuf::from("packages/pkg_a/lib/app.dart"),
            lang: SupportedLanguage::Dart,
            type_or_const_refs: HashSet::from(["UserRecord".to_string()]),
            qualified_refs: Vec::new(),
            call_refs: HashSet::new(),
            import_specs: vec!["package:pkg_a/models/user.dart".to_string()],
            imported_symbols: HashMap::new(),
        };
        let resolved_mono = resolve_candidate_files_for_query(&q_mono, &mono_idx);
        let pinned_paths: Vec<&Path> = resolved_mono
            .iter()
            .filter(|c| c.pinned_by_import)
            .map(|c| c.path.as_path())
            .collect();
        assert_eq!(
            pinned_paths,
            vec![Path::new("packages/pkg_a/lib/models/user.dart")],
            "monorepo package: import must resolve strictly to pkg_a, not pkg_b"
        );

        // Monorepo third-party / mismatched pubspec package name fallback must NOT cross package boundaries
        let mono_ext_files = vec![
            PathBuf::from("packages/pkg_a/lib/app.dart"),
            PathBuf::from("packages/pkg_a/lib/models/local_only.dart"),
            PathBuf::from("packages/pkg_b/lib/models/user.dart"),
            PathBuf::from("mobile/lib/ui/screen.dart"),
            PathBuf::from("mobile/lib/models/user.dart"),
        ];
        let mono_ext_idx = TrackedRepoIndex::build(&mono_ext_files);
        let q_third_party = FileRefQuery {
            file_path: PathBuf::from("packages/pkg_a/lib/app.dart"),
            lang: SupportedLanguage::Dart,
            type_or_const_refs: HashSet::from(["UserRecord".to_string()]),
            qualified_refs: Vec::new(),
            call_refs: HashSet::new(),
            import_specs: vec!["package:some_third_party_sdk/models/user.dart".to_string()],
            imported_symbols: HashMap::new(),
        };
        let third_party_pinned: Vec<PathBuf> =
            resolve_candidate_files_for_query(&q_third_party, &mono_ext_idx)
                .into_iter()
                .filter(|c| c.pinned_by_import)
                .map(|c| c.path)
                .collect();
        assert!(
            third_party_pinned.is_empty(),
            "third-party package: import in pkg_a must NOT cross-pin pkg_b or mobile: {third_party_pinned:?}"
        );
        let q_subdir_pkg = FileRefQuery {
            file_path: PathBuf::from("mobile/lib/ui/screen.dart"),
            lang: SupportedLanguage::Dart,
            type_or_const_refs: HashSet::from(["UserRecord".to_string()]),
            qualified_refs: Vec::new(),
            call_refs: HashSet::new(),
            import_specs: vec!["package:acme_mobile/models/user.dart".to_string()],
            imported_symbols: HashMap::new(),
        };
        let subdir_pinned: Vec<PathBuf> =
            resolve_candidate_files_for_query(&q_subdir_pkg, &mono_ext_idx)
                .into_iter()
                .filter(|c| c.pinned_by_import)
                .map(|c| c.path)
                .collect();
        assert_eq!(
            subdir_pinned,
            vec![PathBuf::from("mobile/lib/models/user.dart")],
            "subdirectory package with different pubspec name must pin only its own lib/, never sibling pkg_b"
        );

        // Single-package repo fallback: `package:my_app/models/user.dart` -> `lib/models/user.dart`
        let single_files = vec![
            PathBuf::from("lib/models/user.dart"),
            PathBuf::from("lib/app.dart"),
            PathBuf::from("test/models/user.dart"),
        ];
        let single_idx = TrackedRepoIndex::build(&single_files);
        let q_single = FileRefQuery {
            file_path: PathBuf::from("lib/app.dart"),
            lang: SupportedLanguage::Dart,
            type_or_const_refs: HashSet::from(["UserRecord".to_string()]),
            qualified_refs: Vec::new(),
            call_refs: HashSet::new(),
            import_specs: vec!["package:my_app/models/user.dart".to_string()],
            imported_symbols: HashMap::new(),
        };
        let resolved_single = resolve_candidate_files_for_query(&q_single, &single_idx);
        let single_pinned: Vec<&Path> = resolved_single
            .iter()
            .filter(|c| c.pinned_by_import)
            .map(|c| c.path.as_path())
            .collect();
        assert_eq!(
            single_pinned,
            vec![Path::new("lib/models/user.dart")],
            "single-package root repo must still resolve package: to lib/ (and ignore test/)"
        );

        // Reverse `import_spec_matches_target_file` checks for monorepo & single-app layouts
        assert!(import_spec_matches_target_file(
            SupportedLanguage::Dart,
            Path::new("packages/app/lib/main.dart"),
            "package:pkg_a/models/user.dart",
            Path::new("packages/pkg_a/lib/models/user.dart"),
            "packages/pkg_a/lib/models/user.dart",
            "user",
        ));
        assert!(!import_spec_matches_target_file(
            SupportedLanguage::Dart,
            Path::new("packages/app/lib/main.dart"),
            "package:pkg_a/models/user.dart",
            Path::new("packages/pkg_b/lib/models/user.dart"),
            "packages/pkg_b/lib/models/user.dart",
            "user",
        ));
        assert!(import_spec_matches_target_file(
            SupportedLanguage::Dart,
            Path::new("lib/main.dart"),
            "package:demo_app/models/user.dart",
            Path::new("lib/models/user.dart"),
            "lib/models/user.dart",
            "user",
        ));
        assert!(import_spec_matches_target_file(
            SupportedLanguage::Dart,
            Path::new("mobile/lib/ui/screen.dart"),
            "package:acme_mobile/models/user.dart",
            Path::new("mobile/lib/models/user.dart"),
            "mobile/lib/models/user.dart",
            "user",
        ));
        assert!(!import_spec_matches_target_file(
            SupportedLanguage::Dart,
            Path::new("lib/main.dart"),
            "package:demo_app/models/user.dart",
            Path::new("test/models/user.dart"),
            "test/models/user.dart",
            "user",
        ));
    }
}
