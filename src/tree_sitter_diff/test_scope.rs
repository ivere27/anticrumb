pub(super) use super::is_test_file_path;
use super::{get_node_text_same_line, is_module_level_container, SupportedLanguage};
use std::path::Path;

/// Iterates over individual `#[...]` / `#![...]` attribute slices in `s`, respecting string quotes and nested brackets.
/// Returns `true` as soon as `predicate` returns `true` for a whitespace-stripped attribute slice.
pub(super) fn any_rust_attr_slice(s: &str, mut predicate: impl FnMut(&str) -> bool) -> bool {
    let mut search_idx = 0;
    while search_idx < s.len() {
        let rem = &s[search_idx..];
        let p1 = rem.find("#[").map(|p| (p, 2));
        let p2 = rem.find("#![").map(|p| (p, 3));
        let offset = match (p1, p2) {
            (Some(a), Some(b)) => {
                if a.0 <= b.0 {
                    Some(a)
                } else {
                    Some(b)
                }
            }
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };

        let (found_pos, prefix_len) = match offset {
            Some(o) => o,
            None => break,
        };

        let attr_start = search_idx + found_pos;
        let after_prefix = &s[attr_start + prefix_len..];

        let mut depth: usize = 1;
        let mut in_quote = false;
        let mut escaped = false;
        let mut attr_end = None;

        for (idx, ch) in after_prefix.char_indices() {
            if in_quote {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_quote = false;
                }
                continue;
            }
            match ch {
                '"' => in_quote = true,
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        attr_end = Some(attr_start + prefix_len + idx + 1);
                        break;
                    }
                }
                _ => {}
            }
        }

        let attr_slice_end = attr_end.unwrap_or(s.len());
        let attr_slice = &s[attr_start..attr_slice_end];
        let clean: String = attr_slice.chars().filter(|c| !c.is_whitespace()).collect();
        if predicate(&clean) {
            return true;
        }

        search_idx = attr_slice_end;
    }
    false
}

/// Evaluates whether a whitespace-stripped `#[cfg(<expr>)]` body positively references `test`
/// (i.e. `test`, `all(..., test, ...)`, `any(..., test, ...)`) while ignoring `not(...)` subtrees
/// and string literals.
fn cfg_expr_has_positive_test(expr: &str) -> bool {
    let mut p_depth: usize = 0;
    let mut in_quote = false;
    let mut escaped = false;
    let mut start = 0;

    let check_term = |term: &str| -> bool {
        if term == "test" {
            return true;
        }
        if term.starts_with("not(") {
            return false;
        }
        if let Some(inner) = term.strip_prefix("all(").and_then(|t| t.strip_suffix(')')) {
            return cfg_expr_has_positive_test(inner);
        }
        if let Some(inner) = term.strip_prefix("any(").and_then(|t| t.strip_suffix(')')) {
            return cfg_expr_has_positive_test(inner);
        }
        false
    };

    for (idx, ch) in expr.char_indices() {
        if in_quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_quote = false;
            }
            continue;
        }
        match ch {
            '"' => in_quote = true,
            '(' => p_depth += 1,
            ')' => p_depth = p_depth.saturating_sub(1),
            ',' if p_depth == 0 => {
                if check_term(&expr[start..idx]) {
                    return true;
                }
                start = idx + 1;
            }
            _ => {}
        }
    }

    start < expr.len() && check_term(&expr[start..])
}

/// Matches #[cfg(test)], #[cfg(all(..., test, ...))], #[cfg(any(..., test, ...))]
/// while rejecting #[cfg(not(test))], #[cfg(not(all(test, ...)))], #[cfg(feature = "attestation")],
/// and preserving #[cfg(all(not(test_util), test))].
pub(super) fn is_cfg_test_attribute(s: &str) -> bool {
    any_rust_attr_slice(s, |clean| {
        let Some(rest) = clean
            .strip_prefix("#[cfg(")
            .or_else(|| clean.strip_prefix("#![cfg("))
        else {
            return false;
        };
        let Some(inner) = rest.strip_suffix(")]") else {
            return false;
        };
        cfg_expr_has_positive_test(inner)
    })
}

/// Helper to check if line(s) contain Rust test attributes like `#[test]`, `#[tokio::test]`,
/// `#[sqlx::test]`, `#[test_log::test]`, `#[rstest]`, `#[test_case]`, `#[wasm_bindgen_test]`,
/// or `#[cfg(test)]`.
pub(super) fn is_rust_test_attr(s: &str) -> bool {
    any_rust_attr_slice(s, |clean| {
        let Some(body) = clean
            .strip_prefix("#[")
            .or_else(|| clean.strip_prefix("#!["))
        else {
            return false;
        };
        let path_len = body
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != ':')
            .unwrap_or(body.len());
        if path_len == 0 {
            return false;
        }
        let attr_path = &body[..path_len];
        let after = &body[path_len..];
        if !(after.starts_with(']') || after.starts_with('(') || after.starts_with(',')) {
            return false;
        }
        attr_path == "test"
            || attr_path == "rstest"
            || attr_path == "test_case"
            || attr_path == "wasm_bindgen_test"
            || attr_path.ends_with("::test")
            || attr_path.ends_with("::rstest")
            || attr_path.ends_with("::test_case")
            || attr_path.ends_with("::wasm_bindgen_test")
    }) || is_cfg_test_attribute(s)
}

fn is_valid_java_test_short_name(short: &str) -> bool {
    matches!(
        short,
        "Test" | "ParameterizedTest" | "RepeatedTest" | "TestFactory" | "TestTemplate"
    )
}

fn is_valid_csharp_test_short_name(short: &str) -> bool {
    matches!(
        short,
        "Test"
            | "TestCase"
            | "TestCaseSource"
            | "TestMethod"
            | "TestFixture"
            | "Fact"
            | "Theory"
            | "DataTestMethod"
    )
}

fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut p_depth: usize = 0;
    let mut in_quote = false;
    let mut escaped = false;
    let mut start = 0;

    for (idx, ch) in s.char_indices() {
        if in_quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_quote = false;
            }
            continue;
        }
        match ch {
            '"' => in_quote = true,
            '(' => p_depth += 1,
            ')' => p_depth = p_depth.saturating_sub(1),
            ',' if p_depth == 0 => {
                parts.push(&s[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    if start <= s.len() {
        parts.push(&s[start..]);
    }
    parts
}

/// Checks whether `line` contains a Java/C# test annotation/attribute such as `@Test`,
/// `@org.junit.jupiter.api.Test`, `@DisplayName("...") @Test`, `[Fact]`, `[Xunit.Fact]`,
/// `[Trait("Category", "Unit")] [Fact]`, `[TestCase(...)]`, etc., respecting word boundaries
/// so `@TestHelper`, `@Testing`, and `[Factory]` are rejected.
pub(super) fn is_test_annotation_or_attr(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }

    let bytes = trimmed.as_bytes();
    let mut i = 0;
    let mut in_quote = false;
    let mut escaped = false;
    let mut p_depth: usize = 0;

    while i < bytes.len() {
        let ch = trimmed[i..].chars().next().unwrap();
        let ch_len = ch.len_utf8();

        if in_quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_quote = false;
            }
            i += ch_len;
            continue;
        }

        if ch == '"' {
            in_quote = true;
            i += ch_len;
            continue;
        }
        if ch == '(' {
            p_depth += 1;
            i += ch_len;
            continue;
        }
        if ch == ')' {
            p_depth = p_depth.saturating_sub(1);
            i += ch_len;
            continue;
        }

        if p_depth == 0 {
            let prev_ok = i == 0
                || trimmed[..i]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_whitespace() || c == ']' || c == ')');

            if ch == '@' && prev_ok {
                let after_at = &trimmed[i + 1..];
                let path_len = after_at
                    .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '.' && c != '$')
                    .unwrap_or(after_at.len());
                if path_len > 0 {
                    let path = &after_at[..path_len];
                    let short = path.rsplit('.').next().unwrap_or(path);
                    let rest = &after_at[path_len..];
                    let next_ok = rest.is_empty()
                        || rest.starts_with('(')
                        || rest.starts_with('@')
                        || rest.starts_with(',')
                        || rest.starts_with(char::is_whitespace);
                    if next_ok && is_valid_java_test_short_name(short) {
                        return true;
                    }
                    i += 1 + path_len;
                    continue;
                }
            } else if ch == '[' && prev_ok {
                // Find matching ']' on this line (or EOL for multi-line attribute)
                let after_open = &trimmed[i + 1..];
                let mut b_in_quote = false;
                let mut b_escaped = false;
                let mut b_p_depth: usize = 0;
                let mut close_rel = None;
                for (rel_idx, bch) in after_open.char_indices() {
                    if b_in_quote {
                        if b_escaped {
                            b_escaped = false;
                        } else if bch == '\\' {
                            b_escaped = true;
                        } else if bch == '"' {
                            b_in_quote = false;
                        }
                        continue;
                    }
                    match bch {
                        '"' => b_in_quote = true,
                        '(' => b_p_depth += 1,
                        ')' => b_p_depth = b_p_depth.saturating_sub(1),
                        ']' if b_p_depth == 0 => {
                            close_rel = Some(rel_idx);
                            break;
                        }
                        _ => {}
                    }
                }
                let inner = match close_rel {
                    Some(end_idx) => &after_open[..end_idx],
                    None => after_open,
                };
                for elem in split_top_level_commas(inner) {
                    let elem_trimmed = elem.trim();
                    let path_len = elem_trimmed
                        .find(|c: char| {
                            !c.is_ascii_alphanumeric() && c != '_' && c != '.' && c != ':'
                        })
                        .unwrap_or(elem_trimmed.len());
                    if path_len > 0 {
                        let path = &elem_trimmed[..path_len];
                        let short = path.rsplit(['.', ':']).next().unwrap_or(path);
                        let rest = elem_trimmed[path_len..].trim_start();
                        let next_ok = rest.is_empty() || rest.starts_with('(');
                        if next_ok && is_valid_csharp_test_short_name(short) {
                            return true;
                        }
                    }
                }
                i += 1 + close_rel.map_or(after_open.len(), |r| r + 1);
                continue;
            }
        }

        i += ch_len;
    }

    false
}

/// Collects and joins non-comment lines in the attribute block immediately preceding `start_row`,
/// optionally also including the prefix of `start_row` before `{`.
fn collect_rust_attr_block(
    lines: &[&str],
    start_row: usize,
    max_lookback: usize,
    include_start_row_before_brace: bool,
) -> String {
    let check_start = start_row.saturating_sub(max_lookback);
    let mut attr_start = start_row;
    for row in (check_start..start_row).rev() {
        if let Some(line) = lines.get(row) {
            let trimmed = line.trim();
            if trimmed.is_empty()
                || trimmed.starts_with("//")
                || trimmed.starts_with("/*")
                || trimmed.starts_with('*')
            {
                continue;
            }
            if trimmed.contains('}')
                || trimmed.contains('{')
                || (trimmed.ends_with(';')
                    && !trimmed.starts_with("#[")
                    && !trimmed.starts_with("#!["))
            {
                break;
            }
            attr_start = row;
        }
    }

    let mut non_comment_lines: Vec<&str> = lines[attr_start..start_row]
        .iter()
        .map(|l| l.trim())
        .filter(|l| {
            !l.is_empty() && !l.starts_with("//") && !l.starts_with("/*") && !l.starts_with('*')
        })
        .collect();

    if include_start_row_before_brace {
        if let Some(start_line) = lines.get(start_row) {
            let before_brace = start_line.split('{').next().unwrap_or(start_line).trim();
            if !before_brace.is_empty()
                && !before_brace.starts_with("//")
                && !before_brace.starts_with("/*")
                && !before_brace.starts_with('*')
            {
                non_comment_lines.push(before_brace);
            }
        }
    }

    non_comment_lines.join(" ")
}

/// Walks nested C/C++ declarators (or macro call / header line fallback for Catch2 string-arg macros)
/// to extract the innermost function or test macro identifier name.
pub(super) fn extract_c_cpp_function_name<'a>(
    node: &tree_sitter::Node<'a>,
    lines: &[&'a str],
) -> Option<&'a str> {
    let mut curr = *node;
    if curr.kind() == "template_declaration" {
        let mut cursor = curr.walk();
        for ch in curr.named_children(&mut cursor) {
            if ch.kind() == "function_definition" {
                curr = ch;
                break;
            }
        }
    }
    if let Some(mut decl) = curr.child_by_field_name("declarator") {
        for _ in 0..8 {
            match decl.kind() {
                "identifier" | "field_identifier" => {
                    if let Some(id) = get_node_text_same_line(&decl, lines) {
                        return Some(id);
                    }
                    break;
                }
                "qualified_identifier" => {
                    if let Some(name) = decl.child_by_field_name("name") {
                        if let Some(id) = get_node_text_same_line(&name, lines) {
                            return Some(id);
                        }
                    }
                    break;
                }
                "call_expression" => {
                    if let Some(func) = decl.child_by_field_name("function") {
                        if let Some(id) = get_node_text_same_line(&func, lines) {
                            return Some(id);
                        }
                    }
                    break;
                }
                _ => {
                    if let Some(next) = decl.child_by_field_name("declarator") {
                        decl = next;
                    } else {
                        break;
                    }
                }
            }
        }
    }

    // Fallback for macros with string literal arguments (e.g. Catch2 `TEST_CASE("desc", "[tag]")`
    // or `SCENARIO("...")`) where tree-sitter-cpp may not build a standard function_declarator.
    let start_row = curr.start_position().row;
    for r in [start_row, start_row.saturating_sub(1)] {
        if let Some(line) = lines.get(r) {
            let trimmed = line.trim_start();
            if let Some(open_idx) = trimmed.find('(') {
                let ident = trimmed[..open_idx].trim_end();
                if !ident.is_empty() && ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    return Some(ident);
                }
            }
        }
    }
    None
}

/// Extracts trimmed text for an AST node from 0-indexed `lines`, skipping comment lines/nodes.
pub(super) fn collect_node_lines_text(node: &tree_sitter::Node, lines: &[&str]) -> String {
    let sp = node.start_position();
    let ep = node.end_position();
    if sp.row >= lines.len() {
        return String::new();
    }
    let end_row = ep.row.min(lines.len() - 1);
    if sp.row == end_row {
        let line = lines[sp.row];
        let s = sp.column.min(line.len());
        let e = ep.column.min(line.len());
        if s <= e && line.is_char_boundary(s) && line.is_char_boundary(e) {
            return line[s..e].trim().to_string();
        }
        return line.trim().to_string();
    }

    let mut parts = Vec::with_capacity(end_row - sp.row + 1);
    for (r, &line) in lines.iter().enumerate().take(end_row + 1).skip(sp.row) {
        let slice = if r == sp.row {
            let s = sp.column.min(line.len());
            if line.is_char_boundary(s) {
                &line[s..]
            } else {
                line
            }
        } else if r == end_row {
            let e = ep.column.min(line.len());
            if line.is_char_boundary(e) {
                &line[..e]
            } else {
                line
            }
        } else {
            line
        };
        let tr = slice.trim();
        if tr.is_empty() || tr.starts_with("//") || tr.starts_with("/*") || tr.starts_with('*') {
            continue;
        }
        parts.push(tr);
    }
    parts.join(" ")
}

/// Checks AST outer `attribute_item` siblings preceding `node` and inner `inner_attribute_item`
/// children inside `node`'s body (for `mod_item` / `function_item`).
pub(super) fn has_ast_rust_attr(
    node: &tree_sitter::Node,
    lines: &[&str],
    mut predicate: impl FnMut(&str) -> bool,
) -> bool {
    let mut prev = node.prev_named_sibling();
    while let Some(sib) = prev {
        match sib.kind() {
            "attribute_item" => {
                let text = collect_node_lines_text(&sib, lines);
                if !text.is_empty() && predicate(&text) {
                    return true;
                }
                prev = sib.prev_named_sibling();
            }
            "line_comment" | "block_comment" => {
                prev = sib.prev_named_sibling();
            }
            _ => break,
        }
    }

    let mut cursor = node.walk();
    for ch in node.named_children(&mut cursor) {
        if matches!(ch.kind(), "attribute_item" | "inner_attribute_item") {
            let text = collect_node_lines_text(&ch, lines);
            if !text.is_empty() && predicate(&text) {
                return true;
            }
        }
    }

    if let Some(body) = node.child_by_field_name("body") {
        let mut body_cursor = body.walk();
        for ch in body.named_children(&mut body_cursor) {
            match ch.kind() {
                "inner_attribute_item" => {
                    let text = collect_node_lines_text(&ch, lines);
                    if !text.is_empty() && predicate(&text) {
                        return true;
                    }
                }
                "line_comment" | "block_comment" => {}
                _ => break,
            }
        }
    }

    false
}

/// Checks AST annotations (`modifiers` -> `marker_annotation`/`annotation` in Java)
/// and attributes (`attribute_list` in C#) attached to `node`.
pub(super) fn has_ast_java_csharp_test_attr(node: &tree_sitter::Node, lines: &[&str]) -> bool {
    let mut cursor = node.walk();
    for ch in node.named_children(&mut cursor) {
        match ch.kind() {
            "modifiers" => {
                let mut mod_cursor = ch.walk();
                for mch in ch.named_children(&mut mod_cursor) {
                    if matches!(mch.kind(), "marker_annotation" | "annotation") {
                        let text = collect_node_lines_text(&mch, lines);
                        if !text.is_empty() && is_test_annotation_or_attr(&text) {
                            return true;
                        }
                    }
                }
            }
            "attribute_list" | "marker_annotation" | "annotation" => {
                let text = collect_node_lines_text(&ch, lines);
                if !text.is_empty() && is_test_annotation_or_attr(&text) {
                    return true;
                }
            }
            _ => {}
        }
    }

    let mut prev = node.prev_named_sibling();
    while let Some(sib) = prev {
        match sib.kind() {
            "attribute_list" | "marker_annotation" | "annotation" => {
                let text = collect_node_lines_text(&sib, lines);
                if !text.is_empty() && is_test_annotation_or_attr(&text) {
                    return true;
                }
                prev = sib.prev_named_sibling();
            }
            k if k.contains("comment") => {
                prev = sib.prev_named_sibling();
            }
            _ => break,
        }
    }

    false
}

/// Determines whether a definition AST node is a test function or test module.
pub(super) fn is_test_scope(
    lang: SupportedLanguage,
    node: &tree_sitter::Node,
    path: Option<&Path>,
    lines: &[&str],
) -> bool {
    if let Some(p) = path {
        if is_test_file_path(p) {
            return true;
        }
    }

    match lang {
        SupportedLanguage::Rust => {
            // Check enclosing test modules and functions via AST attribute siblings first
            let mut curr = Some(*node);
            while let Some(ancestor) = curr {
                match ancestor.kind() {
                    "mod_item" => {
                        if let Some(name_node) = ancestor.child_by_field_name("name") {
                            if let Some(name) = get_node_text_same_line(&name_node, lines) {
                                if name == "tests" || name == "test" || name.starts_with("test_") {
                                    return true;
                                }
                            }
                        }
                        if has_ast_rust_attr(&ancestor, lines, is_cfg_test_attribute) {
                            return true;
                        }
                        if ancestor.has_error() || ancestor.parent().is_some_and(|p| p.has_error())
                        {
                            let mod_start = ancestor.start_position().row;
                            let combined_mod_pre =
                                collect_rust_attr_block(lines, mod_start, 15, true);
                            if !combined_mod_pre.is_empty()
                                && is_cfg_test_attribute(&combined_mod_pre)
                            {
                                return true;
                            }
                        }
                    }
                    "function_item" => {
                        if has_ast_rust_attr(&ancestor, lines, is_rust_test_attr) {
                            return true;
                        }
                        if let Some(name_node) = ancestor.child_by_field_name("name") {
                            if let Some(name) = get_node_text_same_line(&name_node, lines) {
                                if name.starts_with("test_") {
                                    return true;
                                }
                            }
                        }
                    }
                    _ => {}
                }
                curr = ancestor.parent();
            }

            if has_ast_rust_attr(node, lines, is_rust_test_attr) {
                return true;
            }

            // Fallback to line scanning only when AST contains syntax/recovery errors
            if node.has_error() || node.parent().is_some_and(|p| p.has_error()) {
                let start_row = node.start_position().row;
                let combined_pre = collect_rust_attr_block(lines, start_row, 15, false);
                if !combined_pre.is_empty() && is_rust_test_attr(&combined_pre) {
                    return true;
                }
                let own_end = (start_row + 5).min(lines.len());
                for l in &lines[start_row..own_end] {
                    let trimmed = l.trim();
                    if trimmed.starts_with("//")
                        || trimmed.starts_with("/*")
                        || trimmed.starts_with('*')
                    {
                        continue;
                    }
                    if is_rust_test_attr(trimmed) {
                        return true;
                    }
                    if trimmed.contains('{') {
                        break;
                    }
                }
            }

            if let Some(name_node) = node.child_by_field_name("name") {
                if let Some(name) = get_node_text_same_line(&name_node, lines) {
                    if name.starts_with("test_") {
                        return true;
                    }
                }
            }
        }
        SupportedLanguage::Python => {
            let mut curr = Some(*node);
            while let Some(ancestor) = curr {
                if ancestor.kind() == "class_definition" {
                    if let Some(name_node) = ancestor.child_by_field_name("name") {
                        if let Some(name) = get_node_text_same_line(&name_node, lines) {
                            if name.starts_with("Test")
                                || name.ends_with("Test")
                                || name.ends_with("TestCase")
                            {
                                return true;
                            }
                        }
                    }
                }
                curr = ancestor.parent();
            }
            if let Some(name_node) = node.child_by_field_name("name") {
                if let Some(name) = get_node_text_same_line(&name_node, lines) {
                    if name.starts_with("test_") {
                        return true;
                    }
                }
            }
        }
        SupportedLanguage::Go => {
            // In Go, tests ONLY run in `*_test.go` files (handled above).
            // Only apply `Test*` name heuristic when no file path was provided (e.g. snippet checks).
            if path.is_none() {
                if let Some(name_node) = node.child_by_field_name("name") {
                    if let Some(name) = get_node_text_same_line(&name_node, lines) {
                        if name.starts_with("Test")
                            || name.starts_with("Benchmark")
                            || name.starts_with("Fuzz")
                        {
                            return true;
                        }
                    }
                }
            }
        }
        SupportedLanguage::C | SupportedLanguage::Cpp => {
            let mut curr = Some(*node);
            while let Some(anc) = curr {
                if let Some(fn_name) = extract_c_cpp_function_name(&anc, lines) {
                    if matches!(
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
                        return true;
                    }
                }
                curr = anc.parent();
            }
        }
        SupportedLanguage::Ruby => {
            let mut curr = Some(*node);
            while let Some(ancestor) = curr {
                match ancestor.kind() {
                    "call" => {
                        if let Some(method_node) = ancestor.child_by_field_name("method") {
                            if let Some(method_name) = get_node_text_same_line(&method_node, lines)
                            {
                                if matches!(
                                    method_name,
                                    "describe"
                                        | "context"
                                        | "it"
                                        | "specify"
                                        | "feature"
                                        | "scenario"
                                ) {
                                    return true;
                                }
                            }
                        }
                    }
                    "class" => {
                        if let Some(name_node) = ancestor.child_by_field_name("name") {
                            if let Some(name) = get_node_text_same_line(&name_node, lines) {
                                if name.ends_with("Test")
                                    || name.ends_with("TestCase")
                                    || name.ends_with("Spec")
                                {
                                    return true;
                                }
                            }
                        }
                    }
                    _ => {}
                }
                curr = ancestor.parent();
            }
            if let Some(name_node) = node.child_by_field_name("name") {
                if let Some(name) = get_node_text_same_line(&name_node, lines) {
                    if name.starts_with("test_") {
                        return true;
                    }
                }
            }
        }
        SupportedLanguage::Java | SupportedLanguage::CSharp => {
            let mut curr = Some(*node);
            while let Some(ancestor) = curr {
                match ancestor.kind() {
                    "class_declaration" | "class_definition" => {
                        if let Some(name_node) = ancestor.child_by_field_name("name") {
                            if let Some(name) = get_node_text_same_line(&name_node, lines) {
                                if name.starts_with("Test")
                                    || name.ends_with("Test")
                                    || name.ends_with("Tests")
                                    || name.ends_with("TestCase")
                                {
                                    return true;
                                }
                            }
                        }
                        if has_ast_java_csharp_test_attr(&ancestor, lines) {
                            return true;
                        }
                    }
                    "method_declaration"
                    | "constructor_declaration"
                    | "local_function_statement"
                        if has_ast_java_csharp_test_attr(&ancestor, lines) =>
                    {
                        return true;
                    }
                    _ => {}
                }
                curr = ancestor.parent();
            }

            if has_ast_java_csharp_test_attr(node, lines) {
                return true;
            }

            // Fallback to line scanning only when AST contains syntax/recovery errors
            if node.has_error() || node.parent().is_some_and(|p| p.has_error()) {
                let start_row = node.start_position().row;

                // 1. Check lines within the node's own header (start_row..own_end)
                let own_end = (start_row + 8).min(lines.len());
                for l in &lines[start_row..own_end] {
                    let trimmed = l.trim();
                    if trimmed.starts_with("//")
                        || trimmed.starts_with("/*")
                        || trimmed.starts_with('*')
                    {
                        continue;
                    }
                    if is_test_annotation_or_attr(trimmed) {
                        return true;
                    }
                    if trimmed.contains('{')
                        || (trimmed.ends_with(';')
                            && !trimmed.starts_with('@')
                            && !trimmed.starts_with('['))
                    {
                        break;
                    }
                }

                // 2. Reverse scan immediately preceding start_row, stopping at previous item boundary
                let check_start = start_row.saturating_sub(8);
                for row in (check_start..start_row).rev() {
                    if let Some(line) = lines.get(row) {
                        let trimmed = line.trim();
                        if trimmed.is_empty()
                            || trimmed.starts_with("//")
                            || trimmed.starts_with("/*")
                            || trimmed.starts_with('*')
                        {
                            continue;
                        }
                        if trimmed.contains('}') || trimmed.contains('{') || trimmed.ends_with(';')
                        {
                            break;
                        }
                        if is_test_annotation_or_attr(trimmed) {
                            return true;
                        }
                    }
                }
            }
        }
        SupportedLanguage::Dart => {
            let mut curr = Some(*node);
            while let Some(ancestor) = curr {
                match ancestor.kind() {
                    "function_declaration"
                    | "method_declaration"
                    | "member_declaration"
                    | "class_declaration" => {
                        let mut ac = ancestor.walk();
                        for ch in ancestor.named_children(&mut ac) {
                            if ch.kind() == "annotation" {
                                if let Some(name) = ch
                                    .child_by_field_name("name")
                                    .and_then(|n| get_node_text_same_line(&n, lines))
                                {
                                    let short =
                                        name.trim().rsplit('.').next().unwrap_or(name.trim());
                                    if matches!(short, "isTest" | "isTestGroup") {
                                        return true;
                                    }
                                }
                            }
                        }
                    }
                    "call_expression" => {
                        if let Some(fn_node) = ancestor.child_by_field_name("function") {
                            if let Some(fn_name) = get_node_text_same_line(&fn_node, lines) {
                                if matches!(fn_name.trim(), "test" | "testWidgets" | "group") {
                                    return true;
                                }
                            }
                        }
                    }
                    _ => {}
                }
                curr = ancestor.parent();
            }
        }
        _ => {}
    }

    false
}

/// Helper to determine if a string is a valid C# attribute like `[Fact]`, `[HttpGet("/api")]`,
/// `[Fact(Skip = "reason")]`, or stacked `[Trait(...)] [Fact]`, while rejecting array literals
/// like `[1, 2, 3]` and assignments `[a = 1]`.
pub(super) fn is_csharp_attribute(s: &str) -> bool {
    let trimmed = s.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') || trimmed.len() < 3 {
        return false;
    }

    // Support multiple adjacent [Attr1] [Attr2(...)] attributes on one line
    let mut idx = 0;
    let bytes = trimmed.as_bytes();
    while idx < bytes.len() {
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx >= bytes.len() {
            break;
        }
        if bytes[idx] != b'[' {
            return false;
        }
        let after = &trimmed[idx + 1..];
        let Some(first) = after.trim_start().chars().next() else {
            return false;
        };
        if !first.is_ascii_alphabetic() && first != '_' {
            return false;
        }

        let mut p_depth: usize = 0;
        let mut in_quote = false;
        let mut escaped = false;
        let mut closed_at = None;
        for (rel, ch) in after.char_indices() {
            if in_quote {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_quote = false;
                }
                continue;
            }
            match ch {
                '"' => in_quote = true,
                '(' => p_depth += 1,
                ')' => {
                    if p_depth == 0 {
                        return false;
                    }
                    p_depth -= 1;
                }
                '=' if p_depth == 0 => return false,
                ';' => return false,
                ']' if p_depth == 0 => {
                    closed_at = Some(rel);
                    break;
                }
                _ => {}
            }
        }
        let Some(close_rel) = closed_at else {
            return false;
        };
        if close_rel == 0 || p_depth != 0 || in_quote {
            return false;
        }
        idx += 1 + close_rel + 1;
    }
    true
}

fn is_single_line_at_annotations(s: &str) -> bool {
    let mut rem = s.trim();
    if !rem.starts_with('@') {
        return false;
    }
    while !rem.is_empty() {
        if !rem.starts_with('@') {
            return false;
        }
        let after_at = &rem[1..];
        let Some(first) = after_at.chars().next() else {
            return false;
        };
        if !first.is_ascii_alphabetic() && first != '_' {
            return false;
        }
        let name_len = after_at
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '.' && c != '$')
            .unwrap_or(after_at.len());
        let after_name = &after_at[name_len..];
        if let Some(after_ws) = after_name.strip_prefix('(') {
            let mut p_depth: usize = 1;
            let mut in_quote = false;
            let mut escaped = false;
            let mut end_rel = None;
            for (rel, ch) in after_ws.char_indices() {
                if in_quote {
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == '"' {
                        in_quote = false;
                    }
                    continue;
                }
                match ch {
                    '"' => in_quote = true,
                    '(' => p_depth += 1,
                    ')' => {
                        p_depth -= 1;
                        if p_depth == 0 {
                            end_rel = Some(rel);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(end) = end_rel {
                rem = after_ws[end + 1..].trim_start();
            } else {
                // Multi-line decorator opening line
                return true;
            }
        } else if after_name.is_empty() || after_name.starts_with(char::is_whitespace) {
            rem = after_name.trim_start();
        } else {
            return false;
        }
    }
    true
}

/// Determines if all non-empty lines in `lines[start..=end]` (1-indexed) are attributes, decorators, or comments.
pub(super) fn is_attribute_or_comment_only(
    lines: &[&str],
    start: usize,
    end: usize,
    lang: SupportedLanguage,
) -> bool {
    let s = start.saturating_sub(1);
    let e = end.min(lines.len());
    if s >= e {
        return false;
    }
    let is_python_like = matches!(
        lang,
        SupportedLanguage::Python | SupportedLanguage::Bash | SupportedLanguage::Ruby
    );
    let is_ruby = lang == SupportedLanguage::Ruby;
    let is_cs = lang == SupportedLanguage::CSharp;
    let supports_at_decorators = matches!(
        lang,
        SupportedLanguage::Python
            | SupportedLanguage::Java
            | SupportedLanguage::JavaScript
            | SupportedLanguage::TypeScript
            | SupportedLanguage::Tsx
            | SupportedLanguage::Dart
    );

    let mut has_non_empty = false;
    for l in &lines[s..e] {
        let tr = l.trim();
        if tr.is_empty() {
            continue;
        }
        has_non_empty = true;
        let is_attr_or_comment = tr.starts_with("//")
            || tr.starts_with("/*")
            || tr == "*"
            || tr.starts_with("*/")
            || tr.starts_with("* ")
            || tr.starts_with("*\t")
            || tr.starts_with("#[")
            || tr.starts_with("#![")
            || (is_python_like && tr.starts_with('#'))
            || (is_ruby && (tr.starts_with("=begin") || tr.starts_with("=end")))
            || (supports_at_decorators && is_single_line_at_annotations(tr))
            || (is_cs && is_csharp_attribute(tr));
        if !is_attr_or_comment {
            return false;
        }
    }
    has_non_empty
}

/// Checks whether the AST node at 1-indexed `line_1idx` lies inside an attribute, decorator,
/// annotation, or comment node (handling multi-line attributes/comments/docstrings without breaking
/// lookahead prematurely), while rejecting normal statements like `use ...;`, `const ...;`, or `mod ...`.
pub(super) fn is_ast_node_attr_or_comment(
    lang: SupportedLanguage,
    root: &tree_sitter::Node,
    lines: &[&str],
    line_1idx: usize,
) -> bool {
    let row = line_1idx.saturating_sub(1);
    let Some(raw_line) = lines.get(row) else {
        return false;
    };
    let line = raw_line.trim_end_matches(['\r', '\n']);
    if line.trim().is_empty() {
        return false;
    }
    let col = line.len() - line.trim_start().len();
    let pt = tree_sitter::Point { row, column: col };
    let mut curr = root
        .named_descendant_for_point_range(pt, pt)
        .or_else(|| root.descendant_for_point_range(pt, pt));

    while let Some(node) = curr {
        let kind = node.kind();
        if node.id() == root.id() || is_module_level_container(lang, kind) {
            break;
        }
        if kind.contains("attribute")
            || kind.contains("comment")
            || kind.contains("decorator")
            || kind.contains("annotation")
        {
            return true;
        }
        if lang == SupportedLanguage::Python
            && kind == "expression_statement"
            && node.named_child_count() == 1
            && node.named_child(0).is_some_and(|c| c.kind() == "string")
        {
            return true;
        }
        curr = node.parent();
    }
    false
}
