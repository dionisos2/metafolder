//! Thin binary over the GUI library: parses CLI flags and runs the app.

use clap::Parser;

#[derive(Parser)]
#[command(name = "metafolder-gui", about = "Metafolder graphical interface")]
struct Args {
    /// Port of the GUI HTTP server (panel assets + scripting API).
    /// Overrides `gui-port` in config.toml (default 7524).
    #[arg(long)]
    gui_port: Option<u16>,
    /// Base URL of the metafolder daemon. Overrides `daemon-url` in
    /// config.toml (default http://127.0.0.1:7523, the daemon's default).
    #[arg(long)]
    daemon_url: Option<String>,
}

fn main() {
    let args = Args::parse();
    metafolder_gui::run(metafolder_gui::Options {
        gui_port: args.gui_port,
        daemon_url: args.daemon_url,
    });
}
