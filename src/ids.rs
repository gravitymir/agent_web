//! Validation for client-supplied identifiers that become file paths or CLI
//! arguments. Every point that turns a `session_id`/`id` into a path
//! (`load_chat`, `set_meta`, `delete_chat`, `agent::store::path`, the keeper
//! spawn) or a CLI arg (`--session-id`/`--resume`/`--model`) must gate on these,
//! or a crafted value (`..\..\`, `x & calc`, …) can traverse paths or inject
//! commands via `cmd.exe`.

/// A session id is always a UUID — both the Claude CLI and the native store mint
/// `uuid::Uuid::new_v4()`. Validating strictly as a UUID admits only hex + dashes,
/// which closes path-traversal and `cmd.exe`-metacharacter vectors at once.
pub fn is_valid_session_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok()
}

/// A model alias/name: letters, digits and `-._:` only (e.g. `opus`,
/// `claude-opus-4-8`, `kimi-k2.7-code`). Blocks `cmd.exe` metacharacters
/// (`&`, `|`, `%`, quotes, spaces) so `model` can't inject a command.
pub fn is_valid_model(m: &str) -> bool {
    !m.is_empty()
        && m.len() <= 64
        && m.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ':'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids() {
        assert!(is_valid_session_id("6fe5f1f6-0e93-48a6-9ca0-ca0c6dce90dd"));
        assert!(!is_valid_session_id("../../etc/passwd"));
        assert!(!is_valid_session_id(r"..\..\secret"));
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("not-a-uuid"));
    }

    #[test]
    fn models() {
        assert!(is_valid_model("opus"));
        assert!(is_valid_model("claude-opus-4-8"));
        assert!(is_valid_model("kimi-k2.7-code"));
        assert!(!is_valid_model("x & calc"));
        assert!(!is_valid_model("a|b"));
        assert!(!is_valid_model("%PATH%"));
        assert!(!is_valid_model(""));
    }
}
