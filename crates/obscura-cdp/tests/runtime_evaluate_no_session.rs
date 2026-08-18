//! A raw websocket connection to `/devtools/page/<id>` (the
//! `webSocketDebuggerUrl` advertised by `/json`) sends CDP messages with no
//! `sessionId`: the client never called `Target.attachToTarget`. Before #680
//! every such `Runtime.evaluate` failed with `'No page'` because
//! `get_session_page(_mut)` required a session mapping. Chrome auto-creates a
//! session for direct page connections; Obscura now falls back to the first
//! page in the context. This test pins that `Runtime.evaluate` returns the
//! page's real values instead of a 'No page' error when `session_id` is `None`.

use obscura_cdp::dispatch::{dispatch, CdpContext};
use obscura_cdp::types::CdpRequest;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// Serves a page with known DOM state to read back through Runtime.evaluate.
async fn serve_page() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _n = socket.read(&mut buf).await.unwrap();
                let body = r#"<html><head><title>no-session</title></head>
<body><h1 id="h">hello</h1></body></html>"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(resp.as_bytes()).await.unwrap();
            });
        }
    });
    format!("http://{addr}/")
}

// session_id mirrors the sessionId on the wire: None for a direct
// /devtools/page/<id> websocket that never called Target.attachToTarget.
async fn cdp(ctx: &mut CdpContext, id: u64, method: &str, params: Value, session_id: Option<&str>) -> Value {
    let resp = dispatch(
        &CdpRequest {
            id,
            method: method.to_string(),
            params,
            session_id: session_id.map(|s| s.to_string()),
        },
        ctx,
    )
    .await;
    assert!(resp.error.is_none(), "CDP {method} failed: {:?}", resp.error);
    resp.result.unwrap_or_else(|| json!({}))
}

#[tokio::test(flavor = "current_thread")]
async fn runtime_evaluate_returns_real_values_without_session_attach() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let url = serve_page().await;
    let mut ctx = CdpContext::new();
    let page_id = ctx.create_page();
    // Deliberately no ctx.sessions entry: a direct page websocket sends no
    // sessionId, so every request below arrives with session_id None.

    cdp(
        &mut ctx,
        1,
        "Page.navigate",
        json!({"url": url, "waitUntil": "load"}),
        None,
    )
    .await;

    // Real values from the navigated document, not a 'No page' error.
    let title = cdp(
        &mut ctx,
        2,
        "Runtime.evaluate",
        json!({"expression": "document.title", "returnByValue": true}),
        None,
    )
    .await;
    assert_eq!(
        title["result"]["value"], "no-session",
        "Runtime.evaluate without a session must return the real document.title"
    );

    let h1 = cdp(
        &mut ctx,
        3,
        "Runtime.evaluate",
        json!({"expression": "document.getElementById('h').textContent", "returnByValue": true}),
        None,
    )
    .await;
    assert_eq!(
        h1["result"]["value"], "hello",
        "Runtime.evaluate without a session must return the real DOM text"
    );

    let arith = cdp(
        &mut ctx,
        4,
        "Runtime.evaluate",
        json!({"expression": "40 + 2", "returnByValue": true}),
        None,
    )
    .await;
    assert_eq!(
        arith["result"]["value"].as_f64(),
        Some(42.0),
        "Runtime.evaluate without a session must return computed values"
    );

    // State set by one session-less evaluation must be visible to the next:
    // evaluations run against the same real page, not a throwaway context.
    cdp(
        &mut ctx,
        5,
        "Runtime.evaluate",
        json!({"expression": "globalThis.__probe = 'via-raw-socket'"}),
        None,
    )
    .await;
    let probe = cdp(
        &mut ctx,
        6,
        "Runtime.evaluate",
        json!({"expression": "globalThis.__probe", "returnByValue": true}),
        None,
    )
    .await;
    assert_eq!(
        probe["result"]["value"], "via-raw-socket",
        "a session-less Runtime.evaluate must share state with the page"
    );

    let page = ctx.get_page_mut(&page_id).unwrap();
    assert_eq!(
        page.url.as_ref().unwrap().path(),
        "/",
        "the no-session Page.navigate must have loaded the served page"
    );
}

/// Regression test for #680: Runtime.evaluate with a data: URL and no session
/// must return the page's real values, not a 'No page' error.
#[tokio::test(flavor = "current_thread")]
async fn runtime_evaluate_data_url_without_session() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let mut ctx = CdpContext::new();
    ctx.create_page();

    // Navigate to a data: URL with no session attached.
    let data_url = "data:text/html,<h1>hello</h1>";
    cdp(
        &mut ctx,
        1,
        "Page.navigate",
        json!({"url": data_url}),
        None,
    )
    .await;

    // "1 + 1" must return 2, not a 'No page' error.
    let sum = cdp(
        &mut ctx,
        2,
        "Runtime.evaluate",
        json!({"expression": "1 + 1", "returnByValue": true}),
        None,
    )
    .await;
    assert_eq!(
        sum["result"]["value"].as_f64(),
        Some(2.0),
        "Runtime.evaluate('1+1') must return 2 without a session"
    );

    // document.title must be a string (empty for data: URLs), not an error.
    let title = cdp(
        &mut ctx,
        3,
        "Runtime.evaluate",
        json!({"expression": "document.title", "returnByValue": true}),
        None,
    )
    .await;
    assert!(
        title["result"]["value"].is_string(),
        "Runtime.evaluate('document.title') must return a string, got {:?}",
        title["result"]
    );

    // DOM query on the data: URL page.
    let h1 = cdp(
        &mut ctx,
        4,
        "Runtime.evaluate",
        json!({"expression": "document.querySelector('h1').textContent", "returnByValue": true}),
        None,
    )
    .await;
    assert_eq!(
        h1["result"]["value"], "hello",
        "Runtime.evaluate on a data: URL page must return real DOM content"
    );
}