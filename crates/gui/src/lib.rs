//! Metafolder GUI: a Tauri application over the daemon HTTP API (spec-gui).
//!
//! The Rust side owns all canonical state (workspaces, layout, keybindings,
//! command registry); the Svelte shell in `frontend/` is a thin reflection
//! updated through Tauri events. An Axum server (default port 7524) serves
//! panel-type directories and the scripting API.

pub mod command_registry;
pub mod commands;
pub mod config;
pub mod events;
pub mod fs_commands;
pub mod keybindings;
pub mod notifier;
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
        ("command-input:activate", "Open the command input"),
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
        ("panel:close", "Hide the non-focused panel slot"),
        ("panel:focus-next", "Focus the other panel slot"),
        ("panel:set-type", "Switch the focused slot's panel type"),
        ("message:clear", "Clear the workspace message log"),
        ("config:open", "Open the settings view"),
        ("quit", "Exit the GUI"),
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
    let daemon_url = options.daemon_url.clone();

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
            let app = Arc::new(commands::App {
                gui,
                registry,
                config: config.clone(),
                keybindings: Mutex::new(keybindings),
                gui_port,
                daemon_url: Mutex::new(daemon_url),
                style_watcher: Mutex::new(style_watcher),
            });
            tauri::Manager::manage(tauri_app, app);

            // The GUI HTTP server: panel assets, /fsraw, scripting API.
            let server_config = config.clone();
            tauri::async_runtime::spawn(async move {
                let router = server::build_router(server_config.clone());
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
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_initial_state,
            commands::tab_new,
            commands::tab_close,
            commands::tab_rename,
            commands::tab_assign,
            commands::tab_next,
            commands::tab_prev,
            commands::tab_goto,
            commands::panel_split,
            commands::panel_close,
            commands::panel_focus_next,
            commands::panel_set_type,
            commands::ws_get_var,
            commands::ws_set_var,
            commands::ws_vars,
            commands::list_commands,
            commands::register_command,
            commands::suggest_keybinding,
            commands::get_compiled_keybindings,
            commands::list_panel_types,
            commands::load_style,
            commands::config_info,
            fs_commands::fs_read_dir,
            fs_commands::fs_stat,
            shell_exec::run_shell,
            commands::post_status,
            commands::get_messages,
            commands::clear_messages,
            commands::quit,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the metafolder GUI");
}
