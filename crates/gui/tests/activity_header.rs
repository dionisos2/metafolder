//! A request the GUI sends on its own account — a poll — says it is not the
//! user's activity, so it does not hold the daemon's folding back (doc "Folding
//! in idle time").

use std::sync::{Arc, Mutex};

use axum::http::HeaderMap;
use axum::routing::get;
use axum::Router;
use metafolder_core::context::ACTIVITY_HEADER;
use metafolder_gui::daemon_proxy::DaemonProxy;

#[tokio::test]
async fn a_polling_request_says_it_is_not_activity() {
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let record = seen.clone();
    let router = Router::new().route(
        "/repos/r/log/since",
        get(move |headers: HeaderMap| {
            let record = record.clone();
            async move {
                let value = headers.get(ACTIVITY_HEADER).and_then(|v| v.to_str().ok());
                record.lock().unwrap().push(value.map(str::to_string));
                "{}"
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let proxy = DaemonProxy::new(format!("http://127.0.0.1:{port}"));

    proxy.request("GET", "/repos/r/log/since", None).await.unwrap();
    proxy.request_polling("GET", "/repos/r/log/since", None).await.unwrap();
    proxy.request("GET", "/repos/r/log/since", None).await.unwrap();

    assert_eq!(*seen.lock().unwrap(), vec![None, Some("off".to_string()), None]);
}
