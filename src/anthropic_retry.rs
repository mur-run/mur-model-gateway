//! Bounded observation of an externally rotated Anthropic credential.

use crate::{TokenSource, keychain};
use std::time::Duration;

const ROTATION_WAIT: Duration = Duration::from_millis(500);
const ROTATION_POLL: Duration = Duration::from_millis(50);

/// Never refresh credentials or resend a rejected token. The owner may still
/// be publishing its replacement when upstream returns the first 401.
pub(crate) async fn wait_for_rotation(source: &TokenSource, rejected: &str) -> Option<String> {
    let deadline = tokio::time::Instant::now() + ROTATION_WAIT;
    loop {
        if let Ok(Some(stored)) = source.resolve()
            && stored != rejected
        {
            return Some(stored);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep_until((tokio::time::Instant::now() + ROTATION_POLL).min(deadline)).await;
        // A normal keychain read is memoised. Without this, every poll would
        // observe the same rejected token even after the owner replaces it.
        if matches!(source, TokenSource::Keychain) {
            keychain::invalidate_cache();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn observes_delayed_file_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        std::fs::write(&path, r#"{"claudeAiOauth":{"accessToken":"old"}}"#).unwrap();
        let source = TokenSource::CredentialsFile(path.clone());
        let writer = async {
            tokio::time::sleep(ROTATION_POLL * 2).await;
            std::fs::write(&path, r#"{"claudeAiOauth":{"accessToken":"new"}}"#).unwrap();
        };
        let (rotated, ()) = tokio::join!(wait_for_rotation(&source, "old"), writer);
        assert_eq!(rotated.as_deref(), Some("new"));
    }

    #[tokio::test]
    async fn unchanged_token_exhausts_bounded_window() {
        let start = tokio::time::Instant::now();
        let source = TokenSource::Static(std::sync::Arc::new("old".to_owned()));
        assert_eq!(wait_for_rotation(&source, "old").await, None);
        assert!(start.elapsed() >= ROTATION_WAIT);
    }
}
