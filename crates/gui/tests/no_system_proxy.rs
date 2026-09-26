//! The GUI's daemon client must ignore the system proxy.
//!
//! reqwest reads `HTTP_PROXY`/`ALL_PROXY` by default and does *not* exempt
//! loopback: with a proxy set in the environment (a desktop proxy setting, a
//! corporate network) every GUI → daemon request — session token and
//! repository data included — would be handed to that proxy, which may well
//! sit on another machine. The daemon is always on 127.0.0.1: no proxy has any
//! business in between.
//!
//! Its own test binary: the environment is process-wide, and setting a proxy
//! in a shared binary would reroute every other test's requests.

use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::routing::get;
use axum::Json;
use metafolder_gui::daemon_proxy::DaemonProxy;
use serde_json::json;

#[tokio::test]
async fn test_the_daemon_is_reached_directly_even_with_a_proxy_in_the_environment() {
    // A "proxy" that only counts who knocks.
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_addr = proxy.local_addr().unwrap();
    let knocks = Arc::new(AtomicUsize::new(0));
    {
        let knocks = Arc::clone(&knocks);
        std::thread::spawn(move || {
            for stream in proxy.incoming() {
                knocks.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        });
    }
    let proxy_url = format!("http://{proxy_addr}");
    // SAFETY: set before any client exists; this binary holds this one test.
    unsafe {
        for var in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
            std::env::set_var(var, &proxy_url);
        }
        for var in ["NO_PROXY", "no_proxy"] {
            std::env::remove_var(var);
        }
    }

    let daemon = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let daemon_url = format!("http://{}", daemon.local_addr().unwrap());
    let router = axum::Router::new().route(
        "/health",
        get(|| async {
            Json(json!({"status": "ok", "api_version": metafolder_core::API_VERSION}))
        }),
    );
    tokio::spawn(async move { axum::serve(daemon, router).await.unwrap() });

    let client = DaemonProxy::new(daemon_url);
    let answer = client.request("GET", "/health", None).await;

    assert_eq!(knocks.load(Ordering::SeqCst), 0, "a daemon request went through the proxy");
    let answer = answer.expect("the daemon answers directly");
    assert_eq!(answer.status, 200);
}
