use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::net::TcpListener;

use metafolder_daemon::daemon_config;
use metafolder_daemon::routes;
use metafolder_daemon::state::AppState;

#[derive(Parser)]
#[command(name = "metafolder-daemon", about = "Metadata management daemon")]
struct Args {
    /// HTTP listening port
    #[arg(short, long, default_value = "7523")]
    port: u16,

    /// Configuration file (default: $XDG_CONFIG_HOME/metafolder/daemon/config.toml)
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let config_path = args.config.or_else(daemon_config::default_config_path);
    let config = match &config_path {
        Some(path) => match daemon_config::read_config(path) {
            Ok(config) => config,
            Err(e) => {
                eprintln!("[daemon] Invalid configuration: {e:#}");
                std::process::exit(1);
            }
        },
        None => Default::default(),
    };
    for warning in config.settings.obsolete() {
        eprintln!("[daemon] Warning: {warning}");
        metafolder_daemon::diagnostics::warn("config", warning);
    }

    // The copies in memory a daemon that crashed left behind (doc "Holding a
    // repository in memory"): RAM, until the next boot otherwise.
    metafolder_daemon::kvstore::memory::clean_stale(&config.settings.memory_dir);

    let mut state = AppState::new().with_settings(config.settings.clone());
    if let Some(path) = config_path {
        state = state.with_config_file(path);
    }
    let state = Arc::new(state);

    let startup_state = state.clone();
    // `apply` reports each repository — loaded or failed — as it goes.
    tokio::task::spawn_blocking(move || daemon_config::apply(&startup_state, config))
        .await
        .expect("startup repo loading panicked");
    // Each repository's automatic backup, when due (doc "Backups and restore").
    state.start_auto_backups();

    let token = match metafolder_core::auth::ensure_token("daemon") {
        Ok(token) => token,
        Err(e) => {
            eprintln!("[daemon] Cannot establish the session token: {e}");
            std::process::exit(1);
        }
    };
    let app = routes::build_authenticated(state, token.into());

    let addr = format!("127.0.0.1:{}", args.port);
    let listener = TcpListener::bind(&addr).await.expect("Failed to bind the listening port");

    println!("[daemon] Listening on http://{addr}");
    axum::serve(listener, app).await.expect("HTTP server failed");
}
