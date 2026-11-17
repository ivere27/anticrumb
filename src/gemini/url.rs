use crate::url_lite::Url;

pub(crate) fn is_gemini_origin(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("gemini.google.com")
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
}

pub(crate) fn is_gemini_page_url(raw: &str) -> bool {
    Url::parse(raw)
        .ok()
        .is_some_and(|url| is_gemini_origin(&url))
}

pub fn is_reusable_gemini_url(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    if !is_gemini_origin(&url) {
        return false;
    }
    let segments: Vec<_> = url.path_segments().into_iter().flatten().collect();
    matches!(segments.as_slice(), [] | ["app"] | ["app", _])
        || matches!(
            segments.as_slice(),
            ["u", account, "app"] | ["u", account, "app", _]
                if !account.is_empty() && account.bytes().all(|b| b.is_ascii_digit())
        )
}

pub(crate) fn url_has_chat_id(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    let segments: Vec<_> = url.path_segments().into_iter().flatten().collect();
    matches!(segments.as_slice(), ["app", id] if !id.is_empty())
        || matches!(
            segments.as_slice(),
            ["u", account, "app", id]
                if !account.is_empty() && account.bytes().all(|b| b.is_ascii_digit()) && !id.is_empty()
        )
}

pub(crate) fn is_valid_chat_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub(crate) fn parse_gemini_chat_url(raw: &str) -> std::result::Result<Url, &'static str> {
    let url = Url::parse(raw).map_err(|_| "invalid recorded conversation URL")?;
    let segments: Vec<_> = url.path_segments().into_iter().flatten().collect();
    let id = match segments.as_slice() {
        ["app", id] => Some(*id),
        ["u", account, "app", id]
            if account.bytes().all(|b| b.is_ascii_digit()) && !account.is_empty() =>
        {
            Some(*id)
        }
        _ => None,
    };
    if !is_gemini_origin(&url) || !id.is_some_and(is_valid_chat_id) {
        return Err("invalid gemini chat URL");
    }
    Ok(url)
}

pub(crate) fn account_scope(url: &Url) -> Option<String> {
    let segments: Vec<_> = url.path_segments()?.collect();
    let path_scope = match segments.as_slice() {
        ["app"] | ["app", _] => None,
        ["u", account, "app"] | ["u", account, "app", _]
            if !account.is_empty() && account.bytes().all(|b| b.is_ascii_digit()) =>
        {
            Some(*account)
        }
        _ => return None,
    };
    let query_scope = url
        .query_pairs()
        .find(|(key, _)| key == "authuser")
        .map(|(_, value)| value.into_owned());
    if let (Some(path), Some(query)) = (path_scope, query_scope.as_deref()) {
        if path != query {
            return None;
        }
    }
    Some(
        query_scope
            .or_else(|| path_scope.map(str::to_owned))
            .unwrap_or_else(|| "0".into()),
    )
}

pub(crate) fn rpc_account_scope(url: &Url) -> Option<String> {
    if !is_gemini_origin(url) {
        return None;
    }
    let parts: Vec<_> = url.path_segments()?.collect();
    let path_scope = if parts.first() == Some(&"u") {
        parts.get(1).copied()
    } else {
        None
    };
    let query_scope = url
        .query_pairs()
        .find(|(key, _)| key == "authuser")
        .map(|(_, value)| value.into_owned());
    if path_scope
        .zip(query_scope.as_deref())
        .is_some_and(|(a, b)| a != b)
    {
        return None;
    }
    Some(
        query_scope
            .or_else(|| path_scope.map(str::to_owned))
            .unwrap_or_else(|| "0".into()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reusable_gemini_url_and_chat_id_match_expected_paths() {
        assert!(is_reusable_gemini_url("https://gemini.google.com/app"));
        assert!(is_reusable_gemini_url(
            "https://gemini.google.com/app/abc123"
        ));
        assert!(is_reusable_gemini_url("https://gemini.google.com/u/0/app"));
        assert!(is_reusable_gemini_url(
            "https://gemini.google.com/u/1/app/chat_99"
        ));
        assert!(!is_reusable_gemini_url("https://gemini.google.com/sorry"));
        assert!(!is_reusable_gemini_url("https://gemini.google.com/usage"));
        assert!(!is_reusable_gemini_url(
            "https://accounts.google.com/ServiceLogin"
        ));
        assert!(!is_reusable_gemini_url("about:blank"));

        assert!(!url_has_chat_id("https://gemini.google.com/app"));
        assert!(!url_has_chat_id("https://gemini.google.com/u/0/app"));
        assert!(url_has_chat_id("https://gemini.google.com/app/abc123"));
        assert!(url_has_chat_id("https://gemini.google.com/u/0/app/abc123"));
    }
}
