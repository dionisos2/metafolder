use std::path::PathBuf;

use anyhow::Context;
use metafolder_core::entry::{Field, Metadata, Value};
use metafolder_core::query::Query;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Local response types ───────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct RepoInfo {
    pub repo_uuid: Uuid,
    pub root: PathBuf,
    pub version: u32,
    pub created_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReconcileResult {
    pub created: usize,
    pub cleared: usize,
}

// ── Repo endpoints ────────────────────────────────────────────────────────────

pub async fn init_repo(base: &str, root: &str) -> anyhow::Result<Uuid> {
    #[derive(Serialize)]
    struct Req<'a> {
        root: &'a str,
    }
    let uuid: Uuid = Client::new()
        .post(format!("{base}/repos/init"))
        .json(&Req { root })
        .send()
        .await
        .context("init_repo request")?
        .error_for_status()
        .context("init_repo status")?
        .json()
        .await
        .context("init_repo parse")?;
    Ok(uuid)
}

pub async fn load_repo(base: &str, root: &str) -> anyhow::Result<Uuid> {
    #[derive(Serialize)]
    struct Req<'a> {
        root: &'a str,
    }
    let uuid: Uuid = Client::new()
        .post(format!("{base}/repos/load"))
        .json(&Req { root })
        .send()
        .await
        .context("load_repo request")?
        .error_for_status()
        .context("load_repo status")?
        .json()
        .await
        .context("load_repo parse")?;
    Ok(uuid)
}

pub async fn list_repos(base: &str) -> anyhow::Result<Vec<RepoInfo>> {
    let repos: Vec<RepoInfo> = Client::new()
        .get(format!("{base}/repos"))
        .send()
        .await
        .context("list_repos request")?
        .error_for_status()
        .context("list_repos status")?
        .json()
        .await
        .context("list_repos parse")?;
    Ok(repos)
}

// ── Entry endpoints ───────────────────────────────────────────────────────────

#[derive(Serialize)]
struct CreateEntryReq {
    fields: Vec<Field>,
}

pub async fn create_entry(
    base: &str,
    repo: Uuid,
    fields: Vec<Field>,
) -> anyhow::Result<Metadata> {
    let body = CreateEntryReq { fields };
    let m: Metadata = Client::new()
        .post(format!("{base}/repos/{repo}/entries"))
        .json(&body)
        .send()
        .await
        .context("create_entry request")?
        .error_for_status()
        .context("create_entry status")?
        .json()
        .await
        .context("create_entry parse")?;
    Ok(m)
}

pub async fn get_entry(base: &str, repo: Uuid, entry: Uuid) -> anyhow::Result<Metadata> {
    let m: Metadata = Client::new()
        .get(format!("{base}/repos/{repo}/entries/{entry}"))
        .send()
        .await
        .context("get_entry request")?
        .error_for_status()
        .context("get_entry status")?
        .json()
        .await
        .context("get_entry parse")?;
    Ok(m)
}

pub async fn list_entries(base: &str, repo: Uuid) -> anyhow::Result<Vec<Uuid>> {
    let uuids: Vec<Uuid> = Client::new()
        .get(format!("{base}/repos/{repo}/entries"))
        .send()
        .await
        .context("list_entries request")?
        .error_for_status()
        .context("list_entries status")?
        .json()
        .await
        .context("list_entries parse")?;
    Ok(uuids)
}

pub async fn delete_entry(base: &str, repo: Uuid, entry: Uuid) -> anyhow::Result<()> {
    Client::new()
        .delete(format!("{base}/repos/{repo}/entries/{entry}"))
        .send()
        .await
        .context("delete_entry request")?
        .error_for_status()
        .context("delete_entry status")?;
    Ok(())
}

pub async fn set_field(
    base: &str,
    repo: Uuid,
    entry: Uuid,
    name: &str,
    value: metafolder_core::entry::Value,
) -> anyhow::Result<Metadata> {
    #[derive(Serialize)]
    struct Req<'a> {
        name: &'a str,
        value: &'a Value,
    }
    let m: Metadata = Client::new()
        .patch(format!("{base}/repos/{repo}/entries/{entry}"))
        .json(&Req { name, value: &value })
        .send()
        .await
        .context("set_field request")?
        .error_for_status()
        .context("set_field status")?
        .json()
        .await
        .context("set_field parse")?;
    Ok(m)
}

// ── Query / Reconcile ─────────────────────────────────────────────────────────

pub async fn query(base: &str, repo: Uuid, q: &Query) -> anyhow::Result<Vec<Uuid>> {
    let uuids: Vec<Uuid> = Client::new()
        .post(format!("{base}/repos/{repo}/query"))
        .json(q)
        .send()
        .await
        .context("query request")?
        .error_for_status()
        .context("query status")?
        .json()
        .await
        .context("query parse")?;
    Ok(uuids)
}

pub async fn reconcile(base: &str, repo: Uuid) -> anyhow::Result<ReconcileResult> {
    let r: ReconcileResult = Client::new()
        .post(format!("{base}/repos/{repo}/reconcile"))
        .send()
        .await
        .context("reconcile request")?
        .error_for_status()
        .context("reconcile status")?
        .json()
        .await
        .context("reconcile parse")?;
    Ok(r)
}
