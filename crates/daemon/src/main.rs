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

    /// Configuration file (default: $XDG_CONFIG_HOME/metafolder/config.json)
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

    let grammar = metafolder_daemon::simplified::init();
    let mut state = AppState::new();
    state.set_simplified_grammar(grammar);
    let state = Arc::new(state);

    let startup_state = state.clone();
    let warnings = tokio::task::spawn_blocking(move || {
        daemon_config::apply(&startup_state, config)
    })
    .await
    .expect("startup repo loading panicked");
    for warning in warnings {
        eprintln!("[daemon] Warning: {warning}");
    }

    let app = routes::build(state);

    let addr = format!("127.0.0.1:{}", args.port);
    let listener = TcpListener::bind(&addr).await.expect("Failed to bind the listening port");

    println!("[daemon] Listening on http://{addr}");
    axum::serve(listener, app).await.expect("HTTP server failed");
}
