//! Metafolder GUI: a Tauri application over the daemon HTTP API (spec-gui).
//!
//! The Rust side owns all canonical state (workspaces, layout, keybindings,
//! command registry); the Svelte shell in `frontend/` is a thin reflection
//! updated through Tauri events. An Axum server (default port 7524) serves
//! panel-type directories and the scripting API.

pub mod command_registry;
pub mod commands;
pub mod config;
pub mod daemon_proxy;
pub mod events;
pub mod fs_commands;
pub mod keybindings;
pub mod media_support;
pub mod notifier;
pub mod reconcile;
pub mod server;
pub mod shell_exec;
pub mod state;
pub mod style_watcher;
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
    pub daemon_url: Option<String>,
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
    gui.notify(
        events::KEYBINDINGS_CHANGED,
        serde_json::json!({ "bindings": compiled }),
    );
}

/// Shell builtins shown in the command input autocomplete (spec-gui
/// "Command names"). Handlers live in the frontend dispatcher.
fn register_builtins(registry: &CommandRegistry) {
    // The `log` column controls whether an invocation is echoed to the
    // message panel. Basic editing primitives (which fire on nearly every
    // keystroke) opt out to keep the log readable.
    for (name, label, log) in [
        ("command-input:activate", "Focus the command input", false),
        ("editing:unfocus", "Leave the focused text input", false),
        ("editing:discard", "Clear and leave the focused text input", false),
        ("editing:confirm", "Confirm the focused text input", false),
        ("editing:goto-line-start", "Move the cursor to the line start", false),
        ("editing:goto-line-end", "Move the cursor to the line end", false),
        ("tab:new", "Create a workspace in the focused slot", true),
        ("tab:close", "Close the focused slot's workspace", true),
        ("tab:rename", "Rename the focused slot's workspace", true),
        ("tab:next", "Move the focused panel to the next workspace", true),
        ("tab:prev", "Move the focused panel to the previous workspace", true),
        ("tab:goto", "Move both panels to workspace number N", true),
        ("workspace:next", "Move both panels to the next workspace", true),
        ("workspace:prev", "Move both panels to the previous workspace", true),
        ("panel:split", "Show the second panel slot", true),
        ("panel:unsplit", "Hide the non-focused panel slot", true),
        ("panel:hide", "Hide the focused panel slot", true),
        ("panel:split-toggle", "Split when single, unsplit when split", true),
        ("panel:focus-next", "Focus the other panel slot", true),
        ("panel:set-type", "Switch the focused slot's panel type", true),
        ("panel:swap", "Exchange the two slots' panel types", true),
        ("panel:fullscreen", "Show only the focused panel fullscreen (escape exits)", true),
        ("message:clear", "Clear the workspace message log", true),
        ("config:open", "Open the settings view", true),
        ("devtools:open", "Open the WebKit web inspector", true),
        ("quit", "Exit the GUI", true),
        ("daemon:set-url", "Change the daemon URL", true),
        ("repos:open", "Open the repository panel in the focused slot", true),
        ("reconcile:run", "Reconcile the active repository with the filesystem", true),
        ("log:undo", "Undo the last revision of the active repository", true),
        ("log:redo", "Re-apply the revision ahead of HEAD", true),
        ("answer:send", "Resolve the pending script input wait", true),
    ] {
        registry.register_builtin(name, label, log);
    }
}

/// Builds and runs the Tauri application; blocks until the window closes.
pub fn run(options: Options) {
    let config = Arc::new(
        ConfigDir::default_location().expect("cannot resolve the user config directory"),
    );

    let registry = Arc::new(CommandRegistry::new());
    register_builtins(&registry);
    // The configuration is installed by `metafolder-sync-config`; a missing or
    // invalid file is fatal (spec-config "No runtime fallback").
    let keybindings = match config.load_keybindings() {
        Ok(keybindings) => keybindings,
        Err(error) => {
            eprintln!("metafolder-gui: {error}");
            std::process::exit(1);
        }
    };
    // The simplified-query grammar (shared, in core): expansion is done locally
    // by the GUI backend, never proxied to the daemon (spec-query).
    let grammar = match metafolder_core::simplified::load::load() {
        Ok(grammar) => grammar,
        Err(error) => {
            eprintln!("metafolder-gui: {error}");
            std::process::exit(1);
        }
    };

    // GUI settings (config.toml), with the CLI flags as optional overrides.
    // A missing config file is fatal (spec-config "No runtime fallback").
    let gui_config = match config.load_config() {
        Ok(gui_config) => gui_config,
        Err(error) => {
            eprintln!("metafolder-gui: {error}");
            std::process::exit(1);
        }
    };
    let gui_port = options.gui_port.unwrap_or(gui_config.gui_port);
    let page_sizes = gui_config.page_size.clone();
    let daemon_url = options.daemon_url.unwrap_or(gui_config.daemon_url);
    let daemon = Arc::new(daemon_proxy::DaemonProxy::new(daemon_url));

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
                grammar,
                gui_port,
                page_sizes: page_sizes.clone(),
                daemon: daemon.clone(),
                input: input.clone(),
                commands: command_wait.clone(),
                bench: bench.clone(),
                style_watcher: Mutex::new(style_watcher),
            });
            tauri::Manager::manage(tauri_app, app);

            // A WebKit web-process crash (e.g. a GStreamer failure in a
            // media pipeline) would otherwise leave the window frozen on its
            // last frame: the shell and every panel (same realm) share that
            // single process. Reload the shell instead — Rust owns all
            // canonical state, so nothing is lost. A second crash shortly
            // after a reload means reloading re-triggers it: stop there
            // rather than loop.
            #[cfg(target_os = "linux")]
            if let Some(window) = tauri::Manager::get_webview_window(tauri_app, "main") {
                let _ = window.with_webview(|webview| {
                    use webkit2gtk::WebViewExt;
                    let last_crash = std::cell::Cell::new(None::<std::time::Instant>);
                    webview.inner().connect_web_process_terminated(
                        move |webview, reason| {
                            let now = std::time::Instant::now();
                            let rapid = last_crash.get().is_some_and(|previous| {
                                now - previous < std::time::Duration::from_secs(10)
                            });
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
                        },
                    );
                });
            }

            // Daemon health polling (spec-gui "Connection to the daemon").
            let poll_daemon = daemon.clone();
            let poll_gui = gui.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    poll_daemon.check_health(&poll_gui).await;
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            });

            // The GUI HTTP server: panel assets, /fsraw, scripting API.
            let server_state = server::ServerState {
                config: config.clone(),
                gui: gui.clone(),
                daemon: daemon.clone(),
                keybindings,
                input,
                commands: command_wait,
                bench,
            };
            tauri::async_runtime::spawn(async move {
                let router = server::build_router(server_state);
                let address = std::net::SocketAddr::from(([127, 0, 0, 1], gui_port));
                match tokio::net::TcpListener::bind(address).await {
                    Ok(listener) => {
                        // The bound port is fixed by config.toml (the CLI reads
                        // the same file); there is no longer a gui.port file.
                        if let Err(error) = axum::serve(listener, router).await {
                            eprintln!("metafolder-gui: HTTP server failed: {error}");
                        }
                    }
                    Err(error) => {
                        eprintln!("metafolder-gui: cannot bind 127.0.0.1:{gui_port}: {error}")
                    }
                }
            });
            Ok(())
        })
        .on_window_event({
            move |window, event| {
                if let tauri::WindowEvent::Destroyed = event {
                    let handle = tauri::Manager::app_handle(window);
                    let app: tauri::State<'_, Arc<commands::App>> =
                        tauri::Manager::state(handle);
                    // Pending script waits resolve with "closed".
                    app.input.close_all();
                    app.commands.close_all();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_initial_state,
            commands::tab_new,
            commands::tab_close,
            commands::tab_close_ws,
            commands::tab_rename,
            commands::tab_assign,
            commands::tab_next,
            commands::tab_prev,
            commands::tab_goto,
            commands::workspace_next,
            commands::workspace_prev,
            commands::panel_split,
            commands::panel_unsplit,
            commands::panel_split_toggle,
            commands::slot_hide,
            commands::panel_focus_next,
            commands::panel_set_type,
            commands::panel_swap,
            commands::ws_get_var,
            commands::ws_set_var,
            commands::ws_vars,
            commands::adopt_repo,
            commands::list_commands,
            commands::register_command,
            commands::suggest_keybinding,
            commands::get_compiled_keybindings,
            commands::set_user_keybinding,
            commands::remove_user_keybinding,
            commands::list_panel_types,
            commands::load_style,
            commands::config_info,
            fs_commands::fs_read_dir,
            fs_commands::fs_stat,
            shell_exec::run_shell,
            commands::daemon_request,
            commands::daemon_set_url,
            commands::daemon_health,
            commands::parse_query,
            commands::expand_query,
            reconcile::reconcile_run,
            undo::log_navigate,
            commands::answer_send,
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
        for name in ["workspace:next", "workspace:prev", "tab:goto", "panel:fullscreen"] {
            assert!(registry.get(name).is_some(), "{name} registered");
        }
        // The parameter-in-name form is gone.
        assert!(registry.get("tab:goto-N").is_none());
    }
}
