use super::{get_node_text_same_line, SupportedLanguage};

/// Truncates a string safely at character boundaries to at most `max_chars`.
/// If truncated, keeps `max_chars - 3` chars and appends "...".
pub(super) fn truncate_preview(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count > max_chars {
        let prefix: String = s.chars().take(max_chars.saturating_sub(3)).collect();
        format!("{prefix}...")
    } else {
        s.to_string()
    }
}

/// Returns the byte length of a character literal like `'a'`, `'\n'`, `'\\'`, `'\''`, `'\x7f'`, `'\u{...}'`, or `'\u1234'`
/// if `s` begins with a valid character literal.
pub(super) fn char_literal_len(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'\'') || bytes.len() < 3 {
        return None;
    }
    if bytes[1] == b'\\' {
        // Escapes: '\n', '\t', '\\', '\'', '\0', etc. (4 bytes)
        if bytes.len() >= 4 && bytes[2] != b'x' && bytes[2] != b'u' && bytes[3] == b'\'' {
            return Some(4);
        }
        // '\x7f' (6 bytes): '\'' + '\\' + 'x' + hex + hex + '\''
        if bytes.len() >= 6
            && bytes[2] == b'x'
            && bytes[3].is_ascii_hexdigit()
            && bytes[4].is_ascii_hexdigit()
            && bytes[5] == b'\''
        {
            return Some(6);
        }
        // '\u{1f600}' (Rust/ES6: 1..=6 hex digits) or '\u1234' (JS/Py/Java/C#: 4 hex digits, 8 bytes)
        if bytes[2] == b'u' {
            if bytes.get(3) == Some(&b'{') {
                let mut i = 4;
                let max_hex_end = bytes.len().min(4 + 6);
                while i < max_hex_end && bytes[i].is_ascii_hexdigit() {
                    i += 1;
                }
                if i > 4 && bytes.get(i) == Some(&b'}') && bytes.get(i + 1) == Some(&b'\'') {
                    return Some(i + 2);
                }
            } else if bytes.len() >= 8
                && bytes[3..7].iter().all(|b| b.is_ascii_hexdigit())
                && bytes[7] == b'\''
            {
                return Some(8);
            }
        }
    } else {
        // Single char: 'c' or multi-byte UTF-8 character like '한'
        let rem = &s[1..];
        if let Some(ch) = rem.chars().next() {
            let ch_len = ch.len_utf8();
            if rem.as_bytes().get(ch_len) == Some(&b'\'') {
                return Some(1 + ch_len + 1);
            }
        }
    }
    None
}

/// Helper to determine if a `'` token in `s` represents a Rust lifetime (e.g. `'a`, `'_`, `'static`).
pub(super) fn is_rust_lifetime(s: &str, line_prefix: &str, in_angle_brackets: bool) -> bool {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'\'') || bytes.len() < 2 {
        return false;
    }
    let first = bytes[1];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    let mut end = 2;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    // If followed immediately by a single quote, it's a char literal (e.g. 'a'), not a lifetime!
    if end < bytes.len() && bytes[end] == b'\'' {
        return false;
    }

    if in_angle_brackets {
        return true;
    }

    let trimmed = line_prefix.trim_end();
    trimmed.ends_with('&')
        || trimmed.ends_with('+')
        || trimmed.ends_with(':')
        || trimmed.ends_with(',')
        || trimmed.ends_with('<')
        || trimmed.ends_with('(')
        || trimmed.ends_with("where")
        || trimmed.ends_with("impl")
        || trimmed.ends_with("dyn")
        || trimmed.ends_with("for")
}

fn ends_with_keyword_word_boundary(s: &str, kw: &str) -> bool {
    let trimmed = s.trim_end();
    trimmed == kw
        || trimmed.strip_suffix(kw).is_some_and(|pre| {
            pre.chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_')
        })
}

fn is_cpp_operator_lt(before: &str) -> bool {
    ends_with_keyword_word_boundary(before, "operator")
}

fn is_cpp_operator_gt(before: &str) -> bool {
    ends_with_keyword_word_boundary(before, "operator")
        || before
            .trim_end()
            .strip_suffix('>')
            .is_some_and(|b| ends_with_keyword_word_boundary(b, "operator"))
}

fn is_rust_qualified_path_open(after_double_lt: &str) -> bool {
    if after_double_lt.starts_with(|c: char| c.is_whitespace() || c.is_ascii_digit()) {
        return false;
    }
    let end = after_double_lt
        .find(['>', ';', '{'])
        .unwrap_or(after_double_lt.len());
    let segment = &after_double_lt[..end];
    if segment.contains(" as ") {
        return true;
    }
    after_double_lt[end..].starts_with(">::")
}

/// Checks if `b_idx` starts a Rust raw string (`r"..."`, `r#"..."#`, `br#"..."#`, `cr#"..."#`).
/// Returns `Some((prefix_byte_len, hash_count))`.
fn rust_raw_string_prefix(s: &str, b_idx: usize) -> Option<(usize, usize)> {
    let bytes = s.as_bytes();
    if bytes.get(b_idx) != Some(&b'r') {
        return None;
    }
    let valid_prefix = if b_idx == 0 {
        true
    } else {
        let prev = s[..b_idx].chars().next_back()?;
        if !prev.is_alphanumeric() && prev != '_' {
            true
        } else if (prev == 'b' || prev == 'c') && b_idx >= 1 {
            s[..b_idx - 1]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_')
        } else {
            false
        }
    };
    if !valid_prefix {
        return None;
    }
    let mut i = b_idx + 1;
    let mut hashes = 0usize;
    while i < bytes.len() && bytes[i] == b'#' {
        hashes += 1;
        i += 1;
    }
    if bytes.get(i) == Some(&b'"') {
        Some((i - b_idx + 1, hashes))
    } else {
        None
    }
}

/// Checks if `b_idx` starts a C# verbatim string (`@"..."`, `$@"..."`, `@$"..."`).
fn csharp_verbatim_string_prefix(s: &str, b_idx: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.get(b_idx) == Some(&b'@') {
        if bytes.get(b_idx + 1) == Some(&b'"') {
            return Some(2);
        }
        if bytes.get(b_idx + 1) == Some(&b'$') && bytes.get(b_idx + 2) == Some(&b'"') {
            return Some(3);
        }
    } else if bytes.get(b_idx) == Some(&b'$')
        && bytes.get(b_idx + 1) == Some(&b'@')
        && bytes.get(b_idx + 2) == Some(&b'"')
    {
        return Some(3);
    }
    None
}

pub(super) enum ScanAction {
    Continue,
    SkipBytes(usize),
    Break,
}

/// Shared lexical scanner that skips string/char/raw/verbatim-string literals, escape sequences,
/// block comments (`/* ... */`), C++ digit separators, and Rust lifetimes, and tracks angle-bracket depth.
pub(super) struct CodeScanner {
    pub(super) in_double_quote: bool,
    pub(super) in_verbatim_double_quote: bool,
    pub(super) in_single_quote: bool,
    pub(super) in_backtick_quote: bool,
    pub(super) raw_string_hashes: Option<usize>,
    pub(super) in_block_comment: bool,
    pub(super) escaped: bool,
    pub(super) a_depth: usize,
    pub(super) allow_rust_lifetime: bool,
    pub(super) allow_backtick_escapes: bool,
    pub(super) allow_slash_comments: bool,
    pub(super) allow_csharp_verbatim: bool,
}

impl Default for CodeScanner {
    fn default() -> Self {
        Self::new(false)
    }
}

impl CodeScanner {
    pub(super) fn new(allow_rust_lifetime: bool) -> Self {
        Self {
            in_double_quote: false,
            in_verbatim_double_quote: false,
            in_single_quote: false,
            in_backtick_quote: false,
            raw_string_hashes: None,
            in_block_comment: false,
            escaped: false,
            a_depth: 0,
            allow_rust_lifetime,
            allow_backtick_escapes: true,
            allow_slash_comments: true,
            allow_csharp_verbatim: !allow_rust_lifetime,
        }
    }

    pub(super) fn for_lang(lang: SupportedLanguage) -> Self {
        Self {
            allow_rust_lifetime: lang == SupportedLanguage::Rust,
            allow_backtick_escapes: lang != SupportedLanguage::Go,
            allow_slash_comments: !matches!(
                lang,
                SupportedLanguage::Python | SupportedLanguage::Bash | SupportedLanguage::Ruby
            ),
            allow_csharp_verbatim: lang == SupportedLanguage::CSharp,
            ..Self::new(lang == SupportedLanguage::Rust)
        }
    }

    pub(super) fn in_literal(&self) -> bool {
        self.in_double_quote
            || self.in_verbatim_double_quote
            || self.in_single_quote
            || self.in_backtick_quote
            || self.raw_string_hashes.is_some()
            || self.in_block_comment
    }

    /// Updates `a_depth` on `'<'` and `'>'`, skipping `<=`, `<=>`, `operator<...`,
    /// `->`, `=>`, non-generic `<<` shift operators, and spaced comparison `a < b` inside generics.
    /// Returns `None` if `ch` is neither `'<'` nor `'>'`.
    pub(super) fn try_update_angle_depth(
        &mut self,
        s: &str,
        b_idx: usize,
        ch: char,
        prev_ch: char,
    ) -> Option<usize> {
        match ch {
            '<' => {
                let rest = &s[b_idx..];
                if rest.starts_with("<=") || is_cpp_operator_lt(&s[..b_idx]) {
                    let skip = if rest.starts_with("<=>") {
                        3
                    } else if rest.starts_with("<=") || rest.starts_with("<<") {
                        2
                    } else {
                        0
                    };
                    return Some(skip);
                }
                if rest.starts_with("<<")
                    && !(self.allow_rust_lifetime && is_rust_qualified_path_open(&rest[2..]))
                {
                    return Some(2);
                }
                // Inside generic/template argument list (`a_depth > 0`), a spaced binary `<`
                // (whitespace on both sides, e.g. `template<bool B = 1 < 2>`) is a comparison, not a nested `<...>`.
                if self.a_depth > 0
                    && prev_ch.is_whitespace()
                    && rest[1..].chars().next().is_some_and(char::is_whitespace)
                {
                    return Some(0);
                }
                self.a_depth += 1;
                Some(0)
            }
            '>' => {
                if prev_ch != '-' && prev_ch != '=' && !is_cpp_operator_gt(&s[..b_idx]) {
                    self.a_depth = self.a_depth.saturating_sub(1);
                }
                Some(0)
            }
            _ => None,
        }
    }

    /// Iterates significant (non-literal, non-block-comment) characters of `s`,
    /// invoking `on_char(scanner, byte_idx, ch, prev_ch)`.
    /// Returns `true` if `on_char` returned `ScanAction::Break`.
    pub(super) fn scan_str(
        &mut self,
        s: &str,
        on_char: impl FnMut(&mut Self, usize, char, char) -> ScanAction,
    ) -> bool {
        self.scan_str_with_comments(s, None, on_char)
    }

    /// Iterates significant (non-literal, non-block-comment) characters of `s`,
    /// optionally recording byte intervals `[start, end)` of any `/* ... */` block comments.
    pub(super) fn scan_str_with_comments(
        &mut self,
        s: &str,
        mut comment_ranges: Option<&mut Vec<(usize, usize)>>,
        mut on_char: impl FnMut(&mut Self, usize, char, char) -> ScanAction,
    ) -> bool {
        let mut prev_ch = ' ';
        let mut skip_until = 0;
        let mut block_comment_start = 0usize;

        for (b_idx, ch) in s.char_indices() {
            if b_idx < skip_until {
                prev_ch = ch;
                continue;
            }
            if self.in_block_comment {
                if ch == '*' && s.as_bytes().get(b_idx + 1) == Some(&b'/') {
                    self.in_block_comment = false;
                    skip_until = b_idx + 2;
                    prev_ch = '/';
                    if let Some(ranges) = comment_ranges.as_deref_mut() {
                        ranges.push((block_comment_start, b_idx + 2));
                    }
                } else {
                    prev_ch = ch;
                }
                continue;
            }
            if let Some(hashes) = self.raw_string_hashes {
                if ch == '"'
                    && s.as_bytes()[b_idx + 1..]
                        .iter()
                        .take(hashes)
                        .filter(|&&b| b == b'#')
                        .count()
                        == hashes
                {
                    self.raw_string_hashes = None;
                    skip_until = b_idx + 1 + hashes;
                    prev_ch = if hashes > 0 { '#' } else { '"' };
                } else {
                    prev_ch = ch;
                }
                continue;
            }
            if self.in_verbatim_double_quote {
                if ch == '"' {
                    if s.as_bytes().get(b_idx + 1) == Some(&b'"') {
                        skip_until = b_idx + 2;
                    } else {
                        self.in_verbatim_double_quote = false;
                    }
                }
                prev_ch = ch;
                continue;
            }
            if self.in_double_quote {
                if self.escaped {
                    self.escaped = false;
                } else if ch == '\\' {
                    self.escaped = true;
                } else if ch == '"' {
                    self.in_double_quote = false;
                }
                prev_ch = ch;
                continue;
            }
            if self.in_backtick_quote {
                if self.allow_backtick_escapes && self.escaped {
                    self.escaped = false;
                } else if self.allow_backtick_escapes && ch == '\\' {
                    self.escaped = true;
                } else if ch == '`' {
                    self.in_backtick_quote = false;
                }
                prev_ch = ch;
                continue;
            }
            if self.in_single_quote {
                if self.escaped {
                    self.escaped = false;
                } else if ch == '\\' {
                    self.escaped = true;
                } else if ch == '\'' {
                    self.in_single_quote = false;
                }
                prev_ch = ch;
                continue;
            }

            if self.allow_slash_comments && ch == '/' && s.as_bytes().get(b_idx + 1) == Some(&b'*')
            {
                self.in_block_comment = true;
                block_comment_start = b_idx;
                skip_until = b_idx + 2;
                prev_ch = '*';
                continue;
            }

            if self.allow_rust_lifetime && ch == 'r' {
                if let Some((skip_len, hashes)) = rust_raw_string_prefix(s, b_idx) {
                    self.raw_string_hashes = Some(hashes);
                    skip_until = b_idx + skip_len;
                    prev_ch = '"';
                    continue;
                }
            }

            if self.allow_csharp_verbatim && matches!(ch, '@' | '$') {
                if let Some(skip_len) = csharp_verbatim_string_prefix(s, b_idx) {
                    self.in_verbatim_double_quote = true;
                    skip_until = b_idx + skip_len;
                    prev_ch = '"';
                    continue;
                }
            }

            match ch {
                '"' => {
                    self.in_double_quote = true;
                    self.escaped = false;
                }
                '`' => {
                    self.in_backtick_quote = true;
                    self.escaped = false;
                }
                '\'' => {
                    if prev_ch.is_ascii_digit() {
                        // C++ digit separator 1'000
                    } else if self.allow_rust_lifetime
                        && is_rust_lifetime(&s[b_idx..], &s[..b_idx], self.a_depth > 0)
                    {
                        let rem = &s[b_idx + 1..];
                        let ident_len = rem
                            .find(|c: char| !c.is_alphanumeric() && c != '_')
                            .unwrap_or(rem.len());
                        skip_until = b_idx + 1 + ident_len;
                        prev_ch = '\'';
                        continue;
                    } else if let Some(lit_len) = char_literal_len(&s[b_idx..]) {
                        skip_until = b_idx + lit_len;
                        prev_ch = '\'';
                        continue;
                    } else {
                        self.in_single_quote = true;
                        self.escaped = false;
                    }
                }
                _ => match on_char(self, b_idx, ch, prev_ch) {
                    ScanAction::Continue => {}
                    ScanAction::SkipBytes(n) => {
                        skip_until = b_idx + n;
                    }
                    ScanAction::Break => {
                        if self.in_block_comment {
                            if let Some(ranges) = comment_ranges {
                                ranges.push((block_comment_start, b_idx));
                            }
                        }
                        return true;
                    }
                },
            }
            prev_ch = ch;
        }
        if self.in_block_comment {
            if let Some(ranges) = comment_ranges {
                ranges.push((block_comment_start, s.len()));
            }
        }
        false
    }
}

fn split_first_param_impl(
    inner: &str,
    allow_rust_lifetime: bool,
    allow_backtick_escapes: bool,
    track_angles: bool,
    ignore_angles_after_equals: bool,
) -> Option<(&str, bool)> {
    let mut p_depth: usize = 0;
    let mut b_depth: usize = 0;
    let mut c_depth: usize = 0;
    let mut seen_equals_at_top = false;
    let mut scanner = CodeScanner::new(allow_rust_lifetime);
    scanner.allow_backtick_escapes = allow_backtick_escapes;
    let mut split_at: Option<usize> = None;

    scanner.scan_str(inner, |sc, b_idx, ch, prev_ch| {
        if track_angles && p_depth == 0 && b_depth == 0 && c_depth == 0 {
            let skip_lt = ch == '<'
                && seen_equals_at_top
                && (ignore_angles_after_equals || prev_ch.is_whitespace());
            if !skip_lt {
                if let Some(skip) = sc.try_update_angle_depth(inner, b_idx, ch, prev_ch) {
                    return if skip > 0 {
                        ScanAction::SkipBytes(skip)
                    } else {
                        ScanAction::Continue
                    };
                }
            }
        }
        match ch {
            '(' => p_depth += 1,
            ')' => p_depth = p_depth.saturating_sub(1),
            '[' => b_depth += 1,
            ']' => b_depth = b_depth.saturating_sub(1),
            '{' => c_depth += 1,
            '}' => c_depth = c_depth.saturating_sub(1),
            '=' if p_depth == 0 && b_depth == 0 && c_depth == 0 && sc.a_depth == 0 => {
                let next_ch = inner[b_idx + 1..].chars().next();
                if next_ch != Some('>')
                    && next_ch != Some('=')
                    && prev_ch != '='
                    && prev_ch != '!'
                    && prev_ch != '<'
                    && prev_ch != '>'
                {
                    seen_equals_at_top = true;
                }
            }
            ',' if p_depth == 0 && b_depth == 0 && sc.a_depth == 0 && c_depth == 0 => {
                split_at = Some(b_idx);
                return ScanAction::Break;
            }
            _ => {}
        }
        ScanAction::Continue
    });

    if let Some(b_idx) = split_at {
        let first = inner[..b_idx].trim();
        let has_more = !inner[b_idx + 1..].trim().is_empty();
        return Some((first, has_more));
    }

    if track_angles && scanner.a_depth > 0 {
        return None;
    }

    Some((inner.trim(), false))
}

/// Extracts `(first_param, has_more)` from an inner parameter list string,
/// respecting nested `()`, `[]`, `<>`, `{}` and avoiding false positives on `<` comparison / `<<` shift operators.
#[cfg(test)]
pub(super) fn split_first_param_at_depth_zero(
    inner: &str,
    allow_rust_lifetime: bool,
) -> (&str, bool) {
    split_first_param_at_depth_zero_impl(inner, allow_rust_lifetime, true)
}

fn split_first_param_at_depth_zero_impl(
    inner: &str,
    allow_rust_lifetime: bool,
    allow_backtick_escapes: bool,
) -> (&str, bool) {
    if let Some(res) = split_first_param_impl(
        inner,
        allow_rust_lifetime,
        allow_backtick_escapes,
        true,
        false,
    ) {
        return res;
    }
    if let Some(res) = split_first_param_impl(
        inner,
        allow_rust_lifetime,
        allow_backtick_escapes,
        true,
        true,
    ) {
        return res;
    }
    split_first_param_impl(
        inner,
        allow_rust_lifetime,
        allow_backtick_escapes,
        false,
        true,
    )
    .unwrap_or((inner.trim(), false))
}

/// Helper to locate the function/method parameter list `( ... )` span.
/// Correctly distinguishes parameter lists from `pub(...)` visibility modifiers,
/// Go receiver spans `func (...) Method(...)`, inline decorators/annotations like `@SuppressWarnings("...")`,
/// C# attributes like `[HttpGet("...")]`, generic bounds like `Fn(...)`,
/// C++ templates with `(1 < 2)` / `1 < 2` / `operator<<`, and return types like `-> (A, B)`.
#[cfg(test)]
pub(super) fn find_parameter_list_span(
    sig: &str,
    allow_rust_lifetime: bool,
) -> Option<(usize, usize)> {
    let allow_backtick_escapes = !(sig.starts_with("func ") || sig.contains(" func "));
    find_parameter_list_span_impl(sig, allow_rust_lifetime, allow_backtick_escapes)
}

fn find_parameter_list_span_impl(
    sig: &str,
    allow_rust_lifetime: bool,
    allow_backtick_escapes: bool,
) -> Option<(usize, usize)> {
    let mut spans = Vec::new();
    let mut depth: usize = 0;
    let mut inner_p_depth: usize = 0;
    let mut b_depth: usize = 0;
    let mut open_pos = None;
    let mut scanner = CodeScanner::new(allow_rust_lifetime);
    scanner.allow_backtick_escapes = allow_backtick_escapes;

    scanner.scan_str(sig, |sc, idx, ch, prev_ch| {
        if depth == 0 && b_depth == 0 && inner_p_depth == 0 {
            if prev_ch == ' '
                && ((ch == '>' && sig[idx..].starts_with("> "))
                    || (ch == '›' && sig[idx..].starts_with("› ")))
            {
                // " > " / " › " is the scope separator (e.g. "impl<T: Foo<Bar... › fn f(...)");
                // reset any unclosed angle brackets from truncated parent_scope.
                sc.a_depth = 0;
                return ScanAction::Continue;
            }
            if let Some(skip) = sc.try_update_angle_depth(sig, idx, ch, prev_ch) {
                return if skip > 0 {
                    ScanAction::SkipBytes(skip)
                } else {
                    ScanAction::Continue
                };
            }
        }
        match ch {
            '[' if depth == 0 && inner_p_depth == 0 => {
                b_depth += 1;
            }
            ']' if depth == 0 && inner_p_depth == 0 && b_depth > 0 => {
                b_depth -= 1;
            }
            '(' if sc.a_depth == 0 && b_depth == 0 => {
                if depth == 0 {
                    open_pos = Some(idx);
                }
                depth += 1;
            }
            ')' if sc.a_depth == 0 && b_depth == 0 && depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    if let Some(op) = open_pos.take() {
                        spans.push((op, idx));
                    }
                }
            }
            '(' => inner_p_depth += 1,
            ')' => inner_p_depth = inner_p_depth.saturating_sub(1),
            _ => {}
        }
        ScanAction::Continue
    });

    if spans.is_empty() {
        return None;
    }

    // Filter out spans that are definitely not the parameter list:
    // 1. `pub(...)` visibility modifier (with word boundary so e.g. `get_pub(...)` is preserved)
    // 2. Return types preceded by `->` or immediately following `)` (e.g. Go return tuple)
    // 3. Annotations/decorators like `@SuppressWarnings(...)` or `@Route(...)`
    let candidates: Vec<(usize, usize)> = spans
        .into_iter()
        .filter(|&(open, _)| {
            let before = sig[..open].trim_end();
            if ends_with_keyword_word_boundary(before, "pub") {
                return false;
            }
            if before.ends_with("->") || before.ends_with(')') {
                return false;
            }
            if let Some(last_word) = before.split_whitespace().last() {
                if last_word.starts_with('@') {
                    return false;
                }
            }
            true
        })
        .collect();

    if candidates.is_empty() {
        return None;
    }

    // If there's a Go receiver `func (...) Method(...)`, skip the receiver span (requiring word boundary so e.g. `call_func(...)` is preserved)
    if candidates.len() > 1 {
        let first_before = sig[..candidates[0].0].trim_end();
        if ends_with_keyword_word_boundary(first_before, "func") {
            return Some(candidates[1]);
        }
    }

    // If there's `fn `, `def `, `func `, or `function `, pick the first candidate after that keyword
    for kw in &["fn ", "def ", "func ", "function "] {
        let mut search_from = 0;
        while let Some(pos) = sig[search_from..].find(kw) {
            let kw_pos = search_from + pos;
            let is_boundary = sig[..kw_pos]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_');
            if is_boundary {
                if let Some(&cand) = candidates.iter().find(|(open, _)| *open > kw_pos) {
                    return Some(cand);
                }
                break;
            }
            search_from = kw_pos + kw.len();
        }
    }

    Some(candidates[0])
}

/// Compacts a long signature preview into `max_chars` (used in unit tests).
#[cfg(test)]
pub(super) fn compact_signature_preview(sig: &str, max_chars: usize) -> String {
    let allow_rust_lifetime = sig.starts_with("fn ")
        || sig.starts_with("pub ")
        || sig.contains(" fn ")
        || sig.contains("&'")
        || sig.contains("<'")
        || sig.contains("impl ");
    compact_signature_preview_with_opts(sig, max_chars, allow_rust_lifetime, true)
}

pub(super) fn compact_signature_preview_for_lang(
    sig: &str,
    max_chars: usize,
    lang: SupportedLanguage,
) -> String {
    compact_signature_preview_with_opts(
        sig,
        max_chars,
        lang == SupportedLanguage::Rust,
        lang != SupportedLanguage::Go,
    )
}

fn compact_at_param_span(
    sig: &str,
    open_paren: usize,
    close_paren: usize,
    max_chars: usize,
    allow_rust_lifetime: bool,
    allow_backtick_escapes: bool,
) -> Option<String> {
    if open_paren >= close_paren || close_paren >= sig.len() {
        return None;
    }
    let inner = sig[open_paren + 1..close_paren].trim();
    if inner.is_empty() {
        return None;
    }
    let before_paren = &sig[..=open_paren];
    let after_paren = &sig[close_paren..];
    let (first_param, has_more) =
        split_first_param_at_depth_zero_impl(inner, allow_rust_lifetime, allow_backtick_escapes);
    let first_clean = first_param.trim_end_matches(',').trim();

    let candidate1 = if !first_clean.is_empty() && first_clean.chars().count() < 35 {
        if has_more {
            format!("{before_paren}{first_clean}, ...{after_paren}")
        } else {
            format!("{before_paren}{first_clean}{after_paren}")
        }
    } else {
        format!("{before_paren}...{after_paren}")
    };
    if candidate1.chars().count() <= max_chars {
        return Some(candidate1);
    }
    let candidate2 = format!("{before_paren}...{after_paren}");
    if candidate2.chars().count() <= max_chars {
        return Some(candidate2);
    }
    None
}

fn compact_signature_preview_with_opts(
    sig: &str,
    max_chars: usize,
    allow_rust_lifetime: bool,
    allow_backtick_escapes: bool,
) -> String {
    let char_count = sig.chars().count();
    if char_count <= max_chars {
        return sig.to_string();
    }

    if let Some((open_paren, close_paren)) =
        find_parameter_list_span_impl(sig, allow_rust_lifetime, allow_backtick_escapes)
    {
        if let Some(compacted) = compact_at_param_span(
            sig,
            open_paren,
            close_paren,
            max_chars,
            allow_rust_lifetime,
            allow_backtick_escapes,
        ) {
            return compacted;
        }
    }

    truncate_preview(sig, max_chars)
}

/// Walks AST children/declarators to extract the `parameters` (or JS `parameter`) node if present.
pub(super) fn find_ast_parameters_node<'a>(
    lang: SupportedLanguage,
    node: &tree_sitter::Node<'a>,
) -> Option<tree_sitter::Node<'a>> {
    let mut target = *node;
    match target.kind() {
        "decorated_definition" => {
            target = target.child_by_field_name("definition")?;
        }
        "export_statement" => {
            target = target.child_by_field_name("declaration")?;
        }
        "template_declaration" => {
            let mut cursor = target.walk();
            for ch in target.named_children(&mut cursor) {
                if ch.kind() == "function_definition" {
                    target = ch;
                    break;
                }
            }
        }
        "expression_statement" => {
            if let Some(ch) = target.named_child(0) {
                target = ch;
            }
        }
        _ => {}
    }

    match target.kind() {
        "lexical_declaration" | "variable_declaration" => {
            let mut cursor = target.walk();
            for ch in target.named_children(&mut cursor) {
                if ch.kind() == "variable_declarator" {
                    if let Some(val) = ch.child_by_field_name("value") {
                        if matches!(val.kind(), "arrow_function" | "function_expression") {
                            target = val;
                            break;
                        }
                    }
                }
            }
        }
        "variable_declarator"
        | "pair"
        | "field_definition"
        | "public_field_definition"
        | "property_definition" => {
            target = target.child_by_field_name("value")?;
        }
        "assignment_expression" => {
            target = target.child_by_field_name("right")?;
        }
        _ => {}
    }

    if let Some(p) = target.child_by_field_name("parameters") {
        return Some(p);
    }
    if matches!(
        lang,
        SupportedLanguage::JavaScript | SupportedLanguage::TypeScript | SupportedLanguage::Tsx
    ) {
        if let Some(p) = target.child_by_field_name("parameter") {
            return Some(p);
        }
    }
    if matches!(lang, SupportedLanguage::C | SupportedLanguage::Cpp) {
        let mut decl = target.child_by_field_name("declarator")?;
        for _ in 0..8 {
            if let Some(p) = decl.child_by_field_name("parameters") {
                return Some(p);
            }
            decl = decl.child_by_field_name("declarator")?;
        }
    }
    None
}

/// Strips line comments (`//` or Python `#`, excluding `#[` and `#![`) outside quotes/block comments.
#[cfg(test)]
pub(super) fn strip_line_comment_simple(
    line: &str,
    allow_hash: bool,
    allow_rust_lifetime: bool,
) -> &str {
    strip_line_comment_with_opts(line, allow_hash, allow_rust_lifetime, true)
}

#[cfg(test)]
fn strip_line_comment_with_opts(
    line: &str,
    allow_hash: bool,
    allow_rust_lifetime: bool,
    allow_backtick_escapes: bool,
) -> &str {
    let mut scanner = CodeScanner::new(allow_rust_lifetime);
    scanner.allow_backtick_escapes = allow_backtick_escapes;
    scanner.allow_slash_comments = !allow_hash;
    let mut end_idx = line.len();

    scanner.scan_str(line, |sc, b_idx, ch, prev_ch| {
        if let Some(skip) = sc.try_update_angle_depth(line, b_idx, ch, prev_ch) {
            return if skip > 0 {
                ScanAction::SkipBytes(skip)
            } else {
                ScanAction::Continue
            };
        }
        if !allow_hash && ch == '/' && prev_ch == '/' {
            end_idx = b_idx - 1;
            return ScanAction::Break;
        }
        if ch == '#' {
            if allow_hash {
                end_idx = b_idx;
                return ScanAction::Break;
            }
            let is_rust_attr = line[b_idx..].starts_with("#[") || line[b_idx..].starts_with("#![");
            let is_raw_ident_or_string = prev_ch == 'r' || prev_ch == 'R';
            let is_js_private = line
                .get(b_idx + 1..)
                .and_then(|rest| rest.chars().next())
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
            if !is_rust_attr && !is_raw_ident_or_string && !is_js_private && b_idx == 0 {
                end_idx = b_idx;
                return ScanAction::Break;
            }
        }
        ScanAction::Continue
    });

    line[..end_idx].trim_end()
}

/// Strips inline/multiline block comments (`/* ... */`) and line comments (`//` or `#`) across a line,
/// persisting `CodeScanner` state across lines via `CodeScanner::scan_str_with_comments`.
fn strip_comments_preserving_code(
    line: &str,
    scanner: &mut CodeScanner,
    allow_hash: bool,
) -> String {
    let mut comment_ranges = Vec::new();
    let mut line_end = line.len();

    scanner.scan_str_with_comments(line, Some(&mut comment_ranges), |sc, b_idx, ch, prev_ch| {
        if let Some(skip) = sc.try_update_angle_depth(line, b_idx, ch, prev_ch) {
            return if skip > 0 {
                ScanAction::SkipBytes(skip)
            } else {
                ScanAction::Continue
            };
        }
        if sc.allow_slash_comments && ch == '/' && line.as_bytes().get(b_idx + 1) == Some(&b'/') {
            line_end = b_idx;
            return ScanAction::Break;
        }
        if ch == '#' {
            if allow_hash {
                line_end = b_idx;
                return ScanAction::Break;
            }
            let is_rust_attr = line[b_idx..].starts_with("#[") || line[b_idx..].starts_with("#![");
            let is_raw_ident_or_string = prev_ch == 'r' || prev_ch == 'R';
            let is_js_private = line
                .get(b_idx + 1..)
                .and_then(|rest| rest.chars().next())
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
            if !is_rust_attr && !is_raw_ident_or_string && !is_js_private && b_idx == 0 {
                line_end = b_idx;
                return ScanAction::Break;
            }
        }
        ScanAction::Continue
    });

    let effective = &line[..line_end];
    if comment_ranges.is_empty() {
        return effective.trim().to_string();
    }

    let mut out = String::with_capacity(effective.len());
    let mut cursor = 0usize;
    for (cs, ce) in comment_ranges {
        let cs = cs.min(effective.len());
        let ce = ce.min(effective.len());
        if cs > cursor {
            out.push_str(&effective[cursor..cs]);
            let next_non_ws = effective
                .get(ce..)
                .and_then(|s| s.trim_start().chars().next());
            if matches!(next_non_ws, Some(',' | ')' | ';')) {
                while out.ends_with([' ', '\t']) {
                    out.pop();
                }
            } else if !out.is_empty() && !out.ends_with(|c: char| c.is_whitespace()) {
                out.push(' ');
            }
        }
        cursor = cursor.max(ce);
    }
    if cursor < effective.len() {
        out.push_str(&effective[cursor..]);
    }
    out.trim().to_string()
}

#[cfg(test)]
pub(super) fn extract_signature_from_row(start_idx: usize, lines: &[&str]) -> Option<String> {
    extract_signature_from_row_with_limit(start_idx, (start_idx + 60).min(lines.len()), lines)
}

#[cfg(test)]
pub(super) fn extract_signature_from_row_with_limit(
    start_idx: usize,
    end_bound: usize,
    lines: &[&str],
) -> Option<String> {
    extract_signature_from_row_with_limit_raw(start_idx, end_bound, lines, None, None, true, true)
        .map(|s| compact_signature_preview(&s, 120))
}

pub(super) fn extract_signature_from_row_for_lang(
    start_idx: usize,
    lines: &[&str],
    lang: SupportedLanguage,
) -> Option<String> {
    let allow_hash = matches!(
        lang,
        SupportedLanguage::Python | SupportedLanguage::Bash | SupportedLanguage::Ruby
    );
    let allow_rust_lifetime = lang == SupportedLanguage::Rust;
    let allow_backtick_escapes = lang != SupportedLanguage::Go;
    let allow_csharp_verbatim = lang == SupportedLanguage::CSharp;
    extract_signature_from_row_with_limit_raw(
        start_idx,
        (start_idx + 60).min(lines.len()),
        lines,
        Some(allow_hash),
        Some(allow_rust_lifetime),
        allow_backtick_escapes,
        allow_csharp_verbatim,
    )
    .map(|s| compact_signature_preview_for_lang(&s, 120, lang))
}

#[derive(Default)]
pub(super) struct SigScanState {
    pub(super) p_depth: usize,
    b_depth: usize,
    c_depth: usize,
    seen_param_open: bool,
    seen_param_close: bool,
    in_ts_return_type: bool,
    saw_paren_in_return_type: bool,
    saw_function_kw: bool,
    is_var_decl: bool,
    saw_colon_before_assign: bool,
    saw_assign: bool,
    scanner: CodeScanner,
}

impl SigScanState {
    pub(super) fn new(allow_rust_lifetime: bool) -> Self {
        Self::new_with_opts(allow_rust_lifetime, true, true, !allow_rust_lifetime)
    }

    pub(super) fn new_with_opts(
        allow_rust_lifetime: bool,
        allow_backtick_escapes: bool,
        allow_slash_comments: bool,
        allow_csharp_verbatim: bool,
    ) -> Self {
        let mut scanner = CodeScanner::new(allow_rust_lifetime);
        scanner.allow_backtick_escapes = allow_backtick_escapes;
        scanner.allow_slash_comments = allow_slash_comments;
        scanner.allow_csharp_verbatim = allow_csharp_verbatim;
        Self {
            p_depth: 0,
            b_depth: 0,
            c_depth: 0,
            seen_param_open: false,
            seen_param_close: false,
            in_ts_return_type: false,
            saw_paren_in_return_type: false,
            saw_function_kw: false,
            is_var_decl: false,
            saw_colon_before_assign: false,
            saw_assign: false,
            scanner,
        }
    }

    /// Scans a line, updating scanner state. Returns `(ends_sig, first_brace_idx_if_any)`.
    pub(super) fn scan_line(&mut self, line: &str) -> (bool, Option<usize>) {
        let trimmed_start = line.trim_start();
        if !self.saw_function_kw
            && (trimmed_start.starts_with("function ")
                || trimmed_start.starts_with("async function ")
                || trimmed_start.starts_with("export function ")
                || trimmed_start.starts_with("export async function ")
                || trimmed_start.starts_with("export default function "))
        {
            self.saw_function_kw = true;
        }
        if !self.is_var_decl
            && (trimmed_start.starts_with("const ")
                || trimmed_start.starts_with("let ")
                || trimmed_start.starts_with("var ")
                || trimmed_start.starts_with("export const ")
                || trimmed_start.starts_with("export let ")
                || trimmed_start.starts_with("export var "))
        {
            self.is_var_decl = true;
        }

        let mut first_brace = None;
        let mut consumed_return_arrow_on_line = false;
        let p_depth = &mut self.p_depth;
        let b_depth = &mut self.b_depth;
        let c_depth = &mut self.c_depth;
        let seen_param_open = &mut self.seen_param_open;
        let seen_param_close = &mut self.seen_param_close;
        let in_ts_return_type = &mut self.in_ts_return_type;
        let saw_paren_in_return_type = &mut self.saw_paren_in_return_type;
        let saw_colon_before_assign = &mut self.saw_colon_before_assign;
        let saw_assign = &mut self.saw_assign;
        let saw_function_kw = self.saw_function_kw;

        self.scanner.scan_str(line, |sc, b_idx, ch, prev_ch| {
            if let Some(skip) = sc.try_update_angle_depth(line, b_idx, ch, prev_ch) {
                return if skip > 0 {
                    ScanAction::SkipBytes(skip)
                } else {
                    ScanAction::Continue
                };
            }
            match ch {
                '(' => {
                    if *p_depth == 0 && *b_depth == 0 && *c_depth == 0 {
                        if *in_ts_return_type {
                            *saw_paren_in_return_type = true;
                        } else if !*seen_param_close {
                            *seen_param_open = true;
                        }
                    }
                    *p_depth += 1;
                }
                ')' => {
                    *p_depth = p_depth.saturating_sub(1);
                    if *p_depth == 0
                        && *b_depth == 0
                        && *c_depth == 0
                        && *seen_param_open
                        && !*in_ts_return_type
                    {
                        *seen_param_close = true;
                    }
                }
                '[' => *b_depth += 1,
                ']' => *b_depth = b_depth.saturating_sub(1),
                '{' if *p_depth == 0 && *b_depth == 0 && first_brace.is_none() => {
                    first_brace = Some(b_idx);
                    *c_depth += 1;
                }
                '{' => *c_depth += 1,
                '}' => *c_depth = c_depth.saturating_sub(1),
                ':' if *p_depth == 0 && *b_depth == 0 && *c_depth == 0 => {
                    let next_ch = line[b_idx + 1..].chars().next();
                    if next_ch != Some(':') && prev_ch != ':' {
                        if *seen_param_close {
                            *in_ts_return_type = true;
                            *saw_paren_in_return_type = false;
                        } else if !*seen_param_open && !*saw_assign {
                            *saw_colon_before_assign = true;
                        }
                    }
                }
                '=' if *p_depth == 0 && *b_depth == 0 && *c_depth == 0 => {
                    let next_ch = line[b_idx + 1..].chars().next();
                    if next_ch != Some('>')
                        && next_ch != Some('=')
                        && prev_ch != '='
                        && prev_ch != '!'
                        && prev_ch != '<'
                        && prev_ch != '>'
                    {
                        *saw_assign = true;
                        *saw_colon_before_assign = false;
                        *in_ts_return_type = false;
                        *saw_paren_in_return_type = false;
                        // Reset param open/close if they belonged to a variable type annotation before `=`
                        *seen_param_open = false;
                        *seen_param_close = false;
                    } else if next_ch == Some('>')
                        && *in_ts_return_type
                        && !saw_function_kw
                        && *saw_paren_in_return_type
                    {
                        // Consume the return-type `=>` inside `: (...) => ReturnType`
                        *saw_paren_in_return_type = false;
                        consumed_return_arrow_on_line = true;
                    }
                }
                _ => {}
            }
            ScanAction::Continue
        });

        let mut ends_sig = first_brace.is_some();
        if !ends_sig && self.p_depth == 0 && !self.scanner.in_literal() {
            let trimmed = line.trim_end();
            let suppress_arrow_end = self.saw_function_kw
                || consumed_return_arrow_on_line
                || (self.in_ts_return_type && self.saw_paren_in_return_type)
                || (self.is_var_decl
                    && self.saw_colon_before_assign
                    && !self.saw_assign
                    && !trimmed.starts_with("=>"));
            if trimmed.ends_with(';')
                || (trimmed.ends_with(':') && !trimmed.ends_with("::"))
                || (trimmed.ends_with("=>") && !suppress_arrow_end)
            {
                ends_sig = true;
            }
        }

        (ends_sig, first_brace)
    }
}

fn normalize_joined_signature(
    raw: &str,
    allow_rust_lifetime: bool,
    allow_backtick_escapes: bool,
) -> String {
    let mut collapsed = raw.to_string();
    while collapsed.contains("  ") {
        collapsed = collapsed.replace("  ", " ");
    }
    if let Some((open, close)) =
        find_parameter_list_span_impl(&collapsed, allow_rust_lifetime, allow_backtick_escapes)
    {
        if open < close {
            let before = &collapsed[..=open];
            let inner = collapsed[open + 1..close].trim();
            let inner = inner.strip_suffix(',').map_or(inner, str::trim_end);
            let after = &collapsed[close..];
            return format!("{before}{inner}{after}");
        }
    }
    collapsed
}

fn extract_signature_from_row_with_limit_raw(
    start_idx: usize,
    end_bound: usize,
    lines: &[&str],
    allow_hash_override: Option<bool>,
    allow_rust_lifetime_override: Option<bool>,
    allow_backtick_escapes: bool,
    allow_csharp_verbatim: bool,
) -> Option<String> {
    let end_idx = end_bound.min(lines.len());
    let mut found = None;
    let mut bracket_depth: usize = 0;
    let mut paren_depth: usize = 0;
    let mut in_block_comment = false;
    let mut in_decorator = false;

    for (offset, line) in lines.iter().take(end_idx).skip(start_idx).enumerate() {
        let row_idx = start_idx + offset;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut cur = trimmed;

        if in_block_comment {
            if let Some(pos) = cur.find("*/") {
                in_block_comment = false;
                cur = cur[pos + 2..].trim();
                if cur.is_empty() {
                    continue;
                }
            } else {
                continue;
            }
        }

        // Inline block comments, potentially repeated (e.g. /* a */ /* b */ code)
        while cur.starts_with("/*") {
            if let Some(pos) = cur[2..].find("*/") {
                cur = cur[2 + pos + 2..].trim();
            } else {
                in_block_comment = true;
                cur = "";
                break;
            }
        }

        if in_block_comment || cur.is_empty() {
            continue;
        }

        if cur.starts_with("//") || cur.starts_with('*') {
            continue;
        }

        if cur.starts_with('#') && !cur.starts_with("#[") && !cur.starts_with("#![") {
            continue;
        }

        while bracket_depth > 0 || cur.starts_with("#[") || cur.starts_with("#![") {
            if bracket_depth == 0 {
                if cur.starts_with("#![") {
                    cur = &cur[3..];
                } else {
                    cur = &cur[2..];
                }
                bracket_depth = 1;
            }
            let mut closed_at = None;
            for (byte_idx, ch) in cur.char_indices() {
                if ch == '[' {
                    bracket_depth += 1;
                } else if ch == ']' {
                    bracket_depth = bracket_depth.saturating_sub(1);
                    if bracket_depth == 0 {
                        closed_at = Some(byte_idx + ch.len_utf8());
                        break;
                    }
                }
            }
            if let Some(pos) = closed_at {
                cur = cur[pos..].trim();
            } else {
                cur = "";
                break;
            }
        }

        if cur.is_empty() {
            continue;
        }

        if in_decorator || (cur.starts_with('@') && !cur.contains("fn ") && !cur.contains("def ")) {
            if !in_decorator {
                in_decorator = true;
                paren_depth = 0;
            }
            for ch in cur.chars() {
                if ch == '(' {
                    paren_depth += 1;
                } else if ch == ')' {
                    paren_depth = paren_depth.saturating_sub(1);
                }
            }
            if paren_depth == 0 {
                in_decorator = false;
            }
            continue;
        }

        // C# attributes like [HttpGet], [HttpGet("api/v1")], [Serializable]
        if super::test_scope::is_csharp_attribute(cur)
            || (cur.starts_with('[')
                && cur.ends_with(']')
                && !cur.contains("fn ")
                && !cur.contains(':')
                && cur.len() > 2
                && cur[1..cur.len() - 1].chars().all(|c| {
                    c.is_alphanumeric()
                        || c == '_'
                        || c == '.'
                        || c == '('
                        || c == ')'
                        || c == ','
                        || c == ' '
                }))
        {
            cur = "";
        }
        if cur.is_empty() {
            continue;
        }

        // Handle C# inline attributes before signature on same line (e.g. `[HttpGet("api/v1")] public ...`)
        while cur.starts_with('[') && !cur.starts_with("[[") {
            let after = &cur[1..];
            let Some(first) = after.trim_start().chars().next() else {
                break;
            };
            if !first.is_ascii_alphabetic() && first != '_' {
                break;
            }
            let mut p_d = 0usize;
            let mut in_q = false;
            let mut esc = false;
            let mut end_rel = None;
            for (r, ch) in after.char_indices() {
                if in_q {
                    if esc {
                        esc = false;
                    } else if ch == '\\' {
                        esc = true;
                    } else if ch == '"' {
                        in_q = false;
                    }
                    continue;
                }
                match ch {
                    '"' => in_q = true,
                    '(' => p_d += 1,
                    ')' => p_d = p_d.saturating_sub(1),
                    ']' if p_d == 0 => {
                        end_rel = Some(r);
                        break;
                    }
                    _ => {}
                }
            }
            if let Some(close) = end_rel {
                let slice = &cur[..close + 2];
                if super::test_scope::is_csharp_attribute(slice) {
                    cur = cur[close + 2..].trim_start();
                    continue;
                }
            }
            break;
        }
        if cur.is_empty() {
            continue;
        }

        let allow_hash = allow_hash_override.unwrap_or_else(|| {
            cur.starts_with("def ") || cur.starts_with("async def ") || cur.starts_with("class ")
        });
        let allow_rust_lifetime = allow_rust_lifetime_override.unwrap_or_else(|| {
            cur.starts_with("fn ")
                || cur.starts_with("pub fn ")
                || cur.starts_with("pub(crate) fn ")
                || cur.starts_with("impl ")
                || cur.starts_with("pub struct ")
                || cur.starts_with("struct ")
                || cur.starts_with("enum ")
                || cur.starts_with("trait ")
        });

        let mut strip_scanner = CodeScanner::new(allow_rust_lifetime);
        strip_scanner.allow_backtick_escapes = allow_backtick_escapes;
        strip_scanner.allow_slash_comments = !allow_hash;
        strip_scanner.allow_csharp_verbatim = allow_csharp_verbatim;

        let cur_clean = strip_comments_preserving_code(cur, &mut strip_scanner, allow_hash);
        if cur_clean.is_empty() && !strip_scanner.in_block_comment {
            continue;
        }

        let mut sig_parts = Vec::new();
        let mut sig_scanner = SigScanState::new_with_opts(
            allow_rust_lifetime,
            allow_backtick_escapes,
            !allow_hash,
            allow_csharp_verbatim,
        );
        let mut cur_ends_sig = false;
        if !cur_clean.is_empty() {
            let (ends, _) = sig_scanner.scan_line(&cur_clean);
            cur_ends_sig = ends;
            sig_parts.push(cur_clean);
        }

        if !cur_ends_sig || sig_scanner.p_depth > 0 || strip_scanner.in_literal() {
            let next_start = row_idx + 1;
            let next_end = (row_idx + 35).min(end_idx);
            for raw_line in lines.iter().take(next_end).skip(next_start) {
                let cleaned_line =
                    strip_comments_preserving_code(raw_line.trim(), &mut strip_scanner, allow_hash);
                if cleaned_line.is_empty() {
                    continue;
                }

                let (line_ends_sig, _) = sig_scanner.scan_line(&cleaned_line);
                sig_parts.push(cleaned_line);

                if line_ends_sig && sig_scanner.p_depth == 0 {
                    break;
                }
            }
        }

        if sig_parts.is_empty() {
            continue;
        }

        let combined = normalize_joined_signature(
            &sig_parts.join(" "),
            allow_rust_lifetime,
            allow_backtick_escapes,
        );

        let mut final_scanner = SigScanState::new_with_opts(
            allow_rust_lifetime,
            allow_backtick_escapes,
            !allow_hash,
            allow_csharp_verbatim,
        );
        let (_, first_brace) = final_scanner.scan_line(&combined);
        let cleaned = if let Some(pos) = first_brace {
            combined[..=pos].trim().to_string()
        } else {
            combined
        };

        found = Some(cleaned);
        break;
    }
    found.or_else(|| lines.get(start_idx).map(|l| l.trim().to_string()))
}

/// Collects all AST comment node byte intervals `[start, end)` within `[range_start, range_end)`.
fn collect_comment_byte_ranges(
    node: &tree_sitter::Node,
    range_start: usize,
    range_end: usize,
    out: &mut Vec<(usize, usize)>,
) {
    if node.end_byte() <= range_start || node.start_byte() >= range_end {
        return;
    }
    if node.kind().contains("comment") {
        let s = node.start_byte().max(range_start);
        let e = node.end_byte().min(range_end);
        if s < e {
            out.push((s, e));
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_comment_byte_ranges(&child, range_start, range_end, out);
    }
}

/// Strips comment AST byte intervals from `source[range_start..range_end]` and normalizes
/// line breaks/excessive whitespace without altering contents inside strings.
fn strip_ast_comments_in_range(
    source: &str,
    node: &tree_sitter::Node,
    range_start: usize,
    range_end: usize,
    allow_rust_lifetime: bool,
    allow_backtick_escapes: bool,
) -> String {
    let mut comment_ranges = Vec::new();
    collect_comment_byte_ranges(node, range_start, range_end, &mut comment_ranges);
    comment_ranges.sort_unstable();

    let mut stripped = String::with_capacity(range_end.saturating_sub(range_start));
    let mut cursor = range_start;
    for (cs, ce) in comment_ranges {
        if cs > cursor && cursor < range_end {
            stripped.push_str(&source[cursor..cs.min(range_end)]);
            let next_non_ws = source
                .get(ce.min(range_end)..range_end)
                .and_then(|s| s.trim_start().chars().next());
            if matches!(next_non_ws, Some(',' | ')' | ';')) {
                while stripped.ends_with([' ', '\t']) {
                    stripped.pop();
                }
            } else if !stripped.ends_with(|c: char| c.is_whitespace()) {
                stripped.push(' ');
            }
        }
        cursor = cursor.max(ce);
    }
    if cursor < range_end {
        stripped.push_str(&source[cursor..range_end]);
    }

    let parts: Vec<&str> = stripped
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    normalize_joined_signature(
        &parts.join(" "),
        allow_rust_lifetime,
        allow_backtick_escapes,
    )
}

/// Unwraps wrapper nodes (`export_statement`, `template_declaration`, JS/TS variable/property/
/// assignment-bound functions) to locate the AST `body` node for `target`.
pub(super) fn find_ast_body_node<'a>(
    lang: SupportedLanguage,
    target: &tree_sitter::Node<'a>,
) -> Option<tree_sitter::Node<'a>> {
    if let Some(body) = target.child_by_field_name("body") {
        return Some(body);
    }
    let is_js_ts = matches!(
        lang,
        SupportedLanguage::JavaScript | SupportedLanguage::TypeScript | SupportedLanguage::Tsx
    );
    match target.kind() {
        "export_statement" => {
            if let Some(decl) = target.child_by_field_name("declaration") {
                return find_ast_body_node(lang, &decl);
            }
        }
        "template_declaration" => {
            let mut cursor = target.walk();
            for ch in target.named_children(&mut cursor) {
                if let Some(b) = ch.child_by_field_name("body") {
                    return Some(b);
                }
            }
        }
        "expression_statement" => {
            if let Some(ch) = target.named_child(0) {
                return find_ast_body_node(lang, &ch);
            }
        }
        "lexical_declaration" | "variable_declaration" if is_js_ts => {
            let mut cursor = target.walk();
            for ch in target.named_children(&mut cursor) {
                if ch.kind() == "variable_declarator" {
                    if let Some(val) = ch.child_by_field_name("value") {
                        if matches!(val.kind(), "arrow_function" | "function_expression") {
                            return val.child_by_field_name("body");
                        }
                    }
                }
            }
        }
        "variable_declarator"
        | "pair"
        | "field_definition"
        | "public_field_definition"
        | "property_definition"
            if is_js_ts =>
        {
            if let Some(val) = target.child_by_field_name("value") {
                if matches!(val.kind(), "arrow_function" | "function_expression") {
                    return val.child_by_field_name("body");
                }
            }
        }
        "assignment_expression" if is_js_ts => {
            if let Some(val) = target.child_by_field_name("right") {
                if matches!(val.kind(), "arrow_function" | "function_expression") {
                    return val.child_by_field_name("body");
                }
            }
        }
        _ => {}
    }
    None
}

/// Computes the AST signature byte range `(target_node, sig_start, body_start)` excluding leading
/// attributes, annotations, decorators, and comments.
fn find_ast_signature_bounds<'a>(
    lang: SupportedLanguage,
    node: &tree_sitter::Node<'a>,
    source: &str,
) -> Option<(tree_sitter::Node<'a>, usize, usize)> {
    let target = if node.kind() == "decorated_definition" {
        node.child_by_field_name("definition").unwrap_or(*node)
    } else {
        *node
    };
    let body = find_ast_body_node(lang, &target)?;
    let body_start = body.start_byte();
    let target_start = target.start_byte();
    let mut last_anno_end = None;

    let mut cursor = target.walk();
    for child in target.children(&mut cursor) {
        if child.start_byte() >= body_start {
            break;
        }
        let k = child.kind();
        if k == "modifiers" {
            let mut mod_cursor = child.walk();
            let mut found_non_anno = false;
            for mod_child in child.children(&mut mod_cursor) {
                let mk = mod_child.kind();
                if mk.contains("annotation") || mk.contains("attribute") || mk.contains("comment") {
                    last_anno_end = Some(mod_child.end_byte());
                } else {
                    found_non_anno = true;
                    break;
                }
            }
            if found_non_anno {
                break;
            }
        } else if k == "decorator"
            || k.contains("attribute")
            || k.contains("comment")
            || k.contains("annotation")
        {
            last_anno_end = Some(child.end_byte());
        } else {
            break;
        }
    }

    let sig_start = last_anno_end.unwrap_or(target_start);
    if sig_start < body_start && body_start <= source.len() {
        Some((target, sig_start, body_start))
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SigFormatOpts {
    pub(super) max_chars: usize,
    pub(super) scope_sep: &'static str,
    pub(super) preserve_key_decorators: bool,
    pub(super) greedy_params: bool,
}

/// Splits inner contents of a C# attribute list (e.g. `Authorize, HttpPost("/refunds")`) at
/// top-level commas outside parentheses/brackets/strings.
fn split_top_level_attr_items(inner: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut paren = 0usize;
    let mut bracket = 0usize;
    let mut in_str = false;
    let mut esc = false;
    for (i, ch) in inner.char_indices() {
        if in_str {
            if esc {
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            '[' => bracket += 1,
            ']' => bracket = bracket.saturating_sub(1),
            ',' if paren == 0 && bracket == 0 => {
                let item = inner[start..i].trim();
                if !item.is_empty() {
                    out.push(item);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = inner[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

fn is_key_csharp_attr_item(item: &str) -> bool {
    if item.contains('\n') || item.chars().count() > 48 {
        return false;
    }
    // Split parameter list first so route constraints (`{id:int}`) or quoted values (`"admin:billing"`)
    // inside `(...)` are never misparsed as attribute target specifiers (`method:`, `return:`).
    let before_paren = item.split('(').next().unwrap_or(item).trim();
    let after_target = before_paren
        .split_once(':')
        .map_or(before_paren, |(_, rhs)| rhs)
        .trim();
    let short_head = after_target
        .rsplit('.')
        .next()
        .unwrap_or(after_target)
        .trim();
    matches!(
        short_head,
        "HttpGet"
            | "HttpPost"
            | "HttpPut"
            | "HttpDelete"
            | "HttpPatch"
            | "Route"
            | "Authorize"
            | "AllowAnonymous"
            | "ApiController"
            | "ValidateAntiForgeryToken"
            | "Transactional"
            | "Flags"
    )
}

fn sig_contains_decorator(sig: &str, dec: &str) -> bool {
    let mut offset = 0usize;
    while let Some(pos) = sig[offset..].find(dec) {
        let end = offset + pos + dec.len();
        let next_ok = sig[end..]
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || matches!(c, ']' | ')' | '>' | ',' | ':' | ';'));
        if next_ok {
            return true;
        }
        offset += pos + dec.len();
    }
    // Also match individual C# attribute items inside compound `[Attr1, Attr2(...)]` blocks
    if let Some(inner_dec) = dec
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .map(str::trim)
    {
        let mut i = 0usize;
        let bytes = sig.as_bytes();
        while i < bytes.len() {
            if bytes[i] == b'"' {
                i += 1;
                let mut esc = false;
                while i < bytes.len() {
                    if esc {
                        esc = false;
                    } else if bytes[i] == b'\\' {
                        esc = true;
                    } else if bytes[i] == b'"' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            if bytes[i] == b'[' {
                let start = i + 1;
                let mut j = start;
                let mut p_d = 0usize;
                let mut b_d = 1usize;
                let mut in_q = false;
                let mut esc = false;
                let mut close_idx = None;
                while j < bytes.len() {
                    let b = bytes[j];
                    if in_q {
                        if esc {
                            esc = false;
                        } else if b == b'\\' {
                            esc = true;
                        } else if b == b'"' {
                            in_q = false;
                        }
                        j += 1;
                        continue;
                    }
                    match b {
                        b'"' => in_q = true,
                        b'(' => p_d += 1,
                        b')' => p_d = p_d.saturating_sub(1),
                        b'[' if p_d == 0 => b_d += 1,
                        b']' if p_d == 0 => {
                            b_d -= 1;
                            if b_d == 0 {
                                close_idx = Some(j);
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                if let Some(end) = close_idx {
                    let inner = &sig[start..end];
                    if split_top_level_attr_items(inner)
                        .into_iter()
                        .any(|item| item == inner_dec)
                    {
                        return true;
                    }
                    i = end + 1;
                    continue;
                }
            }
            i += 1;
        }
    }
    false
}

/// Extracts short, semantically critical decorators/annotations for Python, Java, C#, Rust, and TS
/// (e.g. `@classmethod`, `@property`, `@Transactional`, `[HttpGet]`, `[Authorize]`) while
/// intentionally skipping noisy/boilerplate ones (`@Override`, `@SuppressWarnings`, large multiline).
fn extract_key_decorators_list(
    lang: SupportedLanguage,
    node: &tree_sitter::Node,
    source: &str,
) -> Vec<String> {
    let mut raw_decs: Vec<String> = Vec::new();
    match lang {
        SupportedLanguage::Python => {
            if node.kind() == "decorated_definition" {
                let mut cursor = node.walk();
                for ch in node.named_children(&mut cursor) {
                    if ch.kind() == "decorator" {
                        if let Some(txt) = source.get(ch.start_byte()..ch.end_byte()) {
                            raw_decs.push(txt.trim().to_string());
                        }
                    }
                }
            }
        }
        SupportedLanguage::Java => {
            let mut cursor = node.walk();
            for ch in node.named_children(&mut cursor) {
                if ch.kind() == "modifiers" {
                    let mut mc = ch.walk();
                    for m in ch.named_children(&mut mc) {
                        if matches!(m.kind(), "marker_annotation" | "annotation") {
                            if let Some(txt) = source.get(m.start_byte()..m.end_byte()) {
                                raw_decs.push(txt.trim().to_string());
                            }
                        }
                    }
                }
            }
        }
        SupportedLanguage::CSharp => {
            let mut cursor = node.walk();
            for ch in node.named_children(&mut cursor) {
                if ch.kind() == "attribute_list" {
                    if let Some(txt) = source.get(ch.start_byte()..ch.end_byte()) {
                        let t = txt.trim();
                        let inner = t.trim_start_matches('[').trim_end_matches(']').trim();
                        let items = split_top_level_attr_items(inner);
                        if items.len() > 1 {
                            for item in items {
                                if is_key_csharp_attr_item(item) {
                                    raw_decs.push(format!("[{item}]"));
                                }
                            }
                        } else {
                            raw_decs.push(t.to_string());
                        }
                    }
                }
            }
        }
        SupportedLanguage::TypeScript | SupportedLanguage::Tsx | SupportedLanguage::JavaScript => {
            // In tree-sitter-typescript, class_declaration / export_statement may own `decorator` children,
            // while `method_definition` inside `class_body` has preceding `decorator` siblings.
            let mut cursor = node.walk();
            for ch in node.named_children(&mut cursor) {
                if ch.kind() == "decorator" {
                    if let Some(txt) = source.get(ch.start_byte()..ch.end_byte()) {
                        raw_decs.push(txt.trim().to_string());
                    }
                }
            }
            if node.kind() == "export_statement" {
                if let Some(decl) = node.child_by_field_name("declaration") {
                    let mut dc = decl.walk();
                    for ch in decl.named_children(&mut dc) {
                        if ch.kind() == "decorator" {
                            if let Some(txt) = source.get(ch.start_byte()..ch.end_byte()) {
                                let t = txt.trim().to_string();
                                if !raw_decs.contains(&t) {
                                    raw_decs.push(t);
                                }
                            }
                        }
                    }
                }
            }
            let mut sib_decs = Vec::new();
            let mut prev_opt = node.prev_named_sibling();
            let mut steps = 0usize;
            while let Some(prev) = prev_opt {
                if prev.kind() != "decorator" || steps >= 4 {
                    break;
                }
                if let Some(txt) = source.get(prev.start_byte()..prev.end_byte()) {
                    let t = txt.trim().to_string();
                    if !raw_decs.contains(&t) {
                        sib_decs.push(t);
                    }
                }
                prev_opt = prev.prev_named_sibling();
                steps += 1;
            }
            sib_decs.reverse();
            if !sib_decs.is_empty() {
                sib_decs.extend(raw_decs);
                raw_decs = sib_decs;
            }
        }
        SupportedLanguage::Rust => {
            // In Rust, `#[must_use]` is a preceding `attribute_item` sibling node
            let mut prev_opt = node.prev_named_sibling();
            let mut steps = 0usize;
            while let Some(prev) = prev_opt {
                if prev.kind() != "attribute_item" || steps >= 3 {
                    break;
                }
                if let Some(txt) = source.get(prev.start_byte()..prev.end_byte()) {
                    raw_decs.push(txt.trim().to_string());
                }
                prev_opt = prev.prev_named_sibling();
                steps += 1;
            }
            raw_decs.reverse();
        }
        _ => return Vec::new(),
    }

    let is_key_dec = |d: &str| -> bool {
        if d.contains('\n') || d.chars().count() > 48 {
            return false;
        }
        match lang {
            SupportedLanguage::Rust => {
                let inner = d.trim_start_matches("#[").trim_end_matches(']').trim();
                inner == "must_use"
                    || inner.starts_with("must_use(")
                    || inner.starts_with("must_use ")
                    || inner.starts_with("must_use=")
            }
            SupportedLanguage::Python => {
                let head = d.split('(').next().unwrap_or(d).trim();
                matches!(
                    head,
                    "@staticmethod"
                        | "@classmethod"
                        | "@property"
                        | "@abstractmethod"
                        | "@dataclass"
                        | "@contextmanager"
                        | "@asynccontextmanager"
                        | "@lru_cache"
                        | "@cache"
                        | "@cached_property"
                        | "@login_required"
                        | "@transaction.atomic"
                ) || head.ends_with(".setter")
                    || head.ends_with(".deleter")
                    || head.starts_with("@app.")
                    || head.starts_with("@router.")
            }
            SupportedLanguage::Java => {
                let head = d.split('(').next().unwrap_or(d).trim();
                matches!(
                    head,
                    "@Transactional"
                        | "@Async"
                        | "@Scheduled"
                        | "@Synchronized"
                        | "@Nullable"
                        | "@NonNull"
                        | "@NotNull"
                        | "@GetMapping"
                        | "@PostMapping"
                        | "@PutMapping"
                        | "@DeleteMapping"
                        | "@PatchMapping"
                        | "@RequestMapping"
                        | "@PreAuthorize"
                        | "@Secured"
                        | "@RolesAllowed"
                        | "@Cacheable"
                        | "@Bean"
                )
            }
            SupportedLanguage::CSharp => {
                let inner = d.trim_start_matches('[').trim_end_matches(']').trim();
                split_top_level_attr_items(inner)
                    .into_iter()
                    .any(is_key_csharp_attr_item)
            }
            SupportedLanguage::TypeScript
            | SupportedLanguage::Tsx
            | SupportedLanguage::JavaScript => {
                let head = d.split('(').next().unwrap_or(d).trim();
                matches!(
                    head,
                    "@Get"
                        | "@Post"
                        | "@Put"
                        | "@Delete"
                        | "@Patch"
                        | "@Controller"
                        | "@Injectable"
                        | "@UseGuards"
                        | "@Transactional"
                        | "@Auth"
                        | "@Roles"
                        | "@computed"
                        | "@action"
                        | "@observable"
                )
            }
            _ => false,
        }
    };

    let mut kept = Vec::new();
    let mut total_chars = 0usize;
    for d in raw_decs {
        if is_key_dec(&d) {
            let len = d.chars().count();
            if kept.len() >= 2 || total_chars + len > 48 {
                break;
            }
            kept.push(d);
            total_chars += len + 1;
        }
    }
    kept
}

/// Builds a compacted signature directly from AST byte slices (`sig_start..p_start`,
/// AST parameter children, `p_end..body_start`) without reverse-searching `combined`.
fn compact_signature_from_ast_slices(
    lang: SupportedLanguage,
    node: &tree_sitter::Node,
    parent_prefix: Option<&str>,
    decorators: &[String],
    source: &str,
    opts: SigFormatOpts,
) -> Option<String> {
    let (target, sig_start, body_start) = find_ast_signature_bounds(lang, node, source)?;
    let params_node = find_ast_parameters_node(lang, &target)?;
    let p_start = params_node.start_byte();
    let p_end = params_node.end_byte();
    if p_start < sig_start || p_start >= p_end || p_end > body_start {
        return None;
    }

    let allow_rust_lifetime = lang == SupportedLanguage::Rust;
    let allow_backtick_escapes = lang != SupportedLanguage::Go;

    let clean_params = strip_ast_comments_in_range(
        source,
        &params_node,
        p_start,
        p_end,
        allow_rust_lifetime,
        allow_backtick_escapes,
    );
    if !clean_params.starts_with('(') || !clean_params.ends_with(')') {
        return None;
    }
    let inner_trimmed = clean_params[1..clean_params.len() - 1].trim();
    if inner_trimmed.is_empty() {
        return None;
    }

    let before_clean = strip_ast_comments_in_range(
        source,
        &target,
        sig_start,
        p_start,
        allow_rust_lifetime,
        allow_backtick_escapes,
    );
    let before_raw = source.get(sig_start..p_start).unwrap_or("");
    let need_space_before_paren =
        !before_clean.is_empty() && before_raw.ends_with(|c: char| c.is_whitespace());

    let after_clean = strip_ast_comments_in_range(
        source,
        &target,
        p_end,
        body_start,
        allow_rust_lifetime,
        allow_backtick_escapes,
    );
    let after_raw = source.get(p_end..body_start).unwrap_or("");
    let need_space_after_paren = !after_clean.is_empty()
        && !after_clean.starts_with([':', ',', ';', ')'])
        && (after_raw.starts_with(|c: char| c.is_whitespace())
            || after_raw.starts_with("/*")
            || after_raw.starts_with("//")
            || after_clean.starts_with("->")
            || after_clean.starts_with("=>"));

    let open_brace_suffix =
        if source.as_bytes().get(body_start) == Some(&b'{') && !after_clean.ends_with('{') {
            " {"
        } else {
            ""
        };

    let mut before_paren = String::new();
    if let Some(p) = parent_prefix {
        before_paren.push_str(p);
        before_paren.push_str(opts.scope_sep);
    }
    for dec in decorators {
        if !sig_contains_decorator(&before_clean, dec) {
            before_paren.push_str(dec);
            before_paren.push(' ');
        }
    }
    before_paren.push_str(&before_clean);
    if need_space_before_paren {
        before_paren.push(' ');
    }
    before_paren.push('(');

    let mut after_paren = String::from(")");
    if need_space_after_paren {
        after_paren.push(' ');
    }
    after_paren.push_str(&after_clean);
    after_paren.push_str(open_brace_suffix);

    let mut param_children = Vec::new();
    let mut cursor = params_node.walk();
    for ch in params_node.named_children(&mut cursor) {
        if !ch.kind().contains("comment") {
            param_children.push(ch);
        }
    }

    if opts.greedy_params && !param_children.is_empty() {
        let mut packed = Vec::new();
        let mut truncated = false;
        for (idx, p_node) in param_children.iter().enumerate() {
            let p_text = strip_ast_comments_in_range(
                source,
                p_node,
                p_node.start_byte(),
                p_node.end_byte(),
                allow_rust_lifetime,
                allow_backtick_escapes,
            );
            let p_trim = p_text.trim();
            if p_trim.is_empty() {
                continue;
            }
            let mut trial = packed.clone();
            trial.push(p_trim.to_string());
            let has_more = idx + 1 < param_children.len();
            let candidate = if has_more {
                format!("{before_paren}{}, ...{after_paren}", trial.join(", "))
            } else {
                format!("{before_paren}{}{after_paren}", trial.join(", "))
            };
            if candidate.chars().count() <= opts.max_chars {
                packed = trial;
            } else {
                truncated = true;
                break;
            }
        }
        if !packed.is_empty() {
            let res = if truncated {
                format!("{before_paren}{}, ...{after_paren}", packed.join(", "))
            } else {
                format!("{before_paren}{}{after_paren}", packed.join(", "))
            };
            if res.chars().count() <= opts.max_chars {
                return Some(res);
            }
        }
    }

    let first_ast_param = param_children.first().map(|first| {
        strip_ast_comments_in_range(
            source,
            first,
            first.start_byte(),
            first.end_byte(),
            allow_rust_lifetime,
            allow_backtick_escapes,
        )
    });
    let has_more = param_children.len() > 1;

    if let Some(first_p) = first_ast_param
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let c1 = if first_p.chars().count() < 35 {
            if has_more {
                format!("{before_paren}{first_p}, ...{after_paren}")
            } else {
                format!("{before_paren}{first_p}{after_paren}")
            }
        } else {
            format!("{before_paren}...{after_paren}")
        };
        if c1.chars().count() <= opts.max_chars {
            return Some(c1);
        }
    }
    let c2 = format!("{before_paren}...{after_paren}");
    if c2.chars().count() <= opts.max_chars {
        return Some(c2);
    }
    None
}

pub(super) fn extract_signature(
    lang: SupportedLanguage,
    node: &tree_sitter::Node,
    lines: &[&str],
    source: &str,
) -> Option<String> {
    if let Some((target, sig_start, body_start)) = find_ast_signature_bounds(lang, node, source) {
        let allow_rust_lifetime = lang == SupportedLanguage::Rust;
        let allow_backtick_escapes = lang != SupportedLanguage::Go;
        let cleaned = strip_ast_comments_in_range(
            source,
            &target,
            sig_start,
            body_start,
            allow_rust_lifetime,
            allow_backtick_escapes,
        );
        let cleaned_trimmed = cleaned.trim();
        if !cleaned_trimmed.is_empty() {
            let first_body_byte = source.as_bytes().get(body_start).copied();
            let full_sig = if first_body_byte == Some(b'{') && !cleaned_trimmed.ends_with('{') {
                format!("{cleaned_trimmed} {{")
            } else {
                cleaned_trimmed.to_string()
            };
            return Some(full_sig);
        }
    }

    let target = if node.kind() == "decorated_definition" {
        node.child_by_field_name("definition").unwrap_or(*node)
    } else {
        *node
    };
    let start_row = target.start_position().row;
    let end_row = (target.end_position().row + 1).min(lines.len());
    let allow_hash = matches!(
        lang,
        SupportedLanguage::Python | SupportedLanguage::Bash | SupportedLanguage::Ruby
    );
    let allow_rust_lifetime = lang == SupportedLanguage::Rust;
    let allow_backtick_escapes = lang != SupportedLanguage::Go;
    let allow_csharp_verbatim = lang == SupportedLanguage::CSharp;
    extract_signature_from_row_with_limit_raw(
        start_row,
        end_row,
        lines,
        Some(allow_hash),
        Some(allow_rust_lifetime),
        allow_backtick_escapes,
        allow_csharp_verbatim,
    )
}

/// Formats and compacts `parent_scope > sig` within `max_chars`, using AST byte slices and AST
/// parameter children first (when available and paren-delimited) before falling back to heuristic
/// scanning.
pub(super) fn compact_definition_signature_with_ast(
    lang: SupportedLanguage,
    node: &tree_sitter::Node,
    parent_scope: Option<&str>,
    raw_sig: Option<&str>,
    source: &str,
    max_chars: usize,
) -> Option<String> {
    compact_definition_signature_with_ast_opts(
        lang,
        node,
        parent_scope,
        raw_sig,
        source,
        SigFormatOpts {
            max_chars,
            scope_sep: " > ",
            preserve_key_decorators: false,
            greedy_params: false,
        },
    )
}

pub(super) fn compact_definition_signature_with_ast_opts(
    lang: SupportedLanguage,
    node: &tree_sitter::Node,
    parent_scope: Option<&str>,
    raw_sig: Option<&str>,
    source: &str,
    opts: SigFormatOpts,
) -> Option<String> {
    let allow_rust_lifetime = lang == SupportedLanguage::Rust;
    let allow_backtick_escapes = lang != SupportedLanguage::Go;

    let dec_list = if opts.preserve_key_decorators {
        extract_key_decorators_list(lang, node, source)
    } else {
        Vec::new()
    };

    let decorated_raw = raw_sig.map(|s| {
        // Inspect only the header before the parameter list so parameter annotations
        // (e.g. `(@Nullable String key)`) never suppress a matching method annotation.
        let header_slice =
            match find_parameter_list_span_impl(s, allow_rust_lifetime, allow_backtick_escapes) {
                Some((open, _)) => &s[..open],
                None => s,
            };
        let missing: Vec<&str> = dec_list
            .iter()
            .map(String::as_str)
            .filter(|d| !sig_contains_decorator(header_slice, d))
            .collect();
        if missing.is_empty() {
            s.to_string()
        } else {
            format!("{} {s}", missing.join(" "))
        }
    });

    let (combined, parent_short) = match (parent_scope, decorated_raw.as_deref()) {
        (Some(p), Some(s)) => {
            let p_short = truncate_preview(p, 45);
            let combined = format!("{p_short}{}{s}", opts.scope_sep);
            (combined, Some(p_short))
        }
        (None, Some(s)) => (s.to_string(), None),
        (Some(p), None) => (p.to_string(), None),
        (None, None) => return None,
    };

    if combined.chars().count() <= opts.max_chars {
        return Some(combined);
    }

    if raw_sig.is_some() {
        if let Some(compacted) = compact_signature_from_ast_slices(
            lang,
            node,
            parent_short.as_deref(),
            &dec_list,
            source,
            opts,
        ) {
            return Some(compacted);
        }
    }

    Some(compact_signature_preview_with_opts(
        &combined,
        opts.max_chars,
        allow_rust_lifetime,
        allow_backtick_escapes,
    ))
}

/// Extracts a parent scope container prefix (e.g. `impl MentionPopup`, `class Parser`, `namespace utils`).
pub(super) fn extract_parent_scope_prefix(
    lang: SupportedLanguage,
    node: &tree_sitter::Node,
    lines: &[&str],
    source: &str,
) -> Option<String> {
    let mut curr = node.parent();
    while let Some(ancestor) = curr {
        let kind = ancestor.kind();
        match lang {
            SupportedLanguage::Rust => {
                if kind == "impl_item" || kind == "trait_item" || kind == "mod_item" {
                    if let Some(body) = ancestor.child_by_field_name("body") {
                        let start = ancestor.start_byte();
                        let end = body.start_byte();
                        if start < end && end <= source.len() {
                            let cleaned = strip_ast_comments_in_range(
                                source, &ancestor, start, end, true, true,
                            );
                            if !cleaned.is_empty() {
                                return Some(cleaned);
                            }
                        }
                    }
                    // Fallback with comment/string-safe CodeScanner across up to 20 lines
                    let start_row = ancestor.start_position().row;
                    let end_bound = (start_row + 20).min(lines.len());
                    let mut parts = Vec::new();
                    let mut scanner = CodeScanner::for_lang(SupportedLanguage::Rust);
                    for r in start_row..end_bound {
                        if let Some(line) = lines.get(r) {
                            let clean = strip_comments_preserving_code(line, &mut scanner, false);
                            if clean.is_empty() {
                                continue;
                            }
                            let mut brace_at = None;
                            let mut brace_scanner = SigScanState::new(true);
                            let (_, fb) = brace_scanner.scan_line(&clean);
                            if let Some(pos) = fb {
                                brace_at = Some(pos);
                            }
                            if let Some(pos) = brace_at {
                                let before = clean[..pos].trim();
                                if !before.is_empty() {
                                    parts.push(before.to_string());
                                }
                                break;
                            } else {
                                parts.push(clean);
                            }
                        }
                    }
                    if !parts.is_empty() {
                        let mut cleaned = parts.join(" ");
                        while cleaned.contains("  ") {
                            cleaned = cleaned.replace("  ", " ");
                        }
                        return Some(cleaned);
                    }
                }
            }
            SupportedLanguage::Python => {
                if kind == "class_definition" {
                    if let Some(name_node) = ancestor.child_by_field_name("name") {
                        if let Some(name) = get_node_text_same_line(&name_node, lines) {
                            return Some(format!("class {name}"));
                        }
                    }
                }
            }
            SupportedLanguage::Ruby => {
                if matches!(kind, "class" | "module") {
                    if let Some(name_node) = ancestor.child_by_field_name("name") {
                        if let Some(name) = get_node_text_same_line(&name_node, lines) {
                            return Some(format!("{kind} {name}"));
                        }
                    }
                }
            }
            SupportedLanguage::JavaScript
            | SupportedLanguage::TypeScript
            | SupportedLanguage::Tsx => {
                if matches!(
                    kind,
                    "class_declaration"
                        | "abstract_class_declaration"
                        | "interface_declaration"
                        | "internal_module"
                        | "class"
                ) {
                    if let Some(name_node) = ancestor.child_by_field_name("name") {
                        if let Some(name) = get_node_text_same_line(&name_node, lines) {
                            let keyword = match kind {
                                "abstract_class_declaration" => "abstract class",
                                "interface_declaration" => "interface",
                                "internal_module" => "namespace",
                                _ => "class",
                            };
                            return Some(format!("{keyword} {name}"));
                        }
                    } else if kind == "class" {
                        // Anonymous class expression assigned to a variable (`const Foo = class { ... }`)
                        if let Some(parent) = ancestor.parent() {
                            if parent.kind() == "variable_declarator" {
                                if let Some(name_node) = parent.child_by_field_name("name") {
                                    if let Some(name) = get_node_text_same_line(&name_node, lines) {
                                        return Some(format!("class {name}"));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            SupportedLanguage::Java | SupportedLanguage::CSharp => {
                if matches!(
                    kind,
                    "class_declaration"
                        | "interface_declaration"
                        | "record_declaration"
                        | "struct_declaration"
                ) {
                    if let Some(name_node) = ancestor.child_by_field_name("name") {
                        if let Some(name) = get_node_text_same_line(&name_node, lines) {
                            let keyword = match kind {
                                "interface_declaration" => "interface",
                                "record_declaration" => "record",
                                "struct_declaration" => "struct",
                                _ => "class",
                            };
                            return Some(format!("{keyword} {name}"));
                        }
                    }
                }
            }
            SupportedLanguage::Cpp | SupportedLanguage::C => {
                if matches!(
                    kind,
                    "class_specifier" | "struct_specifier" | "namespace_definition"
                ) {
                    if let Some(name_node) = ancestor.child_by_field_name("name") {
                        if let Some(name) = get_node_text_same_line(&name_node, lines) {
                            let keyword = match kind {
                                "namespace_definition" => "namespace",
                                "struct_specifier" => "struct",
                                _ => "class",
                            };
                            return Some(format!("{keyword} {name}"));
                        }
                    }
                }
            }
            _ => {}
        }
        curr = ancestor.parent();
    }
    None
}
