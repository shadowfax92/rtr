//! Pure header-rewrite engine and secret redaction.
//!
//! A [`Profile`] is validated once into [`Rewrites`] (header names/values parsed
//! up front) so applying it per-request can't fail mid-flight. Rewrites only
//! apply to requests whose host is one of the tool's configured targets.

use anyhow::{Context, Result};
use http::header::{HeaderMap, HeaderName, HeaderValue};

use crate::config::Profile;

#[derive(Debug, Default, Clone)]
pub struct Rewrites {
    set: Vec<(HeaderName, HeaderValue)>,
    remove: Vec<HeaderName>,
}

impl Rewrites {
    /// Parse and validate a profile. Fails fast on a malformed header name or
    /// value so misconfiguration surfaces at run start, not in the hot path.
    pub fn from_profile(profile: &Profile) -> Result<Self> {
        let mut set = Vec::with_capacity(profile.set.len());
        for (name, value) in &profile.set {
            let hn = HeaderName::from_bytes(name.as_bytes())
                .with_context(|| format!("invalid header name '{name}'"))?;
            let hv = HeaderValue::from_str(value)
                .with_context(|| format!("invalid value for header '{name}'"))?;
            set.push((hn, hv));
        }
        let mut remove = Vec::with_capacity(profile.remove.len());
        for name in &profile.remove {
            remove.push(
                HeaderName::from_bytes(name.as_bytes())
                    .with_context(|| format!("invalid header name '{name}'"))?,
            );
        }
        Ok(Self { set, remove })
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.remove.is_empty()
    }

    /// Apply set (overwrite/add) then remove. Infallible by construction.
    pub fn apply(&self, headers: &mut HeaderMap) {
        for (name, value) in &self.set {
            headers.insert(name.clone(), value.clone());
        }
        for name in &self.remove {
            headers.remove(name);
        }
    }
}

/// Case-insensitive exact host match against the tool's target hosts.
pub fn host_matches(host: &str, hosts: &[String]) -> bool {
    hosts.iter().any(|h| h.eq_ignore_ascii_case(host))
}

/// Apply rewrites only if `host` is a target. Returns whether they were applied.
pub fn rewrite_request_headers(
    headers: &mut HeaderMap,
    host: &str,
    hosts: &[String],
    rewrites: &Rewrites,
) -> bool {
    if host_matches(host, hosts) {
        rewrites.apply(headers);
        true
    } else {
        false
    }
}

/// Header names whose values are treated as secrets in terminal output.
pub fn is_secret_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "authorization"
        || n == "cookie"
        || n == "set-cookie"
        || n == "proxy-authorization"
        || n.contains("api-key")
        || n.contains("apikey")
        || n.contains("token")
        || n.contains("secret")
}

/// Redacted display form of a header value. Keeps a leading scheme word
/// (e.g. `Bearer`) so the shape is visible, but never the credential itself.
pub fn redact_value(name: &str, value: &str) -> String {
    if !is_secret_header(name) {
        return value.to_string();
    }
    match value.split_once(' ') {
        Some((scheme, _)) if !scheme.is_empty() => format!("{scheme} «redacted»"),
        _ => "«redacted»".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Profile;

    fn profile(set: &[(&str, &str)], remove: &[&str]) -> Profile {
        Profile {
            set: set.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            remove: remove.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn set_overwrites_and_adds() {
        let rw = Rewrites::from_profile(&profile(
            &[("authorization", "Bearer NEW"), ("x-extra", "1")],
            &[],
        ))
        .unwrap();
        let mut h = headers(&[("authorization", "Bearer OLD")]);
        rw.apply(&mut h);
        assert_eq!(h.get("authorization").unwrap(), "Bearer NEW");
        assert_eq!(h.get("x-extra").unwrap(), "1");
    }

    #[test]
    fn remove_deletes_present_noop_absent() {
        let rw = Rewrites::from_profile(&profile(&[], &["x-trace", "x-missing"])).unwrap();
        let mut h = headers(&[("x-trace", "abc"), ("keep", "yes")]);
        rw.apply(&mut h);
        assert!(h.get("x-trace").is_none());
        assert_eq!(h.get("keep").unwrap(), "yes");
    }

    #[test]
    fn rewrites_apply_only_to_target_host() {
        let rw = Rewrites::from_profile(&profile(&[("authorization", "Bearer NEW")], &[])).unwrap();
        let hosts = vec!["api.openai.com".to_string()];

        let mut on_target = headers(&[("authorization", "Bearer OLD")]);
        assert!(rewrite_request_headers(&mut on_target, "api.openai.com", &hosts, &rw));
        assert_eq!(on_target.get("authorization").unwrap(), "Bearer NEW");

        let mut off_target = headers(&[("authorization", "Bearer OLD")]);
        assert!(!rewrite_request_headers(&mut off_target, "example.com", &hosts, &rw));
        assert_eq!(off_target.get("authorization").unwrap(), "Bearer OLD");
    }

    #[test]
    fn host_match_is_case_insensitive() {
        let hosts = vec!["API.OpenAI.com".to_string()];
        assert!(host_matches("api.openai.com", &hosts));
        assert!(!host_matches("api.openai.com.evil.com", &hosts));
    }

    #[test]
    fn invalid_header_name_fails_validation() {
        assert!(Rewrites::from_profile(&profile(&[("bad header", "x")], &[])).is_err());
    }

    #[test]
    fn redaction_masks_secrets_keeps_scheme_and_leaves_plain_visible() {
        assert_eq!(redact_value("authorization", "Bearer sk-proj-123"), "Bearer «redacted»");
        assert_eq!(redact_value("x-api-key", "sk-abc"), "«redacted»");
        assert_eq!(redact_value("content-type", "application/json"), "application/json");
        assert!(is_secret_header("Authorization"));
        assert!(is_secret_header("OpenAI-Api-Key"));
        assert!(!is_secret_header("content-type"));
    }
}
