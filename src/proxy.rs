//! The MITM proxy: a hudsucker [`HttpHandler`] that intercepts only configured
//! target hosts, records each request's *original* headers (so the real auth
//! header is discoverable), then applies the active profile's rewrites before
//! forwarding upstream.

use std::future::Future;
use std::sync::Arc;

use anyhow::{Context, Result};
use http::header::{HeaderMap, HOST};
use http::Method;
use hudsucker::certificate_authority::RcgenAuthority;
use hudsucker::hyper::Request;
use hudsucker::{Body, HttpContext, HttpHandler, Proxy, RequestOrResponse};
use tokio::net::TcpListener;

use crate::capture::{CaptureRecord, CaptureSink};
use crate::rewrite::{host_matches, redact_value, Rewrites};

#[derive(Clone)]
pub struct RewriteHandler {
    hosts: Arc<Vec<String>>,
    rewrites: Arc<Rewrites>,
    sink: CaptureSink,
    show_secrets: bool,
    /// Print a per-request summary to the terminal. Off when the child owns the
    /// terminal (e.g. a TUI) so log lines don't corrupt its screen; the capture
    /// file is written regardless.
    announce: bool,
}

impl RewriteHandler {
    pub fn new(
        hosts: Vec<String>,
        rewrites: Rewrites,
        sink: CaptureSink,
        show_secrets: bool,
        announce: bool,
    ) -> Self {
        Self {
            hosts: Arc::new(hosts),
            rewrites: Arc::new(rewrites),
            sink,
            show_secrets,
            announce,
        }
    }

    /// Whether this request's host is one rtr should intercept/rewrite.
    pub fn intercepts(&self, req: &Request<Body>) -> bool {
        request_host(req)
            .map(|h| host_matches(&h, self.hosts.as_slice()))
            .unwrap_or(false)
    }

    /// Record the original request (target hosts only) then apply rewrites.
    /// Pulled out of the trait method so it's testable without an `HttpContext`.
    pub fn apply(&self, mut req: Request<Body>) -> Request<Body> {
        // The CONNECT tunnel-setup request carries no app headers and is rebuilt
        // by the proxy; capturing/rewriting it is noise. The real requests flow
        // through here after TLS is established.
        if req.method() == Method::CONNECT {
            return req;
        }
        let host = match request_host(&req) {
            Some(h) if host_matches(&h, self.hosts.as_slice()) => h,
            _ => return req,
        };

        let method = req.method().to_string();
        let url = full_url(&req, &host);
        let original = headers_vec(req.headers());

        let record = CaptureRecord::new(method.clone(), url, host.clone(), original.clone());
        if let Err(e) = self.sink.record(&record) {
            tracing::warn!("capture write failed: {e:#}");
        }
        if self.announce {
            for (k, v) in &original {
                let shown = if self.show_secrets {
                    v.clone()
                } else {
                    redact_value(k, v)
                };
                tracing::info!(target: "rtr::capture", "{method} {host} {k}: {shown}");
            }
        }

        self.rewrites.apply(req.headers_mut());
        req
    }
}

/// Hostname of a request, from the URI authority or falling back to the Host
/// header (port stripped).
fn request_host(req: &Request<Body>) -> Option<String> {
    if let Some(h) = req.uri().host() {
        return Some(h.to_string());
    }
    req.headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(':').next().unwrap_or(s).to_string())
}

fn full_url(req: &Request<Body>, host: &str) -> String {
    if req.uri().authority().is_some() {
        req.uri().to_string()
    } else {
        let pq = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
        format!("https://{host}{pq}")
    }
}

fn headers_vec(h: &HeaderMap) -> Vec<(String, String)> {
    h.iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect()
}

impl HttpHandler for RewriteHandler {
    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        req: Request<Body>,
    ) -> RequestOrResponse {
        self.apply(req).into()
    }

    async fn should_intercept(&mut self, _ctx: &HttpContext, req: &Request<Body>) -> bool {
        self.intercepts(req)
    }
}

/// Run the proxy on an already-bound listener until `shutdown` resolves.
///
/// Taking a bound listener (rather than an address) lets the caller read the
/// chosen port before the child is spawned, avoiding a readiness race.
pub async fn serve<F>(
    listener: TcpListener,
    authority: RcgenAuthority,
    handler: RewriteHandler,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    // hudsucker's rustls paths expect a process-default provider in some code
    // paths; installing it is idempotent and harmless if already set.
    let _ = hudsucker::rustls::crypto::aws_lc_rs::default_provider().install_default();

    let proxy = Proxy::builder()
        .with_listener(listener)
        .with_ca(authority)
        .with_rustls_connector(hudsucker::rustls::crypto::aws_lc_rs::default_provider())
        .with_http_handler(handler)
        .with_graceful_shutdown(shutdown)
        .build()
        .context("building MITM proxy")?;

    proxy.start().await.context("MITM proxy server error")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Profile;
    use std::collections::BTreeMap;

    fn rewrites_set_auth(value: &str) -> Rewrites {
        let mut set = BTreeMap::new();
        set.insert("authorization".to_string(), value.to_string());
        Rewrites::from_profile(&Profile {
            set,
            remove: vec![],
        })
        .unwrap()
    }

    #[test]
    fn apply_rewrites_target_and_captures_original() {
        let (sink, buf) = CaptureSink::in_memory();
        let handler = RewriteHandler::new(
            vec!["api.openai.com".into()],
            rewrites_set_auth("Bearer NEW"),
            sink,
            false,
            false,
        );

        let req = Request::builder()
            .method("POST")
            .uri("https://api.openai.com/v1/responses")
            .header("authorization", "Bearer OLD")
            .body(Body::empty())
            .unwrap();

        let req = handler.apply(req);
        assert_eq!(req.headers().get("authorization").unwrap(), "Bearer NEW");

        let contents = buf.contents_string();
        assert!(contents.contains("\"host\":\"api.openai.com\""), "{contents}");
        assert!(contents.contains("Bearer OLD"), "capture keeps original: {contents}");
        assert!(!contents.contains("Bearer NEW"), "capture must not show rewrite");
    }

    #[test]
    fn intercepts_only_targets() {
        let (sink, _buf) = CaptureSink::in_memory();
        let handler =
            RewriteHandler::new(vec!["api.openai.com".into()], Rewrites::default(), sink, false, false);
        let on = Request::builder()
            .uri("https://api.openai.com/x")
            .body(Body::empty())
            .unwrap();
        let off = Request::builder()
            .uri("https://example.com/x")
            .body(Body::empty())
            .unwrap();
        assert!(handler.intercepts(&on));
        assert!(!handler.intercepts(&off));
    }

    #[test]
    fn non_target_host_passes_through_unchanged_and_uncaptured() {
        let (sink, buf) = CaptureSink::in_memory();
        let handler = RewriteHandler::new(
            vec!["api.openai.com".into()],
            rewrites_set_auth("Bearer NEW"),
            sink,
            false,
            false,
        );
        let req = Request::builder()
            .uri("https://example.com/x")
            .header("authorization", "Bearer OLD")
            .body(Body::empty())
            .unwrap();
        let req = handler.apply(req);
        assert_eq!(req.headers().get("authorization").unwrap(), "Bearer OLD");
        assert_eq!(buf.contents_string(), "");
    }
}
