//! Definition (symbol) and reference extraction for `code_outline`, `find_symbol`, `find_references`.
//!
//! Reuses the diff engine's per-language rules (`check_primary_definition`, `signature`,
//! `extract_short_doc_comment`, `test_scope`, `ast_compact`) so the AST tools and `@git:...+`
//! expansion share one definition of symbols, signatures, docs, and test scopes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tree_sitter::Node;

use super::ast_compact::{
    extract_dart_def_name, find_inline_test_blocks, is_foldable_function_node,
    rust_start_row_with_attrs, InlineTestBlock,
};
use super::diff_expand::parse_with_timeout;
use super::hybrid::{contains_utf8_ident, extract_short_doc_comment, is_valid_ident};
use super::signature::{self, SigFormatOpts};
use super::test_scope;
use super::{
    check_primary_definition, get_node_text_same_line, is_test_file_path, SupportedLanguage,
    MAX_TREESITTER_FILE_BYTES,
};

/// Maximum nesting depth walked (guards pathological inputs).
const MAX_SYMBOL_DEPTH: usize = 8;
/// JSON outlines only list the top two key levels.
const MAX_JSON_DEPTH: usize = 2;

/// One definition in a source file. Lines are 1-based and inclusive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    /// Name path segments from the outermost container, e.g. `["GrepSearchTool", "execute"]`.
    pub path: Vec<String>,
    pub start_line: usize,
    pub end_line: usize,
    /// Line holding the symbol's name (fallback when no compact signature is extracted).
    pub name_line: usize,
    pub depth: usize,
    /// Body-stripped single-line signature (without parent prefix or trailing `{`).
    pub signature: Option<String>,
    /// One-line doc comment summary (<= 160 bytes).
    pub doc: Option<String>,
    /// True for scope containers (`impl`, `mod`, `namespace`) rather than primary declarations.
    pub is_container: bool,
}

impl Symbol {
    pub fn name(&self) -> &str {
        self.path.last().map(String::as_str).unwrap_or("?")
    }

    /// `Parent/child` form used for display and matching.
    pub fn qualified_name(&self) -> String {
        self.path.join("/")
    }
}

/// Splits a symbol query like `"Grep::execute"`, `"Grep.execute"`, or `"Grep/execute"` into segments.
pub fn split_name_path(name: &str) -> Vec<&str> {
    name.split(['/', '.'])
        .flat_map(|s| s.split("::"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// `query` matches when it is a suffix of the symbol's name path (`execute`, `Tool/execute`).
pub fn symbol_matches(path: &[String], query: &[&str], exact: bool) -> bool {
    if query.is_empty() || query.len() > path.len() {
        return false;
    }
    let tail = &path[path.len() - query.len()..];
    let last = query.len() - 1;
    tail.iter().zip(query).enumerate().all(|(i, (seg, q))| {
        if exact {
            seg == q
        } else if i == last {
            seg.to_lowercase().contains(&q.to_lowercase())
        } else {
            seg.eq_ignore_ascii_case(q)
        }
    })
}

/// Parses `source` (language chosen from `path`) and returns its definitions.
pub fn extract_symbols(
    path: &Path,
    source: &str,
) -> Result<(SupportedLanguage, Vec<Symbol>), String> {
    let lang = SupportedLanguage::from_path(path)
        .ok_or_else(|| format!("unsupported language for '{}'", path.display()))?;
    if source.len() > MAX_TREESITTER_FILE_BYTES {
        return Err(format!(
            "file is larger than {} KiB; use grep_search/read_file",
            MAX_TREESITTER_FILE_BYTES / 1024
        ));
    }
    let tree = parse_with_timeout(lang, source).map_err(str::to_string)?;
    let lines: Vec<&str> = source.lines().collect();
    let mut walker = Walker {
        lang,
        src: source,
        lines,
        seen: HashSet::new(),
        out: Vec::new(),
    };
    walker.walk(tree.root_node(), &[], 0);
    Ok((lang, walker.out))
}

struct Walker<'s> {
    lang: SupportedLanguage,
    src: &'s str,
    lines: Vec<&'s str>,
    seen: HashSet<usize>,
    out: Vec<Symbol>,
}

impl Walker<'_> {
    fn walk(&mut self, node: Node, parents: &[String], depth: usize) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.visit(child, parents, depth);
        }
    }

    fn visit(&mut self, node: Node, parents: &[String], depth: usize) {
        // Go `type (A ...; B ...)`: each `type_spec` is the definition, not the group.
        if self.lang == SupportedLanguage::Go && node.kind() == "type_declaration" {
            self.walk(node, parents, depth);
            return;
        }
        let primary = check_primary_definition(self.lang, &node);
        let (def, is_container) = match primary {
            Some(d) => (d, false),
            None => match self.container(&node) {
                Some(c) => (c, true),
                None => {
                    self.walk(node, parents, depth);
                    return;
                }
            },
        };
        // Several nodes map to one definition (e.g. `export` > `const` > arrow fn);
        // record it once, at the outermost node, and keep walking inside it.
        if !self.seen.insert(def.id()) {
            self.walk(node, parents, depth);
            return;
        }
        let max_depth = if self.lang == SupportedLanguage::Json {
            MAX_JSON_DEPTH
        } else {
            MAX_SYMBOL_DEPTH
        };
        if depth >= max_depth {
            return;
        }
        let core = unwrap_wrappers(def);
        let name_node = name_node(self.lang, core);
        let name = name_node
            .and_then(|n| n.utf8_text(self.src.as_bytes()).ok())
            .map(|t| clean_name(self.lang, core, t))
            .filter(|n| !n.is_empty())
            .or_else(|| {
                (self.lang == SupportedLanguage::Dart)
                    .then(|| extract_dart_def_name(&core, &self.lines))
                    .flatten()
            })
            .unwrap_or_else(|| "<anonymous>".to_string());
        let mut path = parents.to_vec();
        if self.lang == SupportedLanguage::Go && core.kind() == "method_declaration" {
            if let Some(recv) = go_receiver_type(&core, self.src) {
                path.push(recv);
            }
        }
        path.extend(
            name.split("::")
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );

        let (signature, doc) = if self.lang == SupportedLanguage::Json {
            (None, None)
        } else {
            let raw_sig = signature::extract_signature(self.lang, &def, &self.lines, self.src);
            let sig = signature::compact_definition_signature_with_ast_opts(
                self.lang,
                &def,
                None,
                raw_sig.as_deref(),
                self.src,
                SigFormatOpts {
                    max_chars: 160,
                    scope_sep: "::",
                    preserve_key_decorators: false,
                    greedy_params: true,
                },
            )
            .map(|s| s.trim_end_matches('{').trim_end().to_string())
            .filter(|s| !s.is_empty());
            let doc = extract_short_doc_comment(self.lang, &def, &self.lines, self.src);
            (sig, doc)
        };

        self.out.push(Symbol {
            path: path.clone(),
            // Rust `#[...]` attributes are siblings; include them so ranges are complete.
            start_line: if self.lang == SupportedLanguage::Rust {
                rust_start_row_with_attrs(&def) + 1
            } else {
                def.start_position().row + 1
            },
            end_line: def.end_position().row + 1,
            name_line: name_node.unwrap_or(core).start_position().row + 1,
            depth,
            signature,
            doc,
            is_container,
        });
        // Local helpers inside function bodies are noise in an outline.
        if !is_foldable_function_node(self.lang, &core) {
            self.walk(def, &path, depth + 1);
        }
    }

    /// Scopes that are not "definitions" for diff expansion but name their members.
    fn container<'t>(&self, node: &Node<'t>) -> Option<Node<'t>> {
        let kind = node.kind();
        let is_container = match self.lang {
            SupportedLanguage::Rust => matches!(kind, "impl_item" | "mod_item"),
            SupportedLanguage::Cpp => kind == "namespace_definition",
            SupportedLanguage::CSharp => {
                matches!(
                    kind,
                    "namespace_declaration" | "file_scoped_namespace_declaration"
                )
            }
            SupportedLanguage::TypeScript | SupportedLanguage::Tsx => kind == "internal_module",
            _ => false,
        };
        is_container.then_some(*node)
    }
}

/// Strips export / decorator / template wrappers to reach the declaration itself.
fn unwrap_wrappers(mut node: Node) -> Node {
    loop {
        let next = match node.kind() {
            "export_statement" => node.child_by_field_name("declaration"),
            "decorated_definition" => node.child_by_field_name("definition"),
            "template_declaration" => {
                let count = node.named_child_count();
                count.checked_sub(1).and_then(|i| node.named_child(i))
            }
            "expression_statement" => node.named_child(0),
            _ => None,
        };
        match next {
            Some(n) => node = n,
            None => return node,
        }
    }
}

fn first_child_of_kind<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut cursor = node.walk();
    let found = node.named_children(&mut cursor).find(|c| c.kind() == kind);
    found
}

/// Finds the node that spells the definition's name.
fn name_node<'t>(lang: SupportedLanguage, node: Node<'t>) -> Option<Node<'t>> {
    match node.kind() {
        "impl_item" if lang == SupportedLanguage::Rust => {
            return node.child_by_field_name("type");
        }
        "lexical_declaration" | "variable_declaration" => {
            return first_child_of_kind(node, "variable_declarator")
                .and_then(|d| d.child_by_field_name("name"));
        }
        "assignment_expression" => return node.child_by_field_name("left"),
        "pair" => return node.child_by_field_name("key"),
        "type_declaration" if lang == SupportedLanguage::Go => {
            return first_child_of_kind(node, "type_spec")
                .and_then(|s| s.child_by_field_name("name"));
        }
        _ => {}
    }
    if let Some(name) = node.child_by_field_name("name") {
        return Some(name);
    }
    // C/C++: function_definition > (pointer_)declarator > function_declarator > identifier.
    let mut decl = node.child_by_field_name("declarator")?;
    while let Some(inner) = decl.child_by_field_name("declarator") {
        decl = inner;
    }
    Some(decl)
}

fn go_receiver_type(method: &Node, source: &str) -> Option<String> {
    let recv = method.child_by_field_name("receiver")?;
    let mut cursor = recv.walk();
    let param = recv
        .named_children(&mut cursor)
        .find(|c| c.kind() == "parameter_declaration")?;
    let ty = param.child_by_field_name("type")?;
    let raw = source.get(ty.start_byte()..ty.end_byte())?.trim();
    let stripped = raw.trim_start_matches('*').trim();
    let without_generics = stripped.split('[').next().unwrap_or(stripped).trim();
    let leaf = without_generics
        .rsplit('.')
        .next()
        .unwrap_or(without_generics)
        .trim();
    (!leaf.is_empty()).then(|| leaf.to_string())
}

fn clean_rust_type_name(raw: &str) -> &str {
    let mut s = raw.split('<').next().unwrap_or(raw).trim();
    loop {
        let next = if let Some(rest) = s.strip_prefix('&') {
            rest.trim_start()
        } else if let Some(rest) = s.strip_prefix("mut ") {
            rest.trim_start()
        } else if s.starts_with('\'') {
            match s.find(char::is_whitespace) {
                Some(idx) => s[idx..].trim_start(),
                None => break,
            }
        } else {
            break;
        };
        s = next;
    }
    s.rsplit("::").next().unwrap_or(s).trim()
}

fn clean_name(lang: SupportedLanguage, core: Node, raw: &str) -> String {
    let first = raw.lines().next().unwrap_or("").trim();
    let name = match core.kind() {
        // `&mut crate::a::Foo<T>` -> `Foo`
        "impl_item" if lang == SupportedLanguage::Rust => {
            return clean_rust_type_name(first).to_string()
        }
        "impl_item" => first.split('<').next().unwrap_or(first).trim(),
        // `module.exports.foo` / `this.foo` -> `foo`
        "assignment_expression" => first.rsplit('.').next().unwrap_or(first),
        "pair" => first.trim_matches(|c| c == '"' || c == '\''),
        _ => first,
    };
    if lang == SupportedLanguage::Rust {
        // `crate::a::Foo` keeps only the type name.
        return clean_rust_type_name(name).to_string();
    }
    name.to_string()
}

// ---------------------------------------------------------------------------
// Reference finding (AST-verified 1-hop reverse references from worktree)
// ---------------------------------------------------------------------------

/// One AST-verified reference to a symbol.
#[derive(Clone, Debug)]
pub struct Reference {
    /// Workspace-relative file path passed in by the caller.
    pub file: PathBuf,
    /// 1-based line number of the reference.
    pub line: usize,
    /// Trimmed source text at `line` (clipped for display).
    pub call_line: String,
    /// Enclosing function/type signature (body-stripped), or `None` at module level.
    pub enclosing_sig: Option<String>,
}

/// Finds AST-verified references to `target` in a single `(rel_path, source)` file,
/// appending up to `max_refs` total entries into `out`.
pub fn find_references_in_source(
    target: &str,
    rel_path: &Path,
    source: &str,
    include_tests: bool,
    max_refs: usize,
    out: &mut Vec<Reference>,
) {
    if out.len() >= max_refs {
        return;
    }
    let parts = split_name_path(target);
    let Some(&leaf) = parts.last() else {
        return;
    };
    let leaf_ident = leaf.trim_end_matches(['?', '!']);
    if !is_valid_ident(leaf_ident) {
        return;
    }
    let qualifier = (parts.len() >= 2).then(|| parts[parts.len() - 2]);
    if !include_tests && is_test_file_path(rel_path) {
        return;
    }
    let Some(lang) = SupportedLanguage::from_path(rel_path) else {
        return;
    };
    let is_ruby = lang == SupportedLanguage::Ruby;
    if !contains_utf8_ident(source, leaf, is_ruby) {
        return;
    }
    if source.len() > MAX_TREESITTER_FILE_BYTES {
        return;
    }
    let Ok(tree) = parse_with_timeout(lang, source) else {
        return;
    };
    let lines: Vec<&str> = source.lines().collect();
    let inline_tests = if include_tests {
        Vec::new()
    } else {
        find_inline_test_blocks(lang, &tree.root_node(), source)
    };
    let mut walker = RefWalker {
        lang,
        rel_path,
        lines: &lines,
        source,
        leaf,
        qualifier,
        include_tests,
        inline_tests: &inline_tests,
        enclosing: Vec::new(),
        seen_lines: HashSet::new(),
        out,
        max: max_refs,
    };
    walker.walk(tree.root_node());
}

/// Finds AST-verified references to `target` (e.g. `"execute"` or `"Grep::execute"`) across
/// `(rel_path, source)` pairs. Skips comments, strings (except interpolations), imports,
/// definition-site bindings, and (unless `include_tests`) test files & inline test blocks.
pub fn find_references_in_files(
    target: &str,
    files: &[(PathBuf, String)],
    include_tests: bool,
    max_refs: usize,
) -> Vec<Reference> {
    let mut refs = Vec::new();
    for (rel_path, source) in files {
        if refs.len() >= max_refs {
            break;
        }
        find_references_in_source(target, rel_path, source, include_tests, max_refs, &mut refs);
    }
    refs
}

struct RefWalker<'a, 'b> {
    lang: SupportedLanguage,
    rel_path: &'a Path,
    lines: &'a [&'a str],
    source: &'a str,
    leaf: &'a str,
    qualifier: Option<&'a str>,
    include_tests: bool,
    inline_tests: &'a [InlineTestBlock],
    enclosing: Vec<Node<'b>>,
    seen_lines: HashSet<usize>,
    out: &'a mut Vec<Reference>,
    max: usize,
}

impl<'a, 'b> RefWalker<'a, 'b> {
    fn walk(&mut self, node: Node<'b>) {
        if self.out.len() >= self.max {
            return;
        }
        let kind = node.kind();
        if is_skip_ast_kind(kind) {
            return;
        }
        if !self.include_tests {
            let s_line = node.start_position().row + 1;
            let e_line = node.end_position().row + 1;
            if self
                .inline_tests
                .iter()
                .any(|b| s_line >= b.start_line && e_line <= b.end_line)
            {
                return;
            }
        }

        // Skip plain string bodies, descending only into interpolation expressions.
        if (kind == "string"
            && matches!(
                self.lang,
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
                    self.walk(ch);
                }
            }
            return;
        }

        // Skip Ruby `require` / `require_relative`.
        if self.lang == SupportedLanguage::Ruby && kind == "call" {
            if let Some(m) = node.child_by_field_name("method") {
                if matches!(
                    self.source.get(m.start_byte()..m.end_byte()),
                    Some("require" | "require_relative")
                ) {
                    return;
                }
            }
        }

        let pushed = match check_primary_definition(self.lang, &node) {
            Some(matched) if matched.id() == node.id() => {
                if !self.include_tests
                    && test_scope::is_test_scope(
                        self.lang,
                        &matched,
                        Some(self.rel_path),
                        self.lines,
                    )
                {
                    return;
                }
                self.enclosing.push(matched);
                true
            }
            _ => false,
        };

        if matches!(
            kind,
            "identifier"
                | "type_identifier"
                | "field_identifier"
                | "property_identifier"
                | "shorthand_property_identifier"
                | "shorthand_property_identifier_pattern"
                | "namespace_identifier"
                | "constant"
        ) {
            if let Some(text) = get_node_text_same_line(&node, self.lines) {
                if text == self.leaf
                    && !is_definition_binding(self.lang, &node)
                    && self
                        .qualifier
                        .is_none_or(|q| qualifier_compatible(self.lang, &node, self.source, q))
                {
                    let line = node.start_position().row + 1;
                    if self.seen_lines.insert(line) {
                        let raw_line = self
                            .lines
                            .get(line.saturating_sub(1))
                            .map(|l| l.trim())
                            .unwrap_or("");
                        let call_line = clip_chars(raw_line, 160);
                        let enclosing_sig = enclosing_signature(
                            self.lang,
                            &self.enclosing,
                            self.lines,
                            self.source,
                        );
                        self.out.push(Reference {
                            file: self.rel_path.to_path_buf(),
                            line,
                            call_line,
                            enclosing_sig,
                        });
                    }
                    if self.out.len() >= self.max {
                        if pushed {
                            self.enclosing.pop();
                        }
                        return;
                    }
                }
            }
        }

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.walk(child);
            if self.out.len() >= self.max {
                break;
            }
        }
        if pushed {
            self.enclosing.pop();
        }
    }
}

fn clip_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", head.trim_end())
    }
}

fn is_skip_ast_kind(kind: &str) -> bool {
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
            | "heredoc_body"
            | "regex"
            | "simple_symbol"
            | "symbol"
    )
}

/// True if `node` sits at a declaration/parameter/pattern/key binding position rather than a reference.
fn is_definition_binding(lang: SupportedLanguage, node: &Node) -> bool {
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
                | "function_signature_item"
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
                | "impl_item"
                | "mod_item"
                | "namespace_definition"
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
                | "let_declaration"
                | "short_var_declaration"
                | "var_spec"
                | "const_spec"
        )
    {
        return true;
    }
    // JS/TS object literal key (`{ execute: 1 }`) is not a reference, whereas shorthand (`{ execute }`) is.
    if pk == "pair"
        && parent
            .child_by_field_name("key")
            .is_some_and(|c| c.id() == node.id())
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
            | "pattern_list"
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
    if matches!(
        pk,
        "mut_pattern"
            | "ref_pattern"
            | "tuple_pattern"
            | "slice_pattern"
            | "or_pattern"
            | "captured_pattern"
            | "field_pattern"
    ) {
        return true;
    }
    if lang == SupportedLanguage::Dart
        && ((pk == "type_alias" && node.kind() == "type_identifier")
            || (matches!(pk, "mixin_application_class" | "extension_type_name")
                && node.kind() == "identifier"))
    {
        return true;
    }
    false
}

/// When the user asked for `Parent/member` (e.g. `Grep::execute`), reject obvious static/type
/// mismatches like `OtherType::execute()` or `OtherClass.execute()`, while keeping instance calls
/// like `g.execute()` or `self.execute()`.
fn qualifier_compatible(
    lang: SupportedLanguage,
    node: &Node,
    source: &str,
    target_qual: &str,
) -> bool {
    let Some(parent) = node.parent() else {
        return true;
    };
    let pk = parent.kind();
    let is_member_side = matches!(
        pk,
        "scoped_identifier"
            | "qualified_identifier"
            | "scoped_type_identifier"
            | "method_reference"
            | "field_expression"
            | "member_expression"
            | "selector_expression"
            | "method_invocation"
            | "attribute"
            | "call"
    ) && (parent
        .child_by_field_name("name")
        .is_some_and(|c| c.id() == node.id())
        || parent
            .child_by_field_name("field")
            .is_some_and(|c| c.id() == node.id())
        || parent
            .child_by_field_name("property")
            .is_some_and(|c| c.id() == node.id())
        || parent
            .child_by_field_name("attribute")
            .is_some_and(|c| c.id() == node.id())
        || parent
            .child_by_field_name("method")
            .is_some_and(|c| c.id() == node.id()));
    if !is_member_side {
        return false;
    }
    let lhs_node = parent
        .child_by_field_name("path")
        .or_else(|| parent.child_by_field_name("value"))
        .or_else(|| parent.child_by_field_name("object"))
        .or_else(|| parent.child_by_field_name("operand"))
        .or_else(|| parent.child_by_field_name("receiver"))
        .or_else(|| parent.child_by_field_name("scope"))
        .or_else(|| parent.named_child(0).filter(|c| c.id() != node.id()));
    let Some(lhs) = lhs_node.and_then(|n| source.get(n.start_byte()..n.end_byte())) else {
        return true;
    };
    let lhs_tail = extract_lhs_tail(lhs);
    if matches!(lhs_tail, "self" | "Self" | "this") {
        if let Some(enc_type) = enclosing_type_name(lang, node, source) {
            return enc_type == target_qual;
        }
        return true;
    }
    if lhs_tail.is_empty() || matches!(lhs_tail, "super" | "crate") {
        return true;
    }
    let is_static_path = matches!(
        pk,
        "scoped_identifier"
            | "qualified_identifier"
            | "scoped_type_identifier"
            | "method_reference"
    );
    let is_upper_type = lhs_tail.chars().next().is_some_and(char::is_uppercase);
    if is_upper_type || is_static_path {
        return lhs_tail == target_qual;
    }
    true
}

/// Extracts the trailing identifier from a receiver/qualifier expression, stripping trailing
/// generic/turbofish arguments like `Other::<T>` or `Vec<Foo>`.
fn extract_lhs_tail(lhs: &str) -> &str {
    let mut s = lhs.trim();
    if s.ends_with('>') {
        let mut depth = 0usize;
        for (idx, ch) in s.char_indices().rev() {
            match ch {
                '>' => depth += 1,
                '<' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        s = s[..idx].trim_end().trim_end_matches("::").trim_end();
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    s.rsplit("::")
        .next()
        .and_then(|p| p.rsplit('.').next())
        .and_then(|p| p.rsplit("->").next())
        .unwrap_or("")
        .split('<')
        .next()
        .unwrap_or("")
        .trim()
}

/// Returns the nearest enclosing type/impl/class name for `self`/`Self`/`this` disambiguation.
fn enclosing_type_name(lang: SupportedLanguage, node: &Node, source: &str) -> Option<String> {
    let mut curr = node.parent();
    while let Some(anc) = curr {
        let kind = anc.kind();
        match lang {
            SupportedLanguage::Rust if kind == "impl_item" => {
                if let Some(ty) = anc.child_by_field_name("type") {
                    if let Ok(raw) = ty.utf8_text(source.as_bytes()) {
                        let cleaned = clean_rust_type_name(raw);
                        if !cleaned.is_empty() {
                            return Some(cleaned.to_string());
                        }
                    }
                }
            }
            SupportedLanguage::Rust if kind == "trait_item" => {
                if let Some(n) = anc.child_by_field_name("name") {
                    if let Ok(raw) = n.utf8_text(source.as_bytes()) {
                        return Some(raw.trim().to_string());
                    }
                }
            }
            _ if matches!(
                kind,
                "class_definition"
                    | "class_declaration"
                    | "class_specifier"
                    | "struct_specifier"
                    | "struct_declaration"
                    | "record_declaration"
                    | "interface_declaration"
                    | "class"
                    | "module"
            ) =>
            {
                if let Some(n) = anc.child_by_field_name("name") {
                    if let Ok(raw) = n.utf8_text(source.as_bytes()) {
                        let t = raw.trim();
                        if !t.is_empty() {
                            return Some(t.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
        curr = anc.parent();
    }
    None
}

fn enclosing_signature(
    lang: SupportedLanguage,
    stack: &[Node],
    lines: &[&str],
    source: &str,
) -> Option<String> {
    let enc_node = stack
        .iter()
        .rev()
        .find(|n| {
            let core = unwrap_wrappers(**n);
            is_foldable_function_node(lang, &core)
        })
        .or_else(|| stack.last())?;
    let parent_prefix = signature::extract_parent_scope_prefix(lang, enc_node, lines, source);
    let raw_sig = signature::extract_signature(lang, enc_node, lines, source);
    signature::compact_definition_signature_with_ast_opts(
        lang,
        enc_node,
        parent_prefix.as_deref(),
        raw_sig.as_deref(),
        source,
        SigFormatOpts {
            max_chars: 120,
            scope_sep: "::",
            preserve_key_decorators: false,
            greedy_params: true,
        },
    )
    .map(|s| s.trim_end_matches('{').trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outline(file: &str, src: &str) -> Vec<(String, usize, usize, usize)> {
        let (_, syms) = extract_symbols(Path::new(file), src).unwrap();
        syms.into_iter()
            .map(|s| (s.qualified_name(), s.start_line, s.end_line, s.depth))
            .collect()
    }

    #[test]
    fn rust_impl_members_nest_and_fn_locals_are_skipped() {
        let src = "\
/// doc
#[derive(Debug)]
pub struct Tool;

impl<T> Runner for crate::a::Tool<T> {
    fn run(&self) {
        fn local() {}
    }
}

mod inner {
    pub const X: u8 = 1;
}
";
        assert_eq!(
            outline("a.rs", src),
            vec![
                ("Tool".into(), 2, 3, 0),
                ("Tool".into(), 5, 9, 0),
                ("Tool/run".into(), 6, 8, 1),
                ("inner".into(), 11, 13, 0),
                ("inner/X".into(), 12, 12, 1),
            ]
        );
    }

    #[test]
    fn python_decorated_class_methods() {
        let src = "@dataclass\nclass A:\n    def m(self):\n        def inner():\n            pass\n\ndef top():\n    pass\n";
        assert_eq!(
            outline("a.py", src),
            vec![
                ("A".into(), 1, 5, 0),
                ("A/m".into(), 3, 5, 1),
                ("top".into(), 7, 8, 0),
            ]
        );
    }

    #[test]
    fn typescript_exports_arrow_fns_and_classes() {
        let src = "export const f = () => 1;\nexport class C {\n  m() { return 2; }\n}\ninterface I { x: number }\n";
        let got = outline("a.ts", src);
        assert_eq!(got[0], ("f".into(), 1, 1, 0));
        assert_eq!(got[1], ("C".into(), 2, 4, 0));
        assert_eq!(got[2], ("C/m".into(), 3, 3, 1));
        assert_eq!(got[3].0, "I");
    }

    #[test]
    fn go_and_cpp_names() {
        let go = "package p\n\ntype S struct{}\n\nfunc (s *S) M() {}\n\nfunc F() {}\n";
        let names: Vec<String> = outline("a.go", go).into_iter().map(|s| s.0).collect();
        assert_eq!(names, vec!["S", "S/M", "F"]);

        let cpp = "namespace ns {\nint *make(int a) { return 0; }\nclass K { };\n}\n";
        let names: Vec<String> = outline("a.cpp", cpp).into_iter().map(|s| s.0).collect();
        assert_eq!(names, vec!["ns", "ns/make", "ns/K"]);
    }

    #[test]
    fn unsupported_extension_is_an_error() {
        assert!(extract_symbols(Path::new("a.unknownext"), "x").is_err());
    }
}
