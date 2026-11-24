//! Multi-language Tree-Sitter compaction passes for LLM 1-turn commit review:
//! - Strict inline test block detection & AST TOC folding inside production files
//! - Progressive water-fill-aware AST skeletonization for oversized new production files
//! - Budget-bounded AST symbol TOC generation for omitted chunks (new, modified, deleted)

use super::{
    check_primary_definition,
    diff_expand::{
        compute_diff_line_numbers, extract_chunk_header_and_added_source,
        is_container_definition_node, parse_hunk_header, strip_clamped_suffix,
    },
    get_node_text_same_line, language_registry, signature,
    test_scope::{
        collect_node_lines_text, extract_c_cpp_function_name, has_ast_java_csharp_test_attr,
        has_ast_rust_attr, is_cfg_test_attribute, is_rust_test_attr,
    },
    SupportedLanguage, MAX_TREESITTER_FILE_BYTES,
};
use std::path::Path;

/// Minimum span (in lines) for an inline test block to be eligible for TOC folding.
const MIN_INLINE_TEST_FOLD_LINES: usize = 6;
/// Minimum span (in lines) for a function/method body to be folded during AST skeletonization.
const MIN_SKELETON_BODY_LINES: usize = 6;
/// Maximum byte length for an inline test TOC marker line so it stays readable and bounded.
const MAX_INLINE_TEST_MARKER_BYTES: usize = 600;
/// Maximum Python docstring lines preserved inside a skeletonized function body.
const MAX_PRESERVED_PY_DOCSTRING_LINES: usize = 4;

/// Describes an explicit inline test scope inside a production file (1-based inclusive lines).
#[derive(Clone, Debug)]
pub(super) struct InlineTestBlock {
    pub(super) start_line: usize,
    pub(super) end_line: usize,
    pub(super) scope_label: Option<String>,
    pub(super) test_names: Vec<String>,
}

/// Finds the first signature head terminator in `stop_chars` while skipping a leading Rust
/// `pub(...)` visibility modifier (e.g. `pub(crate)`, `pub(in crate::foo)`).
pub(super) fn find_sig_head_cut(raw: &str, stop_chars: &[char]) -> usize {
    let mut search_from = 0usize;
    let trimmed = raw.trim_start();
    let leading_ws = raw.len() - trimmed.len();
    if let Some(after_pub) = trimmed.strip_prefix("pub") {
        let after_pub_ws = after_pub.trim_start();
        if after_pub_ws.starts_with('(') {
            if let Some(close_rel) = after_pub_ws.find(')') {
                let consumed = (after_pub.len() - after_pub_ws.len()) + close_rel + 1;
                search_from = leading_ws + 3 + consumed;
            }
        }
    }
    raw[search_from..]
        .find(stop_chars)
        .map(|rel| search_from + rel)
        .unwrap_or(raw.len())
}

/// Extracts a concise bare symbol name (without `fn`/`struct`/`pub` keywords or parameter lists)
/// for compact comma-separated symbol listings.
pub(super) fn bare_def_name(
    lang: SupportedLanguage,
    matched: &tree_sitter::Node,
    lines: &[&str],
    source: &str,
) -> Option<String> {
    let target = match matched.kind() {
        "decorated_definition" => matched
            .child_by_field_name("definition")
            .unwrap_or(*matched),
        "export_statement" => matched
            .child_by_field_name("declaration")
            .unwrap_or(*matched),
        "template_declaration" => {
            let mut found = *matched;
            let mut cursor = matched.walk();
            for ch in matched.named_children(&mut cursor) {
                if check_primary_definition(lang, &ch).is_some() {
                    found = ch;
                    break;
                }
            }
            found
        }
        "expression_statement" => matched.named_child(0).unwrap_or(*matched),
        _ => *matched,
    };

    // JS/TS `const foo = () => {}` / `let bar = function() {}`
    if matches!(
        target.kind(),
        "lexical_declaration" | "variable_declaration"
    ) {
        let mut cursor = target.walk();
        for ch in target.named_children(&mut cursor) {
            if ch.kind() == "variable_declarator" {
                if let Some(name_node) = ch.child_by_field_name("name") {
                    if let Some(name) = get_node_text_same_line(&name_node, lines) {
                        let trimmed = name.trim();
                        if !trimmed.is_empty() {
                            return Some(signature::truncate_preview(trimmed, 40));
                        }
                    }
                }
            }
        }
    }

    // JS/TS `foo.bar = () => {}` / `pair: key: () => {}`
    if target.kind() == "pair" {
        if let Some(key) = target.child_by_field_name("key") {
            if let Some(text) = get_node_text_same_line(&key, lines) {
                let cleaned = text.trim().trim_matches(['"', '\'']);
                if !cleaned.is_empty() {
                    return Some(signature::truncate_preview(cleaned, 40));
                }
            }
        }
    }
    if target.kind() == "assignment_expression" {
        if let Some(left) = target.child_by_field_name("left") {
            if let Some(text) = get_node_text_same_line(&left, lines) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return Some(signature::truncate_preview(trimmed, 40));
                }
            }
        }
    }

    // C / C++ functions use nested declarators rather than a direct `name` field
    if matches!(lang, SupportedLanguage::C | SupportedLanguage::Cpp)
        && target.kind() == "function_definition"
    {
        if let Some(name) = extract_c_cpp_function_name(&target, lines) {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                return Some(signature::truncate_preview(trimmed, 40));
            }
        }
    }

    // Go `type_declaration` wraps `type_spec`
    if lang == SupportedLanguage::Go && target.kind() == "type_declaration" {
        let mut cursor = target.walk();
        for ch in target.named_children(&mut cursor) {
            if ch.kind() == "type_spec" {
                if let Some(name_node) = ch.child_by_field_name("name") {
                    if let Some(name) = get_node_text_same_line(&name_node, lines) {
                        let trimmed = name.trim();
                        if !trimmed.is_empty() {
                            return Some(signature::truncate_preview(trimmed, 40));
                        }
                    }
                }
            }
        }
    }

    if let Some(name_node) = target
        .child_by_field_name("name")
        .or_else(|| target.child_by_field_name("property"))
    {
        if let Some(name) = get_node_text_same_line(&name_node, lines) {
            let trimmed = name.trim().trim_matches(['"', '\'']);
            if !trimmed.is_empty() {
                return Some(signature::truncate_preview(trimmed, 40));
            }
        }
    }

    // Fallback: extract last identifier token before `(` / `<` / `{` / `:` from signature
    let raw = signature::extract_signature(lang, matched, lines, source)?;
    let cut = find_sig_head_cut(&raw, &['(', '{', '<']);
    let head = raw[..cut].trim().trim_end_matches(['=', ':']).trim();
    let token = head.split_whitespace().last().unwrap_or(head);
    if token.is_empty() {
        Some(signature::truncate_preview(&raw, 40))
    } else {
        Some(signature::truncate_preview(token, 40))
    }
}

/// Checks if a Rust `mod_item` has a body (`declaration_list`, excluding external `mod tests;`)
/// and either carries `#[cfg(test)]` or is named `test`/`tests` and actually contains test-attributed items.
fn is_explicit_rust_test_mod(node: &tree_sitter::Node, lines: &[&str]) -> bool {
    if node.kind() != "mod_item" {
        return false;
    }
    let Some(body) = node.child_by_field_name("body") else {
        return false;
    };
    if body.kind() != "declaration_list" {
        return false;
    }
    if has_ast_rust_attr(node, lines, is_cfg_test_attribute) {
        return true;
    }
    let is_named_tests = node
        .child_by_field_name("name")
        .and_then(|n| get_node_text_same_line(&n, lines))
        .is_some_and(|name| name == "tests" || name == "test");
    if !is_named_tests {
        return false;
    }
    // Require at least one explicit `#[test]` / `#[cfg(test)]` item inside to prevent false positives
    fn has_explicit_test_item(n: tree_sitter::Node, lines: &[&str]) -> bool {
        if n.kind() == "function_item" && has_ast_rust_attr(&n, lines, is_rust_test_attr) {
            return true;
        }
        let mut cursor = n.walk();
        for ch in n.named_children(&mut cursor) {
            if has_explicit_test_item(ch, lines) {
                return true;
            }
        }
        false
    }
    has_explicit_test_item(body, lines)
}

/// Walks backwards over preceding Rust `attribute_item` siblings (and comments sandwiched between them)
/// to find the earliest 0-based start row attached to `node`.
fn rust_start_row_with_attrs(node: &tree_sitter::Node) -> usize {
    let mut start_row = node.start_position().row;
    let mut prev = node.prev_named_sibling();
    while let Some(sib) = prev {
        match sib.kind() {
            "attribute_item" => {
                start_row = sib.start_position().row;
                prev = sib.prev_named_sibling();
            }
            "line_comment" | "block_comment" => {
                // Only absorb a comment if another attribute sits immediately above it
                let mut look = sib.prev_named_sibling();
                let mut has_attr_above = false;
                while let Some(l) = look {
                    if l.kind() == "attribute_item" {
                        has_attr_above = true;
                        break;
                    } else if matches!(l.kind(), "line_comment" | "block_comment") {
                        look = l.prev_named_sibling();
                    } else {
                        break;
                    }
                }
                if has_attr_above {
                    start_row = sib.start_position().row;
                    prev = sib.prev_named_sibling();
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    start_row
}

/// Walks backwards over preceding Java/C# annotation or attribute_list siblings.
fn java_csharp_start_row_with_attrs(node: &tree_sitter::Node) -> usize {
    let mut start_row = node.start_position().row;
    let mut prev = node.prev_named_sibling();
    while let Some(sib) = prev {
        if matches!(
            sib.kind(),
            "attribute_list" | "marker_annotation" | "annotation"
        ) {
            start_row = sib.start_position().row;
            prev = sib.prev_named_sibling();
        } else {
            break;
        }
    }
    start_row
}

fn js_ts_test_call_label(node: &tree_sitter::Node, lines: &[&str]) -> Option<String> {
    let call = if node.kind() == "expression_statement" {
        node.named_child(0)?
    } else {
        *node
    };
    if call.kind() != "call_expression" {
        return None;
    }

    // Ensure the call actually passes a function/arrow callback (avoids matching helper calls like `test(x)`)
    let args = call.child_by_field_name("arguments")?;
    let mut has_callback = false;
    let mut first_str: Option<String> = None;
    let mut arg_cursor = args.walk();
    for arg in args.named_children(&mut arg_cursor) {
        if matches!(arg.kind(), "arrow_function" | "function_expression") {
            has_callback = true;
        } else if first_str.is_none() && matches!(arg.kind(), "string" | "template_string") {
            if let Some(t) = get_node_text_same_line(&arg, lines) {
                let cleaned = t.trim().trim_matches(['"', '\'', '`']);
                if !cleaned.is_empty() {
                    first_str = Some(signature::truncate_preview(cleaned, 40));
                }
            }
        }
    }
    if !has_callback {
        return None;
    }

    let func = call.child_by_field_name("function")?;
    let fn_name = match func.kind() {
        "identifier" => get_node_text_same_line(&func, lines)?,
        "member_expression" => {
            let obj = func
                .child_by_field_name("object")
                .and_then(|n| get_node_text_same_line(&n, lines));
            let prop = func
                .child_by_field_name("property")
                .and_then(|n| get_node_text_same_line(&n, lines));
            match (obj, prop) {
                (Some("Deno"), Some("test")) => "Deno.test",
                (
                    Some(o @ ("describe" | "it" | "test" | "suite" | "context")),
                    Some("only" | "skip" | "todo" | "concurrent" | "sequential" | "describe"),
                ) => o,
                _ => return None,
            }
        }
        "call_expression" => {
            // e.g. `describe.each([...])("...", () => { ... })`
            let inner_fn = func.child_by_field_name("function")?;
            if inner_fn.kind() == "member_expression" {
                let obj = inner_fn
                    .child_by_field_name("object")
                    .and_then(|n| get_node_text_same_line(&n, lines));
                let prop = inner_fn
                    .child_by_field_name("property")
                    .and_then(|n| get_node_text_same_line(&n, lines));
                match (obj, prop) {
                    (Some(o @ ("describe" | "it" | "test" | "suite")), Some("each" | "for")) => o,
                    _ => return None,
                }
            } else {
                return None;
            }
        }
        _ => return None,
    };

    if !matches!(
        fn_name,
        "describe" | "it" | "test" | "suite" | "context" | "Deno.test"
    ) {
        return None;
    }

    Some(match first_str {
        Some(desc) => format!("{fn_name}(\"{desc}\")"),
        None => fn_name.to_string(),
    })
}

fn is_explicit_python_test_class(node: &tree_sitter::Node, lines: &[&str]) -> Option<String> {
    let cls = if node.kind() == "decorated_definition" {
        node.child_by_field_name("definition")?
    } else {
        *node
    };
    if cls.kind() != "class_definition" {
        return None;
    }
    let name = cls
        .child_by_field_name("name")
        .and_then(|n| get_node_text_same_line(&n, lines))?;
    let name_looks_test =
        name.starts_with("Test") || name.ends_with("Test") || name.ends_with("TestCase");
    let inherits_test = cls
        .child_by_field_name("superclasses")
        .map(|sc| collect_node_lines_text(&sc, lines))
        .is_some_and(|t| t.contains("TestCase") || t.contains("unittest"));
    if !name_looks_test && !inherits_test {
        return None;
    }

    // Require either unittest TestCase inheritance or at least one `def test_*` method inside
    if inherits_test {
        return Some(format!("class {name}"));
    }
    let body = cls.child_by_field_name("body")?;
    let mut cursor = body.walk();
    for ch in body.named_children(&mut cursor) {
        let def = if ch.kind() == "decorated_definition" {
            ch.child_by_field_name("definition").unwrap_or(ch)
        } else {
            ch
        };
        if def.kind() == "function_definition"
            && def
                .child_by_field_name("name")
                .and_then(|n| get_node_text_same_line(&n, lines))
                .is_some_and(|fn_name| fn_name.starts_with("test_"))
        {
            return Some(format!("class {name}"));
        }
    }
    None
}

fn is_explicit_python_decorated_test(node: &tree_sitter::Node, lines: &[&str]) -> bool {
    if node.kind() != "decorated_definition" {
        return false;
    }
    let Some(def) = node.child_by_field_name("definition") else {
        return false;
    };
    if def.kind() != "function_definition" {
        return false;
    }
    let mut cursor = node.walk();
    for ch in node.named_children(&mut cursor) {
        if ch.kind() == "decorator" {
            let text = collect_node_lines_text(&ch, lines);
            if text.starts_with("@pytest.mark.")
                || text.starts_with("@unittest.")
                || text.starts_with("@mark.")
            {
                return true;
            }
        }
    }
    false
}

fn is_explicit_go_test_fn(node: &tree_sitter::Node, lines: &[&str]) -> bool {
    if node.kind() != "function_declaration" {
        return false;
    }
    let Some(name) = node
        .child_by_field_name("name")
        .and_then(|n| get_node_text_same_line(&n, lines))
    else {
        return false;
    };
    if !(name.starts_with("Test") || name.starts_with("Benchmark") || name.starts_with("Fuzz")) {
        return false;
    }
    node.child_by_field_name("parameters")
        .map(|p| collect_node_lines_text(&p, lines))
        .is_some_and(|params| {
            params.contains("*testing.T")
                || params.contains("*testing.B")
                || params.contains("*testing.F")
        })
}

/// Inspects a Ruby class body for explicit test definitions (`def test_*`, Rails `test "..." do`,
/// or when `allow_spec_blocks` is true, `it`/`specify` spec blocks) without descending into regular
/// methods or unrelated nested classes.
fn ruby_class_has_explicit_test_members(
    class_node: &tree_sitter::Node,
    lines: &[&str],
    allow_spec_blocks: bool,
) -> bool {
    let Some(body) = class_node.child_by_field_name("body") else {
        return false;
    };
    fn walk_body(node: tree_sitter::Node, lines: &[&str], allow_spec_blocks: bool) -> bool {
        match node.kind() {
            "method" => {
                return node
                    .child_by_field_name("name")
                    .and_then(|n| get_node_text_same_line(&n, lines))
                    .is_some_and(|name| name.starts_with("test_"));
            }
            "singleton_method" | "class" => return false,
            "call" => {
                let has_block = (0..node.named_child_count()).any(|i| {
                    node.named_child(i)
                        .is_some_and(|c| matches!(c.kind(), "do_block" | "block"))
                });
                if has_block {
                    let recv = node.child_by_field_name("receiver");
                    let recv_is_none = recv.is_none();
                    let recv_is_rspec = recv.is_some_and(|r| {
                        matches!(
                            collect_node_lines_text(&r, lines).as_str(),
                            "RSpec" | "::RSpec"
                        )
                    });
                    if let Some(method) = node
                        .child_by_field_name("method")
                        .and_then(|n| get_node_text_same_line(&n, lines))
                    {
                        if recv_is_none && method == "test" {
                            return true;
                        }
                        if allow_spec_blocks
                            && (recv_is_none || recv_is_rspec)
                            && matches!(method, "it" | "specify")
                        {
                            return true;
                        }
                    }
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for ch in node.named_children(&mut cursor) {
            if walk_body(ch, lines, allow_spec_blocks) {
                return true;
            }
        }
        false
    }
    walk_body(body, lines, allow_spec_blocks)
}

fn ruby_test_block_label(
    node: &tree_sitter::Node,
    lines: &[&str],
    in_class_or_module: bool,
) -> Option<(String, bool)> {
    if node.kind() == "call" {
        let is_rspec_receiver = match node.child_by_field_name("receiver") {
            None => false,
            Some(recv) => {
                let recv_text = collect_node_lines_text(&recv, lines);
                if matches!(recv_text.as_str(), "RSpec" | "::RSpec") {
                    true
                } else {
                    return None;
                }
            }
        };
        let method = node
            .child_by_field_name("method")
            .and_then(|n| get_node_text_same_line(&n, lines))?;
        if !matches!(
            method,
            "describe" | "context" | "it" | "specify" | "feature" | "scenario"
        ) {
            return None;
        }
        // Inside non-test production `class`/`module` bodies, bare DSL calls like
        // `feature :billing do ... end` or `context :admin do ... end` are common in
        // Rails/Grape/Flipper; only match explicit `RSpec.*` calls there.
        if in_class_or_module && !is_rspec_receiver {
            return None;
        }
        let has_block = (0..node.named_child_count()).any(|i| {
            node.named_child(i)
                .is_some_and(|c| matches!(c.kind(), "do_block" | "block"))
        });
        if !has_block {
            return None;
        }
        return Some((method.to_string(), is_rspec_receiver));
    }
    if node.kind() == "class" {
        let name = node
            .child_by_field_name("name")
            .and_then(|n| get_node_text_same_line(&n, lines))?;
        let inherits_test = node
            .child_by_field_name("superclass")
            .map(|sc| collect_node_lines_text(&sc, lines))
            .is_some_and(|t| {
                let trimmed = t.trim_start_matches('<').trim();
                trimmed.contains("Minitest")
                    || trimmed.contains("MiniTest")
                    || trimmed.contains("Test::Unit")
                    || trimmed.ends_with("TestCase")
                    || trimmed.ends_with("Test")
            });
        if inherits_test {
            return Some((format!("class {name}"), true));
        }
        // Without an explicit test superclass, require actual test methods / spec blocks inside
        // so production domain classes like `ApiSpec`, `JobSpec`, `GemSpec`, or `SelfTest` are
        // never misclassified as inline tests.
        let short_name = name.rsplit("::").next().unwrap_or(name);
        if short_name.ends_with("Test") || short_name.ends_with("TestCase") {
            if ruby_class_has_explicit_test_members(node, lines, false) {
                return Some((format!("class {name}"), true));
            }
        } else if short_name.ends_with("Spec")
            && ruby_class_has_explicit_test_members(node, lines, true)
        {
            return Some((format!("class {name}"), true));
        }
    }
    None
}

fn is_explicit_c_cpp_test_macro(node: &tree_sitter::Node, lines: &[&str]) -> Option<String> {
    if !matches!(
        node.kind(),
        "function_definition" | "declaration" | "expression_statement"
    ) {
        return None;
    }
    let fn_name = extract_c_cpp_function_name(node, lines)?;
    if !matches!(
        fn_name,
        "TEST"
            | "TEST_F"
            | "TEST_P"
            | "TYPED_TEST"
            | "TYPED_TEST_P"
            | "TEST_CASE"
            | "TEMPLATE_TEST_CASE"
            | "SCENARIO"
    ) {
        return None;
    }
    // Must span multiple lines with a body block
    let text = collect_node_lines_text(node, lines);
    if !text.contains('{') {
        return None;
    }
    // Extract concise header up to `{`
    let sig = text.split('{').next().unwrap_or(fn_name).trim();
    Some(signature::truncate_preview(sig, 48))
}

fn collect_inner_test_names(
    lang: SupportedLanguage,
    node: tree_sitter::Node,
    lines: &[&str],
    source: &str,
    out: &mut Vec<String>,
) {
    match lang {
        SupportedLanguage::Rust => {
            if node.kind() == "function_item" {
                if let Some(name) = bare_def_name(lang, &node, lines, source) {
                    if !out.contains(&name) {
                        out.push(name);
                    }
                }
                return;
            }
        }
        SupportedLanguage::JavaScript | SupportedLanguage::TypeScript | SupportedLanguage::Tsx => {
            if let Some(label) = js_ts_test_call_label(&node, lines) {
                if (label.starts_with("it(")
                    || label.starts_with("test(")
                    || label.starts_with("Deno.test(")
                    || label == "it"
                    || label == "test")
                    && !out.contains(&label)
                {
                    out.push(label);
                    return;
                }
            }
        }
        SupportedLanguage::Python => {
            if node.kind() == "function_definition" {
                if let Some(name) = bare_def_name(lang, &node, lines, source) {
                    if !out.contains(&name) {
                        out.push(name);
                    }
                }
                return;
            }
        }
        SupportedLanguage::Java | SupportedLanguage::CSharp => {
            if node.kind() == "method_declaration" {
                if let Some(name) = bare_def_name(lang, &node, lines, source) {
                    if !out.contains(&name) {
                        out.push(name);
                    }
                }
                return;
            }
        }
        SupportedLanguage::Ruby => {
            if node.kind() == "method" {
                if let Some(name) = bare_def_name(lang, &node, lines, source) {
                    if !out.contains(&name) {
                        out.push(name);
                    }
                }
                return;
            }
            if node.kind() == "call" && node.child_by_field_name("receiver").is_none() {
                let has_block = (0..node.named_child_count()).any(|i| {
                    node.named_child(i)
                        .is_some_and(|c| matches!(c.kind(), "do_block" | "block"))
                });
                if has_block {
                    if let Some(m) = node
                        .child_by_field_name("method")
                        .and_then(|n| get_node_text_same_line(&n, lines))
                    {
                        if matches!(m, "it" | "specify" | "scenario") {
                            let text = collect_node_lines_text(&node, lines);
                            let head = text.split(" do").next().unwrap_or(m).trim();
                            let label = signature::truncate_preview(head, 40);
                            if !out.contains(&label) {
                                out.push(label);
                            }
                            return;
                        }
                    }
                }
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for ch in node.named_children(&mut cursor) {
        collect_inner_test_names(lang, ch, lines, source, out);
    }
}

/// Checks whether all lines strictly between `prev_end` and `next_start` (1-based) are blank or comments
/// for `lang` (so `#define`/`#ifdef` in C/C++ are never misclassified as `#` comments).
fn only_blank_or_comment_between(
    lang: SupportedLanguage,
    lines: &[&str],
    prev_end: usize,
    next_start: usize,
) -> bool {
    if next_start <= prev_end + 1 {
        return true;
    }
    let allows_hash_comment = matches!(
        lang,
        SupportedLanguage::Python | SupportedLanguage::Ruby | SupportedLanguage::Bash
    );
    let allows_slash_comment = !allows_hash_comment;
    for ln in (prev_end + 1)..next_start {
        if let Some(text) = lines.get(ln - 1) {
            let t = text.trim();
            let is_comment = t.is_empty()
                || (allows_hash_comment && t.starts_with('#'))
                || (allows_slash_comment
                    && (t.starts_with("//")
                        || t.starts_with("/*")
                        || t.starts_with('*')
                        || t.starts_with("*/")));
            if !is_comment {
                return false;
            }
        }
    }
    true
}

/// Identifies explicit inline test blocks inside a production source file across all supported languages.
/// Strict by design: never relies on bare function names like `fn test_connection()` to avoid hiding
/// production logic.
pub(super) fn find_inline_test_blocks(
    lang: SupportedLanguage,
    root: &tree_sitter::Node,
    source: &str,
) -> Vec<InlineTestBlock> {
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let max_line = lines.len();
    let mut raw_blocks: Vec<InlineTestBlock> = Vec::new();

    fn walk(
        lang: SupportedLanguage,
        node: tree_sitter::Node,
        lines: &[&str],
        source: &str,
        max_line: usize,
        in_ruby_container: bool,
        out: &mut Vec<InlineTestBlock>,
    ) {
        match lang {
            SupportedLanguage::Rust => {
                if is_explicit_rust_test_mod(&node, lines) {
                    let s = rust_start_row_with_attrs(&node) + 1;
                    let e = (node.end_position().row + 1).min(max_line);
                    let mod_name = node
                        .child_by_field_name("name")
                        .and_then(|n| get_node_text_same_line(&n, lines))
                        .map(|n| format!("mod {n}"));
                    let mut names = Vec::new();
                    collect_inner_test_names(lang, node, lines, source, &mut names);
                    out.push(InlineTestBlock {
                        start_line: s,
                        end_line: e,
                        scope_label: mod_name,
                        test_names: names,
                    });
                    return;
                }
                if node.kind() == "impl_item"
                    && has_ast_rust_attr(&node, lines, is_cfg_test_attribute)
                {
                    let s = rust_start_row_with_attrs(&node) + 1;
                    let e = (node.end_position().row + 1).min(max_line);
                    let scope = signature::extract_signature(lang, &node, lines, source)
                        .map(|t| signature::truncate_preview(t.trim_end_matches('{').trim(), 48));
                    let mut names = Vec::new();
                    collect_inner_test_names(lang, node, lines, source, &mut names);
                    out.push(InlineTestBlock {
                        start_line: s,
                        end_line: e,
                        scope_label: scope,
                        test_names: names,
                    });
                    return;
                }
                if node.kind() == "function_item"
                    && has_ast_rust_attr(&node, lines, is_rust_test_attr)
                {
                    let s = rust_start_row_with_attrs(&node) + 1;
                    let e = (node.end_position().row + 1).min(max_line);
                    let name = bare_def_name(lang, &node, lines, source)
                        .unwrap_or_else(|| "test".to_string());
                    out.push(InlineTestBlock {
                        start_line: s,
                        end_line: e,
                        scope_label: None,
                        test_names: vec![name],
                    });
                    return;
                }
            }
            SupportedLanguage::JavaScript
            | SupportedLanguage::TypeScript
            | SupportedLanguage::Tsx => {
                if node.kind() == "if_statement" {
                    if let Some(cond) = node.child_by_field_name("condition") {
                        let cond_text = collect_node_lines_text(&cond, lines);
                        if cond_text.contains("import.meta.vitest") {
                            let s = node.start_position().row + 1;
                            let e = (node.end_position().row + 1).min(max_line);
                            let mut names = Vec::new();
                            collect_inner_test_names(lang, node, lines, source, &mut names);
                            out.push(InlineTestBlock {
                                start_line: s,
                                end_line: e,
                                scope_label: Some("import.meta.vitest".to_string()),
                                test_names: names,
                            });
                            return;
                        }
                    }
                }
                if node.kind() == "expression_statement" {
                    if let Some(label) = js_ts_test_call_label(&node, lines) {
                        let s = node.start_position().row + 1;
                        let e = (node.end_position().row + 1).min(max_line);
                        let is_suite = label.starts_with("describe")
                            || label.starts_with("suite")
                            || label.starts_with("context");
                        let mut names = Vec::new();
                        if is_suite {
                            collect_inner_test_names(lang, node, lines, source, &mut names);
                            out.push(InlineTestBlock {
                                start_line: s,
                                end_line: e,
                                scope_label: Some(label),
                                test_names: names,
                            });
                        } else {
                            out.push(InlineTestBlock {
                                start_line: s,
                                end_line: e,
                                scope_label: None,
                                test_names: vec![label],
                            });
                        }
                        return;
                    }
                }
                // Do not descend into regular function bodies for JS/TS test detection
                if matches!(
                    node.kind(),
                    "function_declaration"
                        | "method_definition"
                        | "arrow_function"
                        | "function_expression"
                ) {
                    return;
                }
            }
            SupportedLanguage::Python => {
                if matches!(node.kind(), "class_definition" | "decorated_definition") {
                    if let Some(label) = is_explicit_python_test_class(&node, lines) {
                        let s = node.start_position().row + 1;
                        let e = (node.end_position().row + 1).min(max_line);
                        let mut names = Vec::new();
                        collect_inner_test_names(lang, node, lines, source, &mut names);
                        out.push(InlineTestBlock {
                            start_line: s,
                            end_line: e,
                            scope_label: Some(label),
                            test_names: names,
                        });
                        return;
                    }
                    if is_explicit_python_decorated_test(&node, lines) {
                        let s = node.start_position().row + 1;
                        let e = (node.end_position().row + 1).min(max_line);
                        let name = bare_def_name(lang, &node, lines, source)
                            .unwrap_or_else(|| "test".to_string());
                        out.push(InlineTestBlock {
                            start_line: s,
                            end_line: e,
                            scope_label: None,
                            test_names: vec![name],
                        });
                        return;
                    }
                }
                if node.kind() == "function_definition" {
                    return;
                }
            }
            SupportedLanguage::Java | SupportedLanguage::CSharp => {
                if node.kind() == "class_declaration" && has_ast_java_csharp_test_attr(&node, lines)
                {
                    let s = java_csharp_start_row_with_attrs(&node) + 1;
                    let e = (node.end_position().row + 1).min(max_line);
                    let cls_name = node
                        .child_by_field_name("name")
                        .and_then(|n| get_node_text_same_line(&n, lines))
                        .map(|n| format!("class {n}"));
                    let mut names = Vec::new();
                    collect_inner_test_names(lang, node, lines, source, &mut names);
                    out.push(InlineTestBlock {
                        start_line: s,
                        end_line: e,
                        scope_label: cls_name,
                        test_names: names,
                    });
                    return;
                }
                if node.kind() == "method_declaration" {
                    if has_ast_java_csharp_test_attr(&node, lines) {
                        let s = java_csharp_start_row_with_attrs(&node) + 1;
                        let e = (node.end_position().row + 1).min(max_line);
                        let name = bare_def_name(lang, &node, lines, source)
                            .unwrap_or_else(|| "test".to_string());
                        out.push(InlineTestBlock {
                            start_line: s,
                            end_line: e,
                            scope_label: None,
                            test_names: vec![name],
                        });
                    }
                    return;
                }
            }
            SupportedLanguage::C | SupportedLanguage::Cpp => {
                if let Some(label) = is_explicit_c_cpp_test_macro(&node, lines) {
                    let s = node.start_position().row + 1;
                    let e = (node.end_position().row + 1).min(max_line);
                    out.push(InlineTestBlock {
                        start_line: s,
                        end_line: e,
                        scope_label: None,
                        test_names: vec![label],
                    });
                    return;
                }
                if node.kind() == "function_definition" {
                    return;
                }
            }
            SupportedLanguage::Ruby => {
                if let Some((label, is_explicit_rspec)) =
                    ruby_test_block_label(&node, lines, in_ruby_container)
                {
                    let s = node.start_position().row + 1;
                    let e = (node.end_position().row + 1).min(max_line);
                    let is_container = label.starts_with("class ")
                        || matches!(label.as_str(), "describe" | "context" | "feature");
                    if is_container {
                        let mut names = Vec::new();
                        collect_inner_test_names(lang, node, lines, source, &mut names);
                        // For bare top-level `context` / `feature` blocks without an `RSpec`
                        // receiver, require at least one inner `it`/`specify`/`scenario` block
                        // so top-level DSL blocks (e.g. Flipper feature definitions) are never folded.
                        if is_explicit_rspec
                            || !matches!(label.as_str(), "context" | "feature")
                            || !names.is_empty()
                        {
                            out.push(InlineTestBlock {
                                start_line: s,
                                end_line: e,
                                scope_label: Some(label),
                                test_names: names,
                            });
                            return;
                        }
                    } else {
                        let text = collect_node_lines_text(&node, lines);
                        let head = text.split(" do").next().unwrap_or(&label).trim();
                        out.push(InlineTestBlock {
                            start_line: s,
                            end_line: e,
                            scope_label: None,
                            test_names: vec![signature::truncate_preview(head, 40)],
                        });
                        return;
                    }
                }
                // Do not descend into regular Ruby method bodies (prevents matching DSL calls like `feature`/`context`)
                if matches!(node.kind(), "method" | "singleton_method") {
                    return;
                }
            }
            SupportedLanguage::Go => {
                if is_explicit_go_test_fn(&node, lines) {
                    let s = node.start_position().row + 1;
                    let e = (node.end_position().row + 1).min(max_line);
                    let name = bare_def_name(lang, &node, lines, source)
                        .unwrap_or_else(|| "Test".to_string());
                    out.push(InlineTestBlock {
                        start_line: s,
                        end_line: e,
                        scope_label: None,
                        test_names: vec![name],
                    });
                    return;
                }
                if node.kind() == "function_declaration" {
                    return;
                }
            }
            SupportedLanguage::Bash | SupportedLanguage::Json => return,
        }

        let next_in_ruby_container = in_ruby_container
            || (lang == SupportedLanguage::Ruby && matches!(node.kind(), "class" | "module"));
        let mut cursor = node.walk();
        for ch in node.named_children(&mut cursor) {
            walk(
                lang,
                ch,
                lines,
                source,
                max_line,
                next_in_ruby_container,
                out,
            );
        }
    }

    walk(
        lang,
        *root,
        &lines,
        source,
        max_line,
        false,
        &mut raw_blocks,
    );
    if raw_blocks.is_empty() {
        return Vec::new();
    }

    raw_blocks.sort_by_key(|b| (b.start_line, std::cmp::Reverse(b.end_line)));

    // Drop nested duplicates and merge contiguous standalone test blocks separated only by whitespace/comments
    let mut merged: Vec<InlineTestBlock> = Vec::new();
    for blk in raw_blocks {
        if let Some(last) = merged.last_mut() {
            if blk.end_line <= last.end_line {
                continue;
            }
            let both_standalone = last.scope_label.is_none() && blk.scope_label.is_none();
            if both_standalone
                && only_blank_or_comment_between(lang, &lines, last.end_line, blk.start_line)
            {
                last.end_line = blk.end_line;
                for name in blk.test_names {
                    if !last.test_names.contains(&name) {
                        last.test_names.push(name);
                    }
                }
                continue;
            }
        }
        merged.push(blk);
    }

    merged
}

/// Formats a compact, bounded `[omitted inline tests: ...]` marker string (without trailing newline).
pub(super) fn format_inline_test_marker(
    start_line: usize,
    end_line: usize,
    scope_label: Option<&str>,
    test_names: &[String],
    explicit_test_count: Option<usize>,
    churn: Option<(usize, usize)>,
) -> String {
    let mut meta_parts = Vec::new();
    if let Some(scope) = scope_label {
        meta_parts.push(scope.to_string());
    }
    let test_count = explicit_test_count.unwrap_or(test_names.len());
    if test_count > 0 {
        let noun = if test_count == 1 { "test" } else { "tests" };
        meta_parts.push(format!("{test_count} {noun}"));
    } else {
        let span = end_line.saturating_sub(start_line) + 1;
        meta_parts.push(format!("{span} lines"));
    }
    if let Some((added, deleted)) = churn {
        meta_parts.push(format!("+{added}/-{deleted} lines"));
    }

    let prefix = format!(
        "[omitted inline tests: L{start_line}-L{end_line} ({})",
        meta_parts.join("; ")
    );
    if test_names.is_empty() {
        return format!("{prefix}]");
    }

    let mut out = format!("{prefix}: ");
    for (idx, name) in test_names.iter().enumerate() {
        let sep = if idx == 0 { "" } else { ", " };
        let remaining_after = test_names.len().saturating_sub(idx + 1);
        let reserve = if remaining_after > 0 { 16 } else { 1 };
        if idx > 0 && out.len() + sep.len() + name.len() + reserve > MAX_INLINE_TEST_MARKER_BYTES {
            out.push_str(&format!(", +{} more", test_names.len() - idx));
            break;
        }
        out.push_str(sep);
        out.push_str(name);
    }
    out.push(']');
    out
}

/// Parses `N` from a `+[moved verbatim from ... (+N lines)]` summary line.
fn parse_moved_line_span(line: &str) -> Option<usize> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if !trimmed.starts_with("+[moved verbatim ") {
        return None;
    }
    let before_suffix = trimmed.strip_suffix(" lines)]")?;
    let (_, num_str) = before_suffix.rsplit_once(" (+")?;
    num_str.parse::<usize>().ok().filter(|&n| n > 0)
}

/// Folded chunk text paired with its per-line original 1-based source line map.
pub(super) type FoldedChunkResult = (String, super::SourceLineMap);
/// Progressive skeletonization output: `(rendered, line_map, folded_count, total_foldable)`.
pub(super) type ProgressiveSkeletonResult = (String, super::SourceLineMap, usize, usize);

/// Folds inline test blocks inside a newly added production file chunk, preserving exact original
/// 1-based source line spans `(start_line, end_line)` in the returned `orig_line_map`.
pub(super) fn fold_inline_tests_in_new_chunk(
    raw_chunk: &str,
    rendered: &str,
    orig_line_map: Option<&[Option<super::SourceLineSpan>]>,
    path: &Path,
) -> Option<FoldedChunkResult> {
    let _ = SupportedLanguage::candidates_for_path(path)?;
    let (_hdr, added_source, added_count) = extract_chunk_header_and_added_source(raw_chunk);
    if added_count < MIN_INLINE_TEST_FOLD_LINES || added_source.len() > MAX_TREESITTER_FILE_BYTES {
        return None;
    }
    let (lang, tree) = language_registry::parse_for_path(path, &added_source)?;
    let blocks: Vec<InlineTestBlock> =
        find_inline_test_blocks(lang, &tree.root_node(), &added_source)
            .into_iter()
            .filter(|b| b.end_line.saturating_sub(b.start_line) + 1 >= MIN_INLINE_TEST_FOLD_LINES)
            .collect();
    if blocks.is_empty() {
        return None;
    }

    let rendered_lines: Vec<&str> = rendered.split_inclusive('\n').collect();
    let base_map: Vec<Option<(usize, usize)>> = match orig_line_map {
        Some(m) if m.len() == rendered_lines.len() => m.to_vec(),
        _ => compute_diff_line_numbers(&rendered_lines),
    };

    let mut out = String::with_capacity(rendered.len());
    let mut new_map: Vec<Option<(usize, usize)>> = Vec::with_capacity(rendered_lines.len());
    let mut folded_any = false;
    let mut in_hunk_body = false;
    let mut last_seen_src_ln = 0usize;
    let mut i = 0usize;

    while i < rendered_lines.len() {
        if !in_hunk_body {
            if rendered_lines[i].starts_with("@@ ") {
                in_hunk_body = true;
                last_seen_src_ln = 0;
            }
            out.push_str(rendered_lines[i]);
            new_map.push(base_map[i]);
            i += 1;
            continue;
        }

        let moved_start_span = if base_map[i].is_none() {
            parse_moved_line_span(rendered_lines[i])
        } else {
            None
        };
        let matched_block = if let Some((src_start, src_end)) = base_map[i] {
            blocks
                .iter()
                .find(|b| src_start >= b.start_line && src_end <= b.end_line)
                .map(|b| (b, src_end))
        } else if let Some(span) = moved_start_span {
            let moved_start = last_seen_src_ln.saturating_add(1);
            let moved_end = last_seen_src_ln.saturating_add(span);
            blocks
                .iter()
                .find(|b| moved_start >= b.start_line && moved_end <= b.end_line)
                .map(|b| (b, moved_end))
        } else {
            None
        };

        if let Some((blk, initial_consumed_ln)) = matched_block {
            let mut j = i + 1;
            let mut orig_bytes = rendered_lines[i].len();
            let mut last_consumed_ln = initial_consumed_ln;
            while j < rendered_lines.len() {
                if let Some((_s, end_ln)) = base_map[j] {
                    if end_ln <= blk.end_line {
                        orig_bytes += rendered_lines[j].len();
                        last_consumed_ln = end_ln;
                        j += 1;
                        continue;
                    }
                    break;
                }
                // Absorb synthetic `None` lines (e.g. `+[moved verbatim ...]`) that sit inside
                // `blk.start_line..=blk.end_line` so the block is never split into duplicate markers,
                // advancing `last_consumed_ln` by the moved span so a moved production definition
                // immediately following the test block is never swallowed.
                let next_some = base_map[j + 1..].iter().copied().flatten().next();
                let moved_span = parse_moved_line_span(rendered_lines[j]);
                let inside_by_lookahead = next_some.is_some_and(|(s, _)| s <= blk.end_line);
                let moved_tail_inside =
                    moved_span.is_some_and(|n| last_consumed_ln.saturating_add(n) <= blk.end_line);
                if inside_by_lookahead || moved_tail_inside {
                    orig_bytes += rendered_lines[j].len();
                    if let Some(n) = moved_span {
                        last_consumed_ln = last_consumed_ln.saturating_add(n);
                    }
                    j += 1;
                    continue;
                }
                break;
            }
            let marker = format!(
                "+{}\n",
                format_inline_test_marker(
                    blk.start_line,
                    blk.end_line,
                    blk.scope_label.as_deref(),
                    &blk.test_names,
                    None,
                    None,
                )
            );
            if marker.len() < orig_bytes {
                out.push_str(&marker);
                new_map.push(None);
                folded_any = true;
                last_seen_src_ln = last_consumed_ln;
                i = j;
                continue;
            }
        }

        if let Some((_s, end_ln)) = base_map[i] {
            last_seen_src_ln = end_ln;
        } else if let Some(span) = moved_start_span {
            last_seen_src_ln = last_seen_src_ln.saturating_add(span);
        }
        out.push_str(rendered_lines[i]);
        new_map.push(base_map[i]);
        i += 1;
    }

    if folded_any && out.len() < rendered.len() {
        Some((out, new_map))
    } else {
        None
    }
}

/// Parses an optional trailing ` (+N more)` or ` [+ a, b, ...]` count from a merged hunk signature,
/// strips a redundant leading `<scope> > ` / `<scope> › ` prefix when it already matches `scope_label`,
/// and returns `None` when the signature is just the enclosing `scope_label` itself (e.g. `use`
/// edits inside `mod tests` outside any test function).
fn normalize_modified_inline_sig(
    sig: &str,
    scope_label: Option<&str>,
) -> (Option<String>, Option<usize>) {
    let mut trimmed = sig.trim();
    if let Some(scope) = scope_label {
        let prefix_gt = format!("{scope} > ");
        let prefix_guil = format!("{scope} › ");
        if let Some(rest) = trimmed
            .strip_prefix(&prefix_gt)
            .or_else(|| trimmed.strip_prefix(&prefix_guil))
        {
            trimmed = rest.trim();
        } else {
            let head = trimmed.trim_end_matches(['{', ':']).trim();
            let head_no_pub = head.strip_prefix("pub ").map(str::trim).unwrap_or(head);
            if head == scope || head_no_pub == scope {
                return (None, None);
            }
        }
    }
    if trimmed.is_empty() {
        return (None, None);
    }
    let extra_count = trimmed
        .strip_suffix(" more)")
        .and_then(|before| before.rsplit_once(" (+"))
        .and_then(|(_, num)| num.parse::<usize>().ok())
        .or_else(|| {
            let inner = trimmed
                .strip_suffix(']')
                .and_then(|before| before.rsplit_once(" [+ "))
                .map(|(_, list)| list)?;
            let mut count = 0usize;
            for item in inner.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                if let Some(n) = item
                    .strip_prefix('+')
                    .and_then(|r| r.strip_suffix(" more"))
                    .and_then(|n| n.parse::<usize>().ok())
                {
                    count += n;
                } else {
                    count += 1;
                }
            }
            (count > 0).then_some(count)
        });
    (
        Some(trimmed.to_string()),
        extra_count.map(|extra| 1 + extra),
    )
}

/// Folds hunks inside a modified production file chunk that lie completely within detected inline
/// test blocks, replacing each inline-test hunk with a standalone `[omitted inline tests: ...]` line.
pub(super) fn fold_inline_tests_in_modified_chunk(
    rendered: &str,
    inline_blocks: &[InlineTestBlock],
) -> Option<String> {
    if inline_blocks.is_empty() {
        return None;
    }

    let lines: Vec<&str> = rendered.split_inclusive('\n').collect();
    let mut out = String::with_capacity(rendered.len());
    let mut folded_any = false;
    let mut i = 0usize;

    while i < lines.len() {
        let trimmed = lines[i].trim_end_matches(['\r', '\n']);
        if let Some((_os, _oc, new_start, new_count, sig)) = parse_hunk_header(trimmed) {
            let hunk_start_idx = i;
            i += 1;
            let mut added = 0usize;
            let mut deleted = 0usize;
            while i < lines.len() {
                let l = lines[i];
                if l.starts_with("@@ ") || l.starts_with("[omitted inline tests:") {
                    break;
                }
                if l.starts_with('+') {
                    added += 1;
                } else if l.starts_with('-') {
                    deleted += 1;
                }
                i += 1;
            }
            let hunk_end_line = if new_count == 0 {
                new_start
            } else {
                new_start + new_count.saturating_sub(1)
            };

            // For pure deletions (`new_count == 0`), `new_start` is the line immediately preceding
            // the deletion point; require strict `< b.end_line` so deleting production code right
            // after an inline test block's closing line is never misclassified as inline tests.
            let matching_block = inline_blocks.iter().find(|b| {
                if new_count == 0 {
                    new_start >= b.start_line && new_start < b.end_line
                } else {
                    new_start >= b.start_line && hunk_end_line <= b.end_line
                }
            });

            if let Some(blk) = matching_block {
                let hunk_bytes: usize = lines[hunk_start_idx..i].iter().map(|l| l.len()).sum();
                let clean_sig = strip_clamped_suffix(&sig);
                let (names, explicit_count) = if !clean_sig.is_empty() {
                    let (norm_sig, count) =
                        normalize_modified_inline_sig(&clean_sig, blk.scope_label.as_deref());
                    match norm_sig {
                        Some(s) => (vec![s], count),
                        None => (Vec::new(), None),
                    }
                } else {
                    (blk.test_names.clone(), None)
                };
                let marker = format!(
                    "{}\n",
                    format_inline_test_marker(
                        new_start,
                        hunk_end_line.max(new_start),
                        blk.scope_label.as_deref(),
                        &names,
                        explicit_count,
                        Some((added, deleted)),
                    )
                );
                if marker.len() < hunk_bytes {
                    out.push_str(&marker);
                    folded_any = true;
                    continue;
                }
            }

            for l in &lines[hunk_start_idx..i] {
                out.push_str(l);
            }
        } else {
            out.push_str(lines[i]);
            i += 1;
        }
    }

    if folded_any && out.len() < rendered.len() {
        Some(out)
    } else {
        None
    }
}

#[derive(Clone, Debug)]
struct FoldableBody {
    /// 1-based inclusive original source line range to replace.
    replace_start_line: usize,
    replace_end_line: usize,
    /// Replacement diff lines (already `+`-prefixed, ending with `\n`) paired with the
    /// 1-based original `(start_line, end_line)` span each emitted line represents.
    replacement_lines: Vec<(String, Option<(usize, usize)>)>,
    /// Approximate byte savings when folding this body.
    savings_bytes: usize,
}

/// Finds the leading ASCII whitespace indentation of the first non-empty line in
/// `start_row..=end_row`, falling back to `default_indent` when all lines in range are blank.
fn first_non_empty_indent<'a>(
    orig_lines: &[&'a str],
    start_row: usize,
    end_row: usize,
    default_indent: &'a str,
) -> &'a str {
    let last = end_row.min(orig_lines.len().saturating_sub(1));
    for line in orig_lines.iter().take(last + 1).skip(start_row) {
        if !line.trim().is_empty() {
            let indent_len = line.len() - line.trim_start_matches([' ', '\t']).len();
            return &line[..indent_len];
        }
    }
    default_indent
}

pub(super) fn is_foldable_function_node(lang: SupportedLanguage, node: &tree_sitter::Node) -> bool {
    let kind = node.kind();
    match lang {
        SupportedLanguage::Rust => matches!(kind, "function_item" | "macro_definition"),
        SupportedLanguage::Python => kind == "function_definition",
        SupportedLanguage::JavaScript | SupportedLanguage::TypeScript | SupportedLanguage::Tsx => {
            matches!(kind, "function_declaration" | "method_definition")
                || (matches!(
                    kind,
                    "lexical_declaration"
                        | "variable_declaration"
                        | "pair"
                        | "assignment_expression"
                        | "field_definition"
                        | "public_field_definition"
                        | "property_definition"
                ) && check_primary_definition(lang, node).is_some())
        }
        SupportedLanguage::Go => matches!(kind, "function_declaration" | "method_declaration"),
        SupportedLanguage::C | SupportedLanguage::Cpp => kind == "function_definition",
        SupportedLanguage::Java => matches!(kind, "method_declaration" | "constructor_declaration"),
        SupportedLanguage::CSharp => matches!(
            kind,
            "method_declaration" | "constructor_declaration" | "destructor_declaration"
        ),
        SupportedLanguage::Ruby => matches!(kind, "method" | "singleton_method"),
        SupportedLanguage::Bash => kind == "function_definition",
        SupportedLanguage::Json => false,
    }
}

fn build_foldable_body(
    lang: SupportedLanguage,
    node: &tree_sitter::Node,
    orig_lines: &[&str],
    orig_source: &str,
) -> Option<FoldableBody> {
    if lang == SupportedLanguage::Rust && node.kind() == "macro_definition" {
        let mut open_pos = None;
        let mut close_pos = None;
        let mut cursor = node.walk();
        for ch in node.children(&mut cursor) {
            if ch.kind() == "{" && open_pos.is_none() {
                open_pos = Some(ch.start_position());
            } else if ch.kind() == "}" {
                close_pos = Some(ch.end_position());
            }
        }
        let (sp, ep) = (open_pos?, close_pos?);
        return build_brace_fold(
            sp.row, sp.column, ep.row, ep.column, orig_lines, /* is_bash = */ false,
        );
    }

    let body = signature::find_ast_body_node(lang, node)?;
    let b_start_row = body.start_position().row;
    let b_end_row = body
        .end_position()
        .row
        .min(orig_lines.len().saturating_sub(1));
    if b_end_row <= b_start_row {
        return None;
    }

    match lang {
        SupportedLanguage::Python => {
            // Preserve short leading docstring if present
            let mut fold_start_row = b_start_row;
            if let Some(first_stmt) = body.named_child(0) {
                if first_stmt.kind() == "expression_statement"
                    && first_stmt.named_child_count() == 1
                    && first_stmt
                        .named_child(0)
                        .is_some_and(|c| c.kind() == "string")
                {
                    let doc_lines = first_stmt
                        .end_position()
                        .row
                        .saturating_sub(first_stmt.start_position().row)
                        + 1;
                    if doc_lines <= MAX_PRESERVED_PY_DOCSTRING_LINES {
                        fold_start_row = first_stmt.end_position().row + 1;
                    }
                }
            }
            if b_end_row < fold_start_row {
                return None;
            }
            let span = b_end_row - fold_start_row + 1;
            if span < MIN_SKELETON_BODY_LINES {
                return None;
            }
            let indent = first_non_empty_indent(orig_lines, fold_start_row, b_end_row, "    ");
            let s_line = fold_start_row + 1;
            let e_line = b_end_row + 1;
            let repl = format!("+{indent}...  # ... {span} lines (L{s_line}-L{e_line})\n");
            let orig_bytes: usize = orig_lines[fold_start_row..=b_end_row]
                .iter()
                .map(|l| l.len() + 2)
                .sum();
            let savings = orig_bytes.saturating_sub(repl.len());
            if savings == 0 {
                return None;
            }
            Some(FoldableBody {
                replace_start_line: s_line,
                replace_end_line: e_line,
                replacement_lines: vec![(repl, Some((s_line, e_line)))],
                savings_bytes: savings,
            })
        }
        SupportedLanguage::Ruby => {
            let def_start_row = node.start_position().row;
            let def_end_row = node.end_position().row;
            if b_start_row <= def_start_row || b_end_row >= def_end_row {
                return None;
            }
            let span = b_end_row - b_start_row + 1;
            if span < MIN_SKELETON_BODY_LINES {
                return None;
            }
            let indent = first_non_empty_indent(orig_lines, b_start_row, b_end_row, "  ");
            let s_line = b_start_row + 1;
            let e_line = b_end_row + 1;
            let repl = format!("+{indent}# ... {span} lines (L{s_line}-L{e_line})\n");
            let orig_bytes: usize = orig_lines[b_start_row..=b_end_row]
                .iter()
                .map(|l| l.len() + 2)
                .sum();
            let savings = orig_bytes.saturating_sub(repl.len());
            if savings == 0 {
                return None;
            }
            Some(FoldableBody {
                replace_start_line: s_line,
                replace_end_line: e_line,
                replacement_lines: vec![(repl, Some((s_line, e_line)))],
                savings_bytes: savings,
            })
        }
        SupportedLanguage::Bash => {
            if orig_source.as_bytes().get(body.start_byte()) != Some(&b'{')
                || orig_source
                    .as_bytes()
                    .get(body.end_byte().saturating_sub(1))
                    != Some(&b'}')
            {
                return None;
            }
            build_brace_fold(
                body.start_position().row,
                body.start_position().column,
                body.end_position().row,
                body.end_position().column,
                orig_lines,
                /* is_bash = */ true,
            )
        }
        _ => {
            // Brace-delimited languages: Rust, JS/TS/TSX, Go, C, C++, Java, C#
            if orig_source.as_bytes().get(body.start_byte()) != Some(&b'{')
                || orig_source
                    .as_bytes()
                    .get(body.end_byte().saturating_sub(1))
                    != Some(&b'}')
            {
                return None;
            }
            build_brace_fold(
                body.start_position().row,
                body.start_position().column,
                body.end_position().row,
                body.end_position().column,
                orig_lines,
                /* is_bash = */ false,
            )
        }
    }
}

fn build_brace_fold(
    s_row: usize,
    s_col: usize,
    e_row: usize,
    e_col: usize,
    orig_lines: &[&str],
    is_bash: bool,
) -> Option<FoldableBody> {
    if e_row <= s_row || e_row >= orig_lines.len() {
        return None;
    }
    let span = e_row - s_row + 1;
    if span < MIN_SKELETON_BODY_LINES {
        return None;
    }
    let start_line_str = orig_lines[s_row];
    let end_line_str = orig_lines[e_row];
    if s_col > start_line_str.len()
        || !start_line_str.is_char_boundary(s_col)
        || e_col > end_line_str.len()
        || !end_line_str.is_char_boundary(e_col)
    {
        return None;
    }
    let suffix = end_line_str[e_col..].trim_end();
    // Skip if another statement shares the closing brace line (e.g. `} fn next() {`)
    if !suffix
        .trim()
        .chars()
        .all(|c| matches!(c, ';' | ',' | ')' | ']'))
    {
        return None;
    }

    let s_line = s_row + 1;
    let e_line = e_row + 1;
    let orig_bytes: usize = orig_lines[s_row..=e_row].iter().map(|l| l.len() + 2).sum();

    if is_bash {
        // Keep opening `{` line and closing `}` line with an indented `# ...` line between
        if e_row <= s_row + 1 {
            return None;
        }
        let inner_start = s_row + 1;
        let inner_end = e_row - 1;
        let inner_span = inner_end - inner_start + 1;
        let indent = first_non_empty_indent(orig_lines, inner_start, inner_end, "  ");
        let open_line = format!("+{start_line_str}\n");
        let mid_line = format!(
            "+{indent}# ... {inner_span} lines (L{}-L{})\n",
            inner_start + 1,
            inner_end + 1
        );
        let close_line = format!("+{end_line_str}\n");
        let repl_bytes = open_line.len() + mid_line.len() + close_line.len();
        let savings = orig_bytes.saturating_sub(repl_bytes);
        if savings == 0 {
            return None;
        }
        return Some(FoldableBody {
            replace_start_line: s_line,
            replace_end_line: e_line,
            replacement_lines: vec![
                (open_line, Some((s_line, s_line))),
                (mid_line, Some((inner_start + 1, inner_end + 1))),
                (close_line, Some((e_line, e_line))),
            ],
            savings_bytes: savings,
        });
    }

    let prefix = &start_line_str[..s_col];
    let repl = format!("+{prefix}{{ /* ... {span} lines (L{s_line}-L{e_line}) */ }}{suffix}\n");
    let savings = orig_bytes.saturating_sub(repl.len());
    if savings == 0 {
        return None;
    }
    Some(FoldableBody {
        replace_start_line: s_line,
        replace_end_line: e_line,
        replacement_lines: vec![(repl, Some((s_line, e_line)))],
        savings_bytes: savings,
    })
}

/// Progressively skeletonizes an oversized newly added production code file by folding its longest
/// function/method bodies first until `rendered.len() <= target_budget` (or all foldable bodies are
/// folded), keeping all type definitions, `impl`/class headers, doc comments, and function signatures
/// intact with exact original line-number annotations.
pub(super) fn skeletonize_new_chunk_progressive(
    raw_chunk: &str,
    rendered: &str,
    orig_line_map: Option<&[Option<super::SourceLineSpan>]>,
    path: &Path,
    target_budget: usize,
) -> Option<ProgressiveSkeletonResult> {
    let candidates = SupportedLanguage::candidates_for_path(path)?;
    if matches!(
        candidates,
        super::LanguageCandidates::Exact(SupportedLanguage::Json)
    ) {
        return None;
    }
    let (_hdr, orig_source, added_count) = extract_chunk_header_and_added_source(raw_chunk);
    if added_count < MIN_SKELETON_BODY_LINES || orig_source.len() > MAX_TREESITTER_FILE_BYTES {
        return None;
    }
    let (lang, tree) = language_registry::parse_for_path(path, &orig_source)?;
    if lang == SupportedLanguage::Json {
        return None;
    }
    let orig_lines: Vec<&str> = orig_source.lines().collect();
    if orig_lines.is_empty() {
        return None;
    }

    let rendered_lines: Vec<&str> = rendered.split_inclusive('\n').collect();
    let base_map: Vec<Option<(usize, usize)>> = match orig_line_map {
        Some(m) if m.len() == rendered_lines.len() => m.to_vec(),
        _ => compute_diff_line_numbers(&rendered_lines),
    };

    // Build quick lookup from intact single original 1-based line -> index in `rendered_lines`
    let mut line_to_idx: Vec<Option<usize>> = vec![None; orig_lines.len() + 2];
    for (idx, opt_span) in base_map.iter().enumerate() {
        if let Some((s, e)) = *opt_span {
            if s == e && s < line_to_idx.len() && line_to_idx[s].is_none() {
                line_to_idx[s] = Some(idx);
            }
        }
    }

    let mut candidates: Vec<FoldableBody> = Vec::new();
    fn collect_foldables(
        lang: SupportedLanguage,
        node: tree_sitter::Node,
        orig_lines: &[&str],
        orig_source: &str,
        out: &mut Vec<FoldableBody>,
    ) {
        if is_foldable_function_node(lang, &node) {
            if let Some(fb) = build_foldable_body(lang, &node, orig_lines, orig_source) {
                out.push(fb);
            }
            // Never recurse inside a function/method body (prevents overlapping nested folds)
            return;
        }
        let mut cursor = node.walk();
        for ch in node.named_children(&mut cursor) {
            collect_foldables(lang, ch, orig_lines, orig_source, out);
        }
    }

    collect_foldables(
        lang,
        tree.root_node(),
        &orig_lines,
        &orig_source,
        &mut candidates,
    );

    // Filter to candidates whose entire original line range is still intact & contiguous in `rendered`
    let mut eligible: Vec<(FoldableBody, usize, usize)> = Vec::new();
    for fb in candidates {
        let Some(start_idx) = line_to_idx.get(fb.replace_start_line).copied().flatten() else {
            continue;
        };
        let Some(end_idx) = line_to_idx.get(fb.replace_end_line).copied().flatten() else {
            continue;
        };
        if end_idx < start_idx {
            continue;
        }
        // Verify all lines in [start_idx..=end_idx] map intact inside [replace_start_line..=replace_end_line]
        let contiguous = (start_idx..=end_idx).all(|idx| {
            base_map[idx].is_some_and(|(s, e)| {
                s == e && s >= fb.replace_start_line && e <= fb.replace_end_line
            })
        });
        if !contiguous {
            continue;
        }
        let actual_orig_bytes: usize = rendered_lines[start_idx..=end_idx]
            .iter()
            .map(|l| l.len())
            .sum();
        let repl_bytes: usize = fb.replacement_lines.iter().map(|(l, _)| l.len()).sum();
        if actual_orig_bytes > repl_bytes {
            let mut adjusted = fb;
            adjusted.savings_bytes = actual_orig_bytes - repl_bytes;
            eligible.push((adjusted, start_idx, end_idx));
        }
    }

    if eligible.is_empty() {
        return None;
    }

    let total_foldable = eligible.len();
    // Sort by byte savings descending (longest bodies folded first; small helpers preserved!)
    let mut order: Vec<usize> = (0..eligible.len()).collect();
    order.sort_by(|&a, &b| {
        eligible[b]
            .0
            .savings_bytes
            .cmp(&eligible[a].0.savings_bytes)
    });

    let marker_overhead = 48usize; // `[skeletonized: XX/YY function bodies folded]\n`
    let mut projected_len = rendered.len() + marker_overhead;
    let mut selected_indices: Vec<usize> = Vec::new();
    for idx in order {
        if projected_len <= target_budget && !selected_indices.is_empty() {
            break;
        }
        projected_len = projected_len.saturating_sub(eligible[idx].0.savings_bytes);
        selected_indices.push(idx);
    }

    if selected_indices.is_empty() {
        return None;
    }

    // Sort selected folds in ascending source order to emit in a single linear pass
    selected_indices.sort_by_key(|&idx| eligible[idx].1);
    let folded_count = selected_indices.len();

    let mut body_out = String::with_capacity(projected_len + 64);
    let mut body_map: Vec<Option<(usize, usize)>> = Vec::with_capacity(rendered_lines.len());
    let mut cursor_idx = 0usize;
    let mut sel_pos = 0usize;

    while cursor_idx < rendered_lines.len() {
        if sel_pos < selected_indices.len() {
            let (ref fb, s_idx, e_idx) = eligible[selected_indices[sel_pos]];
            if cursor_idx == s_idx {
                for (line_text, mapped_ln) in &fb.replacement_lines {
                    body_out.push_str(line_text);
                    body_map.push(*mapped_ln);
                }
                cursor_idx = e_idx + 1;
                sel_pos += 1;
                continue;
            }
        }
        body_out.push_str(rendered_lines[cursor_idx]);
        body_map.push(base_map[cursor_idx]);
        cursor_idx += 1;
    }

    let marker = format!("[skeletonized: {folded_count}/{total_foldable} function bodies folded]");
    // Insert file-level `[skeletonized: ...]` marker right after file header, keeping `body_map` aligned!
    let mut final_out = String::with_capacity(body_out.len() + marker.len() + 2);
    let mut final_map = Vec::with_capacity(body_map.len() + 1);
    let mut inserted = false;
    for (raw_line, mapped_ln) in body_out.split_inclusive('\n').zip(body_map) {
        if !inserted
            && (raw_line.starts_with("@@ ")
                || raw_line.starts_with("[omitted inline tests:")
                || raw_line.starts_with("[degraded to "))
        {
            final_out.push_str(&marker);
            final_out.push('\n');
            final_map.push(None);
            inserted = true;
        }
        final_out.push_str(raw_line);
        final_map.push(mapped_ln);
    }
    if !inserted {
        final_out.push_str(&marker);
        final_out.push('\n');
        final_map.push(None);
    }

    if final_out.len() < rendered.len() {
        Some((final_out, final_map, folded_count, total_foldable))
    } else {
        None
    }
}

/// Extracts a budget-bounded AST symbol TOC (`Vec<String>` lines of `  - L{s}-L{e}: {sig}`)
/// for a budget-omitted chunk (supports new files, deleted files, and modified files).
pub(super) fn build_budget_omitted_toc(
    raw_chunk: &str,
    rendered: &str,
    path: &Path,
    is_new: bool,
    is_deleted: bool,
    max_toc_bytes: usize,
) -> Option<(Vec<String>, usize)> {
    if max_toc_bytes < 48 {
        return None;
    }

    // 1. New or deleted files: parse full added (`+`) or deleted (`-`) source via tree-sitter
    if is_new || is_deleted {
        let _ = SupportedLanguage::candidates_for_path(path)?;
        let mut source = String::new();
        let mut in_hunk = false;
        let prefix_ch = if is_deleted { '-' } else { '+' };
        for raw_line in raw_chunk.split_inclusive('\n') {
            if !in_hunk {
                if raw_line.starts_with("@@") {
                    in_hunk = true;
                }
                continue;
            }
            if raw_line.starts_with("@@") {
                continue;
            }
            if let Some(rest) = raw_line.strip_prefix(prefix_ch) {
                source.push_str(rest);
            }
        }
        if source.is_empty() || source.len() > MAX_TREESITTER_FILE_BYTES {
            return None;
        }
        let (lang, tree) = language_registry::parse_for_path(path, &source)?;
        let lines: Vec<&str> = source.lines().collect();
        if lines.is_empty() {
            return None;
        }
        let mut collected: Vec<(usize, usize, String)> = Vec::new();
        fn walk_toc(
            lang: SupportedLanguage,
            node: tree_sitter::Node,
            lines: &[&str],
            source: &str,
            out: &mut Vec<(usize, usize, String)>,
        ) {
            if let Some(matched) = check_primary_definition(lang, &node) {
                let s = matched.start_position().row + 1;
                let e = (matched.end_position().row + 1).min(lines.len().max(1));
                let is_dup = out.last().is_some_and(|p| p.0 == s && p.1 == e);
                if !is_dup {
                    let raw_sig = signature::extract_signature(lang, &matched, lines, source);
                    let parent =
                        signature::extract_parent_scope_prefix(lang, &matched, lines, source);
                    let sig = signature::compact_definition_signature_with_ast(
                        lang,
                        &matched,
                        parent.as_deref(),
                        raw_sig.as_deref(),
                        source,
                        96,
                    )
                    .or_else(|| bare_def_name(lang, &matched, lines, source));
                    if let Some(sig_text) = sig {
                        out.push((s, e, sig_text));
                        if !is_container_definition_node(&matched) {
                            return;
                        }
                    }
                }
            }
            let mut cursor = node.walk();
            for ch in node.named_children(&mut cursor) {
                walk_toc(lang, ch, lines, source, out);
            }
        }
        walk_toc(lang, tree.root_node(), &lines, &source, &mut collected);
        if collected.is_empty() {
            return None;
        }
        return pack_toc_entries(&collected, max_toc_bytes);
    }

    // 2. Modified files: extract modified hunk line ranges and AST signatures from `rendered`
    let mut entries: Vec<(usize, usize, String)> = Vec::new();
    for raw_line in rendered.split_inclusive('\n') {
        let trimmed = raw_line.trim_end_matches(['\r', '\n']);
        if let Some((_os, _oc, new_start, new_count, sig)) = parse_hunk_header(trimmed) {
            let clean = strip_clamped_suffix(&sig);
            if !clean.is_empty() {
                let end_line = if new_count == 0 {
                    new_start
                } else {
                    new_start + new_count.saturating_sub(1)
                };
                if !entries.iter().any(|(_, _, s)| s == &clean) {
                    entries.push((new_start, end_line, clean));
                }
            }
        }
    }
    if entries.is_empty() {
        return None;
    }
    pack_toc_entries(&entries, max_toc_bytes)
}

fn pack_toc_entries(
    entries: &[(usize, usize, String)],
    max_toc_bytes: usize,
) -> Option<(Vec<String>, usize)> {
    let total = entries.len();
    let mut out = Vec::new();
    let mut used = 0usize;
    let footer_reserve = 32usize;
    for (idx, (s, e, sig)) in entries.iter().enumerate() {
        let line = if *s == 0 {
            format!("  - {sig}")
        } else {
            format!("  - L{s}-L{e}: {sig}")
        };
        let needed = line.len() + 1;
        let has_more = idx + 1 < total;
        if !out.is_empty()
            && used + needed + if has_more { footer_reserve } else { 0 } > max_toc_bytes
        {
            break;
        }
        if out.is_empty() && needed > max_toc_bytes {
            return None;
        }
        used += needed;
        out.push(line);
    }
    if out.is_empty() {
        None
    } else {
        Some((out, total))
    }
}
