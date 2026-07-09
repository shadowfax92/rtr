//! End-to-end: a request through the real proxy is rewritten before reaching
//! the upstream.
//!
//! Uses the plain-HTTP proxy path so the assertion targets *our* logic
//! (host-match + rewrite + forward) over real sockets; per-host TLS
//! leaf forging is hudsucker's own concern and is covered by its test suite.

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use rtr::ca::{self, CaMaterial};
use rtr::config::Profile;
use rtr::proxy::{serve, RewriteHandler};
use rtr::rewrite::Rewrites;

async fn read_http_head(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

async fn connect_retry(port: u16) -> TcpStream {
    for _ in 0..100 {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", port)).await {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("could not connect to proxy on port {port}");
}

#[tokio::test]
async fn rewrites_through_proxy() {
    // Upstream echo server: capture the first request's head, then 200 OK.
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    tokio::spawn(async move {
        let (mut sock, _) = upstream.accept().await.unwrap();
        let head = read_http_head(&mut sock).await;
        let _ = sock
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await;
        let _ = sock.flush().await;
        let _ = tx.send(head);
    });

    // Proxy with our CA, intercepting 127.0.0.1, rewriting Authorization.
    let (cert_pem, key_pem) = ca::generate().unwrap();
    let authority = CaMaterial {
        cert_pem,
        key_pem,
        cert_path: PathBuf::new(),
    }
    .authority()
    .unwrap();

    let rewrites = Rewrites::from_profile(&Profile {
        set: [("authorization".to_string(), "Bearer NEW".to_string())]
            .into_iter()
            .collect(),
        remove: vec![],
        ..Profile::default()
    })
    .unwrap();
    let handler = RewriteHandler::new(vec!["127.0.0.1".to_string()], rewrites);

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = proxy_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        serve(
            proxy_listener,
            authority,
            handler,
            std::future::pending::<()>(),
        )
        .await
        .unwrap();
    });

    // Client speaks to the proxy in absolute-form (standard HTTP proxying).
    let mut client = connect_retry(proxy_port).await;
    let req = format!(
        "GET http://127.0.0.1:{up_port}/v1/test HTTP/1.1\r\nHost: 127.0.0.1:{up_port}\r\nauthorization: Bearer OLD\r\nConnection: close\r\n\r\n"
    );
    client.write_all(req.as_bytes()).await.unwrap();
    let _resp = read_http_head(&mut client).await;

    // Upstream must have seen the rewritten header, never the original.
    let head = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("upstream did not receive a request")
        .unwrap();
    let head_lc = head.to_lowercase();
    assert!(
        head_lc.contains("authorization: bearer new"),
        "upstream head was: {head}"
    );
    assert!(!head_lc.contains("bearer old"), "upstream head was: {head}");
}
