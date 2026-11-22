//! Tree-sitter AST-expanded git diff generation and language support.
use crate::domain::{Result, ServiceError};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[cfg(feature = "tree-sitter")]
mod ast_compact;
mod diff_expand;
pub mod hybrid;
#[cfg(feature = "tree-sitter")]
mod signature;
#[cfg(feature = "tree-sitter")]
mod test_scope;
#[cfg(all(test, feature = "tree-sitter"))]
mod tests;

pub use diff_expand::{
    generate_expanded_commit_diff, generate_expanded_diff, generate_expanded_diff_for_source,
    generate_expanded_diff_with_config, git_commit_expanded_file, git_diff_expanded_file,
    git_expanded_patch_file, is_lockfile_or_generated,
};
pub use hybrid::HybridConfig;

/// 1-based inclusive source line span `(start_line, end_line)` for a rendered diff line.
type SourceLineSpan = (usize, usize);
/// Per-line mapping from rendered diff lines to optional 1-based source line spans.
#[cfg(feature = "tree-sitter")]
type SourceLineMap = Vec<Option<SourceLineSpan>>;

/// Maximum number of lines an AST definition is allowed to expand to for general code.
/// Prevents large classes, structs, or JSON objects from dumping massive amounts of context.
#[cfg(feature = "tree-sitter")]
const MAX_EXPANSION_LINES: usize = 220;

/// Maximum number of lines allowed for test functions/blocks before clamping.
#[cfg(feature = "tree-sitter")]
const MAX_TEST_EXPANSION_LINES: usize = 40;

/// Maximum number of lines allowed before attribute/comment-only edits on a definition get clamped.
#[cfg(feature = "tree-sitter")]
const ATTR_ONLY_MAX_EXPANSION_LINES: usize = 15;

/// Minimum span for a function to be considered large for low-churn clamping.
#[cfg(feature = "tree-sitter")]
const LOW_CHURN_MIN_SPAN: usize = 150;

/// Maximum changed lines in a large function to trigger low-churn clamping.
#[cfg(feature = "tree-sitter")]
const LOW_CHURN_MAX_CHANGE_LINES: usize = 3;

/// Number of context lines to provide around a change when an AST definition exceeds MAX_EXPANSION_LINES.
#[cfg(feature = "tree-sitter")]
const CLAMPED_CONTEXT_LINES: usize = 10;

/// Soft byte budget for combined expanded diff output before progressive chunk compaction triggers.
/// Tuned for 1M-context reasoning models (~140K-150K tokens) so large commits keep full production logic.
const DEFAULT_PATCH_BUDGET_BYTES: usize = 512 * 1024;
const MIN_PATCH_BUDGET_BYTES: usize = 16 * 1024;
const MAX_PATCH_BUDGET_BYTES: usize = 16 * 1024 * 1024;

/// Parses an optional KiB override value (e.g. from `ANTICRUMB_PATCH_BUDGET_KIB`) into bytes,
/// clamped to `[16 KiB, 16 MiB]`.
fn parse_patch_budget_kib(raw: Option<&str>) -> usize {
    raw.and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&kib| kib > 0)
        .map(|kib| {
            kib.saturating_mul(1024)
                .clamp(MIN_PATCH_BUDGET_BYTES, MAX_PATCH_BUDGET_BYTES)
        })
        .unwrap_or(DEFAULT_PATCH_BUDGET_BYTES)
}

/// Resolves the active soft patch budget from `ANTICRUMB_PATCH_BUDGET_KIB` or `DEFAULT_PATCH_BUDGET_BYTES`.
fn resolve_patch_budget_bytes() -> usize {
    let val = std::env::var("ANTICRUMB_PATCH_BUDGET_KIB").ok();
    parse_patch_budget_kib(val.as_deref())
}

/// Maximum file size (in bytes) allowed for Tree-sitter in-memory parsing (1 MiB).
/// Files exceeding this limit fall back to `git diff -W` to prevent memory and CPU spikes.
#[cfg(feature = "tree-sitter")]
const MAX_TREESITTER_FILE_BYTES: usize = 1024 * 1024;

/// Timeout in microseconds for tree-sitter parsing (500ms).
#[cfg(feature = "tree-sitter")]
const TREESITTER_PARSE_TIMEOUT_MICROS: u64 = 500_000;

/// Languages with built-in Tree-Sitter grammars supported for diff context expansion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SupportedLanguage {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    C,
    Cpp,
    Java,
    CSharp,
    Bash,
    Ruby,
    Json,
}

impl SupportedLanguage {
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "js" | "mjs" | "cjs" | "jsx" => Some(Self::JavaScript),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "go" => Some(Self::Go),
            "c" => Some(Self::C),
            "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Some(Self::Cpp),
            "java" => Some(Self::Java),
            "cs" => Some(Self::CSharp),
            "sh" | "bash" => Some(Self::Bash),
            "rb" => Some(Self::Ruby),
            "json" => Some(Self::Json),
            _ => None,
        }
    }

    #[cfg(feature = "tree-sitter")]
    pub fn language_fn(&self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::C => tree_sitter_c::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            Self::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Self::Bash => tree_sitter_bash::LANGUAGE.into(),
            Self::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Self::Json => tree_sitter_json::LANGUAGE.into(),
        }
    }
}

/// Source of diff changes to inspect and expand with Tree-Sitter AST context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitDiffSource<'a> {
    Unstaged,
    Staged,
    Commit(&'a str),
}

/// Unquotes Git C-style quoted paths, e.g. `"sub_module/unicode_caf\\303\\251.rs"`.
fn unquote_c_style(s: &str) -> String {
    let trimmed = s.trim_matches(|c: char| c == '\r' || c == '\n');
    if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
        let inner = &trimmed[1..trimmed.len() - 1];
        let mut bytes = Vec::with_capacity(inner.len());
        let mut chars = inner.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('\\') => bytes.push(b'\\'),
                    Some('"') => bytes.push(b'"'),
                    Some('n') => bytes.push(b'\n'),
                    Some('t') => bytes.push(b'\t'),
                    Some('r') => bytes.push(b'\r'),
                    Some('b') => bytes.push(0x08),
                    Some('f') => bytes.push(0x0c),
                    Some('a') => bytes.push(0x07),
                    Some('v') => bytes.push(0x0b),
                    Some(o1 @ '0'..='7') => {
                        let mut octal = o1.to_digit(8).unwrap() as u8;
                        if let Some(&o2 @ '0'..='7') = chars.peek() {
                            chars.next();
                            octal = (octal << 3) | (o2.to_digit(8).unwrap() as u8);
                            if let Some(&o3 @ '0'..='7') = chars.peek() {
                                chars.next();
                                octal = (octal << 3) | (o3.to_digit(8).unwrap() as u8);
                            }
                        }
                        bytes.push(octal);
                    }
                    Some(other) => {
                        bytes.push(b'\\');
                        let mut buf = [0u8; 4];
                        bytes.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
                    }
                    None => bytes.push(b'\\'),
                }
            } else {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        trimmed.to_string()
    }
}

fn git_cmd(repo_root: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo_root);
    cmd.args([
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

fn get_repo_root(workspace: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .current_dir(workspace)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| ServiceError::invalid(format!("git command failed: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.to_ascii_lowercase().contains("not a git repository") {
            return Err(ServiceError::invalid("not a git repository"));
        }
        return Err(ServiceError::invalid(format!(
            "git rev-parse failed: {stderr}"
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let root = stdout.trim();
    if root.is_empty() {
        return Err(ServiceError::invalid("not a git repository"));
    }
    Ok(PathBuf::from(root))
}

/// Identifies top-level module, file, or class container node kinds whose full scope
/// should not be expanded unconditionally when a change touches only some child items.
#[cfg(feature = "tree-sitter")]
fn is_module_level_container(lang: SupportedLanguage, kind: &str) -> bool {
    match lang {
        SupportedLanguage::Json => matches!(kind, "document" | "object" | "array"),
        SupportedLanguage::Rust => {
            matches!(kind, "source_file" | "mod_item" | "declaration_list")
        }
        SupportedLanguage::C | SupportedLanguage::Cpp => {
            matches!(kind, "translation_unit" | "declaration_list")
        }
        SupportedLanguage::Java => {
            matches!(
                kind,
                "program" | "class_body" | "interface_body" | "enum_body"
            )
        }
        SupportedLanguage::CSharp => {
            matches!(
                kind,
                "compilation_unit" | "namespace_declaration" | "declaration_list"
            )
        }
        SupportedLanguage::JavaScript | SupportedLanguage::TypeScript | SupportedLanguage::Tsx => {
            matches!(
                kind,
                "program" | "class_body" | "internal_module" | "interface_body"
            )
        }
        SupportedLanguage::Python => matches!(kind, "module"),
        SupportedLanguage::Ruby => matches!(kind, "program"),
        SupportedLanguage::Go => matches!(kind, "source_file"),
        SupportedLanguage::Bash => matches!(kind, "program"),
    }
}

#[cfg(feature = "tree-sitter")]
fn is_module_level_container_node(lang: SupportedLanguage, node: &tree_sitter::Node) -> bool {
    let kind = node.kind();
    if is_module_level_container(lang, kind) {
        return true;
    }
    if matches!(lang, SupportedLanguage::TypeScript | SupportedLanguage::Tsx)
        && kind == "statement_block"
        && node.parent().is_some_and(|p| p.kind() == "internal_module")
    {
        return true;
    }
    false
}

/// Safely extracts the text of a node if it is entirely contained on a single line.
/// Protects against panics from multi-line nodes, error recovery, out-of-bounds, or invalid UTF-8 slice boundaries.
#[cfg(feature = "tree-sitter")]
fn get_node_text_same_line<'a>(node: &tree_sitter::Node, lines: &[&'a str]) -> Option<&'a str> {
    let sp = node.start_position();
    let ep = node.end_position();
    if sp.row != ep.row {
        return None;
    }
    let line = lines.get(sp.row)?;
    line.get(sp.column..ep.column)
}

/// Computes the bounding line range of named children in `container` that intersect
/// `[start_row, end_row]`. Returns `(start_line, end_line, primary_child, def_count)`.
#[cfg(feature = "tree-sitter")]
#[allow(dead_code)]
fn bounding_range_of_intersecting_children<'a>(
    lang: SupportedLanguage,
    container: &tree_sitter::Node<'a>,
    start_row: usize,
    end_row: usize,
) -> Option<(usize, usize, Option<tree_sitter::Node<'a>>, usize)> {
    let bounds =
        bounding_range_of_intersecting_children_with_defs(lang, container, start_row, end_row)?;
    Some((
        bounds.start_line,
        bounds.end_line,
        bounds.primary_child,
        bounds.def_count,
    ))
}

#[cfg(feature = "tree-sitter")]
struct IntersectingChildrenBounds<'a> {
    start_line: usize,
    end_line: usize,
    primary_child: Option<tree_sitter::Node<'a>>,
    def_count: usize,
    primary_defs: Vec<tree_sitter::Node<'a>>,
}

#[cfg(feature = "tree-sitter")]
fn bounding_range_of_intersecting_children_with_defs<'a>(
    lang: SupportedLanguage,
    container: &tree_sitter::Node<'a>,
    start_row: usize,
    end_row: usize,
) -> Option<IntersectingChildrenBounds<'a>> {
    let mut min_row = None;
    let mut max_row = None;
    let mut primary_child = None;
    let mut def_count = 0;
    let mut primary_defs = Vec::new();

    let mut cursor = container.walk();
    for child in container.named_children(&mut cursor) {
        let c_start = child.start_position().row;
        let c_end = child.end_position().row;
        if c_end >= start_row && c_start <= end_row {
            min_row = Some(min_row.map_or(c_start, |m: usize| m.min(c_start)));
            max_row = Some(max_row.map_or(c_end, |m: usize| m.max(c_end)));
            let matched_primary = check_primary_definition(lang, &child);
            let is_primary = matched_primary.is_some();
            if let Some(mp) = matched_primary {
                def_count += 1;
                primary_defs.push(mp);
            }
            let should_replace = match &primary_child {
                None => true,
                Some(curr) => {
                    let curr_is_primary = check_primary_definition(lang, curr).is_some();
                    !curr_is_primary
                        && (is_primary
                            || curr.kind().contains("attribute")
                            || curr.kind().contains("comment"))
                }
            };
            if should_replace {
                primary_child = Some(child);
            }
        }
    }

    if let (Some(s), Some(e)) = (min_row, max_row) {
        Some(IntersectingChildrenBounds {
            start_line: s + 1,
            end_line: e + 1,
            primary_child,
            def_count,
            primary_defs,
        })
    } else {
        None
    }
}

/// Checks JavaScript / TypeScript primary definitions.
/// Arrow functions and function expressions are only treated as definitions if assigned to a variable,
/// property, or class field (e.g. `const handler = () => { ... }`). Callbacks like `items.map(x => x * 2)` return None.
#[cfg(feature = "tree-sitter")]
fn is_js_primary_definition<'a>(node: &tree_sitter::Node<'a>) -> Option<tree_sitter::Node<'a>> {
    match node.kind() {
        "function_declaration"
        | "method_definition"
        | "class_declaration"
        | "abstract_class_declaration" => {
            let mut res = *node;
            if let Some(p) = node.parent() {
                if p.kind() == "export_statement" {
                    res = p;
                }
            }
            Some(res)
        }
        "arrow_function" | "function_expression" => {
            if let Some(parent) = node.parent() {
                if parent.kind() == "variable_declarator" {
                    let mut top = parent.parent();
                    if let Some(decl) = top {
                        if let Some(exp) = decl.parent().filter(|p| p.kind() == "export_statement")
                        {
                            top = Some(exp);
                        }
                    }
                    return top.or(Some(parent));
                } else if matches!(
                    parent.kind(),
                    "assignment_expression"
                        | "pair"
                        | "field_definition"
                        | "public_field_definition"
                        | "property_definition"
                ) {
                    return Some(parent);
                }
            }
            None
        }
        "variable_declarator" => {
            if node
                .child_by_field_name("value")
                .is_some_and(|v| matches!(v.kind(), "arrow_function" | "function_expression"))
            {
                let mut top = node.parent();
                if let Some(decl) = top {
                    if let Some(exp) = decl.parent().filter(|p| p.kind() == "export_statement") {
                        top = Some(exp);
                    }
                }
                top.or(Some(*node))
            } else {
                None
            }
        }
        "lexical_declaration" | "variable_declaration" => {
            let mut has_func = false;
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "variable_declarator"
                    && child.child_by_field_name("value").is_some_and(|v| {
                        matches!(v.kind(), "arrow_function" | "function_expression")
                    })
                {
                    has_func = true;
                    break;
                }
            }
            if has_func {
                if let Some(exp) = node.parent().filter(|p| p.kind() == "export_statement") {
                    Some(exp)
                } else {
                    Some(*node)
                }
            } else {
                None
            }
        }
        "export_statement" => {
            if let Some(decl) = node.child_by_field_name("declaration") {
                if is_js_primary_definition(&decl).is_some() {
                    return Some(*node);
                }
            }
            None
        }
        "pair" => {
            if node
                .child_by_field_name("value")
                .is_some_and(|v| matches!(v.kind(), "arrow_function" | "function_expression"))
            {
                Some(*node)
            } else {
                None
            }
        }
        "assignment_expression" => {
            if node
                .child_by_field_name("right")
                .is_some_and(|v| matches!(v.kind(), "arrow_function" | "function_expression"))
            {
                let mut top = *node;
                if let Some(parent) = node.parent() {
                    if parent.kind() == "expression_statement" {
                        top = parent;
                    }
                }
                Some(top)
            } else {
                None
            }
        }
        "field_definition" | "public_field_definition" | "property_definition" => {
            if node
                .child_by_field_name("value")
                .is_some_and(|v| matches!(v.kind(), "arrow_function" | "function_expression"))
            {
                Some(*node)
            } else {
                None
            }
        }
        "interface_declaration"
        | "type_alias_declaration"
        | "enum_declaration"
        | "method_signature"
        | "property_signature"
        | "call_signature"
        | "construct_signature"
        | "index_signature" => {
            let mut res = *node;
            if let Some(p) = node.parent() {
                if p.kind() == "export_statement" {
                    res = p;
                }
            }
            Some(res)
        }
        _ => None,
    }
}

/// Checks C / C++ primary definitions.
/// Struct / union / class / enum specifiers are only definitions if they have a body (`field_declaration_list`).
/// Forward declarations and type usages like `struct Foo *p = NULL;` return None.
#[cfg(feature = "tree-sitter")]
fn is_c_cpp_primary_definition<'a>(node: &tree_sitter::Node<'a>) -> Option<tree_sitter::Node<'a>> {
    match node.kind() {
        "function_definition" => {
            let mut res = *node;
            if let Some(p) = node.parent() {
                if p.kind() == "template_declaration" {
                    res = p;
                }
            }
            Some(res)
        }
        "struct_specifier" | "union_specifier" | "class_specifier" => {
            let has_body = node.child_by_field_name("body").is_some()
                || (0..node.child_count()).any(|i| {
                    node.child(i)
                        .is_some_and(|c| c.kind() == "field_declaration_list")
                });
            if has_body {
                let mut res = *node;
                if let Some(p) = node.parent() {
                    if p.kind() == "template_declaration" {
                        res = p;
                    }
                }
                Some(res)
            } else {
                None
            }
        }
        "enum_specifier" => {
            let has_body = node.child_by_field_name("body").is_some()
                || (0..node.child_count())
                    .any(|i| node.child(i).is_some_and(|c| c.kind() == "enumerator_list"));
            if has_body {
                Some(*node)
            } else {
                None
            }
        }
        "type_definition" => Some(*node),
        _ => None,
    }
}

#[cfg(feature = "tree-sitter")]
fn check_primary_definition<'a>(
    lang: SupportedLanguage,
    node: &tree_sitter::Node<'a>,
) -> Option<tree_sitter::Node<'a>> {
    let kind = node.kind();
    match lang {
        SupportedLanguage::JavaScript | SupportedLanguage::TypeScript | SupportedLanguage::Tsx => {
            is_js_primary_definition(node)
        }
        SupportedLanguage::C | SupportedLanguage::Cpp => is_c_cpp_primary_definition(node),
        SupportedLanguage::Rust => {
            if matches!(
                kind,
                "function_item"
                    | "struct_item"
                    | "enum_item"
                    | "trait_item"
                    | "type_item"
                    | "macro_definition"
                    | "const_item"
                    | "static_item"
            ) {
                Some(*node)
            } else {
                None
            }
        }
        SupportedLanguage::Python => {
            if matches!(
                kind,
                "function_definition" | "class_definition" | "decorated_definition"
            ) {
                let mut chosen = *node;
                if let Some(parent) = chosen.parent() {
                    if parent.kind() == "decorated_definition" {
                        chosen = parent;
                    }
                }
                Some(chosen)
            } else {
                None
            }
        }
        SupportedLanguage::Java => {
            if matches!(
                kind,
                "method_declaration"
                    | "constructor_declaration"
                    | "class_declaration"
                    | "interface_declaration"
                    | "enum_declaration"
                    | "record_declaration"
                    | "annotation_type_declaration"
            ) {
                Some(*node)
            } else {
                None
            }
        }
        SupportedLanguage::Go => {
            if matches!(
                kind,
                "function_declaration" | "method_declaration" | "type_declaration" | "type_spec"
            ) {
                Some(*node)
            } else {
                None
            }
        }
        SupportedLanguage::CSharp => {
            if matches!(
                kind,
                "method_declaration"
                    | "constructor_declaration"
                    | "destructor_declaration"
                    | "class_declaration"
                    | "struct_declaration"
                    | "interface_declaration"
                    | "enum_declaration"
                    | "property_declaration"
                    | "record_declaration"
                    | "indexer_declaration"
            ) {
                Some(*node)
            } else {
                None
            }
        }
        SupportedLanguage::Bash => {
            if kind == "function_definition" {
                Some(*node)
            } else {
                None
            }
        }
        SupportedLanguage::Ruby => {
            if matches!(kind, "method" | "singleton_method" | "class" | "module") {
                Some(*node)
            } else {
                None
            }
        }
        SupportedLanguage::Json => {
            if kind == "pair" {
                Some(*node)
            } else {
                None
            }
        }
    }
}

/// Finds the enclosing definition AST node spanning `[start_line, end_line]` (1-indexed).
/// Returns `(expanded_start_line, expanded_end_line, signature_preview)`.
#[cfg(feature = "tree-sitter")]
pub fn find_enclosing_definition(
    lang: SupportedLanguage,
    root: &tree_sitter::Node,
    start_line: usize,
    end_line: usize,
    source: &str,
) -> (usize, usize, Option<String>) {
    let (s, e, sig, _clamped) =
        find_enclosing_definition_full(lang, root, start_line, end_line, source, None, 0);
    (s, e, sig)
}

/// Finds the enclosing definition AST node spanning `[start_line, end_line]` (1-indexed)
/// with test/path awareness and clamp status tracking.
/// Returns `(expanded_start_line, expanded_end_line, signature_preview, is_clamped)`.
#[cfg(feature = "tree-sitter")]
pub fn find_enclosing_definition_full(
    lang: SupportedLanguage,
    root: &tree_sitter::Node,
    start_line: usize,
    end_line: usize,
    source: &str,
    file_path: Option<&Path>,
    deleted_lines: usize,
) -> (usize, usize, Option<String>, bool) {
    let (exp_start, exp_end, sig, is_clamped, _full_start, _full_end) =
        find_enclosing_definition_full_range(
            lang,
            root,
            start_line,
            end_line,
            source,
            file_path,
            deleted_lines,
        );
    (exp_start, exp_end, sig, is_clamped)
}

fn has_camel_it_suffix(orig_p_str: &str, suffix: &str) -> bool {
    let Some(pre) = orig_p_str.strip_suffix(suffix) else {
        return false;
    };
    pre.chars()
        .next_back()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Checks whether a file path matches standard test directory or test file naming conventions.
pub(super) fn is_test_file_path(path: &Path) -> bool {
    let orig_p_str = path.to_string_lossy().replace('\\', "/");
    let lower_p_str = orig_p_str.to_ascii_lowercase();

    let in_test_dir = lower_p_str.starts_with("tests/")
        || lower_p_str.contains("/tests/")
        || lower_p_str.starts_with("test/")
        || lower_p_str.contains("/test/")
        || lower_p_str.starts_with("__tests__/")
        || lower_p_str.contains("/__tests__/")
        || lower_p_str.starts_with("spec/")
        || lower_p_str.contains("/spec/");

    if in_test_dir {
        return true;
    }

    if lower_p_str.ends_with("_test.rs")
        || lower_p_str == "test.rs"
        || lower_p_str.ends_with("/test.rs")
        || lower_p_str == "tests.rs"
        || lower_p_str.ends_with("/tests.rs")
        || lower_p_str.ends_with("_test.go")
        || lower_p_str.ends_with("_test.py")
        || (lower_p_str.ends_with(".py")
            && (lower_p_str.starts_with("test_") || lower_p_str.contains("/test_")))
        || lower_p_str.ends_with("_spec.rb")
        || lower_p_str.ends_with("_test.rb")
        || lower_p_str.ends_with(".test.ts")
        || lower_p_str.ends_with(".spec.ts")
        || lower_p_str.ends_with(".test.mts")
        || lower_p_str.ends_with(".spec.mts")
        || lower_p_str.ends_with(".test.tsx")
        || lower_p_str.ends_with(".spec.tsx")
        || lower_p_str.ends_with(".test.js")
        || lower_p_str.ends_with(".spec.js")
        || lower_p_str.ends_with(".test.mjs")
        || lower_p_str.ends_with(".spec.mjs")
        || lower_p_str.ends_with(".test.jsx")
        || lower_p_str.ends_with(".spec.jsx")
        || lower_p_str.ends_with("_test.c")
        || lower_p_str.ends_with("_unittest.c")
        || lower_p_str.ends_with("_test.cpp")
        || lower_p_str.ends_with("_unittest.cpp")
        || lower_p_str.ends_with("_test.cc")
        || lower_p_str.ends_with("_unittest.cc")
        || lower_p_str.ends_with("_test.cxx")
        || lower_p_str.ends_with("_unittest.cxx")
        || ((lower_p_str.ends_with(".c")
            || lower_p_str.ends_with(".cc")
            || lower_p_str.ends_with(".cpp")
            || lower_p_str.ends_with(".cxx"))
            && (lower_p_str.starts_with("test_") || lower_p_str.contains("/test_")))
        || lower_p_str.ends_with("_test.sh")
        || lower_p_str.ends_with(".bats")
    {
        return true;
    }

    if orig_p_str.ends_with("Test.java")
        || orig_p_str.ends_with("Tests.java")
        || orig_p_str.ends_with("TestCase.java")
        || orig_p_str.ends_with("_test.java")
        || orig_p_str.ends_with("ITCase.java")
        || orig_p_str.ends_with("Spec.java")
        || has_camel_it_suffix(&orig_p_str, "IT.java")
        || orig_p_str.ends_with("Test.kt")
        || orig_p_str.ends_with("Tests.kt")
        || orig_p_str.ends_with("Spec.kt")
        || orig_p_str.ends_with("_test.kt")
        || orig_p_str.ends_with("ITCase.kt")
        || has_camel_it_suffix(&orig_p_str, "IT.kt")
        || orig_p_str.ends_with("Test.cs")
        || orig_p_str.ends_with("Tests.cs")
        || orig_p_str.ends_with("_test.cs")
    {
        return true;
    }

    false
}

#[cfg(feature = "tree-sitter")]
struct EnclosingMatch {
    exp_start: usize,
    exp_end: usize,
    signature: Option<String>,
    bare_name: Option<String>,
    parent_scope: Option<String>,
    is_clamped: bool,
    full_start: usize,
    full_end: usize,
    extra_defs: usize,
    extra_names: Vec<String>,
    /// For single-function matches: `(body_start_line_1idx, body_start_col_bytes, body_end_line_1idx)`.
    func_body_span: Option<(usize, usize, usize)>,
}

/// Finds the enclosing definition AST node spanning `[start_line, end_line]` (1-indexed),
/// returning both the clamped expanded range and the original un-clamped AST bounds:
/// `(expanded_start, expanded_end, signature_preview, is_clamped, full_start, full_end)`.
#[cfg(feature = "tree-sitter")]
fn find_enclosing_definition_full_range(
    lang: SupportedLanguage,
    root: &tree_sitter::Node,
    start_line: usize,
    end_line: usize,
    source: &str,
    file_path: Option<&Path>,
    deleted_lines: usize,
) -> (usize, usize, Option<String>, bool, usize, usize) {
    let m = find_enclosing_definition_detail(
        lang,
        root,
        start_line,
        end_line,
        source,
        file_path,
        deleted_lines,
        HybridConfig::off(),
    );
    let sig = match (m.signature, m.extra_defs) {
        (Some(s), extra) if extra > 0 => Some(format!("{s} (+{extra} more)")),
        (s, _) => s,
    };
    (
        m.exp_start,
        m.exp_end,
        sig,
        m.is_clamped,
        m.full_start,
        m.full_end,
    )
}

#[cfg(feature = "tree-sitter")]
#[allow(clippy::too_many_arguments)]
fn find_enclosing_definition_detail(
    lang: SupportedLanguage,
    root: &tree_sitter::Node,
    start_line: usize,
    end_line: usize,
    source: &str,
    file_path: Option<&Path>,
    deleted_lines: usize,
    hybrid: HybridConfig,
) -> EnclosingMatch {
    let start_row = start_line.saturating_sub(1);
    let end_row = end_line.saturating_sub(1);
    let lines: Vec<&str> = source.lines().collect();
    let max_line = lines.len().max(1);
    let start_col = lines
        .get(start_row)
        .map(|l| l.len() - l.trim_start().len())
        .unwrap_or(0);
    let end_col = if start_row == end_row {
        start_col
    } else {
        lines
            .get(end_row)
            .map(|l| {
                l.trim_end_matches(|c: char| c.is_whitespace() || c == ',' || c == ';')
                    .len()
            })
            .unwrap_or(0)
    };
    let start_point = tree_sitter::Point {
        row: start_row,
        column: start_col,
    };
    let end_point = tree_sitter::Point {
        row: end_row,
        column: end_col,
    };

    let mut curr = root
        .named_descendant_for_point_range(start_point, end_point)
        .or_else(|| root.descendant_for_point_range(start_point, end_point));

    while let Some(node) = curr {
        let is_root = node.id() == root.id();

        if let Some(matched) = check_primary_definition(lang, &node) {
            let matched_start = (matched.start_position().row + 1).min(max_line);
            let matched_end = (matched.end_position().row + 1).min(max_line);
            let span = matched_end.saturating_sub(matched_start) + 1;

            let is_test = test_scope::is_test_scope(lang, &matched, file_path, &lines);
            let change_span = (end_line.saturating_sub(start_line) + 1).max(deleted_lines);
            let max_allowed = if is_test {
                MAX_TEST_EXPANSION_LINES
            } else {
                MAX_EXPANSION_LINES
            };
            let is_low_churn =
                !is_test && span >= LOW_CHURN_MIN_SPAN && change_span <= LOW_CHURN_MAX_CHANGE_LINES;

            let full_start = matched_start.min(start_line);
            let full_end = matched_end.max(end_line).min(max_line);

            let (exp_start, exp_end, is_clamped) = if span <= max_allowed && !is_low_churn {
                (full_start, full_end, false)
            } else {
                let cs = start_line
                    .saturating_sub(CLAMPED_CONTEXT_LINES)
                    .max(matched_start);
                let ce = (end_line + CLAMPED_CONTEXT_LINES).min(matched_end);
                (cs, ce, cs > matched_start || ce < matched_end)
            };

            let decl_cut_off = exp_start > matched_start;
            let sig_max_chars = if hybrid.layer2 && decl_cut_off {
                200
            } else {
                120
            };
            let raw_sig = signature::extract_signature(lang, &matched, &lines, source);
            let parent_scope =
                signature::extract_parent_scope_prefix(lang, &matched, &lines, source);
            let sig = signature::compact_definition_signature_with_ast_opts(
                lang,
                &matched,
                parent_scope.as_deref(),
                raw_sig.as_deref(),
                source,
                signature::SigFormatOpts {
                    max_chars: sig_max_chars,
                    scope_sep: hybrid.scope_sep(),
                    preserve_key_decorators: hybrid.layer2,
                    greedy_params: hybrid.layer2,
                },
            );
            let bare_name = ast_compact::bare_def_name(lang, &matched, &lines, source);
            let func_body_span = if ast_compact::is_foldable_function_node(lang, &matched)
                || matched.kind() == "decorated_definition"
                || matched.kind() == "export_statement"
            {
                let inner = match matched.kind() {
                    "decorated_definition" => {
                        matched.child_by_field_name("definition").unwrap_or(matched)
                    }
                    "export_statement" => matched
                        .child_by_field_name("declaration")
                        .unwrap_or(matched),
                    _ => matched,
                };
                if ast_compact::is_foldable_function_node(lang, &inner) {
                    signature::find_ast_body_node(lang, &inner).map(|b| {
                        (
                            b.start_position().row + 1,
                            b.start_position().column,
                            (b.end_position().row + 1).min(max_line),
                        )
                    })
                } else {
                    None
                }
            } else {
                None
            };

            return EnclosingMatch {
                exp_start,
                exp_end,
                signature: sig,
                bare_name,
                parent_scope,
                is_clamped,
                full_start,
                full_end,
                extra_defs: 0,
                extra_names: Vec::new(),
                func_body_span,
            };
        }

        if is_root || is_module_level_container_node(lang, &node) {
            if let Some(IntersectingChildrenBounds {
                start_line: raw_c_start,
                end_line: raw_c_end,
                primary_child: sig_node,
                def_count,
                primary_defs,
            }) =
                bounding_range_of_intersecting_children_with_defs(lang, &node, start_row, end_row)
            {
                if def_count > 0 || is_root {
                    let c_start = raw_c_start.min(max_line);
                    let c_end = raw_c_end.min(max_line);
                    let extra_defs = def_count.saturating_sub(1);
                    let span = c_end.saturating_sub(c_start) + 1;
                    let is_test = test_scope::is_test_scope(lang, &node, file_path, &lines)
                        || sig_node.as_ref().is_some_and(|sn| {
                            test_scope::is_test_scope(lang, sn, file_path, &lines)
                        });
                    let max_allowed = if is_test {
                        MAX_TEST_EXPANSION_LINES
                    } else {
                        MAX_EXPANSION_LINES
                    };

                    let full_start = c_start.min(start_line);
                    let full_end = c_end.max(end_line).min(max_line);

                    let (exp_start, exp_end, is_clamped) = if span <= max_allowed {
                        (full_start, full_end, false)
                    } else {
                        let cs = start_line
                            .saturating_sub(CLAMPED_CONTEXT_LINES)
                            .max(c_start);
                        let ce = (end_line + CLAMPED_CONTEXT_LINES).min(c_end);
                        (cs, ce, cs > c_start || ce < c_end)
                    };

                    let parent_scope = if def_count > 0 {
                        sig_node.as_ref().and_then(|n| {
                            signature::extract_parent_scope_prefix(lang, n, &lines, source)
                        })
                    } else {
                        None
                    };
                    let raw_sig = if def_count > 0 {
                        sig_node
                            .as_ref()
                            .and_then(|n| signature::extract_signature(lang, n, &lines, source))
                            .or_else(|| {
                                signature::extract_signature_from_row_for_lang(
                                    c_start - 1,
                                    &lines,
                                    lang,
                                )
                            })
                    } else {
                        None
                    };
                    let sig_max_chars = if hybrid.layer2 && exp_start > c_start {
                        200
                    } else {
                        120
                    };
                    let sig = if let Some(sn) = &sig_node {
                        signature::compact_definition_signature_with_ast_opts(
                            lang,
                            sn,
                            parent_scope.as_deref(),
                            raw_sig.as_deref(),
                            source,
                            signature::SigFormatOpts {
                                max_chars: sig_max_chars,
                                scope_sep: hybrid.scope_sep(),
                                preserve_key_decorators: hybrid.layer2,
                                greedy_params: hybrid.layer2,
                            },
                        )
                    } else {
                        None
                    };
                    let bare_name = sig_node
                        .as_ref()
                        .and_then(|sn| ast_compact::bare_def_name(lang, sn, &lines, source));
                    let mut extra_names = Vec::new();
                    if hybrid.layer2 && primary_defs.len() > 1 {
                        for def in &primary_defs {
                            if let Some(nm) = ast_compact::bare_def_name(lang, def, &lines, source)
                            {
                                if bare_name.as_deref() != Some(nm.as_str())
                                    && !extra_names.contains(&nm)
                                {
                                    extra_names.push(nm);
                                }
                            }
                        }
                    }

                    return EnclosingMatch {
                        exp_start,
                        exp_end,
                        signature: sig,
                        bare_name,
                        parent_scope,
                        is_clamped,
                        full_start,
                        full_end,
                        extra_defs,
                        extra_names,
                        func_body_span: None,
                    };
                }
            }
            if is_root {
                break;
            }
        }

        if is_root {
            break;
        }
        curr = node.parent();
    }

    EnclosingMatch {
        exp_start: start_line,
        exp_end: end_line,
        signature: None,
        bare_name: None,
        parent_scope: None,
        is_clamped: false,
        full_start: start_line,
        full_end: end_line,
        extra_defs: 0,
        extra_names: Vec::new(),
        func_body_span: None,
    }
}
