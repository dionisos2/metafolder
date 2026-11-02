use super::*;

// ── Entry manipulation ────────────────────────────────────────────────────────

#[test]
fn test_create_and_get_by_uuid() {
    let (repo, _root) = init_repo("create");
    let uuid = create_metarecord(&repo, &["rating:int=5", "genre:string=jazz"]);
    let entries = get_entries(&repo, &uuid);
    let list = entries.as_array().expect("a JSON array");
    assert_eq!(list.len(), 1);
    let entry = &list[0];
    assert_eq!(entry["uuid"], serde_json::json!(uuid));
    let fields = entry["fields"].as_array().unwrap();
    assert_eq!(fields.len(), 2);
    let rating = fields.iter().find(|f| f["name"] == "rating").expect("rating field");
    assert_eq!(rating["value"]["type"], "int");
    assert_eq!(rating["value"]["value"], 5);
    assert!(rating["id"].is_i64(), "mf get must include field ids");
}

#[test]
fn test_retype_converts_field_type() {
    let (repo, _root) = init_repo("retype");
    let uuid = create_metarecord(&repo, &["rating:int=5"]);

    let out = mf(&["-u", &repo, "retype", "rating", "string"]);
    assert_ok(&out);

    // The value now reads back as a string.
    let entries = get_entries(&repo, &uuid);
    let entry = &entries.as_array().unwrap()[0];
    let rating = entry["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "rating")
        .expect("rating field");
    assert_eq!(rating["value"]["type"], "string");
    assert_eq!(rating["value"]["value"], "5");

    // A conflicting Int write to the now-String field is rejected (exit != 0).
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=9"]);
    assert_ne!(out.code, 0, "a conflicting-type write must fail: {}", out.stderr);
}

#[test]
fn test_field_list_enumerates_names_and_types() {
    let (repo, _root) = init_repo("field_list");
    create_metarecord(&repo, &["rating:int=5", "genre:string=jazz"]);
    create_metarecord(&repo, &["rating:int=3"]);

    // Unfiltered: one "name\ttype" line per distinct field name, deduplicated.
    let out = mf(&["-u", &repo, "field", "list"]);
    assert_ok(&out);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert!(lines.contains(&"rating\tint"), "got: {}", out.stdout);
    assert!(lines.contains(&"genre\tstring"), "got: {}", out.stdout);
    // The init-time root metarecord contributes these.
    assert!(lines.contains(&"mfr_path\ttree_ref"), "got: {}", out.stdout);
    // `rating` appears once despite two metarecords carrying it.
    assert_eq!(lines.iter().filter(|l| l.starts_with("rating\t")).count(), 1);

    // `list` is the group's default: bare `mf field` lists too.
    let bare = mf(&["-u", &repo, "field"]);
    assert_ok(&bare);
    assert_eq!(bare.stdout, out.stdout, "bare `field` must equal `field list`");

    // Filtered by type.
    let out = mf(&["-u", &repo, "field", "list", "--type", "tree_ref"]);
    assert_ok(&out);
    assert!(out.stdout.lines().all(|l| l.ends_with("\ttree_ref")), "got: {}", out.stdout);
    assert!(out.stdout.lines().any(|l| l == "mfr_path\ttree_ref"), "got: {}", out.stdout);
    assert!(!out.stdout.contains("rating"), "type filter must exclude int fields: {}", out.stdout);
}

#[test]
fn test_create_reserved_field_requires_force() {
    let (repo, _root) = init_repo("create_force");
    let out = mf(&["-u", &repo, "metarecord", "add", "mfr_path:tree_ref=/created_name"]);
    assert_eq!(out.code, 1, "creating with mfr_* without --force must fail");
    assert!(out.stderr.starts_with("error:"), "stderr: {}", out.stderr);

    let out = mf(&["-u", &repo, "metarecord", "add", "mfr_path:tree_ref=/created_name", "--force"]);
    assert_ok(&out);
    let uuid = out.stdout.trim().to_string();
    assert!(is_hex_uuid(&uuid));
    let entries = get_entries(&repo, &uuid);
    assert_eq!(entries[0]["fields"][0]["name"], "mfr_path");
}

#[test]
fn test_get_with_fields_filter() {
    let (repo, _root) = init_repo("get_fields");
    let uuid = create_metarecord(&repo, &["rating:int=5", "genre:string=jazz"]);
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "get", "--select", "genre"]);
    assert_ok(&out);
    let entries: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let fields = entries[0]["fields"].as_array().unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0]["name"], "genre");
}

#[test]
fn test_get_with_predicate() {
    let (repo, _root) = init_repo("get_pred");
    let jazz = create_metarecord(&repo, &["genre:string=jazz"]);
    let _rock = create_metarecord(&repo, &["genre:string=rock"]);
    let entries = get_entries(&repo, r#"genre = "jazz""#);
    let list = entries.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["uuid"], serde_json::json!(jazz));
}

#[test]
fn test_get_with_a_uuid_in_list() {
    // The DSL list form reaches the daemon as the `uuid_in` IR node: the shell
    // spelling of a multi-selection (doc "Query DSL grammar").
    let (repo, _root) = init_repo("get_uuid_in");
    let a = create_metarecord(&repo, &["x:int=1"]);
    let b = create_metarecord(&repo, &["x:int=2"]);
    let _c = create_metarecord(&repo, &["x:int=3"]);

    let out = mf(&["-u", &repo, "metarecord", "-q", &format!("uuid_in({a}, {b})"), "get"]);
    assert_ok(&out);
    let mut lines: Vec<&str> = out.stdout.lines().collect();
    lines.sort_unstable();
    let mut want = vec![a.as_str(), b.as_str()];
    want.sort_unstable();
    assert_eq!(lines, want, "stdout: {}", out.stdout);

    // An empty list is the empty set, not an error.
    let out = mf(&["-u", &repo, "metarecord", "-q", "uuid_in()", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "", "uuid_in() should select nothing");
}

#[test]
fn test_get_predicate_with_limit_and_sort() {
    let (repo, _root) = init_repo("get_limit_sort");
    create_metarecord(&repo, &["rating:int=1"]);
    create_metarecord(&repo, &["rating:int=2"]);
    create_metarecord(&repo, &["rating:int=3"]);

    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        "rating >= 1",
        "get",
        "--select",
        "*",
        "--sort",
        "rating:desc",
        "--limit",
        "2",
    ]);
    assert_ok(&out);
    let list: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let arr = list.as_array().unwrap();
    assert_eq!(arr.len(), 2, "--limit must cap the result at 2");

    let rating = |m: &serde_json::Value| -> i64 {
        m["fields"].as_array().unwrap().iter().find(|f| f["name"] == "rating").unwrap()["value"]
            ["value"]
            .as_i64()
            .unwrap()
    };
    // --sort rating:desc → the two highest, in order.
    assert_eq!(rating(&arr[0]), 3);
    assert_eq!(rating(&arr[1]), 2);
}

#[test]
fn test_get_count_prints_the_total() {
    let (repo, _root) = init_repo("get_count");
    create_metarecord(&repo, &["rating:int=1"]);
    create_metarecord(&repo, &["rating:int=2"]);
    create_metarecord(&repo, &["rating:int=3"]);

    // A query scope: the number of matches on one line, and no uuid.
    let out = mf(&["-u", &repo, "metarecord", "-q", "rating > 1", "get", "--count"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "2", "stdout: {}", out.stdout);

    // No selector is the whole repository, like the listing it replaces — the
    // answer must be that listing's length (root metarecord included).
    let all = mf(&["-u", &repo, "metarecord", "get"]);
    assert_ok(&all);
    let out = mf(&["-u", &repo, "metarecord", "get", "--count"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), all.stdout.lines().count().to_string());

    // An empty set counts zero; it is not an error and prints no blank uuid.
    let out = mf(&["-u", &repo, "metarecord", "-q", "rating > 99", "get", "--count"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "0", "stdout: {}", out.stdout);
}

#[test]
fn test_get_count_ignores_the_page_size() {
    // The point of `--count` is that it costs one round-trip whatever the scope:
    // a page smaller than the answer must not truncate it (nor page for it).
    let (repo, _root) = init_repo("count_page");
    for i in 0..5 {
        create_metarecord(&repo, &[&format!("rating:int={i}")]);
    }
    let cfg = temp_dir("count_cfg");
    let cli = cfg.path().join("metafolder").join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    std::fs::write(
        cli.join("config.toml"),
        SHIPPED_CLI_CONFIG.replace("page-size = 500", "page-size = 2"),
    )
    .unwrap();

    let out = mf_full(
        &["-u", &repo, "metarecord", "-q", "rating >= 0", "get", "--count"],
        None,
        &[("XDG_CONFIG_HOME", cfg.path().to_str().unwrap())],
        true,
    );
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "5", "stdout: {}", out.stdout);
}

#[test]
fn test_get_count_rejects_a_uuid_selector_and_the_output_flags() {
    let (repo, _root) = init_repo("count_usage");
    let uuid = create_metarecord(&repo, &["x:int=1"]);

    // `-i` addresses one metarecord: counting it answers its own question.
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "get", "--count"]);
    assert_eq!(out.code, 2, "stdout: {}\nstderr: {}", out.stdout, out.stderr);

    // A count has no rows, so it shapes none.
    for flag in [vec!["--select", "x"], vec!["--limit", "2"], vec!["--resolve-tree", "mfr_path"]] {
        let mut args = vec!["-u", &repo, "metarecord", "get", "--count"];
        args.extend(flag.iter().copied());
        let out = mf(&args);
        assert_eq!(out.code, 2, "{flag:?} should conflict with --count: {}", out.stderr);
    }
}

/// The `next-cursor <token>` line `--limit` leaves on stderr, if any.
fn next_cursor(out: &Out) -> Option<String> {
    out.stderr.lines().find_map(|l| l.strip_prefix("next-cursor ")).map(str::to_string)
}

#[test]
fn test_get_reads_a_scope_by_batches_with_cursor() {
    let (repo, _root) = init_repo("get_cursor");
    for i in 0..5 {
        create_metarecord(&repo, &[&format!("rating:int={i}")]);
    }
    // A page size smaller than the batch: the cursor printed must be the one
    // after the last *printed* record, not after an internal page.
    let cfg = temp_dir("cursor_cfg");
    let cli = cfg.path().join("metafolder").join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    std::fs::write(
        cli.join("config.toml"),
        SHIPPED_CLI_CONFIG.replace("page-size = 500", "page-size = 2"),
    )
    .unwrap();
    let xdg = cfg.path().to_str().unwrap();
    let get = |extra: &[&str]| {
        let mut args = vec!["-u", &repo, "metarecord", "-q", "rating >= 0", "get"];
        args.extend_from_slice(&["--sort", "rating"]);
        args.extend_from_slice(extra);
        mf_full(&args, None, &[("XDG_CONFIG_HOME", xdg)], true)
    };
    let all = get(&[]);
    assert_ok(&all);
    assert_eq!(next_cursor(&all), None, "a full read has nothing to resume");
    let all: Vec<&str> = all.stdout.lines().collect();
    assert_eq!(all.len(), 5);

    // First batch: three uuids on stdout, the resume token on stderr only.
    let first = get(&["--limit", "3"]);
    assert_ok(&first);
    assert_eq!(first.stdout.lines().collect::<Vec<_>>(), all[..3]);
    let cursor = next_cursor(&first).expect("a truncated read prints its next cursor");

    // The rest, from the cursor: exactly what was left, and no further cursor.
    let rest = get(&["--cursor", &cursor]);
    assert_ok(&rest);
    assert_eq!(rest.stdout.lines().collect::<Vec<_>>(), all[3..]);
    assert_eq!(next_cursor(&rest), None, "stderr: {}", rest.stderr);

    // A limit that ends exactly on the last match has nothing to resume either.
    let exact = get(&["--cursor", &cursor, "--limit", "2"]);
    assert_ok(&exact);
    assert_eq!(exact.stdout.lines().collect::<Vec<_>>(), all[3..]);
    assert_eq!(next_cursor(&exact), None, "stderr: {}", exact.stderr);

    // A cursor is bound to its query and sort: another one is refused.
    let mut args = vec!["-u", &repo, "metarecord", "-q", "rating >= 1", "get"];
    args.extend_from_slice(&["--sort", "rating", "--cursor", &cursor]);
    let other = mf_full(&args, None, &[("XDG_CONFIG_HOME", xdg)], true);
    assert_eq!(other.code, 1, "stdout: {}\nstderr: {}", other.stdout, other.stderr);
    assert!(other.stderr.contains("cursor"), "stderr: {}", other.stderr);
}

#[test]
fn test_get_cursor_also_resumes_the_whole_repository_and_select() {
    let (repo, _root) = init_repo("get_cursor_all");
    for i in 0..3 {
        create_metarecord(&repo, &[&format!("rating:int={i}")]);
    }
    // No selector: the whole repository (root metarecord included) by batches.
    let all = mf(&["-u", &repo, "metarecord", "get"]);
    assert_ok(&all);
    let first = mf(&["-u", &repo, "metarecord", "get", "--limit", "1"]);
    assert_ok(&first);
    let cursor = next_cursor(&first).unwrap();
    let rest = mf(&["-u", &repo, "metarecord", "get", "--cursor", &cursor]);
    assert_ok(&rest);
    assert_eq!(format!("{}{}", first.stdout, rest.stdout), all.stdout);

    // `--select` output (one JSON array per batch) resumes the same way.
    let first = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        "rating >= 0",
        "get",
        "--select",
        "rating",
        "--sort",
        "rating",
        "--limit",
        "2",
    ]);
    assert_ok(&first);
    let cursor = next_cursor(&first).unwrap();
    let rest = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        "rating >= 0",
        "get",
        "--select",
        "rating",
        "--sort",
        "rating",
        "--cursor",
        &cursor,
    ]);
    assert_ok(&rest);
    let rest: serde_json::Value = serde_json::from_str(&rest.stdout).unwrap();
    assert_eq!(rest.as_array().unwrap().len(), 1);
}

#[test]
fn test_get_cursor_usage_errors() {
    let (repo, _root) = init_repo("cursor_usage");
    let uuid = create_metarecord(&repo, &["x:int=1"]);
    // One metarecord has no batches.
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "get", "--cursor", "abc"]);
    assert_eq!(out.code, 2, "stdout: {}\nstderr: {}", out.stdout, out.stderr);
    // Neither has a count nor a tree resolution.
    for flag in [vec!["--count"], vec!["--resolve-tree", "mfr_path"]] {
        let mut args = vec!["-u", &repo, "metarecord", "-q", "x = 1", "get", "--cursor", "abc"];
        args.extend(flag.iter().copied());
        let out = mf(&args);
        assert_eq!(out.code, 2, "{flag:?} should conflict with --cursor: {}", out.stderr);
    }
}

#[test]
fn test_list_prints_uuids_one_per_line() {
    let (repo, _root) = init_repo("list");
    let a = create_metarecord(&repo, &["x:int=1"]);
    let b = create_metarecord(&repo, &["x:int=2"]);
    let out = mf(&["-u", &repo, "metarecord", "get"]);
    assert_ok(&out);
    let lines: Vec<&str> = out.stdout.lines().collect();
    // Root entry + the two created entries.
    assert_eq!(lines.len(), 3, "stdout: {}", out.stdout);
    assert!(lines.iter().all(|l| is_hex_uuid(l)));
    assert!(lines.contains(&a.as_str()) && lines.contains(&b.as_str()));

    let out = mf(&["-u", &repo, "metarecord", "get", "--limit", "2"]);
    assert_ok(&out);
    assert_eq!(out.stdout.lines().count(), 2);
}

#[test]
fn test_set_uuid_replaces_all_rows() {
    let (repo, _root) = init_repo("set");
    let uuid = create_metarecord(&repo, &["tag:string=a", "tag:string=b"]);
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "tag:string=c"]);
    assert_ok(&out);
    let entries = get_entries(&repo, &uuid);
    let fields = entries[0]["fields"].as_array().unwrap();
    let tags: Vec<&serde_json::Value> = fields.iter().filter(|f| f["name"] == "tag").collect();
    assert_eq!(tags.len(), 1, "set_field must replace all rows of the name");
    assert_eq!(tags[0]["value"]["value"], "c");
}

#[test]
fn test_set_specs_must_share_one_name() {
    let (repo, _root) = init_repo("set_names");
    let uuid = create_metarecord(&repo, &["tag:string=a"]);
    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &uuid,
        "field",
        "set",
        "tag:string=c",
        "genre:string=d",
    ]);
    assert_eq!(out.code, 2, "{}", out.stderr);
    assert!(out.stderr.contains("must share the same name"), "{}", out.stderr);
}

#[test]
fn test_set_with_predicate_prints_updated_count() {
    let (repo, _root) = init_repo("set_pred");
    create_metarecord(&repo, &["genre:string=jazz"]);
    create_metarecord(&repo, &["genre:string=jazz"]);
    create_metarecord(&repo, &["genre:string=rock"]);
    let out =
        mf(&["-u", &repo, "metarecord", "-q", r#"genre = "jazz""#, "field", "set", "rating:int=4"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "2");
    let out = mf(&["-u", &repo, "metarecord", "-q", "rating = 4", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.lines().count(), 2);
}

#[test]
fn test_set_reserved_field_requires_force() {
    let (repo, _root) = init_repo("set_force");
    let uuid = create_metarecord(&repo, &["x:int=1"]);
    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &uuid,
        "field",
        "set",
        "mfr_path:tree_ref=/forced_name",
    ]);
    assert_eq!(out.code, 1, "writing mfr_* without --force must fail");
    assert!(out.stderr.starts_with("error:"));
    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &uuid,
        "field",
        "set",
        "mfr_path:tree_ref=/forced_name",
        "--force",
    ]);
    assert_ok(&out);
}

#[test]
fn test_add_appends_multimap_row() {
    let (repo, _root) = init_repo("add");
    let uuid = create_metarecord(&repo, &["genre:string=jazz"]);
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "add", "genre:string=blues"]);
    assert_ok(&out);
    let entries = get_entries(&repo, &uuid);
    let fields = entries[0]["fields"].as_array().unwrap();
    assert_eq!(fields.iter().filter(|f| f["name"] == "genre").count(), 2);
}

#[test]
fn test_add_with_predicate_appends_to_matches() {
    let (repo, _root) = init_repo("add_pred");
    create_metarecord(&repo, &["genre:string=jazz"]);
    create_metarecord(&repo, &["genre:string=jazz"]);
    create_metarecord(&repo, &["genre:string=rock"]);
    let out =
        mf(&["-u", &repo, "metarecord", "-q", r#"genre = "jazz""#, "field", "add", "tag:string=x"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "2");
    let out = mf(&["-u", &repo, "metarecord", "-q", r#"tag = "x""#, "get"]);
    assert_eq!(out.stdout.lines().count(), 2);
}

#[test]
fn test_remove_by_uuid_drops_only_matching_value_rows() {
    let (repo, _root) = init_repo("remove_uuid");
    let uuid = create_metarecord(&repo, &["tag:string=test", "tag:string=keep"]);
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "remove", "tag:string=test"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "1");
    let entries = get_entries(&repo, &uuid);
    let fields = entries[0]["fields"].as_array().unwrap();
    let tags: Vec<&serde_json::Value> = fields.iter().filter(|f| f["name"] == "tag").collect();
    assert_eq!(tags.len(), 1, "only the matching-value row is removed");
    assert_eq!(tags[0]["value"]["value"], "keep");
}

/// One vocabulary across the daemon, the CLI and the GUI: `set` / `add` /
/// `remove` / `unset` take a *value* out of a multi-map field, while `delete`
/// stays reserved for destroying an entity (`mf metarecord delete`, and the
/// row-addressed `mf field delete <id>`). The field-level verb used to be
/// `delete`, which collided with that meaning; it is gone rather than aliased.
#[test]
fn test_field_delete_verb_is_gone() {
    let (repo, _root) = init_repo("field_delete_gone");
    let uuid = create_metarecord(&repo, &["tag:string=test"]);

    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "delete", "tag:string=test"]);
    assert_eq!(out.code, 2, "unknown verb is a usage error.\nstderr: {}", out.stderr);

    // The row-addressed form keeps `delete`: it destroys a directly-named row,
    // it does not take a value out of a multi-map.
    let entries = get_entries(&repo, &uuid);
    let id = entries[0]["fields"][0]["id"].as_i64().unwrap();
    assert_ok(&mf(&["-u", &repo, "field", "delete", &id.to_string()]));
}

#[test]
fn test_remove_by_predicate_prints_changed_count() {
    let (repo, _root) = init_repo("remove_pred");
    create_metarecord(&repo, &["tag:string=test", "tag:string=keep"]);
    create_metarecord(&repo, &["tag:string=test"]);
    create_metarecord(&repo, &["tag:string=keep"]);
    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        "tag IS PRESENT",
        "field",
        "remove",
        "tag:string=test",
    ]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "2", "two metarecords carried tag=test");
    assert_eq!(
        mf(&["-u", &repo, "metarecord", "-q", r#"tag = "test""#, "get"]).stdout.lines().count(),
        0
    );
    assert_eq!(
        mf(&["-u", &repo, "metarecord", "-q", r#"tag = "keep""#, "get"]).stdout.lines().count(),
        2
    );
}

#[test]
fn test_unset_deletes_single_row_by_id() {
    let (repo, _root) = init_repo("unset");
    let uuid = create_metarecord(&repo, &["genre:string=jazz", "genre:string=blues"]);
    let entries = get_entries(&repo, &uuid);
    let fields = entries[0]["fields"].as_array().unwrap();
    let jazz_id = fields
        .iter()
        .find(|f| f["value"]["value"] == "jazz")
        .and_then(|f| f["id"].as_i64())
        .expect("jazz row id");
    let out = mf(&["-u", &repo, "field", "delete", &jazz_id.to_string()]);
    assert_ok(&out);
    let entries = get_entries(&repo, &uuid);
    let fields = entries[0]["fields"].as_array().unwrap();
    let genres: Vec<&serde_json::Value> = fields.iter().filter(|f| f["name"] == "genre").collect();
    assert_eq!(genres.len(), 1);
    assert_eq!(genres[0]["value"]["value"], "blues");
}

#[test]
fn test_delete_by_uuid_prints_count() {
    let (repo, _root) = init_repo("delete");
    let uuid = create_metarecord(&repo, &["x:int=1"]);
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "delete"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "1");
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "get"]);
    assert_eq!(out.code, 1);
}

#[test]
fn test_delete_predicate_asks_for_confirmation() {
    let (repo, _root) = init_repo("delete_confirm");
    create_metarecord(&repo, &["genre:string=del_me"]);
    create_metarecord(&repo, &["genre:string=del_me"]);

    // Refusing the confirmation aborts without deleting.
    let out = mf_full(
        &["-p", daemon_port(), "-u", &repo, "metarecord", "-q", r#"genre = "del_me""#, "delete"],
        Some("n\n"),
        &[],
        false,
    );
    assert_eq!(out.code, 1, "refused confirmation should exit 1");
    let out = mf(&["-u", &repo, "metarecord", "-q", r#"genre = "del_me""#, "get"]);
    assert_eq!(out.stdout.lines().count(), 2, "entries must survive a refused confirmation");

    // --force skips the prompt.
    let out = mf(&["-u", &repo, "metarecord", "-q", r#"genre = "del_me""#, "delete", "--force"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "2");
    let out = mf(&["-u", &repo, "metarecord", "-q", r#"genre = "del_me""#, "get"]);
    assert_eq!(out.stdout.trim(), "");
}

// ── Path resolution ───────────────────────────────────────────────────────────

#[test]
fn test_path_resolves_tracked_file() {
    let (repo, root) = init_repo("path");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/file.txt"), b"hello").unwrap();

    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "add",
        "--parents",
        "--path",
        root.join("sub/file.txt").to_str().unwrap(),
    ]);
    assert_ok(&out);
    let uuid = out.stdout.trim().to_string();

    let out = mf(&["-u", &repo, "path", &uuid]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), root.join("sub/file.txt").to_str().unwrap());

    let out = mf(&["-u", &repo, "path", "--relative", &uuid]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "/sub/file.txt");
}

#[test]
fn test_path_of_the_root_entry() {
    let (repo, root) = init_repo("path_root");
    let out = mf(&["-u", &repo, "metarecord", "get"]);
    assert_ok(&out);
    let root_uuid = out.stdout.trim().to_string();

    let out = mf(&["-u", &repo, "path", &root_uuid]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), root.to_str().unwrap());

    let out = mf(&["-u", &repo, "path", "--relative", &root_uuid]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "/");
}

#[test]
fn test_repo_root_prints_the_absolute_root() {
    let (repo, root) = init_repo("repo_root");
    let out = mf(&["-u", &repo, "repo", "root"]);
    assert_ok(&out);
    assert_eq!(out.stdout, format!("{}\n", root.to_str().unwrap()));
}

#[test]
fn test_repo_root_needs_a_repo() {
    let out = mf(&["repo", "root"]);
    assert_eq!(out.code, 2, "stdout: {}", out.stdout);
}

#[test]
fn test_path_fails_on_entry_without_mfr_path() {
    let (repo, _root) = init_repo("path_none");
    let uuid = create_metarecord(&repo, &["title:string=no path"]);
    let out = mf(&["-u", &repo, "path", &uuid]);
    assert_eq!(out.code, 1, "stdout: {}", out.stdout);
    assert!(out.stderr.contains("mfr_path"), "stderr: {}", out.stderr);
}

// ── Schema ────────────────────────────────────────────────────────────────────

pub(super) const FILM_SCHEMA: &str = r#"{
  "version": 1,
  "groups": [
    {"targets": ["film"],
     "constraints": [{"field": "rating", "type": "int"}]}
  ]
}"#;

#[test]
fn test_schema_workflow() {
    let (repo, root) = init_repo("schema");
    // Violating entry created before any schema exists (delta validation
    // would reject it afterwards).
    let bad = create_metarecord(&repo, &["mf_schema:string=film", "rating:string=oops"]);

    std::fs::write(root.join(".metafolder/config/schema.json"), FILM_SCHEMA).unwrap();
    let out = mf(&["-u", &repo, "schema", "reload"]);
    assert_ok(&out);

    let out = mf(&["-u", &repo, "schema", "show"]);
    assert_ok(&out);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(parsed["version"], 1);

    // One violation: exit code 1, one line per violation plus the summary.
    let out = mf(&["-u", &repo, "schema", "check"]);
    assert_eq!(out.code, 1, "violations must yield exit code 1\nstdout: {}", out.stdout);
    assert!(out.stdout.contains(&bad), "violation line should name the entry");
    assert!(out.stdout.contains("Checked 2 metarecords, 1 violation"), "stdout: {}", out.stdout);

    // Fix the wrong-typed field: under the one-value-type-per-field invariant a
    // String field cannot be set to an Int directly — `retype` is the way to
    // change an established type (the un-parsable "oops" falls back to 0).
    let out = mf(&["-u", &repo, "retype", "rating", "int"]);
    assert_ok(&out);
    let out = mf(&["-u", &repo, "schema", "check"]);
    assert_ok(&out);
    assert!(out.stdout.contains("0 violations"), "stdout: {}", out.stdout);

    // --json prints the raw response body.
    let out = mf(&["-u", &repo, "schema", "check", "--json"]);
    assert_ok(&out);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(parsed["checked"], 2);
}

#[test]
fn test_schema_check_with_predicate() {
    let (repo, root) = init_repo("schema_pred");
    create_metarecord(&repo, &["mf_schema:string=film", "rating:string=bad"]);
    std::fs::write(root.join(".metafolder/config/schema.json"), FILM_SCHEMA).unwrap();
    let out = mf(&["-u", &repo, "schema", "reload"]);
    assert_ok(&out);

    // The predicate restricts the scan to non-matching entries: no violation.
    let out = mf(&["-u", &repo, "schema", "check", r#"mf_schema = "documentary""#]);
    assert_ok(&out);
    assert!(out.stdout.contains("Checked 0 metarecords"), "stdout: {}", out.stdout);
}

#[test]
fn test_schema_reload_invalid_file_fails() {
    let (repo, root) = init_repo("schema_invalid");
    std::fs::write(root.join(".metafolder/config/schema.json"), "{not json").unwrap();
    let out = mf(&["-u", &repo, "schema", "reload"]);
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
    assert!(out.stderr.starts_with("error:"));
}

// ── Verb-tree additions (doc "The verb pattern") ──────────────────────────────

#[test]
fn test_repo_selected_by_name() {
    // The repo's name is derived from its (unique) directory basename; -n
    // resolves it to the uuid through GET /repos.
    let (uuid, root) = init_repo("by_name");
    let name = root.file_name().unwrap().to_str().unwrap().to_string();

    let by_name = mf(&["-n", &name, "metarecord", "add", "tag:string=x"]);
    assert_ok(&by_name);
    // The record is visible when addressing the same repo by uuid.
    let listed = mf(&["-u", &uuid, "metarecord", "get"]);
    assert_ok(&listed);
    assert!(listed.stdout.contains(by_name.stdout.trim()));

    // An unknown name is an operation error.
    let missing = mf(&["-n", "no-such-repo-xyz", "metarecord", "get"]);
    assert_eq!(missing.code, 1, "stderr: {}", missing.stderr);
}

#[test]
fn test_metarecord_set_overwrites_whole_record_and_needs_force() {
    let (repo, _root) = init_repo("mset");
    let uuid = create_metarecord(&repo, &["a:int=1", "b:string=keep"]);

    // Without -f it refuses and changes nothing.
    let no_force = mf(&["-u", &repo, "metarecord", "-i", &uuid, "set", "c:int=9"]);
    assert_eq!(no_force.code, 2, "stderr: {}", no_force.stderr);

    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "set", "c:int=9", "-f"]);
    assert_ok(&out);
    let entries = get_entries(&repo, &uuid);
    let entry = &entries.as_array().unwrap()[0];
    let names: Vec<&str> =
        entry["fields"].as_array().unwrap().iter().map(|f| f["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["c"], "old fields dropped, only the new set remains");
}

#[test]
fn test_field_multi_value_set_and_unset() {
    let (repo, _root) = init_repo("fmulti");
    let uuid = create_metarecord(&repo, &["genre:string=jazz"]);

    // Set two values of `tag` at once (multi-map).
    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &uuid,
        "field",
        "set",
        "tag:string=a",
        "tag:string=b",
    ]);
    assert_ok(&out);
    let count_tags = |target: &str| -> usize {
        let entries = get_entries(&repo, target);
        let entry = &entries.as_array().unwrap()[0];
        entry["fields"].as_array().unwrap().iter().filter(|f| f["name"] == "tag").count()
    };
    assert_eq!(count_tags(&uuid), 2);

    // Unset removes the whole field.
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "unset", "tag"]);
    assert_ok(&out);
    assert_eq!(count_tags(&uuid), 0);
}

#[test]
fn test_field_by_id_get_set_delete() {
    let (repo, _root) = init_repo("fbyid");
    let uuid = create_metarecord(&repo, &["rating:int=5"]);
    let entries = get_entries(&repo, &uuid);
    let id = entries.as_array().unwrap()[0]["fields"][0]["id"].as_i64().unwrap().to_string();

    // get by id
    let got = mf(&["-u", &repo, "field", "get", &id]);
    assert_ok(&got);
    assert!(got.stdout.contains("rating"));

    // set by id: rename + revalue, keeping the id
    let set = mf(&["-u", &repo, "field", "set", &id, "score:int=9"]);
    assert_ok(&set);
    let entries = get_entries(&repo, &uuid);
    let entry = &entries.as_array().unwrap()[0];
    assert_eq!(entry["fields"][0]["id"].as_i64().unwrap().to_string(), id);
    assert_eq!(entry["fields"][0]["name"], "score");

    // delete by id
    let del = mf(&["-u", &repo, "field", "delete", &id]);
    assert_ok(&del);
    let entries = get_entries(&repo, &uuid);
    assert_eq!(entries.as_array().unwrap()[0]["fields"].as_array().unwrap().len(), 0);
}

// ── CLI primitives: --eq, --tsv, --resolve ────────────────────────────────────

#[test]
fn test_cli_primitives_eq_tsv_resolve() {
    let (repo, _root) = init_repo("prim");
    let coltrane =
        create_metarecord(&repo, &["type:string=person", "name:string=Coltrane", "lead:bool=true"]);
    let davis = create_metarecord(&repo, &["type:string=person", "name:string=Davis"]);
    let rec = create_metarecord(
        &repo,
        &[&format!("author:ref={coltrane}"), &format!("author:ref={davis}")],
    );

    // --eq: safe exact match (no DSL interpolation) → the Coltrane uuid only.
    let out =
        mf(&["-u", &repo, "metarecord", "--eq", "type=person", "--eq", "name=Coltrane", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), coltrane);

    // --tsv: name<TAB>lead, one row per person (absent field = empty).
    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "--eq",
        "type=person",
        "get",
        "--select",
        "name,lead",
        "--tsv",
    ]);
    assert_ok(&out);
    let mut lines: Vec<&str> = out.stdout.lines().collect();
    lines.sort();
    assert_eq!(lines, vec!["Coltrane\ttrue", "Davis\t"]);

    // --resolve on a string target field: the `author` refs → their `name`, in
    // one round-trip (the tree-aware path form is covered by the tag test).
    let out =
        mf(&["-u", &repo, "metarecord", "-i", &rec, "field", "get", "author", "--resolve", "name"]);
    assert_ok(&out);
    let mut names: Vec<&str> = out.stdout.lines().collect();
    names.sort();
    assert_eq!(names, vec!["Coltrane", "Davis"]);
}

#[test]
fn test_metarecord_get_resolve_tree_lists_paths() {
    let (repo, root) = init_repo("resolve_tree");
    std::fs::create_dir_all(root.join("a/b")).unwrap();
    std::fs::write(root.join("a/f.txt"), b"x").unwrap();
    // Track the two directories and one file (track builds the parent chain).
    assert_ok(&mf(&["-u", &repo, "metarecord", "add", "--path", root.join("a").to_str().unwrap()]));
    assert_ok(&mf(&[
        "-u",
        &repo,
        "metarecord",
        "add",
        "--path",
        root.join("a/b").to_str().unwrap(),
    ]));
    let f =
        mf(&["-u", &repo, "metarecord", "add", "--path", root.join("a/f.txt").to_str().unwrap()]);
    assert_ok(&f);
    let f_uuid = f.stdout.trim().to_string();

    // Bulk (query selector): every directory's mfr_path resolved to its
    // repo-root-relative path, one per line (leading-"/"-rooted, the DSL form —
    // paste-able straight into an `mfr_path -> "…"` query) — the one round-trip
    // that lets a GUI script offer folder completions.
    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        "mfr_type = \"dir\"",
        "get",
        "--resolve-tree",
        "mfr_path",
    ]);
    assert_ok(&out);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert!(lines.contains(&"/a"), "got: {}", out.stdout);
    assert!(lines.contains(&"/a/b"), "got: {}", out.stdout);

    // Direct selector (-i): the single record's resolved path.
    let out = mf(&["-u", &repo, "metarecord", "-i", &f_uuid, "get", "--resolve-tree", "mfr_path"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "/a/f.txt");

    // No selector is a usage error (exit 2, no HTTP round-trip).
    let out = mf(&["-u", &repo, "metarecord", "get", "--resolve-tree", "mfr_path"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

/// `--resolve-tree --tsv` keeps the uuid next to the path. Without it the
/// endpoint's uuid→paths map is flattened to bare paths, so a script that has to
/// act on the records — not just offer completions — cannot tell which record a
/// path belongs to and falls back to one `mf path` call per record.
#[test]
fn test_metarecord_get_resolve_tree_tsv_keeps_the_uuid() {
    let (repo, root) = init_repo("resolve_tree_tsv");
    std::fs::create_dir_all(root.join("a/b")).unwrap();
    assert_ok(&mf(&["-u", &repo, "metarecord", "add", "--path", root.join("a").to_str().unwrap()]));
    let b = mf(&["-u", &repo, "metarecord", "add", "--path", root.join("a/b").to_str().unwrap()]);
    assert_ok(&b);
    let b_uuid = b.stdout.trim().to_string();

    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        "mfr_type = \"dir\"",
        "get",
        "--resolve-tree",
        "mfr_path",
        "--tsv",
    ]);
    assert_ok(&out);
    let rows: Vec<(&str, &str)> =
        out.stdout.lines().filter_map(|line| line.split_once('\t')).collect();
    assert!(
        rows.contains(&(b_uuid.as_str(), "/a/b")),
        "expected {b_uuid}\t/a/b, got: {}",
        out.stdout
    );
    // One row per (metarecord, path): every line carries a uuid.
    assert!(
        out.stdout.lines().all(|line| line.contains('\t')),
        "every line is a uuid/path pair: {}",
        out.stdout
    );

    // --tsv still needs a field list when it is not resolving a tree.
    let out = mf(&["-u", &repo, "metarecord", "-q", "mfr_type = \"dir\"", "get", "--tsv"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

// ── field get over a query, duplicate list's sort and limit ───────────────────

/// `field get` with a query selector prints each match's values; `--resolve`
/// needs a single metarecord.
#[test]
fn test_field_get_with_a_query_selector() {
    let (repo, _root) = init_repo("fieldgetq");
    create_metarecord(&repo, &["kind:string=fgq", "title:string=first"]);
    create_metarecord(&repo, &["kind:string=fgq", "title:string=second"]);

    let out = mf(&["-u", &repo, "metarecord", "-q", r#"kind = "fgq""#, "field", "get", "title"]);
    assert_ok(&out);
    let mut values: Vec<&str> = out.stdout.lines().collect();
    values.sort();
    assert_eq!(values, ["first", "second"], "stdout: {}", out.stdout);

    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        r#"kind = "fgq""#,
        "field",
        "get",
        "title",
        "--resolve",
        "name",
    ]);
    assert_eq!(out.code, 2, "stdout: {}\nstderr: {}", out.stdout, out.stderr);
    assert!(out.stderr.contains("--resolve requires -i"), "stderr: {}", out.stderr);
}

/// `duplicate list --sort field:asc` reverses the default order, `--limit`
/// caps the groups listed, and an unknown order is a usage error.
#[test]
fn test_duplicate_list_sort_and_limit() {
    let (repo, _dir) = tracked_repo(
        "dupsort",
        &[
            ("s1.txt", b"short"),
            ("s2.txt", b"short"),
            ("l1.txt", b"a much longer content"),
            ("l2.txt", b"a much longer content"),
        ],
    );
    assert_ok(&mf(&["-u", &repo, "duplicate", "scan"]));
    let rows = |args: &[&str]| -> Vec<String> {
        let mut all = vec!["-u", &repo, "duplicate", "list"];
        all.extend_from_slice(args);
        let out = mf(&all);
        assert_ok(&out);
        out.stdout.lines().skip(1).map(str::to_string).collect()
    };
    let desc = rows(&[]);
    assert_eq!(desc.len(), 2, "{desc:?}");
    let asc = rows(&["--sort", "mfr_duplicate_reclaimable:asc"]);
    assert_eq!(asc, desc.iter().rev().cloned().collect::<Vec<_>>());
    assert_eq!(rows(&["--sort", "mfr_content_size:desc", "--limit", "1"]), desc[..1]);

    let out = mf(&["-u", &repo, "duplicate", "list", "--sort", "size:sideways"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
    assert!(out.stderr.contains("unknown sort order 'sideways'"), "stderr: {}", out.stderr);
}
