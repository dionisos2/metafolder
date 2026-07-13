//! Guards the "no internet" property of the GUI's web layer.
//!
//! Everything else in metafolder is already unable to reach the network: the
//! daemon and the GUI server bind 127.0.0.1, the HTTP clients only ever build
//! loopback URLs, the media helpers run with `--unshare-net`, and the config
//! sync never touches a git remote. The WebView is the exception — it is a
//! full browser engine, and its realm holds the session token, the filesystem
//! API and `!` shell commands.
//!
//! The Content-Security-Policy is what keeps that realm off the network. It is
//! the difference between "no panel currently calls out" and "no panel *can*":
//! one HTML injection (the help panel writes `innerHTML`) would otherwise be an
//! exfiltration channel straight to any host.

use serde_json::Value;

fn tauri_config() -> Value {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
    let source = std::fs::read_to_string(path).expect("tauri.conf.json");
    serde_json::from_str(&source).expect("valid JSON")
}

fn csp() -> String {
    tauri_config()["app"]["security"]["csp"]
        .as_str()
        .expect("a CSP must be set: `csp: null` leaves the WebView free to reach any host")
        .to_string()
}

/// The origins the web layer is allowed to talk to: itself, the loopback GUI
/// server (panel assets, `/fsraw`, `/thumbnail` — its port is configurable, so
/// the whole of loopback is allowed), and Tauri's IPC.
fn is_local_origin(source: &str) -> bool {
    const LOCAL: &[&str] = &[
        "'self'",
        "'none'",
        "'unsafe-inline'",
        "data:",
        "blob:",
        "ipc:",
        "asset:",
        "http://ipc.localhost",
        "http://asset.localhost",
        "http://127.0.0.1:*",
        "http://localhost:*",
        "tauri:",
    ];
    LOCAL.contains(&source)
}

#[test]
fn test_the_csp_allows_no_remote_origin() {
    let csp = csp();
    let mut offences = Vec::new();

    for directive in csp.split(';').map(str::trim).filter(|part| !part.is_empty()) {
        let mut tokens = directive.split_whitespace();
        let name = tokens.next().expect("a directive name");
        for source in tokens {
            if !is_local_origin(source) {
                offences.push(format!("{name}: {source}"));
            }
        }
    }

    assert!(
        offences.is_empty(),
        "the WebView's CSP allows a non-local source — the realm holding the session token, \
         the fs API and shell commands could then reach the internet:\n{}",
        offences.join("\n")
    );
}

/// A missing directive falls back to `default-src`, so `default-src 'self'`
/// must be there to close everything not spelled out.
#[test]
fn test_the_csp_locks_down_everything_by_default() {
    let csp = csp();
    assert!(csp.contains("default-src 'self'"), "no default-src 'self' in: {csp}");
    // A document view of a raw file is the /fsraw invariant's nightmare
    // (see server/fsraw.rs); the CSP says so too.
    assert!(csp.contains("object-src 'none'"), "no object-src 'none' in: {csp}");
    assert!(csp.contains("frame-src 'none'"), "no frame-src 'none' in: {csp}");
}
