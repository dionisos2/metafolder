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
    /// Start even when the WebView cannot be sandboxed — images and video are
    /// then decoded with your full privileges, so a crafted file can take over
    /// the session. For development inside a container that forbids bubblewrap
    /// (WebKit needs to mount /proc); never for a real repository. Deliberately
    /// a per-run flag, with no config.toml equivalent: it must not rot into a
    /// permanently disabled sandbox.
    #[arg(long)]
    allow_unsandboxed_webview: bool,
}

fn main() {
    let args = Args::parse();
    metafolder_gui::run(metafolder_gui::Options {
        gui_port: args.gui_port,
        daemon_port: args.daemon_port,
        allow_unsandboxed_webview: args.allow_unsandboxed_webview,
    });
}
