//! The binary itself: its arguments reach the service, and a broker that
//! cannot start says so and exits non-zero — rather than stream nothing.

#[test]
fn test_a_broker_that_cannot_start_says_so() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_metafolder-watchd"))
        .args(["--socket", "/nonexistent/metafolder-watchd/watchd.sock"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("refusing to start"), "{text}");
}
