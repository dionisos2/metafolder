//! `mf script` — the user's scripts directory, made findable (doc "mf script").
//! No daemon: the scripts are files of the configuration repository, in
//! `~/.config/metafolder/scripts/`; the templates beside them, under
//! `templates/`, are copied in by `mf script new`. The work is in
//! `metafolder_core::scripts`, shared with the GUI's `script:run`.

use std::io::{BufRead, Write};
use std::path::Path;

use metafolder_core::scripts::{list_scripts_in, list_templates_in, new_from_template, ScriptInfo};

use crate::client::CliError;

fn io(error: std::io::Error) -> CliError {
    CliError::Op(format!("cannot write the output: {error}"))
}

/// `name` padded to `width`, then the summary: one line per script.
fn rows(out: &mut impl Write, items: &[ScriptInfo], strip_sh: bool) -> std::io::Result<()> {
    let label = |s: &ScriptInfo| {
        if strip_sh {
            s.name.trim_end_matches(".sh").to_string()
        } else {
            s.name.clone()
        }
    };
    let width = items.iter().map(|s| label(s).chars().count()).max().unwrap_or(0);
    for item in items {
        writeln!(out, "  {:width$}  {}", label(item), item.summary)?;
    }
    Ok(())
}

/// `mf script [list]`: where the scripts are, which ones the GUI offers
/// (`script:run`), and the templates `mf script new` copies.
pub fn list(dir: &Path, out: &mut impl Write) -> Result<i32, CliError> {
    match write_list(dir, out) {
        // The reader stopped reading (`mf script | head`): what it read is all
        // it wanted, so that is no failure.
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(0),
        result => result.map(|()| 0).map_err(io),
    }
}

fn write_list(dir: &Path, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "{}", dir.display())?;
    if !dir.is_dir() {
        return writeln!(out, "  (not installed: run metafolder-sync-config)");
    }
    let scripts = list_scripts_in(dir);
    writeln!(out)?;
    if scripts.is_empty() {
        writeln!(out, "No script to run.")?;
    } else {
        writeln!(out, "Scripts (run one in the GUI with script:run, or `bash <path>`):")?;
        rows(out, &scripts, false)?;
    }
    let templates = list_templates_in(dir);
    if !templates.is_empty() {
        writeln!(out)?;
        writeln!(out, "Templates (copy one to adapt it: mf script new <template>):")?;
        rows(out, &templates, true)?;
    }
    Ok(())
}

/// `mf script new [TEMPLATE] [--name NAME]`: copies a template into the
/// scripts directory. Without a template, lists them and — when `answer`
/// can ask (a terminal) — reads which one, by number or name; otherwise that
/// is a usage error naming them.
pub fn new(
    dir: &Path,
    template: Option<&str>,
    name: Option<&str>,
    answer: Option<&mut dyn BufRead>,
    out: &mut impl Write,
) -> Result<i32, CliError> {
    let templates = list_templates_in(dir);
    let chosen = match template {
        Some(template) => template.to_string(),
        None => {
            if templates.is_empty() {
                return Err(CliError::Op(format!(
                    "no template in {} (run metafolder-sync-config)",
                    dir.join(metafolder_core::scripts::TEMPLATES).display()
                )));
            }
            let names: Vec<_> =
                templates.iter().map(|t| t.name.trim_end_matches(".sh").to_string()).collect();
            let Some(answer) = answer else {
                return Err(CliError::Usage(format!(
                    "which template? mf script new <template> ({})",
                    names.join(", ")
                )));
            };
            writeln!(out, "Templates:").map_err(io)?;
            let width = names.iter().map(|n| n.chars().count()).max().unwrap_or(0);
            for (i, (name, t)) in names.iter().zip(&templates).enumerate() {
                writeln!(out, "  {}. {:width$}  {}", i + 1, name, t.summary).map_err(io)?;
            }
            write!(out, "Which one? (number or name) ").map_err(io)?;
            out.flush().map_err(io)?;
            let mut line = String::new();
            answer.read_line(&mut line).map_err(|e| CliError::Op(format!("cannot read: {e}")))?;
            let line = line.trim();
            match line.parse::<usize>() {
                Ok(n) if (1..=names.len()).contains(&n) => names[n - 1].clone(),
                Ok(_) => return Err(CliError::Usage(format!("no template numbered {line}"))),
                Err(_) if line.is_empty() => {
                    return Err(CliError::Usage("no template chosen".into()))
                }
                Err(_) => line.to_string(),
            }
        }
    };
    let made = new_from_template(dir, &chosen, name).map_err(CliError::Op)?;
    writeln!(out, "{}", made.display()).map_err(io)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A scripts directory with one script, a helper and one template.
    fn installed() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("mf-script-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("templates")).unwrap();
        std::fs::write(dir.join("gui-tag-folder.sh"), "#!/bin/sh\n# Summary: Tag a folder.\n")
            .unwrap();
        std::fs::write(dir.join("helper.sh"), "#!/bin/sh\n").unwrap();
        std::fs::write(
            dir.join("templates/sort-folder.sh"),
            "#!/bin/sh\n# Summary: Sort a folder.\n",
        )
        .unwrap();
        dir
    }

    fn text(out: Vec<u8>) -> String {
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn list_shows_the_directory_the_scripts_and_the_templates() {
        let dir = installed();
        let mut out = Vec::new();
        assert_eq!(list(&dir, &mut out).unwrap(), 0);
        let out = text(out);
        assert!(out.starts_with(&format!("{}\n", dir.display())), "{out}");
        assert!(out.contains("gui-tag-folder.sh  Tag a folder."), "{out}");
        assert!(!out.contains("helper.sh"), "{out}");
        assert!(out.contains("mf script new <template>"), "{out}");
        assert!(out.contains("sort-folder  Sort a folder."), "{out}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn list_says_when_nothing_is_installed() {
        let dir = installed().join("missing");
        let mut out = Vec::new();
        list(&dir, &mut out).unwrap();
        assert!(text(out).contains("run metafolder-sync-config"));
    }

    #[test]
    fn new_copies_the_named_template_and_prints_where() {
        let dir = installed();
        let mut out = Vec::new();
        new(&dir, Some("sort-folder"), Some("mine"), None, &mut out).unwrap();
        assert_eq!(text(out), format!("{}\n", dir.join("mine.sh").display()));
        assert!(dir.join("mine.sh").is_file());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn new_without_a_template_asks_which_one_by_number() {
        let dir = installed();
        let mut out = Vec::new();
        let mut answer: &[u8] = b"1\n";
        new(&dir, None, None, Some(&mut answer), &mut out).unwrap();
        let out = text(out);
        assert!(out.contains("1. sort-folder  Sort a folder."), "{out}");
        assert!(dir.join("sort-folder.sh").is_file());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn new_without_a_template_nor_a_terminal_is_a_usage_error_naming_them() {
        let dir = installed();
        let err = new(&dir, None, None, None, &mut Vec::new()).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.message().contains("sort-folder"), "{}", err.message());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn new_refuses_to_overwrite_a_script() {
        let dir = installed();
        let err = new(&dir, Some("sort-folder"), Some("gui-tag-folder"), None, &mut Vec::new())
            .unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.message().contains("already exists"), "{}", err.message());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A reader that stopped reading (`mf script | head -1`): the pipe closes.
    struct ClosedPipe;
    impl Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_closed_pipe_is_not_an_error() {
        let dir = installed();
        assert_eq!(list(&dir, &mut ClosedPipe).unwrap(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
