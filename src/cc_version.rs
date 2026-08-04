//! Auto-detect the locally-installed Claude Code version, so the
//! billing-header `cc_version=` claim tracks reality instead of a
//! frozen constant. This is the single most important reason mur-model-gateway
//! exists as a separate component: a hardcoded `cc_version` rots the
//! moment Anthropic enforces a minimum version.
//!
//! Detection runs `claude --version` (~10 ms) and caches for 5 minutes.
//! Falls back to a known-good constant if the binary isn't on PATH.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Last-known-good Claude Code version, used when detection fails.
/// Update opportunistically; auto-detection is the primary mechanism.
pub const FALLBACK_VERSION: &str = "2.1.126";

const CACHE_TTL: Duration = Duration::from_secs(300);

/// Strategy for producing a `cc_version` string.
#[derive(Clone)]
pub enum VersionStrategy {
    /// Run `claude --version` (cached 5 min); fall back on any failure.
    DetectFromClaude,
    /// Always return this exact version. Used for tests and overrides.
    Static(String),
    /// Always return [`FALLBACK_VERSION`]. Useful in environments where
    /// running `claude` is undesirable.
    FallbackOnly,
}

pub struct VersionCache {
    strategy: VersionStrategy,
    cached: Mutex<Option<(String, Instant)>>,
}

impl VersionCache {
    pub fn new(strategy: VersionStrategy) -> Self {
        Self {
            strategy,
            cached: Mutex::new(None),
        }
    }

    /// Production constructor — detect from `claude --version`, fall back on failure.
    pub fn detect_or_fallback() -> Self {
        Self::new(VersionStrategy::DetectFromClaude)
    }

    /// Resolve current version. Cheap on cache hit (Mutex + clone).
    pub fn get(&self) -> String {
        match &self.strategy {
            VersionStrategy::Static(v) => return v.clone(),
            VersionStrategy::FallbackOnly => return FALLBACK_VERSION.to_string(),
            VersionStrategy::DetectFromClaude => {}
        }

        // Cache hit?
        {
            let lock = self.cached.lock().unwrap();
            if let Some((v, t)) = lock.as_ref()
                && t.elapsed() < CACHE_TTL
            {
                return v.clone();
            }
        }

        // Cache miss: detect, store, return.
        let detected = detect_cc_version().unwrap_or_else(|| {
            tracing::warn!(
                fallback = FALLBACK_VERSION,
                "claude --version unavailable; using fallback cc_version"
            );
            FALLBACK_VERSION.to_string()
        });
        tracing::debug!(version = %detected, "cached cc_version");
        *self.cached.lock().unwrap() = Some((detected.clone(), Instant::now()));
        detected
    }
}

/// Spawn `claude --version` and parse the leading semver-ish token.
/// Format observed: `2.1.126 (Claude Code)\n`.
pub fn detect_cc_version() -> Option<String> {
    let output = std::process::Command::new("claude")
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    parse_version(&s)
}

/// Pull the leading dotted-number token out of `claude --version` output.
/// Returns `None` if no recognizable version is found.
pub fn parse_version(raw: &str) -> Option<String> {
    let first = raw.split_whitespace().next()?;
    if first.is_empty() {
        return None;
    }
    // Must look like dotted numerics (`1.2.3`, `2.1.126`, etc.).
    if !first
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
    {
        return None;
    }
    if !first.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(first.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_semver() {
        assert_eq!(
            parse_version("2.1.126 (Claude Code)\n").as_deref(),
            Some("2.1.126")
        );
    }

    #[test]
    fn parses_bare_version() {
        assert_eq!(parse_version("1.0.0").as_deref(), Some("1.0.0"));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("\n"), None);
        assert_eq!(parse_version("hello world"), None);
    }

    #[test]
    fn rejects_letters_in_version_token() {
        // First whitespace-delimited token must be all digit/dot/dash.
        assert_eq!(parse_version("v2.1.0 build"), None);
    }

    #[test]
    fn cache_static_strategy() {
        let c = VersionCache::new(VersionStrategy::Static("9.9.9".into()));
        assert_eq!(c.get(), "9.9.9");
        assert_eq!(c.get(), "9.9.9");
    }

    #[test]
    fn cache_fallback_strategy() {
        let c = VersionCache::new(VersionStrategy::FallbackOnly);
        assert_eq!(c.get(), FALLBACK_VERSION);
    }
}
