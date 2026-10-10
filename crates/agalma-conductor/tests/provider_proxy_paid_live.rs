//! Gated live smoke for the paid HTTP-forward proxy (`agalma-csw.2`).
//!
//! Proves the loopback proxy maps a plain-HTTP worker request onto the
//! configured OpenCode Go base, terminates TLS with rustls, injects the parent
//! credential (never held by the worker), and streams a real completion back.
//!
//! Ignored by default. Requires a real OpenCode token in the auth store
//! (`~/.local/share/opencode/auth.json`, override with `AGALMA_OPENCODE_AUTH`).
//!
//! ```text
//! AGALMA_PAID_LIVE=1 cargo test -p agalma-conductor \
//!     --test provider_proxy_paid_live -- --ignored --nocapture
//! ```
//!
//! The token is never printed: only the presence, host, and non-cryptographic
//! fingerprint are logged.

use std::time::Duration;

use agalma_conductor::provider_proxy::{ProviderProxy, ProxyConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn live_enabled() -> bool {
    std::env::var("AGALMA_PAID_LIVE").ok().as_deref() == Some("1")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn paid_forward_injects_credential_and_reaches_provider() {
    if !live_enabled() {
        eprintln!("paid proxy live smoke skipped: set AGALMA_PAID_LIVE=1 to run");
        return;
    }

    let proxy = ProviderProxy::start(ProxyConfig::from_env()).expect("start provider proxy");
    let auth = proxy.auth_state();
    eprintln!(
        "paid auth: present={} host={} fingerprint={}",
        auth.present,
        auth.host,
        auth.fingerprint.as_deref().unwrap_or("-")
    );
    if !auth.present {
        eprintln!(
            "paid proxy live smoke cannot run: no credential ({:?})",
            auth.reason
        );
        proxy.stop().await;
        return;
    }

    // A tiny OpenAI-compatible completion for the cheapest paid builder route.
    let body = r#"{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"reply with the single word OK"}],"max_tokens":16,"stream":false}"#;
    let port = proxy.port();
    assert!(
        TcpStream::connect(("127.0.0.1", port)).await.is_ok(),
        "proxy is listening"
    );
    let request = format!(
        "POST /chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Content-Type: application/json\r\nContent-Length: {len}\r\n\
         Connection: close\r\n\r\n{body}",
        len = body.len()
    );

    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect proxy");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(90), stream.read_to_end(&mut raw)).await;
    assert!(read.is_ok(), "upstream response timed out");

    let text = String::from_utf8_lossy(&raw);
    let status = text.lines().next().unwrap_or("").trim().to_string();
    eprintln!("paid proxy upstream status: {status}");
    eprintln!(
        "paid proxy body (first 400 bytes): {}",
        &text[..text.len().min(400)]
    );

    let log = proxy.requests();
    for request in &log {
        eprintln!(
            "proxy request: allowed={} {} {}:{}",
            request.allowed, request.method, request.host, request.port
        );
    }
    proxy.stop().await;

    assert!(
        status.contains("200"),
        "expected a 200 from the provider, got: {status}"
    );
    assert!(
        log.iter()
            .any(|r| r.allowed && r.host.ends_with("opencode.ai")),
        "no allowlisted forward to the provider was recorded: {log:?}"
    );
    assert!(
        !text.contains("MissingSessionID"),
        "proxy must inject x-opencode-session: {text}"
    );
}
