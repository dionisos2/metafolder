use std::sync::Arc;

use axum::Router;
use clap::Parser;
use tokio::net::TcpListener;

mod db;
mod routes;
mod state;
mod watcher;

#[derive(Parser)]
#[command(name = "metafolder-daemon", about = "Metadata management daemon")]
struct Args {
    /// Root directory of the repository to load
    #[arg(short, long)]
    root: std::path::PathBuf,

    /// HTTP listening port
    #[arg(short, long, default_value = "7523")]
    port: u16,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let state = state::AppState::new(&args.root)
        .expect("Failed to initialize the database");

    watcher::start(args.root.clone(), state.conn.clone(), state.db_id);

    let app: Router = routes::build(Arc::new(state));

    let addr = format!("127.0.0.1:{}", args.port);
    let listener = TcpListener::bind(&addr)
        .await
        .expect("Failed to start the server");

    println!("metafolder-daemon listening on http://{addr}");
    axum::serve(listener, app).await.unwrap();
}
