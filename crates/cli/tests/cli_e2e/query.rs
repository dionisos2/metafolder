use super::*;

// ── Query ─────────────────────────────────────────────────────────────────────

#[test]
fn test_query_prints_matching_uuids() {
    let (repo, _root) = init_repo("query");
    let high = create_metarecord(&repo, &["rating:int=5"]);
    let _low = create_metarecord(&repo, &["rating:int=1"]);
    let out = mf(&["-u", &repo, "metarecord", "-q", "rating > 3", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), high);
}

// A bare UUID atom in the DSL (doc "Query DSL grammar"): the whole point is
// that a UUID copied out of the GUI runs as a query and composes with the rest.
#[test]
fn test_query_bare_uuid_atom_selects_that_metarecord() {
    let (repo, _root) = init_repo("query_uuid_atom");
    let a = create_metarecord(&repo, &["rating:int=5"]);
    let b = create_metarecord(&repo, &["rating:int=1"]);

    let out = mf(&["-u", &repo, "metarecord", "-q", &a, "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), a);

    // Two pasted UUIDs fold into one set membership.
    let out = mf(&["-u", &repo, "metarecord", "-q", &format!("{a} OR {b}"), "get"]);
    assert_ok(&out);
    let mut got: Vec<&str> = out.stdout.lines().collect();
    got.sort_unstable();
    let mut want = vec![a.as_str(), b.as_str()];
    want.sort_unstable();
    assert_eq!(got, want);

    // ... and compose with an ordinary predicate.
    let out = mf(&["-u", &repo, "metarecord", "-q", &format!("{a} AND rating > 3"), "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), a);
    let out = mf(&["-u", &repo, "metarecord", "-q", &format!("{b} AND rating > 3"), "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "");

    // The shipped simplified grammar passes a pasted UUID through, so -s works
    // too (mf_cfg installs the shipped core config).
    let out = mf_cfg(&["-u", &repo, "metarecord", "-q", &a, "-s", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), a);
    let out = mf_cfg(&["-u", &repo, "metarecord", "-q", &format!("{a} rating>3"), "-s", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), a);

    // The typed flag decides the output shape (doc "Editing metarecords from the command line"):
    // the same UUID through -i is the full JSON object.
    let out = mf(&["-u", &repo, "metarecord", "-i", &a, "get"]);
    assert_ok(&out);
    let json: serde_json::Value =
        serde_json::from_str(&out.stdout).expect("-i get should print JSON");
    assert_eq!(json[0]["uuid"], serde_json::Value::String(a.clone()));
}

#[test]
fn test_query_simplified_expands_before_running() {
    let (repo, _root) = init_repo("query_simplified");
    let high = create_metarecord(&repo, &["rating:int=5"]);
    let _low = create_metarecord(&repo, &["rating:int=1"]);
    // `rating=5` expands to `rating = 5` locally via the core grammar.
    let out = mf_cfg(&["-u", &repo, "metarecord", "-q", "rating=5", "-s", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), high);
}

#[test]
fn test_query_simplified_date_macro_filters() {
    let (repo, _root) = init_repo("query_date_macro");
    // mfr_btime is reserved, so set it with --force. The datetime field spec
    // parses the ISO string to Unix ms.
    let recent =
        mf(&["-u", &repo, "metarecord", "add", "mfr_btime:datetime=2024-06-01", "--force"]);
    assert_ok(&recent);
    let recent = recent.stdout.trim().to_string();
    let old = mf(&["-u", &repo, "metarecord", "add", "mfr_btime:datetime=2020-01-01", "--force"]);
    assert_ok(&old);
    // `created since "2023-01-01"` → mfr_btime >= @"2023-01-01": only the recent one.
    let out =
        mf_cfg(&["-u", &repo, "metarecord", "-q", "created since \"2023-01-01\"", "-s", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), recent);
}

#[test]
fn test_query_select_star_prints_objects() {
    let (repo, _root) = init_repo("query_star");
    create_metarecord(&repo, &["rating:int=5", "genre:string=jazz"]);
    let out = mf(&["-u", &repo, "metarecord", "-q", "rating = 5", "get", "--select", "*"]);
    assert_ok(&out);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let list = parsed.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["fields"].as_array().unwrap().len(), 2);
}

#[test]
fn test_query_select_field_list_restricts_fields() {
    let (repo, _root) = init_repo("query_select");
    create_metarecord(&repo, &["rating:int=5", "genre:string=jazz"]);
    let out = mf(&["-u", &repo, "metarecord", "-q", "rating = 5", "get", "--select", "genre"]);
    assert_ok(&out);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let fields = parsed[0]["fields"].as_array().unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0]["name"], "genre");
}

#[test]
fn test_query_sort_and_limit() {
    let (repo, _root) = init_repo("query_sort");
    let r1 = create_metarecord(&repo, &["rating:int=1", "kind:string=s"]);
    let r3 = create_metarecord(&repo, &["rating:int=3", "kind:string=s"]);
    let r2 = create_metarecord(&repo, &["rating:int=2", "kind:string=s"]);
    let out =
        mf(&["-u", &repo, "metarecord", "-q", r#"kind = "s""#, "get", "--sort", "rating:desc"]);
    assert_ok(&out);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines, vec![r3.as_str(), r2.as_str(), r1.as_str()]);

    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        r#"kind = "s""#,
        "get",
        "--sort",
        "rating:asc",
        "--limit",
        "2",
    ]);
    assert_ok(&out);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines, vec![r1.as_str(), r2.as_str()]);
}

#[test]
fn test_query_bad_dsl_is_usage_error() {
    let (repo, _root) = init_repo("query_bad");
    let out = mf(&["-u", &repo, "metarecord", "-q", "a = 1 and b = 2", "get"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
    assert!(out.stderr.starts_with("error:"));
}

#[test]
fn test_query_bad_sort_is_usage_error() {
    let (repo, _root) = init_repo("query_bad_sort");
    let out = mf(&["-u", &repo, "metarecord", "-q", "a = 1", "get", "--sort", "rating:sideways"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

// ── Query --values ────────────────────────────────────────────────────────────

#[test]
fn test_query_values_prints_raw_scalars() {
    let (repo, _root) = init_repo("values");
    create_metarecord(&repo, &["mf_schema:string=tag", "name:string=jazz"]);
    create_metarecord(&repo, &["mf_schema:string=tag", "name:string=rock", "weight:int=3"]);

    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        "mf_schema = \"tag\"",
        "get",
        "--select",
        "name",
        "--values",
    ]);
    assert_ok(&out);
    let mut names: Vec<&str> = out.stdout.lines().collect();
    names.sort_unstable();
    assert_eq!(names, vec!["jazz", "rock"]);

    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        "name = \"rock\"",
        "get",
        "--select",
        "weight",
        "--values",
    ]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "3");
}

#[test]
fn test_query_values_requires_a_single_selected_field() {
    let (repo, _root) = init_repo("values_usage");
    let out = mf(&["-u", &repo, "metarecord", "-q", "name = \"x\"", "get", "--values"]);
    assert_eq!(out.code, 2, "stdout: {}", out.stdout);
    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        "name = \"x\"",
        "get",
        "--select",
        "a,b",
        "--values",
    ]);
    assert_eq!(out.code, 2, "stdout: {}", out.stdout);
}
