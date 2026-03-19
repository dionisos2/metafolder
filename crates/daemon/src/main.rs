use std::sync::Arc;

use axum::Router;
use clap::Parser;
use tokio::net::TcpListener;

mod config;
mod db;
mod query_exec;
mod routes;
mod state;
mod watcher;

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

    let state = Arc::new(state::AppState::new());
    let app: Router = routes::build(state);

    let addr = format!("127.0.0.1:{}", args.port);
    let listener = TcpListener::bind(&addr)
        .await
        .expect("Failed to start the server");

    println!("[daemon] Listening on http://{addr}");
    axum::serve(listener, app).await.unwrap();
}
