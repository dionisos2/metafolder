//! Metafolder GUI: a Tauri application over the daemon HTTP API (spec-gui).
//!
//! The Rust side owns all canonical state (workspaces, layout, keybindings,
//! command registry); the Svelte shell in `frontend/` is a thin reflection
//! updated through Tauri events. An Axum server (default port 7524) serves
//! panel-type directories and the scripting API.

pub mod command_registry;
pub mod config;
pub mod events;
pub mod keybindings;
pub mod notifier;
pub mod server;
pub mod state;

/// Startup options, from CLI flags.
pub struct Options {
    pub gui_port: u16,
    pub daemon_url: String,
}

/// Builds and runs the Tauri application; blocks until the window closes.
pub fn run(options: Options) {
    let _ = &options; // wired to GuiState in later milestones
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running the metafolder GUI");
}
