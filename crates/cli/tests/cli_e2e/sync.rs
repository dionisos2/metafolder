use super::*;

// ── Cross-repo sync: utility subcommands (doc "Sync") ─────

#[test]
fn test_sync_link_status_unlink_roundtrip() {
    let (repo_a, _ra) = init_repo("synca");
    let (repo_b, _rb) = init_repo("syncb");
    let rec_a = create_metarecord(&repo_a, &["tag:string=x"]);
    let rec_b = create_metarecord(&repo_b, &["tag:string=y"]);

    // Link the two records (URL/positional order need not be canonical).
    let out = mf(&["sync", "link", &repo_a, &repo_b, &rec_a, &rec_b]);
    assert_ok(&out);
    let link = out.stdout.trim().to_string();
    assert!(is_hex_uuid(&link), "link should print the link uuid, got: '{}'", out.stdout);

    // status lists the link in the never-synced state.
    let out = mf(&["sync", "status", &repo_a, &repo_b]);
    assert_ok(&out);
    assert!(out.stdout.contains(&link), "status should list the link: {}", out.stdout);
    assert!(out.stdout.contains("never_synced"), "state: {}", out.stdout);

    // status --json exposes the structured per-link states.
    let out = mf(&["sync", "status", &repo_b, &repo_a, "--json"]);
    assert_ok(&out);
    let body: serde_json::Value = serde_json::from_str(&out.stdout).expect("json");
    assert_eq!(body["links"][0]["state"], "never_synced");

    // unlink removes it.
    let out = mf(&["sync", "unlink", &repo_a, &repo_b, &link]);
    assert_ok(&out);
    let out = mf(&["sync", "status", &repo_a, &repo_b]);
    assert_ok(&out);
    assert!(!out.stdout.contains(&link), "the link is gone: {}", out.stdout);
}

#[test]
fn test_sync_link_missing_record_errors() {
    let (repo_a, _ra) = init_repo("synce_a");
    let (repo_b, _rb) = init_repo("synce_b");
    let rec_a = create_metarecord(&repo_a, &["tag:string=x"]);
    let ghost = Uuid::new_v4().as_simple().to_string();
    let out = mf(&["sync", "link", &repo_a, &repo_b, &rec_a, &ghost]);
    assert_eq!(out.code, 1, "missing endpoint is an Op error: {}", out.stderr);
}

#[test]
fn test_sync_unlink_with_endpoint_deletes_record() {
    let (repo_a, _ra) = init_repo("syncw_a");
    let (repo_b, _rb) = init_repo("syncw_b");
    let rec_a = create_metarecord(&repo_a, &["tag:string=x"]);
    let rec_b = create_metarecord(&repo_b, &["tag:string=y"]);
    let out = mf(&["sync", "link", &repo_a, &repo_b, &rec_a, &rec_b]);
    assert_ok(&out);
    let link = out.stdout.trim().to_string();

    // Drop the link plus the endpoint record in the first-named repo.
    let out = mf(&["sync", "unlink", &repo_a, &repo_b, &link, "--with-endpoint", "a"]);
    assert_ok(&out);
    // rec_a is gone …
    let out = mf(&["-u", &repo_a, "metarecord", "-i", &rec_a, "get"]);
    assert_eq!(out.code, 1, "record A should be deleted: {}{}", out.stdout, out.stderr);
    // … rec_b remains.
    let out = mf(&["-u", &repo_b, "metarecord", "-i", &rec_b, "get"]);
    assert_ok(&out);
}

// ── Cross-repo sync: mf sync plan (plan-repo lifecycle) ───────────────────────

#[test]
fn test_sync_plan_creates_hidden_plan_repo() {
    let (a, _ra) = init_repo("plan_a");
    let (b, _rb) = init_repo("plan_b");
    let dir = temp_dir("plan_intents");
    let intents = dir.join("i.toml");
    std::fs::write(&intents, format!("[[intents]]\nrepo = '{a}'\nquery = 'mf_watch = true'\n"))
        .unwrap();

    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);

    // The plan repo is hidden from the default listing …
    let list = mf(&["repo", "list"]);
    assert_ok(&list);
    assert!(!list.stdout.contains("plan-"), "plan repo must be hidden: {}", list.stdout);

    // … but visible with --all, as exactly one system repo for this pair.
    // (The shared daemon may hold other pairs' plan repos, so scope by name.)
    let name = plan_name(&a, &b);
    assert_eq!(count_repos_named(&name), 1, "one plan repo for this pair");

    // Re-running recreates it without error (only the latest plan exists).
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
    assert_eq!(count_repos_named(&name), 1, "still exactly one plan repo after re-plan");
}

/// The plan repo name for a pair (canonical order = sorted simple-hex UUIDs).
pub(super) fn plan_name(a: &str, b: &str) -> String {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    format!("plan-{lo}-{hi}")
}

/// Number of loaded repos (system included) with the given exact name.
fn count_repos_named(name: &str) -> usize {
    let all = mf(&["repo", "list", "--all"]);
    assert_ok(&all);
    let repos: serde_json::Value = serde_json::from_str(&all.stdout).expect("repo list json");
    repos
        .as_array()
        .map(|a| a.iter().filter(|r| r["name"].as_str() == Some(name)).count())
        .unwrap_or(0)
}

/// Extracts the `plan repo: <uuid>` line printed by `mf sync plan`.
fn plan_repo_uuid(out: &Out) -> String {
    out.stdout
        .lines()
        .find_map(|l| l.strip_prefix("plan repo: "))
        .expect("plan should print 'plan repo: <uuid>'")
        .trim()
        .to_string()
}

/// Inits a repo, writes `files` (relative path → bytes), enables tracking on the
/// root and reconciles, so file records get realistic `mfr_path` children.
/// Returns the repo uuid *and* the directory guard — the caller has to keep the
/// latter, or the files go away with it.
pub(super) fn tracked_repo(prefix: &str, files: &[(&str, &[u8])]) -> (String, TempDir) {
    let (repo, root) = init_repo(prefix);
    for (rel, content) in files {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
    }
    // A fresh repo has one entry: the filesystem root.
    let root_uuid = mf(&["-u", &repo, "metarecord", "get"]).stdout.trim().to_string();
    assert_ok(&mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &root_uuid,
        "field",
        "set",
        "mf_watch:bool=true",
    ]));
    assert_ok(&mf(&["-u", &repo, "reconcile"]));
    (repo, root)
}

/// The single uuid matching a DSL query in a repo (asserts exactly one).
pub(super) fn query_one(repo: &str, dsl: &str) -> String {
    let out = mf(&["-u", repo, "metarecord", "-q", dsl, "get"]);
    assert_ok(&out);
    let uuids: Vec<&str> = out.stdout.split_whitespace().collect();
    assert_eq!(uuids.len(), 1, "expected one match for {dsl:?}, got: {}", out.stdout);
    uuids[0].to_string()
}

/// Writes an intents TOML to a fresh temp file. The returned guard *is* the
/// path (it derefs to one) and removes the file with its directory when the
/// test ends — so keep it bound: a temporary would take the file with it.
pub(super) fn write_intents(prefix: &str, content: &str) -> TempFile {
    TempFile::new(prefix, content.as_bytes())
}

#[test]
fn test_sync_plan_exact_match_writes_create_link() {
    // Same relative path on both sides, *different bytes* — so a match can only
    // be by TreeRef identity (path), never by content.
    let (a, _adir) = tracked_repo("plan_ex_a", &[("song.mp3", b"aaa")]);
    let (b, _bdir) = tracked_repo("plan_ex_b", &[("song.mp3", b"bbb")]);
    let rec_a = query_one(&a, "mfr_path:value = \"song.mp3\"");
    let rec_b = query_one(&b, "mfr_path:value = \"song.mp3\"");

    let intents = write_intents(
        "plan_ex",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    // The link, its sync, and — the bytes differing on a first sync — a
    // content conflict (left unresolved: nothing answers the prompt) with the
    // copy it would decide.
    assert!(out.stdout.contains("operations: 4"), "{}", out.stdout);
    let plan = plan_repo_uuid(&out);
    assert_eq!(plan_ops(&a, &b, "conflict").len(), 1, "a content conflict");

    // The plan repo holds one create-link op linking both file records by path.
    let got = mf(&[
        "-u",
        &plan,
        "metarecord",
        "-q",
        "plan_kind = \"create-link\"",
        "get",
        "--select",
        "*",
    ]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    let ops = ops.as_array().unwrap();
    assert_eq!(ops.len(), 1, "one create-link op: {}", got.stdout);
    let endpoints = op_endpoints(&ops[0]);
    assert!(
        endpoints.contains(&rec_a) && endpoints.contains(&rec_b),
        "op must link both records: {endpoints:?}"
    );
    // No link was actually created — plan is read-only w.r.t. the synced repos.
    let status = mf(&["sync", "status", &a, &b]);
    assert!(status.stderr.contains("no links") || status.stdout.trim().is_empty());
}

/// The two endpoint metarecord UUIDs of a plan op (`plan_a`, `plan_b`).
fn op_endpoints(op: &serde_json::Value) -> Vec<String> {
    let fields = op["fields"].as_array().unwrap();
    ["plan_a", "plan_b"]
        .iter()
        .filter_map(|name| {
            fields.iter().find(|f| f["name"] == *name)?.get("value")?["value"]["metarecord"]
                .as_str()
                .map(String::from)
        })
        .collect()
}

#[test]
fn test_sync_plan_no_match_allocates_bare_record() {
    // In scope in A, with no counterpart at the same path in B → bare record.
    let (a, _adir) = tracked_repo("plan_bare_a", &[("lonely.mp3", b"x")]);
    let (b, _bdir) = tracked_repo("plan_bare_b", &[]);
    let rec_a = query_one(&a, "mfr_path:value = \"lonely.mp3\"");
    let intents = write_intents(
        "plan_bare",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );

    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    // A bare *file* link → create-link + sync (placement) + copy (content) + chmod (mode).
    assert!(
        out.stdout.contains("operations: 4"),
        "create-link + sync + copy + chmod: {}",
        out.stdout
    );
    let plan = plan_repo_uuid(&out);

    // A copy op sources the content from the existing side (plan_from = a|b).
    let cp = mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"copy\"", "get", "--select", "*"]);
    assert_ok(&cp);
    let copies: serde_json::Value = serde_json::from_str(&cp.stdout).unwrap();
    let copy = copies.as_array().unwrap();
    assert_eq!(copy.len(), 1, "one copy op: {}", cp.stdout);
    assert!(op_endpoints(&copy[0]).contains(&rec_a), "copy references the source file");
    let from = copy[0]["fields"].as_array().unwrap().iter().find(|f| f["name"] == "plan_from");
    assert!(from.is_some(), "copy op records plan_from: {}", cp.stdout);
    // And a chmod op, from the same source side, to set the new file's mode.
    let chm =
        mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"chmod\"", "get", "--select", "*"]);
    assert_ok(&chm);
    let chmods: serde_json::Value = serde_json::from_str(&chm.stdout).unwrap();
    assert_eq!(chmods.as_array().unwrap().len(), 1, "one chmod op: {}", chm.stdout);
    let cfrom = chmods.as_array().unwrap()[0]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "plan_from");
    assert!(cfrom.is_some(), "chmod op records plan_from: {}", chm.stdout);

    let got = mf(&[
        "-u",
        &plan,
        "metarecord",
        "-q",
        "plan_kind = \"create-link\"",
        "get",
        "--select",
        "*",
    ]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    let op = ops.as_array().unwrap()[0].clone();
    // One endpoint is the existing record; the other is a fresh bare UUID.
    let endpoints = op_endpoints(&op);
    assert!(endpoints.contains(&rec_a), "existing endpoint present: {endpoints:?}");
    let bare = endpoints.iter().find(|u| **u != rec_a).expect("a bare endpoint");
    assert!(is_hex_uuid(bare), "bare endpoint is a uuid: {bare}");
    // The bare side carries NO baseline (the record does not exist yet).
    let fields = op["fields"].as_array().unwrap();
    let bare_is_a = fields
        .iter()
        .any(|f| f["name"] == "plan_a" && f["value"]["value"]["metarecord"].as_str() == Some(bare));
    let bare_version_field = if bare_is_a { "plan_version_a" } else { "plan_version_b" };
    assert!(
        !fields.iter().any(|f| f["name"] == bare_version_field),
        "bare side must have no {bare_version_field}: {op}"
    );
    // The existing side does carry its baseline.
    let existing_version_field = if bare_is_a { "plan_version_b" } else { "plan_version_a" };
    assert!(
        fields.iter().any(|f| f["name"] == existing_version_field),
        "existing side keeps its baseline: {op}"
    );
}

#[test]
fn test_sync_plan_closes_over_no_identity_ref_target() {
    // A file X (in scope) refs an abstract, out-of-scope, identity-less record.
    let (a, _adir) = tracked_repo("clos_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("clos_b", &[("doc.txt", b"y")]);
    let x = query_one(&a, "mfr_path:value = \"doc.txt\"");
    let person = create_metarecord(&a, &["name:string=alice"]); // no tree_ref → no identity
    assert_ok(&mf(&[
        "-u",
        &a,
        "metarecord",
        "-i",
        &x,
        "field",
        "add",
        &format!("author:ref={person}"),
    ]));

    // Scope selects files only → `person` is out of scope.
    let intents = write_intents(
        "clos",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    let plan = plan_repo_uuid(&out);

    // Two links: X↔X_B (by path) and person↔bare (referential closure).
    let got = mf(&[
        "-u",
        &plan,
        "metarecord",
        "-q",
        "plan_kind = \"create-link\"",
        "get",
        "--select",
        "*",
    ]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    let ops = ops.as_array().unwrap();
    assert_eq!(ops.len(), 2, "two create-link ops");
    // One op links `person` to a freshly allocated bare record.
    let person_op = ops
        .iter()
        .find(|o| op_endpoints(o).contains(&person))
        .expect("a link for the referenced record");
    let endpoints = op_endpoints(person_op);
    let bare = endpoints.iter().find(|u| **u != person).expect("bare counterpart");
    assert!(is_hex_uuid(bare) && *bare != person);
}

#[test]
fn test_sync_plan_case0_field_equality_links() {
    // No-identity records (no tree_ref) with equal fields link by the case-0
    // heuristic — the typical tombstone-style match.
    let (a, _ar) = init_repo("case0_a");
    let (b, _br) = init_repo("case0_b");
    let rec_a = create_metarecord(&a, &["tag:string=alice", "rating:int=5"]);
    let rec_b = create_metarecord(&b, &["tag:string=alice", "rating:int=5"]);
    // A distractor in B with different fields must not match.
    create_metarecord(&b, &["tag:string=bob"]);

    let intents =
        write_intents("case0", &format!("[[intents]]\nrepo = '{a}'\nquery = 'tag = \"alice\"'\n"));
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    assert!(out.stdout.contains("operations: 2"), "one link, and its sync: {}", out.stdout);
    let plan = plan_repo_uuid(&out);

    let got = mf(&[
        "-u",
        &plan,
        "metarecord",
        "-q",
        "plan_kind = \"create-link\"",
        "get",
        "--select",
        "*",
    ]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    let endpoints = op_endpoints(&ops.as_array().unwrap()[0]);
    assert!(
        endpoints.contains(&rec_a) && endpoints.contains(&rec_b),
        "must link the two field-equal records: {endpoints:?}"
    );
}

#[test]
fn test_sync_plan_case0_ambiguous_stays_bare() {
    // Two identical B candidates → ambiguous → no field match → bare record.
    let (a, _ar) = init_repo("case0amb_a");
    let (b, _br) = init_repo("case0amb_b");
    let rec_a = create_metarecord(&a, &["tag:string=dup"]);
    create_metarecord(&b, &["tag:string=dup"]);
    create_metarecord(&b, &["tag:string=dup"]);

    let intents =
        write_intents("case0amb", &format!("[[intents]]\nrepo = '{a}'\nquery = 'tag = \"dup\"'\n"));
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    // Bare link (ambiguous → no match) → create-link + sync.
    assert!(out.stdout.contains("operations: 2"), "create-link + sync: {}", out.stdout);
    let plan = plan_repo_uuid(&out);
    let got = mf(&[
        "-u",
        &plan,
        "metarecord",
        "-q",
        "plan_kind = \"create-link\"",
        "get",
        "--select",
        "*",
    ]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    let endpoints = op_endpoints(&ops.as_array().unwrap()[0]);
    // rec_a is linked to a fresh bare record, not to either ambiguous candidate.
    assert!(endpoints.contains(&rec_a));
    let other = endpoints.iter().find(|u| **u != rec_a).unwrap();
    assert!(is_hex_uuid(other));
}

#[test]
fn test_sync_plan_writes_sync_op_on_field_diff() {
    // Same file (matched by path) but a user field on one side only → a `sync`
    // op propagates it, alongside the create-link.
    let (a, _adir) = tracked_repo("syncop_a", &[("doc.txt", b"same")]);
    let (b, _bdir) = tracked_repo("syncop_b", &[("doc.txt", b"same")]);
    let x = query_one(&a, "mfr_path:value = \"doc.txt\"");
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &x, "field", "add", "tag:string=jazz"]));

    let intents = write_intents(
        "syncop",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    assert!(out.stdout.contains("operations: 2"), "create-link + sync: {}", out.stdout);
    let plan = plan_repo_uuid(&out);

    // Exactly one sync op, referencing the linked pair.
    let got =
        mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"sync\"", "get", "--select", "*"]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    assert_eq!(ops.as_array().unwrap().len(), 1, "one sync op: {}", got.stdout);
    assert!(op_endpoints(&ops.as_array().unwrap()[0]).contains(&x));
}

/// Two matched files with nothing to propagate still get a `sync` op: it is
/// what records the new link's state, so that the next plan finds the pair in
/// sync and plans nothing at all.
#[test]
fn test_sync_plan_a_link_with_nothing_to_propagate_is_still_recorded() {
    let (a, _adir) = tracked_repo("nosync_a", &[("doc.txt", b"aaa")]);
    let (b, _bdir) = tracked_repo("nosync_b", &[("doc.txt", b"aaa")]);
    let intents = write_intents(
        "nosync",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    assert!(out.stdout.contains("operations: 2"), "create-link + sync: {}", out.stdout);
    assert_ok(&mf(&["sync", "run", &a, &b, "--yes"]));
    let status = mf(&["sync", "status", &a, &b]);
    assert!(status.stdout.contains("in_sync"), "{}", status.stdout);

    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    assert!(out.stdout.contains("operations: 0"), "nothing left to do: {}", out.stdout);
}

#[test]
fn test_sync_plan_conflict_resolved_by_on_conflict() {
    // Matched files, same field with different values on each side → a conflict,
    // resolved non-interactively by --on-conflict prefer:<repo_a>.
    let (a, _adir) = tracked_repo("conf_a", &[("doc.txt", b"same")]);
    let (b, _bdir) = tracked_repo("conf_b", &[("doc.txt", b"same")]);
    let xa = query_one(&a, "mfr_path:value = \"doc.txt\"");
    let xb = query_one(&b, "mfr_path:value = \"doc.txt\"");
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &xa, "field", "add", "tag:string=jazz"]));
    assert_ok(&mf(&["-u", &b, "metarecord", "-i", &xb, "field", "add", "tag:string=rock"]));

    let intents = write_intents(
        "conf",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    let out = mf(&[
        "sync",
        "plan",
        &a,
        &b,
        "--intents",
        intents.to_str().unwrap(),
        "--on-conflict",
        &format!("prefer:{a}"),
    ]);
    assert_ok(&out);
    let plan = plan_repo_uuid(&out);

    // One conflict op on `tag`, resolved to a canonical side, with both values.
    let got =
        mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"conflict\"", "get", "--select", "*"]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    let ops = ops.as_array().unwrap();
    assert_eq!(ops.len(), 1, "one conflict op: {}", got.stdout);
    let f = |name: &str| {
        ops[0]["fields"].as_array().unwrap().iter().find(|x| x["name"] == name).cloned()
    };
    assert_eq!(f("plan_field").unwrap()["value"]["value"], "tag");
    // prefer:repo_a → resolved to repo_a's canonical side (plan_a/plan_b are canonical).
    let expect_side = if a < b { "a" } else { "b" };
    let resolve = f("plan_resolve").unwrap()["value"]["value"].as_str().unwrap().to_string();
    assert_eq!(resolve, expect_side);
    // The resolved (repo_a) side holds "jazz"; the other holds "rock".
    let fields = ops[0]["fields"].as_array().unwrap();
    let val = |name: &str| {
        fields
            .iter()
            .filter(|x| x["name"] == name)
            .map(|x| x["value"]["value"].clone())
            .collect::<Vec<_>>()
    };
    let (resolved_field, other_field) = if resolve == "a" {
        ("plan_value_a", "plan_value_b")
    } else {
        ("plan_value_b", "plan_value_a")
    };
    assert_eq!(val(resolved_field), vec!["jazz"], "repo_a's value wins");
    assert_eq!(val(other_field), vec!["rock"]);
}

#[test]
fn test_sync_plan_resyncs_existing_link() {
    // A pre-existing link (as if from a prior sync) is re-synced, not recreated.
    let (a, _adir) = tracked_repo("resync_a", &[("doc.txt", b"same")]);
    let (b, _bdir) = tracked_repo("resync_b", &[("doc.txt", b"same")]);
    let xa = query_one(&a, "mfr_path:value = \"doc.txt\"");
    let xb = query_one(&b, "mfr_path:value = \"doc.txt\"");
    assert_ok(&mf(&["sync", "link", &a, &b, &xa, &xb]));
    // A user field appears on A only since the link was made.
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &xa, "field", "add", "tag:string=x"]));

    let intents = write_intents(
        "resync",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    let plan = plan_repo_uuid(&out);

    // No new link — the pair was already linked.
    let creates = mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"create-link\"", "get"]);
    assert_ok(&creates);
    assert!(creates.stdout.trim().is_empty(), "no create-link: {}", creates.stdout);
    // One re-sync op for the existing link.
    let syncs =
        mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"sync\"", "get", "--select", "*"]);
    assert_ok(&syncs);
    let ops: serde_json::Value = serde_json::from_str(&syncs.stdout).unwrap();
    assert_eq!(ops.as_array().unwrap().len(), 1, "one re-sync op: {}", syncs.stdout);
    assert!(op_endpoints(&ops.as_array().unwrap()[0]).contains(&xa));
}

#[test]
fn test_sync_plan_keeps_out_of_scope_link() {
    // An existing link whose endpoints are out of the current scope is left
    // untouched (persistent state) — never dropped.
    let (a, _adir) = tracked_repo("keep_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("keep_b", &[("doc.txt", b"x")]);
    let xa = query_one(&a, "mfr_path:value = \"doc.txt\"");
    let xb = query_one(&b, "mfr_path:value = \"doc.txt\"");
    let out = mf(&["sync", "link", &a, &b, &xa, &xb]);
    assert_ok(&out);
    let link = out.stdout.trim().to_string();

    // A scope that matches nothing → the doc.txt link is out of scope.
    let intents =
        write_intents("keep", &format!("[[intents]]\nrepo = '{a}'\nquery = 'tag = \"none\"'\n"));
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    let plan = plan_repo_uuid(&out);

    // No drop-link op, and the link is still in the sync database.
    let drops = mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"drop-link\"", "get"]);
    assert_ok(&drops);
    assert!(drops.stdout.trim().is_empty(), "no drop-link op: {}", drops.stdout);
    let status = mf(&["sync", "status", &a, &b]);
    assert_ok(&status);
    assert!(status.stdout.contains(&link), "the link is kept: {}", status.stdout);
}

#[test]
fn test_sync_plan_move_op_on_diverged_path() {
    // Two linked files whose positions diverge (a.txt ↔ b.txt) → a move op. This
    // is the state after a rename on one side, or a manual cross-path link.
    let (a, _adir) = tracked_repo("move_a", &[("a.txt", b"content")]);
    let (b, _bdir) = tracked_repo("move_b", &[("b.txt", b"content")]);
    let xa = query_one(&a, "mfr_path:value = \"a.txt\"");
    let xb = query_one(&b, "mfr_path:value = \"b.txt\"");
    assert_ok(&mf(&["sync", "link", &a, &b, &xa, &xb]));

    let intents = write_intents(
        "move",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    let plan = plan_repo_uuid(&out);

    let got =
        mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"move\"", "get", "--select", "*"]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    assert_eq!(ops.as_array().unwrap().len(), 1, "one move op: {}", got.stdout);
    assert!(op_endpoints(&ops.as_array().unwrap()[0]).contains(&xa));
}

#[test]
fn test_sync_plan_delete_op_on_deleted_endpoint() {
    // A linked record deleted on side A → a delete op removing the surviving B.
    let (a, _adir) = tracked_repo("del_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("del_b", &[("doc.txt", b"x")]);
    let xa = query_one(&a, "mfr_path:value = \"doc.txt\"");
    let xb = query_one(&b, "mfr_path:value = \"doc.txt\"");
    assert_ok(&mf(&["sync", "link", &a, &b, &xa, &xb]));
    // Delete A's metarecord.
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &xa, "delete"]));

    // Scope selects the surviving side (B).
    let intents = write_intents(
        "del",
        &format!("[[intents]]\nrepo = '{b}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    let plan = plan_repo_uuid(&out);

    let got =
        mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"delete\"", "get", "--select", "*"]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    let ops = ops.as_array().unwrap();
    assert_eq!(ops.len(), 1, "one delete op: {}", got.stdout);
    // plan_side names the surviving side (repo b, where xb lives) in canonical terms.
    let expect_side = if a < b { "b" } else { "a" };
    let side =
        ops[0]["fields"].as_array().unwrap().iter().find(|f| f["name"] == "plan_side").unwrap();
    assert_eq!(side["value"]["value"], expect_side);
    assert!(op_endpoints(&ops[0]).contains(&xb), "references the survivor");
}

#[test]
fn test_sync_plan_conflict_query_scoped_rule() {
    // A [[conflict]] rule scoped by a query (matching one endpoint) resolves the
    // conflict without --on-conflict.
    let (a, _adir) = tracked_repo("cq_a", &[("doc.txt", b"same")]);
    let (b, _bdir) = tracked_repo("cq_b", &[("doc.txt", b"same")]);
    let xa = query_one(&a, "mfr_path:value = \"doc.txt\"");
    let xb = query_one(&b, "mfr_path:value = \"doc.txt\"");
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &xa, "field", "add", "tag:string=jazz"]));
    assert_ok(&mf(&["-u", &b, "metarecord", "-i", &xb, "field", "add", "tag:string=rock"]));

    // Rule: for records matching `tag = "jazz"` (A's side), prefer repo A.
    let content = format!(
        "[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n\n[[conflict]]\nquery = 'tag = \"jazz\"'\npolicy = 'prefer:{a}'\n"
    );
    let intents = write_intents("cq", &content);
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    let plan = plan_repo_uuid(&out);

    let got =
        mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"conflict\"", "get", "--select", "*"]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    let ops = ops.as_array().unwrap();
    assert_eq!(ops.len(), 1, "one conflict op: {}", got.stdout);
    // Resolved to repo_a's canonical side (which holds "jazz").
    let expect_side = if a < b { "a" } else { "b" };
    let f = |name: &str| {
        ops[0]["fields"].as_array().unwrap().iter().find(|x| x["name"] == name).cloned()
    };
    assert_eq!(f("plan_resolve").unwrap()["value"]["value"], expect_side);
}

#[test]
fn test_sync_run_creates_file_in_target() {
    // plan then run: a file present only in A is materialised in B (record + bytes).
    let (a, _adir) = tracked_repo("run_a", &[("hello.txt", b"world")]);
    let (b, _bdir) = tracked_repo("run_b", &[]);
    let intents = write_intents(
        "run",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));

    let out = mf(&["sync", "run", &a, &b, "--yes"]);
    assert_ok(&out);

    // B now has a metarecord at hello.txt …
    let rec_b = query_one(&b, "mfr_path:value = \"hello.txt\"");
    assert!(is_hex_uuid(&rec_b), "B has a record at hello.txt: {rec_b}");
    // … and the file on disk with the right content.
    let root_b = repo_root_of(&b);
    let content = std::fs::read(root_b.join("hello.txt")).expect("file exists in B");
    assert_eq!(content, b"world", "content transferred");

    // The link is now in sync, and a second run does nothing.
    let status = mf(&["sync", "status", &a, &b]);
    assert!(status.stdout.contains("in_sync"), "link in sync: {}", status.stdout);
    let again = mf(&["sync", "run", &a, &b, "--yes"]);
    assert_ok(&again);
    assert!(again.stdout.contains("nothing to run"), "second run is a no-op: {}", again.stdout);
}

/// The filesystem root of a loaded repo (from `mf repo list`).
pub(super) fn repo_root_of(repo: &str) -> PathBuf {
    let list = mf(&["repo", "list", "--all"]);
    assert_ok(&list);
    let repos: serde_json::Value = serde_json::from_str(&list.stdout).unwrap();
    let root = repos
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["repo_uuid"] == repo)
        .and_then(|r| r["root"].as_str())
        .expect("repo root");
    PathBuf::from(root)
}

#[test]
fn test_sync_run_propagates_deletion() {
    // A linked record deleted in A → run trashes B's file and removes B's record
    // and the link. Nothing is destroyed (the file lands in B's trash).
    let (a, _adir) = tracked_repo("rundel_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("rundel_b", &[("doc.txt", b"x")]);
    let xa = query_one(&a, "mfr_path:value = \"doc.txt\"");
    let xb = query_one(&b, "mfr_path:value = \"doc.txt\"");
    assert_ok(&mf(&["sync", "link", &a, &b, &xa, &xb]));
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &xa, "delete"]));

    let intents = write_intents(
        "rundel",
        &format!("[[intents]]\nrepo = '{b}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
    assert_ok(&mf(&["sync", "run", &a, &b, "--yes"]));

    // B's record is gone …
    let got = mf(&["-u", &b, "metarecord", "-i", &xb, "get"]);
    assert_eq!(got.code, 1, "B's record deleted: {}{}", got.stdout, got.stderr);
    // … the file is off disk but in the trash (not destroyed) …
    let root_b = repo_root_of(&b);
    assert!(!root_b.join("doc.txt").exists(), "file removed from B");
    let trash = mf(&["-u", &b, "trash", "list"]);
    assert_ok(&trash);
    assert!(trash.stdout.contains("sync"), "file is in the trash (reason sync): {}", trash.stdout);
    // … and the link is gone.
    let status = mf(&["sync", "status", &a, &b]);
    assert!(status.stderr.contains("no links") || status.stdout.trim().is_empty(), "link removed");
}

#[test]
fn test_sync_run_resync_propagates_field() {
    // First sync links the pair and commits a snapshot; then a field added on A
    // propagates to B on the next plan+run (re-sync direction).
    let (a, _adir) = tracked_repo("rerun_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("rerun_b", &[("doc.txt", b"x")]);
    let intents = write_intents(
        "rerun",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
    assert_ok(&mf(&["sync", "run", &a, &b, "--yes"]));

    // Add a field on A, re-plan, re-run.
    let xa = query_one(&a, "mfr_path:value = \"doc.txt\"");
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &xa, "field", "add", "tag:string=jazz"]));
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
    assert_ok(&mf(&["sync", "run", &a, &b, "--yes"]));

    // B's record now carries tag=jazz.
    let xb = query_one(&b, "mfr_path:value = \"doc.txt\"");
    let got = mf(&["-u", &b, "metarecord", "-i", &xb, "get", "--select", "*"]);
    assert_ok(&got);
    let m: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    let tag = m[0]["fields"].as_array().unwrap().iter().find(|f| f["name"] == "tag");
    assert_eq!(
        tag.and_then(|f| f["value"]["value"].as_str()),
        Some("jazz"),
        "tag propagated: {}",
        got.stdout
    );
}

/// A tag of several parents syncs as the same node, by its path, and the side
/// holding it under fewer parents gains the others (doc "Hierarchy
/// references"): a path the target lacks is a parent to add, not another
/// position the record would be forced out of.
#[test]
fn test_sync_a_hierarchy_adds_the_missing_parent() {
    let (a, _adir) = tracked_repo("hier_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("hier_b", &[("doc.txt", b"x")]);
    let tag = |repo: &str, specs: &[String]| {
        let mut all = vec!["mf_schema:string=tag".to_string()];
        all.extend_from_slice(specs);
        let refs: Vec<&str> = all.iter().map(String::as_str).collect();
        create_metarecord(repo, &refs)
    };
    let sport_a = tag(&a, &["path:hierarchy_ref=/sport".into()]);
    let courir_a = tag(&a, &[format!("path:hierarchy_ref={sport_a}/courir")]);
    let event_a = tag(&a, &["path:hierarchy_ref=/évènement".into()]);
    tag(
        &a,
        &[
            format!("path:hierarchy_ref={courir_a}/maraton"),
            format!("path:hierarchy_ref={event_a}/maraton"),
        ],
    );
    let sport_b = tag(&b, &["path:hierarchy_ref=/sport".into()]);
    let courir_b = tag(&b, &[format!("path:hierarchy_ref={sport_b}/courir")]);
    let maraton_b = tag(&b, &[format!("path:hierarchy_ref={courir_b}/maraton")]);

    let intents = write_intents(
        "hier",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mf_schema = \"tag\"'\n"),
    );
    // A first sync finds two different sets of parents: a conflict, settled
    // here for A's.
    let prefer = format!("prefer:{a}");
    let intents = intents.to_str().unwrap();
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents, "--on-conflict", &prefer]));
    let run = mf(&["sync", "run", &a, &b, "--yes"]);
    assert_ok(&run);
    assert!(run.stdout.contains("skipped: 0"), "{}", run.stdout);

    // B's marathon is the same node, now under both parents.
    let out =
        mf(&["-u", &b, "metarecord", "-i", &maraton_b, "get", "--resolve-tree", "path", "--tsv"]);
    assert_ok(&out);
    let mut paths: Vec<&str> = out.stdout.lines().map(|l| l.rsplit('\t').next().unwrap()).collect();
    paths.sort();
    assert_eq!(paths, vec!["sport/courir/maraton", "évènement/maraton"], "{}", out.stdout);
    let out = mf(&["-u", &b, "metarecord", "-q", r#"path:value = "maraton""#, "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.lines().count(), 1, "one marathon in B: {}", out.stdout);
}

/// A record's first value for `field` (as a string), or None.
pub(super) fn field_value_of(repo: &str, uuid: &str, field: &str) -> Option<String> {
    let got = mf(&["-u", repo, "metarecord", "-i", uuid, "get", "--select", "*"]);
    assert_ok(&got);
    let m: serde_json::Value = serde_json::from_str(&got.stdout).ok()?;
    m[0]["fields"]
        .as_array()?
        .iter()
        .find(|f| f["name"] == field)
        .and_then(|f| f["value"]["value"].as_str())
        .map(String::from)
}

#[test]
fn test_sync_run_applies_conflict_resolution() {
    // After a first sync, both sides change the same field → conflict resolved by
    // --on-conflict prefer:<repo_a>, so repo_a's value wins on both sides at run.
    let (a, _adir) = tracked_repo("cfr_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("cfr_b", &[("doc.txt", b"x")]);
    let intents = write_intents(
        "cfr",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
    assert_ok(&mf(&["sync", "run", &a, &b, "--yes"]));

    let xa = query_one(&a, "mfr_path:value = \"doc.txt\"");
    let xb = query_one(&b, "mfr_path:value = \"doc.txt\"");
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &xa, "field", "add", "tag:string=jazz"]));
    assert_ok(&mf(&["-u", &b, "metarecord", "-i", &xb, "field", "add", "tag:string=rock"]));

    assert_ok(&mf(&[
        "sync",
        "plan",
        &a,
        &b,
        "--intents",
        intents.to_str().unwrap(),
        "--on-conflict",
        &format!("prefer:{a}"),
    ]));
    assert_ok(&mf(&["sync", "run", &a, &b, "--yes"]));

    // repo_a's value (jazz) wins on both sides.
    assert_eq!(field_value_of(&a, &xa, "tag").as_deref(), Some("jazz"), "A keeps jazz");
    assert_eq!(field_value_of(&b, &xb, "tag").as_deref(), Some("jazz"), "B took jazz");
}

#[test]
fn test_sync_run_external_divergence_reported() {
    // A file to create where the target subtree is external → metafolder writes no
    // file; the metadata still syncs and the divergence is reported.
    let (a, _adir) = tracked_repo("ext_a", &[("doc.txt", b"aaa")]);
    let (b, _bdir) = tracked_repo("ext_b", &[]);
    let b_root = mf(&["-u", &b, "metarecord", "get"]).stdout.trim().to_string();
    assert_ok(&mf(&[
        "-u",
        &b,
        "metarecord",
        "-i",
        &b_root,
        "field",
        "set",
        "mf_sync:string=external",
        "--force",
    ]));

    let intents = write_intents(
        "ext",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
    let out = mf(&["sync", "run", &a, &b, "--yes"]);
    assert_ok(&out);

    // The divergence is reported (aggregated), and no file was written in B …
    assert!(out.stderr.contains("external divergences"), "reported: {}", out.stderr);
    assert!(!repo_root_of(&b).join("doc.txt").exists(), "external file not created by metafolder");
    // … but the metadata record was placed (metadata syncs normally).
    let rec_b = query_one(&b, "mfr_path:value = \"doc.txt\"");
    assert!(is_hex_uuid(&rec_b), "record placed in B: {rec_b}");
}

#[test]
fn test_sync_closes_over_references_transitively() {
    // X (in scope) refs person, who refs org; neither has a position nor is in
    // scope. The closure links person, so person is synced: its own ref has to
    // reach a counterpart too (doc "Matching records across repositories").
    let (a, _adir) = tracked_repo("ttrans_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("ttrans_b", &[]);
    let xa = query_one(&a, "mfr_path:value = \"doc.txt\"");
    let org_a = create_metarecord(&a, &["name:string=acme"]);
    let person_a = create_metarecord(&a, &["name:string=alice", &format!("employer:ref={org_a}")]);
    assert_ok(&mf(&[
        "-u",
        &a,
        "metarecord",
        "-i",
        &xa,
        "field",
        "add",
        &format!("author:ref={person_a}"),
    ]));

    let intents = write_intents(
        "ttrans",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
    let run = mf(&["sync", "run", &a, &b, "--yes"]);
    assert_ok(&run);
    assert!(!run.stderr.contains("no counterpart"), "nothing left out: {}", run.stderr);

    let org_b = query_one(&b, r#"name = "acme""#);
    let person_b = query_one(&b, r#"name = "alice""#);
    let employer = field_value_of(&b, &person_b, "employer");
    assert_eq!(employer.as_deref(), Some(org_b.as_str()), "the second ref is translated");
}

#[test]
fn test_sync_run_translates_ref() {
    // A file X refs an abstract record; both are materialised in B and X_B's ref
    // is translated to person_B (the linked counterpart), not left dangling.
    let (a, _adir) = tracked_repo("tref_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("tref_b", &[]);
    let xa = query_one(&a, "mfr_path:value = \"doc.txt\"");
    let person_a = create_metarecord(&a, &["name:string=alice"]);
    assert_ok(&mf(&[
        "-u",
        &a,
        "metarecord",
        "-i",
        &xa,
        "field",
        "add",
        &format!("author:ref={person_a}"),
    ]));

    let intents = write_intents(
        "tref",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
    assert_ok(&mf(&["sync", "run", &a, &b, "--yes"]));

    // B has person (name=alice) and X_B whose author ref points to it.
    let person_b = query_one(&b, r#"name = "alice""#);
    let xb = query_one(&b, "mfr_path:value = \"doc.txt\"");
    let author = field_value_of(&b, &xb, "author");
    assert_eq!(author.as_deref(), Some(person_b.as_str()), "author ref translated to person_b");
    assert_ne!(person_b, person_a, "distinct local uuids");
}
