//! Test helpers shared by the modules' tests: running one test again in a
//! process of its own, for what a test cannot do inside the shared test
//! process — change something process-wide (a resource limit, a signal
//! handler), or hold the capabilities a user namespace grants.

use std::path::PathBuf;

/// Set in the child: the scratch directory it may use (`in_userns`) or `1`.
const CHILD_ENV: &str = "METAFOLDER_WATCHD_TEST_CHILD";

/// Runs the test `test` (its path under the crate, `module::tests::name`)
/// alone in a new process, through `wrapper` (a command prefix, possibly
/// empty), and asserts it passed there.
fn rerun(test: &str, wrapper: &[&str], scratch: &str) {
    let exe = std::env::current_exe().unwrap();
    let args = [test, "--exact", "--nocapture", "--test-threads=1"];
    let mut cmd = match wrapper.split_first() {
        Some((program, rest)) => {
            let mut c = std::process::Command::new(program);
            c.args(rest).arg(&exe);
            c
        }
        None => std::process::Command::new(&exe),
    };
    let out = cmd.args(args).env(CHILD_ENV, scratch).output().unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success() && text.contains("1 passed"), "{text}");
}

/// `true` in the child process (run the body there); outside, runs `test` in
/// one and returns `false`. For a test that changes something process-wide.
pub fn in_child(test: &str) -> bool {
    if std::env::var_os(CHILD_ENV).is_some() {
        return true;
    }
    rerun(test, &[], "1");
    false
}

/// [`in_child`] inside a new user and mount namespace (`unshare -rm`), where
/// the test holds every capability over what it mounts itself. Skipped, and
/// announced, where user namespaces are refused.
pub fn in_userns(test: &str) -> bool {
    if std::env::var_os(CHILD_ENV).is_some() {
        return true;
    }
    let usable = std::process::Command::new("unshare")
        .args(["-rm", "true"])
        .status()
        .is_ok_and(|s| s.success());
    if !usable {
        eprintln!("skipped: user namespaces are not available here");
        return false;
    }
    let scratch = std::env::temp_dir().join("metafolder-tests").join(format!(
        "watchd-{}-{}",
        test.rsplit("::").next().unwrap(),
        std::process::id()
    ));
    std::fs::create_dir_all(&scratch).unwrap();
    rerun(test, &["unshare", "-rm", "--"], scratch.to_str().unwrap());
    std::fs::remove_dir_all(&scratch).ok();
    false
}

/// The child's scratch directory (inside [`in_userns`]).
pub fn scratch() -> PathBuf {
    PathBuf::from(std::env::var_os(CHILD_ENV).expect("inside in_userns"))
}
