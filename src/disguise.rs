//! Disguise layer: rewrites requests bound for `/v1/messages*` so they
//! present as Claude Code (OAuth Bearer + claude-code-* betas + a
//! billing-header prefix in the `system` field).
//!
//! Only kicks in when the inbound request has no `Authorization` and no
//! `x-api-key` — clients that already authenticate are passed through
//! untouched, on the assumption they know what they're doing. This
//! lets old-mur (which still does its own disguise) and new-mur (which
//! sends nothing) coexist behind the same proxy.
//!
//! `cc_version` is auto-detected at runtime via [`crate::cc_version`].

use anyhow::Context;
use serde_json::{Value, json};

/// Beta capabilities Claude Code claims when authenticating via OAuth.
pub const OAUTH_BETAS: &str =
    "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,compact-2026-01-12";

/// Merge `OAUTH_BETAS` with whatever `anthropic-beta` values the client
/// already sent, preserving order (OAuth betas first) and deduping. The
/// client's betas describe body features the request uses (e.g.
/// `clear-thinking-2025-10-15`). Stripping them in disguise mode causes
/// the upstream to reject body shapes it would otherwise accept.
pub fn merge_betas(oauth: &str, client: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let client_iter = client.iter().flat_map(|s| s.split(','));
    for token in oauth.split(',').chain(client_iter) {
        let t = token.trim();
        if t.is_empty() {
            continue;
        }
        if seen.insert(t.to_string()) {
            out.push(t.to_string());
        }
    }
    out.join(",")
}

/// Build the billing-header text block for a given Claude Code version.
/// Without this prefix the OAuth path returns 429 rate_limit_error.
pub fn billing_prefix(cc_version: &str) -> String {
    format!("x-anthropic-billing-header: cc_version={cc_version}; cc_entrypoint=sdk-cli;")
}

/// True if `path` is one of the Anthropic Messages endpoints we disguise on.
pub fn should_disguise(path: &str) -> bool {
    matches!(path, "/v1/messages" | "/v1/messages/count_tokens")
        || path.starts_with("/v1/messages?")
        || path.starts_with("/v1/messages/count_tokens?")
}

/// Inject the billing-header text block (built from `cc_version`) at the
/// head of the request's `system` field. Anthropic accepts `system` as
/// either a string, an array of content blocks, or absent — handle all three.
///
/// Returns the rewritten body. On non-JSON or unrecognized shape, returns
/// the original bytes unchanged so we don't corrupt requests we don't
/// understand.
pub fn inject_billing_prefix(body: &[u8], cc_version: &str) -> anyhow::Result<Vec<u8>> {
    let mut value: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Ok(body.to_vec()),
    };

    let Some(obj) = value.as_object_mut() else {
        return Ok(body.to_vec());
    };

    let prefix = billing_prefix(cc_version);

    match obj.get_mut("system") {
        Some(Value::String(s)) => {
            let original = std::mem::take(s);
            obj.insert(
                "system".into(),
                json!([
                    {"type": "text", "text": prefix},
                    {"type": "text", "text": original},
                ]),
            );
        }
        Some(Value::Array(arr)) => {
            arr.insert(0, json!({"type": "text", "text": prefix}));
        }
        Some(_) => {
            // Unknown shape (object/number/bool/null) — leave alone.
        }
        None => {
            obj.insert("system".into(), json!(prefix));
        }
    }

    serde_json::to_vec(&value).context("serialize rewritten body")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_VER: &str = "9.9.9";

    fn expected_prefix() -> String {
        billing_prefix(TEST_VER)
    }

    #[test]
    fn injects_into_string_system() {
        let body = br#"{"model":"x","system":"hello","messages":[]}"#;
        let out = inject_billing_prefix(body, TEST_VER).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let arr = v["system"].as_array().expect("array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["text"], expected_prefix());
        assert_eq!(arr[1]["text"], "hello");
    }

    #[test]
    fn injects_into_array_system_preserving_cache_control() {
        let body =
            br#"{"system":[{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]}"#;
        let out = inject_billing_prefix(body, TEST_VER).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let arr = v["system"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["text"], expected_prefix());
        assert_eq!(arr[1]["text"], "hi");
        assert_eq!(arr[1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn injects_when_system_absent() {
        let body = br#"{"model":"x","messages":[]}"#;
        let out = inject_billing_prefix(body, TEST_VER).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["system"], expected_prefix());
    }

    #[test]
    fn passes_through_non_json() {
        let body = b"not json at all";
        let out = inject_billing_prefix(body, TEST_VER).unwrap();
        assert_eq!(out, body);
    }

    #[test]
    fn billing_prefix_includes_version() {
        let p = billing_prefix("2.1.126");
        assert!(p.contains("cc_version=2.1.126"));
        assert!(p.contains("cc_entrypoint=sdk-cli"));
    }

    #[test]
    fn merge_betas_appends_client_values() {
        let merged = merge_betas("a,b", &["c".to_string(), "d".to_string()]);
        assert_eq!(merged, "a,b,c,d");
    }

    #[test]
    fn merge_betas_dedupes_and_preserves_oauth_order() {
        let merged = merge_betas(
            "claude-code-20250219,compact-2026-01-12",
            &["compact-2026-01-12,clear-thinking-2025-10-15".to_string()],
        );
        assert_eq!(
            merged,
            "claude-code-20250219,compact-2026-01-12,clear-thinking-2025-10-15"
        );
    }

    #[test]
    fn merge_betas_handles_empty_client() {
        assert_eq!(merge_betas(OAUTH_BETAS, &[]), OAUTH_BETAS);
    }

    #[test]
    fn merge_betas_trims_whitespace() {
        let merged = merge_betas("a,b", &[" c , d ".to_string()]);
        assert_eq!(merged, "a,b,c,d");
    }

    #[test]
    fn should_disguise_paths() {
        assert!(should_disguise("/v1/messages"));
        assert!(should_disguise("/v1/messages/count_tokens"));
        assert!(should_disguise("/v1/messages?beta=true"));
        assert!(should_disguise("/v1/messages/count_tokens?x=1"));
        assert!(!should_disguise("/v1/messages/batches"));
        assert!(!should_disguise("/v1/files"));
        assert!(!should_disguise("/"));
    }
}
