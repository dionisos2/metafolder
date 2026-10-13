//! A GUI command is a label (doc "Labels and contexts"): what it writes,
//! through the daemon proxy, carries the command's key and name, so the
//! daemon opens one label for it at its first write.

use std::sync::{Arc, Mutex};

use axum::http::HeaderMap;
use axum::routing::post;
use axum::Router;
use metafolder_gui::daemon_proxy::DaemonProxy;

type Seen = Arc<Mutex<Vec<(Option<String>, Option<String>)>>>;

async fn spawn_stub() -> (String, Seen) {
    let seen: Seen = Arc::default();
    let record = seen.clone();
    let router = Router::new().route(
        "/repos/r/metarecords",
        post(move |headers: HeaderMap| {
            let record = record.clone();
            async move {
                let get = |name: &str| {
                    headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
                };
                record.lock().unwrap().push((
                    get(metafolder_core::context::COMMAND_HEADER),
                    get(metafolder_core::context::COMMAND_NAME_HEADER),
                ));
                "{}"
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

#[tokio::test]
async fn the_requests_of_a_command_carry_its_key_and_name() {
    let (url, seen) = spawn_stub().await;
    let proxy = DaemonProxy::new(url);

    let key = proxy.command_begin("metarecord:field set rating 4");
    proxy.request("POST", "/repos/r/metarecords", None).await.unwrap();
    // A command run from inside another is part of it.
    assert_eq!(proxy.command_begin("nested"), key);
    proxy.request("POST", "/repos/r/metarecords", None).await.unwrap();
    proxy.command_end(&key);
    proxy.request("POST", "/repos/r/metarecords", None).await.unwrap();
    proxy.command_end(&key);
    proxy.request("POST", "/repos/r/metarecords", None).await.unwrap();

    let seen = seen.lock().unwrap().clone();
    let name = metafolder_core::context::encode("metarecord:field set rating 4");
    let during = (Some(key.clone()), Some(name));
    assert_eq!(seen[0], during);
    assert_eq!(seen[1], during, "the nested command writes in the outer one's label");
    assert_eq!(seen[2], during, "until the outer one ends");
    assert_eq!(seen[3], (None, None), "outside any command, nothing is said");

    // Ending a command that is not the current one changes nothing.
    let other = proxy.command_begin("other");
    proxy.command_end("not-it");
    assert_ne!(other, key);
    assert_eq!(proxy.command_begin("again"), other);
}
