//! Wire-level tool_result compression: rewrites `/v1/messages` request
//! bodies through mur-compress before they leave for the upstream.
//! Fail-open — any parse or engine failure forwards the original bytes.

/// Paths whose bodies we compress: message sends only. `count_tokens` is
/// deliberately excluded — its body never reaches the model, and it runs on
/// a hot path. The client over-counting context (vs the compressed send) is
/// the fail-safe direction.
pub fn should_compress(path: &str) -> bool {
    path == "/v1/messages" || path.starts_with("/v1/messages?")
}

/// True if `text` already carries a mur-compress retrieval marker
/// (`hash=` followed by 16+ hex chars). Over-matching is fail-safe: a
/// skipped block just stays uncompressed.
pub fn has_retrieve_marker(text: &str) -> bool {
    let mut rest = text;
    while let Some(i) = rest.find("hash=") {
        let tail = &rest[i + 5..];
        let hex_start = tail.strip_prefix('"').unwrap_or(tail);
        if hex_start
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .count()
            >= 16
        {
            return true;
        }
        rest = tail;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_compress_matches_messages_send_only() {
        assert!(should_compress("/v1/messages"));
        assert!(should_compress("/v1/messages?beta=true"));
        assert!(!should_compress("/v1/messages/count_tokens"));
        assert!(!should_compress("/v1/models"));
        assert!(!should_compress("/"));
    }

    #[test]
    fn marker_detection() {
        assert!(has_retrieve_marker(
            "[408 items compressed to 312. Retrieve more: hash=365700af5c32b04c0a4b0443]"
        ));
        assert!(has_retrieve_marker(
            "Call mur_retrieve with hash=\"e5f97460c423350e9d59d79e\" for the full result."
        ));
        // short hex after hash= is not a marker (e.g. a URL param)
        assert!(!has_retrieve_marker("GET /commit?hash=deadbeef done"));
        assert!(!has_retrieve_marker("no marker here"));
        // hash= at end of string must not panic
        assert!(!has_retrieve_marker("trailing hash="));
    }
}
