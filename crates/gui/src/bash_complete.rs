//! Bash completion for the bash input (spec-gui "Command input"): runs an
//! embedded harness script in a throwaway `bash`, using the system
//! bash-completion package when installed and falling back to `compgen`
//! (command names in command position, filenames elsewhere). The line is
//! never `eval`ed: completion cannot expand or execute anything the user
//! typed (completion *functions* may run their own helpers, exactly like
//! pressing Tab in a terminal).

use serde::Serialize;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

const HARNESS: &str = include_str!("bash_complete.sh");

/// Upper bound on returned candidates: a directory of thousands of files, or a
/// one-letter prefix, still matches more than a list can usefully show.
/// (The harness no longer produces the worst case — an empty command word — at
/// all: see `complete_fallback` in bash_complete.sh.)
const MAX_CANDIDATES: usize = 500;

/// A stuck completion function must never freeze the input.
const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, PartialEq, Serialize)]
pub struct Completion {
    /// The word being completed: the trailing part of the submitted line
    /// that the candidates replace.
    pub word: String,
    pub candidates: Vec<String>,
}

/// Completes `line_to_cursor` (the command line truncated at the cursor).
/// A missing `bash` or a timeout is an error; "no candidates" is not.
pub async fn complete(line_to_cursor: &str) -> Result<Completion, String> {
    let child = Command::new("bash")
        .arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(HARNESS)
        .arg("bash-complete")
        .arg(line_to_cursor)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot run bash: {e}"))?;

    let output = tokio::time::timeout(TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "bash completion timed out".to_string())?
        .map_err(|e| format!("bash completion failed: {e}"))?;
    if !output.status.success() {
        return Err("bash completion failed".to_string());
    }

    // NUL-separated (filenames may contain newlines): the completed word
    // first, then the candidates, deduplicated preserving bash's order.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut parts = stdout.split('\0');
    let word = parts.next().unwrap_or("").to_string();
    let mut candidates: Vec<String> = Vec::new();
    for candidate in parts.filter(|c| !c.is_empty()) {
        if !candidates.iter().any(|seen| seen == candidate) {
            candidates.push(candidate.to_string());
        }
        if candidates.len() >= MAX_CANDIDATES {
            break;
        }
    }
    Ok(Completion { word, candidates })
}

#[tauri::command]
pub async fn bash_complete(line: String) -> Result<Completion, String> {
    complete(&line).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The programmable-completion path needs the bash-completion package;
    /// without it the harness must use the compgen fallback, whose exact
    /// output some tests assert.
    fn fallback_only() -> bool {
        !std::path::Path::new("/usr/share/bash-completion/bash_completion").exists()
    }

    #[tokio::test]
    async fn test_command_position_completes_command_names() {
        let completion = complete("ech").await.unwrap();
        assert_eq!(completion.word, "ech");
        assert!(
            completion.candidates.iter().any(|c| c == "echo"),
            "echo missing: {:?}",
            completion.candidates
        );
    }

    #[tokio::test]
    async fn test_empty_line_lists_commands() {
        let completion = complete("").await.unwrap();
        assert_eq!(completion.word, "");
        assert!(!completion.candidates.is_empty());
    }

    /// An empty command word lists the shell's own commands, and does *not*
    /// walk PATH: `compgen -c ""` generates every executable on the system
    /// (8000+ here, ~1 s) to produce a list nobody can read — bash itself
    /// refuses to print it. One typed character switches to the full search,
    /// which is what the next test pins down.
    #[tokio::test]
    async fn test_empty_word_does_not_enumerate_path() {
        let completion = complete("").await.unwrap();
        assert!(
            completion.candidates.iter().any(|c| c == "if"),
            "shell keywords missing: {:?}",
            completion.candidates
        );
        assert!(
            completion.candidates.iter().any(|c| c == "cd"),
            "shell builtins missing: {:?}",
            completion.candidates
        );
        // `bash` is on PATH and is neither a builtin nor a keyword, so its
        // presence would mean PATH was walked after all.
        assert!(
            !completion.candidates.iter().any(|c| c == "bash"),
            "PATH was enumerated for the empty word"
        );
    }

    /// ...and a prefix — even one character — still searches all of PATH.
    #[tokio::test]
    async fn test_one_character_prefix_searches_path() {
        let completion = complete("ba").await.unwrap();
        assert!(
            completion.candidates.iter().any(|c| c == "bash"),
            "PATH not searched for a prefixed word: {:?}",
            completion.candidates
        );
    }

    #[tokio::test]
    async fn test_argument_position_completes_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alpha.txt"), "").unwrap();
        std::fs::write(dir.path().join("alpha2.txt"), "").unwrap();
        let base = dir.path().to_str().unwrap();

        let completion = complete(&format!("cat {base}/alph")).await.unwrap();
        assert_eq!(completion.word, format!("{base}/alph"));
        // Trailing-slash decoration differs between the two paths; compare
        // the bare names.
        let names: Vec<&str> = completion
            .candidates
            .iter()
            .map(|c| c.trim_end_matches('/'))
            .collect();
        assert!(names.contains(&format!("{base}/alpha.txt").as_str()), "{names:?}");
        assert!(names.contains(&format!("{base}/alpha2.txt").as_str()), "{names:?}");
    }

    #[tokio::test]
    async fn test_directories_get_a_trailing_slash() {
        if !fallback_only() {
            return; // bash-completion's _filedir leaves the decoration to readline
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("alphadir")).unwrap();
        let base = dir.path().to_str().unwrap();

        let completion = complete(&format!("cat {base}/alph")).await.unwrap();
        assert!(
            completion.candidates.iter().any(|c| c == &format!("{base}/alphadir/")),
            "{:?}",
            completion.candidates
        );
    }

    #[tokio::test]
    async fn test_trailing_space_starts_a_new_word() {
        let completion = complete("ls ").await.unwrap();
        assert_eq!(completion.word, "");
    }

    #[tokio::test]
    async fn test_quote_prefix_is_stripped_from_the_word() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alpha.txt"), "").unwrap();
        let base = dir.path().to_str().unwrap();

        let completion = complete(&format!("cat \"{base}/alph")).await.unwrap();
        // The returned word excludes the quote, so the frontend replaces
        // only the text after it.
        assert_eq!(completion.word, format!("{base}/alph"));
        let names: Vec<&str> = completion
            .candidates
            .iter()
            .map(|c| c.trim_end_matches('/'))
            .collect();
        assert!(names.contains(&format!("{base}/alpha.txt").as_str()), "{names:?}");
    }

    #[tokio::test]
    async fn test_tilde_word_is_returned_verbatim() {
        // Candidates may be $HOME-expanded, but the replaced word must be
        // exactly what is on screen.
        let completion = complete("ls ~/").await.unwrap();
        assert_eq!(completion.word, "~/");
    }

    #[tokio::test]
    async fn test_command_substitution_is_not_executed() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("executed");
        let line = format!("echo $(touch {} )x", marker.to_str().unwrap());
        let completion = complete(&line).await;
        assert!(completion.is_ok());
        assert!(!marker.exists(), "completion executed the command substitution");
    }

    #[tokio::test]
    async fn test_unclosed_quote_does_not_error() {
        assert!(complete("echo \"unclosed").await.is_ok());
        assert!(complete("echo 'unclosed").await.is_ok());
    }

    #[tokio::test]
    async fn test_no_match_yields_empty_candidates() {
        let completion = complete("cat /no/such/dir-mf-test/x").await.unwrap();
        assert!(completion.candidates.is_empty());
    }

    #[tokio::test]
    async fn test_candidates_are_capped() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(MAX_CANDIDATES + 50) {
            std::fs::write(dir.path().join(format!("f{i:04}")), "").unwrap();
        }
        let base = dir.path().to_str().unwrap();
        let completion = complete(&format!("cat {base}/f")).await.unwrap();
        assert_eq!(completion.candidates.len(), MAX_CANDIDATES);
    }
}
