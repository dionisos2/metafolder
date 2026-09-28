//! The real data folders as repositories of the suite (spec-perf "Where the
//! data comes from"): `benchmarks/bench_data*` are consume-only — the data
//! bench inits them in place and needs them bare — so the suite measures a
//! copy of each under `target/bench-data/`, its files hard-linked (no byte
//! copied) and its repository built.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::json;
use uuid::Uuid;

/// Recreates `src`'s tree under `dest`, every file hard-linked, leaving out a
/// repository the source may hold at its top (`.metafolder`: someone's
/// repository over the folder, not data). Returns how many files it linked.
pub fn link_tree(src: &Path, dest: &Path) -> Result<usize> {
    let mut files = 0;
    let mut stack = vec![(src.to_path_buf(), dest.to_path_buf())];
    while let Some((from, to)) = stack.pop() {
        std::fs::create_dir_all(&to).with_context(|| format!("create {}", to.display()))?;
        for entry in std::fs::read_dir(&from)? {
            let entry = entry?;
            let name = entry.file_name();
            if from == src && name == ".metafolder" {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                stack.push((entry.path(), to.join(&name)));
            } else if kind.is_file() {
                std::fs::hard_link(entry.path(), to.join(&name))
                    .with_context(|| format!("link {}", entry.path().display()))?;
                files += 1;
            }
        }
    }
    Ok(files)
}

/// Builds (or reuses) the measured copy of the real folder `src` at `dest`:
/// linked files, a repository named `name`, tracking enabled
/// and a full reconcile run — through the suite's daemon at `url`, which is
/// left without it loaded. Returns whether it had to build it.
pub async fn ensure(url: &str, src: &Path, dest: &Path, name: &str) -> Result<bool> {
    let stamp_path = dest.join(".bench-shape");
    let stamp = format!("{name}:{}:api{}", src.display(), metafolder_core::API_VERSION);
    if std::fs::read_to_string(&stamp_path).is_ok_and(|s| s.trim() == stamp) {
        println!("reusing {} ({stamp})", dest.display());
        return Ok(false);
    }
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    print!("building {} from {} ... ", dest.display(), src.display());
    use std::io::Write as _;
    std::io::stdout().flush().ok();
    let t = Instant::now();
    let files = link_tree(src, dest)?;
    let v: serde_json::Value = crate::daemon_client()
        .post(format!("{url}/repos/init"))
        .json(&json!({"root": dest, "name": name}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let repo: Uuid = v["repo_uuid"].as_str().context("missing repo_uuid")?.parse()?;
    crate::api_enable_watch(url, repo).await?;
    let (created, _) = crate::api_reconcile(url, repo, false).await?;
    crate::daemon_client()
        .post(format!("{url}/repos/{repo}/unload"))
        .json(&json!({}))
        .send()
        .await?
        .error_for_status()?;
    std::fs::write(&stamp_path, &stamp)?;
    println!("{files} files, {created} metarecords, {:.1}s", t.elapsed().as_secs_f64());
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_linked_copy_holds_the_files_and_not_the_source_repository() {
        let base = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("bench-real-{}", std::process::id()));
        let (src, dest) = (base.join("src"), base.join("dest"));
        std::fs::create_dir_all(src.join("a/b")).unwrap();
        std::fs::create_dir_all(src.join(".metafolder/internal")).unwrap();
        std::fs::write(src.join("top.txt"), "top").unwrap();
        std::fs::write(src.join("a/b/deep.txt"), "deep").unwrap();
        std::fs::write(src.join(".metafolder/internal/db"), "db").unwrap();

        let files = link_tree(&src, &dest).unwrap();
        assert_eq!(files, 2);
        assert_eq!(std::fs::read_to_string(dest.join("a/b/deep.txt")).unwrap(), "deep");
        assert!(!dest.join(".metafolder").exists(), "the source's repository is not data");
        // Linked, not copied: one inode.
        use std::os::unix::fs::MetadataExt;
        let ino = |p: &std::path::Path| std::fs::metadata(p).unwrap().ino();
        assert_eq!(ino(&src.join("top.txt")), ino(&dest.join("top.txt")));
        std::fs::remove_dir_all(&base).unwrap();
    }
}
