use super::*;

// ── Orphans (doc "Orphan scan") ────────────────────────────────

/// `mf orphan list` — scan for tracked metarecords whose file is gone and print
/// `uuid <TAB> stale_path`, one per line.
pub fn orphan_list(ctx: &Ctx) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let resp = ctx.client.post(&format!("{base}/orphans/scan"), &json!({}))?;
    let orphans = resp["orphans"].as_array().cloned().unwrap_or_default();
    if orphans.is_empty() {
        println!("No orphans (every tracked file is present on disk).");
        return Ok(0);
    }
    for o in &orphans {
        println!(
            "{}\t{}",
            o["uuid"].as_str().unwrap_or("?"),
            o["stale_path"].as_str().unwrap_or("?"),
        );
    }
    Ok(0)
}

/// `mf orphan detect` — flag every orphaned metarecord with `orphan = true`
/// and take the flag back from the records that are no longer orphaned
/// (doc "Finding and clearing orphans"). Unlike `clear` it touches no
/// `mfr_path`: it only makes the state queryable.
pub fn orphan_detect(ctx: &Ctx) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let resp = ctx.client.post(&format!("{base}/orphans/mark"), &json!({}))?;
    let count = |key: &str| resp[key].as_u64().unwrap_or(0);
    println!(
        "{} orphan(s): {} marked, {} unmarked",
        count("orphans"),
        count("marked"),
        count("unmarked"),
    );
    Ok(0)
}

/// `mf orphan clear` — scan, then orphan the found records (mfr_path_old frozen,
/// mfr_path → Nothing, cascading). Confirms first unless `yes`.
pub fn orphan_clear(ctx: &Ctx, yes: bool) -> Result<i32, CliError> {
    let base = ctx.repo_base()?;
    let resp = ctx.client.post(&format!("{base}/orphans/scan"), &json!({}))?;
    let orphans = resp["orphans"].as_array().cloned().unwrap_or_default();
    if orphans.is_empty() {
        println!("No orphans to clear.");
        return Ok(0);
    }
    let uuids: Vec<String> =
        orphans.iter().filter_map(|o| o["uuid"].as_str().map(String::from)).collect();
    if !yes {
        for o in &orphans {
            eprintln!(
                "  {}  {}",
                o["uuid"].as_str().unwrap_or("?"),
                o["stale_path"].as_str().unwrap_or("?"),
            );
        }
        let prompt = format!("Orphan {} metarecord(s) (mfr_path → Nothing)? [y/N] ", uuids.len());
        if !confirm(&prompt)? {
            println!("Aborted.");
            return Ok(0);
        }
    }
    let resp = ctx.client.post(&format!("{base}/orphans/clear"), &json!({ "uuids": uuids }))?;
    let cleared = resp["cleared"].as_u64().unwrap_or(0);
    println!("cleared {cleared}");
    Ok(0)
}
