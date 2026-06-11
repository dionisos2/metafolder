use std::sync::Arc;

use clap::Parser;
use tokio::net::TcpListener;

use metafolder_daemon::routes;
use metafolder_daemon::state::AppState;

#[derive(Parser)]
#[command(name = "metafolder-daemon", about = "Metadata management daemon")]
struct Args {
    /// HTTP listening port
    #[arg(short, long, default_value = "7523")]
    port: u16,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let state = Arc::new(AppState::new());
    let app = routes::build(state);

    let addr = format!("127.0.0.1:{}", args.port);
    let listener = TcpListener::bind(&addr).await.expect("Failed to bind the listening port");

    println!("[daemon] Listening on http://{addr}");
    axum::serve(listener, app).await.expect("HTTP server failed");
}
