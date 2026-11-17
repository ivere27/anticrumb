//! Provider manifests and DOM scripts shipped in the single executable.

pub const IDS: &[&str] = &["gemini-web", "claude-web", "chatgpt-web"];

pub fn manifest(id: &str) -> Option<&'static str> {
    match id {
        "gemini-web" => Some(include_str!("../../providers/gemini-web/provider.json")),
        "claude-web" => Some(include_str!("../../providers/claude-web/provider.json")),
        "chatgpt-web" => Some(include_str!("../../providers/chatgpt-web/provider.json")),
        _ => None,
    }
}

pub fn script(id: &str, name: &str) -> Option<&'static str> {
    match (id, name) {
        ("gemini-web", "driver.js") => {
            Some(include_str!("../../providers/gemini-web/scripts/driver.js"))
        }
        ("gemini-web", "observer.js") => Some(include_str!(
            "../../providers/gemini-web/scripts/observer.js"
        )),
        ("gemini-web", "extract-response.js") => Some(include_str!(
            "../../providers/gemini-web/scripts/extract-response.js"
        )),
        ("gemini-web", "usage.js") => {
            Some(include_str!("../../providers/gemini-web/scripts/usage.js"))
        }
        ("gemini-web", "delete-conversation.js") => Some(include_str!(
            "../../providers/gemini-web/scripts/delete-conversation.js"
        )),
        ("gemini-web", "list-conversations.js") => Some(include_str!(
            "../../providers/gemini-web/scripts/list-conversations.js"
        )),
        ("gemini-web", "read-history.js") => Some(include_str!(
            "../../providers/gemini-web/scripts/read-history.js"
        )),
        ("gemini-web", "manifest.json") => Some(include_str!(
            "../../providers/gemini-web/scripts/manifest.json"
        )),
        ("claude-web", "driver.js") => {
            Some(include_str!("../../providers/claude-web/scripts/driver.js"))
        }
        ("claude-web", "observer.js") => Some(include_str!(
            "../../providers/claude-web/scripts/observer.js"
        )),
        ("claude-web", "extract-response.js") => Some(include_str!(
            "../../providers/claude-web/scripts/extract-response.js"
        )),
        ("claude-web", "usage.js") => {
            Some(include_str!("../../providers/claude-web/scripts/usage.js"))
        }
        ("claude-web", "delete-conversation.js") => Some(include_str!(
            "../../providers/claude-web/scripts/delete-conversation.js"
        )),
        ("claude-web", "read-history.js") => Some(include_str!(
            "../../providers/claude-web/scripts/read-history.js"
        )),
        ("chatgpt-web", "driver.js") => Some(include_str!(
            "../../providers/chatgpt-web/scripts/driver.js"
        )),
        ("chatgpt-web", "observer.js") => Some(include_str!(
            "../../providers/chatgpt-web/scripts/observer.js"
        )),
        ("chatgpt-web", "extract-response.js") => Some(include_str!(
            "../../providers/chatgpt-web/scripts/extract-response.js"
        )),
        ("chatgpt-web", "delete-conversation.js") => Some(include_str!(
            "../../providers/chatgpt-web/scripts/delete-conversation.js"
        )),
        ("chatgpt-web", "read-history.js") => Some(include_str!(
            "../../providers/chatgpt-web/scripts/read-history.js"
        )),
        _ => None,
    }
}
