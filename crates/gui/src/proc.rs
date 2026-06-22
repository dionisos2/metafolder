//! Running short-lived helper subprocesses (ffmpeg, gst-discoverer) with a
//! hard wall-clock timeout, so a process that hangs — a FIFO mistaken for a
//! file, a pathological input — cannot pin a `spawn_blocking` thread forever.

use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Runs `cmd` to completion, capturing its output, but kills it if it does not
/// finish within `timeout`. Returns `None` on spawn failure or timeout.
///
/// stdin is closed (a child that reads stdin cannot block on it) and
/// stdout/stderr are captured. The caller must only use this for commands with
/// *bounded, small* output: output is read after the process exits, so a child
/// that writes more than the OS pipe buffer (~64 KiB) before exiting would
/// block — which the timeout still bounds, but the output would be truncated.
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Option<Output> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fast_command_returns_output() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf hello");
        let output = run_with_timeout(cmd, Duration::from_secs(5)).expect("should finish");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"hello");
    }

    #[test]
    fn test_hanging_command_is_killed_and_returns_none() {
        // `sleep 10` would block far past the timeout; it must be killed.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 10");
        let start = Instant::now();
        let result = run_with_timeout(cmd, Duration::from_millis(150));
        assert!(result.is_none(), "a timed-out command must return None");
        assert!(start.elapsed() < Duration::from_secs(2), "must not wait for the child");
    }

    #[test]
    fn test_missing_binary_returns_none() {
        let cmd = Command::new("definitely-not-a-real-binary-xyz");
        assert!(run_with_timeout(cmd, Duration::from_secs(1)).is_none());
    }
}
