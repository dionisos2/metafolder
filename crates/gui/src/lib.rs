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

use command_registry::CommandRegistry;
use config::ConfigDir;
use keybindings::CompiledBinding;
use notifier::FrontendNotifier;
use state::GuiState;
use std::sync::{Arc, Mutex};
use tauri::Emitter;

/// Startup options, from CLI flags.
pub struct Options {
    pub gui_port: u16,
    pub daemon_url: String,
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
    for (name, label) in [
        ("command-input:activate", "Focus the command input"),
        ("editing:unfocus", "Leave the focused text input"),
        ("editing:confirm", "Confirm the focused text input"),
        ("editing:goto-line-start", "Move the cursor to the line start"),
        ("editing:goto-line-end", "Move the cursor to the line end"),
        ("tab:new", "Create a workspace in the focused slot"),
        ("tab:close", "Close the focused slot's workspace"),
        ("tab:rename", "Rename the focused slot's workspace"),
        ("tab:next", "Show the next workspace"),
        ("tab:prev", "Show the previous workspace"),
        ("tab:goto-N", "Show workspace number N"),
        ("panel:split", "Show the second panel slot"),
        ("panel:unsplit", "Hide the non-focused panel slot"),
        ("panel:split-toggle", "Split when single, unsplit when split"),
        ("panel:focus-next", "Focus the other panel slot"),
        ("panel:set-type", "Switch the focused slot's panel type"),
        ("panel:swap", "Exchange the two slots' panel types"),
        ("message:clear", "Clear the workspace message log"),
        ("config:open", "Open the settings view"),
        ("devtools:open", "Open the WebKit web inspector"),
        ("quit", "Exit the GUI"),
        ("daemon:set-url", "Change the daemon URL"),
        ("repos:open", "Open the repository panel in the focused slot"),
        ("reconcile:run", "Reconcile the active repository with the filesystem"),
        ("answer:send", "Resolve the pending script input wait"),
    ] {
        registry.register_builtin(name, label);
    }
}

/// Builds and runs the Tauri application; blocks until the window closes.
pub fn run(options: Options) {
    let config = Arc::new(
        ConfigDir::default_location().expect("cannot resolve the user config directory"),
    );
    config
        .install_defaults()
        .expect("cannot install default configuration");

    let registry = Arc::new(CommandRegistry::new());
    register_builtins(&registry);
    let keybindings = config
        .load_keybindings()
        .expect("invalid keybindings configuration");

    let gui_port = options.gui_port;
    let daemon = Arc::new(daemon_proxy::DaemonProxy::new(options.daemon_url.clone()));

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
            let app = Arc::new(commands::App {
                gui: gui.clone(),
                registry,
                config: config.clone(),
                keybindings: keybindings.clone(),
                gui_port,
                daemon: daemon.clone(),
                input: input.clone(),
                style_watcher: Mutex::new(style_watcher),
            });
            tauri::Manager::manage(tauri_app, app);

            // A WebKit web-process crash (e.g. a GStreamer failure in a
            // media pipeline) would otherwise leave the window frozen on
            // its last frame: the shell and every panel iframe share that
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
            let server_config = config.clone();
            let server_state = server::ServerState {
                config: config.clone(),
                gui: gui.clone(),
                daemon: daemon.clone(),
                keybindings,
                input,
            };
            tauri::async_runtime::spawn(async move {
                let router = server::build_router(server_state);
                let address = std::net::SocketAddr::from(([127, 0, 0, 1], gui_port));
                match tokio::net::TcpListener::bind(address).await {
                    Ok(listener) => {
                        let bound = listener.local_addr().map(|a| a.port()).unwrap_or(gui_port);
                        if let Err(error) = server_config.write_port_file(bound) {
                            eprintln!("metafolder-gui: {error}");
                        }
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
                    app.config.remove_port_file();
                    // Pending script waits resolve with "closed".
                    app.input.close_all();
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
            reconcile::reconcile_run,
            commands::answer_send,
            commands::prompt_resolve,
            commands::panel_ready,
            commands::post_status,
            commands::get_messages,
            commands::clear_messages,
            commands::open_devtools,
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
        assert_eq!(def.scope, crate::command_registry::Scope::Global);
    }
}
