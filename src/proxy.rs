//! The MITM proxy: a hudsucker [`HttpHandler`] that intercepts configured target
//! hosts and applies the active profile's rewrites before forwarding upstream.

use std::future::Future;
use std::sync::Arc;

use anyhow::{Context, Result};
use http::header::HOST;
use http::Method;
use hudsucker::certificate_authority::RcgenAuthority;
use hudsucker::hyper::Request;
use hudsucker::{Body, HttpContext, HttpHandler, Proxy, RequestOrResponse};
use tokio::net::TcpListener;

use crate::rewrite::{host_matches, Rewrites};

#[derive(Clone)]
pub struct RewriteHandler {
    hosts: Arc<Vec<String>>,
    rewrites: Arc<Rewrites>,
}

impl RewriteHandler {
    pub fn new(hosts: Vec<String>, rewrites: Rewrites) -> Self {
        Self {
            hosts: Arc::new(hosts),
            rewrites: Arc::new(rewrites),
        }
    }

    /// Whether this request's host is one rtr should intercept/rewrite.
    pub fn intercepts(&self, req: &Request<Body>) -> bool {
        request_host(req)
            .map(|h| host_matches(&h, self.hosts.as_slice()))
            .unwrap_or(false)
    }

    /// Apply target-host rewrites outside the trait method for direct testing.
    pub fn apply(&self, mut req: Request<Body>) -> Request<Body> {
        if req.method() == Method::CONNECT {
            return req;
        }
        match request_host(&req) {
            Some(host) if host_matches(&host, self.hosts.as_slice()) => {}
            _ => return req,
        }

        self.rewrites.apply(req.headers_mut());

        // hudsucker's WebSocket impl (tungstenite) can't decode permessage-deflate
        // frames. Leaving this header lets the upstream negotiate compression
        // (RSV1 set) and the proxied stream dies with "Reserved bits are non-zero".
        // Stripping it before the upgrade forces an uncompressed, forwardable WS;
        // the auth-header rewrite on the upgrade request is unaffected.
        req.headers_mut().remove("sec-websocket-extensions");
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
            ..Profile::default()
        })
        .unwrap()
    }

    #[test]
    fn applies_rewrites_to_target() {
        let handler = RewriteHandler::new(
            vec!["api.openai.com".into()],
            rewrites_set_auth("Bearer NEW"),
        );

        let req = Request::builder()
            .method("POST")
            .uri("https://api.openai.com/v1/responses")
            .header("authorization", "Bearer OLD")
            .body(Body::empty())
            .unwrap();

        let req = handler.apply(req);
        assert_eq!(req.headers().get("authorization").unwrap(), "Bearer NEW");
    }

    #[test]
    fn strips_websocket_extensions_on_target_upgrade() {
        let handler =
            RewriteHandler::new(vec!["chatgpt.com".into()], rewrites_set_auth("Bearer NEW"));
        let req = Request::builder()
            .uri("https://chatgpt.com/backend-api/codex/responses")
            .header("upgrade", "websocket")
            .header(
                "sec-websocket-extensions",
                "permessage-deflate; client_max_window_bits",
            )
            .header("authorization", "Bearer OLD")
            .body(Body::empty())
            .unwrap();

        let req = handler.apply(req);
        // Compression extension removed so the MITM'd WS stays uncompressed...
        assert!(req.headers().get("sec-websocket-extensions").is_none());
        // ...while the auth rewrite on the upgrade still applies.
        assert_eq!(req.headers().get("authorization").unwrap(), "Bearer NEW");
        assert_eq!(req.headers().get("upgrade").unwrap(), "websocket");
    }

    #[test]
    fn intercepts_only_targets() {
        let handler = RewriteHandler::new(vec!["api.openai.com".into()], Rewrites::default());
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
    fn wildcard_intercepts_and_rewrites_any_host() {
        let handler = RewriteHandler::new(vec!["*".into()], rewrites_set_auth("Bearer NEW"));
        let req = Request::builder()
            .method("POST")
            .uri("https://some.random.host/anything")
            .header("authorization", "Bearer OLD")
            .body(Body::empty())
            .unwrap();
        assert!(handler.intercepts(&req));
        let req = handler.apply(req);
        assert_eq!(req.headers().get("authorization").unwrap(), "Bearer NEW");
    }

    #[test]
    fn empty_hosts_intercepts_any_host() {
        let handler = RewriteHandler::new(vec![], Rewrites::default());
        let req = Request::builder()
            .uri("https://example.com/x")
            .body(Body::empty())
            .unwrap();
        assert!(handler.intercepts(&req));
    }

    #[test]
    fn non_target_host_passes_through_unchanged() {
        let handler = RewriteHandler::new(
            vec!["api.openai.com".into()],
            rewrites_set_auth("Bearer NEW"),
        );
        let req = Request::builder()
            .uri("https://example.com/x")
            .header("authorization", "Bearer OLD")
            .body(Body::empty())
            .unwrap();
        let req = handler.apply(req);
        assert_eq!(req.headers().get("authorization").unwrap(), "Bearer OLD");
    }
}
