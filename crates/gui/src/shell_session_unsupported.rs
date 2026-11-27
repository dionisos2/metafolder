//! The workspace's persistent shell where it cannot run (doc "shell panel",
//! doc "Platform"): bash, its pipes, the FIFO of a line's input and the
//! process group a line is stopped by are Unix's. Same interface as
//! `shell_session.rs`; every line is refused, naming why.

use crate::state::GuiState;
use std::path::PathBuf;
use std::sync::Arc;

const UNSUPPORTED: &str = "the workspace shell needs Unix (bash, pipes, process groups)";

/// The workspaces' persistent shells: none, here.
#[derive(Default)]
pub struct ShellSessions;

impl ShellSessions {
    /// Refuses `command_line`: there is no shell to run it in.
    pub async fn run(
        &self,
        _gui: &Arc<GuiState>,
        _ws_id: &str,
        _command_line: &str,
        _context: Option<metafolder_core::context::Context>,
        _root: Option<PathBuf>,
        _rc: Option<PathBuf>,
    ) -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }

    /// No line runs, so none reads what is typed.
    pub fn input(
        &self,
        _gui: &GuiState,
        _ws_id: &str,
        _text: Option<&str>,
    ) -> Result<bool, String> {
        Ok(false)
    }

    pub fn running(&self, _ws_id: &str) -> Option<String> {
        None
    }

    pub fn holds(&self, _task_id: &str) -> bool {
        false
    }

    pub fn close(&self, _ws_id: &str) {}
}
