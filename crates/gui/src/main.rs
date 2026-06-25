//! Thin binary over the GUI library: parses CLI flags and runs the app.

use clap::Parser;

#[derive(Parser)]
#[command(name = "metafolder-gui", about = "Metafolder graphical interface")]
struct Args {
    /// Port of the GUI HTTP server (panel assets + scripting API).
    /// Overrides `gui-port` in config.toml (default 7524).
    #[arg(long)]
    gui_port: Option<u16>,
    /// Port of the metafolder daemon on 127.0.0.1. Overrides `daemon-port` in
    /// config.toml (default 7523, the daemon's default).
    #[arg(long)]
    daemon_port: Option<u16>,
}

fn main() {
    let args = Args::parse();
    metafolder_gui::run(metafolder_gui::Options {
        gui_port: args.gui_port,
        daemon_port: args.daemon_port,
    });
}
