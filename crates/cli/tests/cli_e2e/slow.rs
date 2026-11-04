use super::*;

// ── mf slow (doc "Slow log") ───────────────────────────────────────────────────

#[test]
fn test_slow_lists_the_log_newest_first_and_clears_it() {
    use metafolder_core::slowlog::{self, Entry, Phase};

    let (repo, root) = init_repo("slow");
    let dir = slowlog::slow_dir(&root.join(".metafolder").join("internal"));
    let mut slow = Entry::new("daemon", "POST /repos/:repo/query", 1_757_426_602_000, 4820);
    slow.note("client", "rating > 3");
    slow.phases.push(Phase { name: "wait:conn".into(), ms: 3100, count: 1, depth: 0, reads: 0 });
    slowlog::Sink::new(&dir, "daemon").append(&slow);
    slowlog::Sink::new(&dir, "gui").append(&Entry::new(
        "gui",
        "GET /repos/:repo/metarecords/:uuid",
        1_757_426_600_000,
        2500,
    ));

    let out = mf(&["-u", &repo, "slow"]);
    assert_ok(&out);
    assert!(out.stdout.starts_with(&format!("{}  ", slow.id())), "id first:\n{}", out.stdout);
    let query_at = out.stdout.find("POST /repos/:repo/query").expect(&out.stdout);
    let gui_at = out.stdout.find("GET /repos/:repo/metarecords/:uuid").expect(&out.stdout);
    assert!(query_at < gui_at, "newest first, both sources merged:\n{}", out.stdout);
    assert!(out.stdout.contains("4.82s"), "{}", out.stdout);
    assert!(out.stdout.contains("wait:conn"), "the breakdown is the point:\n{}", out.stdout);
    assert!(out.stdout.contains("rating > 3"), "{}", out.stdout);

    // `--op` narrows to one shape; `--json` is the machine form.
    let out = mf(&["-u", &repo, "slow", "list", "--op", "query", "--json"]);
    assert_ok(&out);
    assert_eq!(out.stdout.lines().count(), 1, "{}", out.stdout);
    assert!(out.stdout.contains("\"op\":\"POST /repos/:repo/query\""), "{}", out.stdout);
    assert!(out.stdout.contains(&format!("\"id\":\"{}\"", slow.id())), "{}", out.stdout);

    // So is `--op`: the GUI entry, older, is found past the newer query.
    let out = mf(&["-u", &repo, "slow", "list", "--op", "metarecords", "--limit", "1", "--json"]);
    assert_ok(&out);
    assert_eq!(out.stdout.lines().count(), 1, "{}", out.stdout);
    assert!(out.stdout.contains("\"op\":\"GET /repos/:repo/metarecords/:uuid\""), "{}", out.stdout);

    // `--min` keeps what took at least that long — applied before `--limit`, so
    // an old long operation is found without scrolling past the short ones.
    let out = mf(&["-u", &repo, "slow", "list", "--min", "3s", "--limit", "1", "--json"]);
    assert_ok(&out);
    assert_eq!(out.stdout.lines().count(), 1, "{}", out.stdout);
    assert!(out.stdout.contains("\"op\":\"POST /repos/:repo/query\""), "{}", out.stdout);
    // `mf slow` is `mf slow list`, options included.
    let out = mf(&["-u", &repo, "slow", "--min", "3s", "--json"]);
    assert_ok(&out);
    assert_eq!(out.stdout.lines().count(), 1, "{}", out.stdout);
    let out = mf(&["-u", &repo, "slow", "list", "--min", "1m"]);
    assert_ok(&out);
    assert!(out.stdout.contains("Nothing took 1m or more"), "{}", out.stdout);

    let out = mf(&["-u", &repo, "slow", "clear", "-y"]);
    assert_ok(&out);
    assert!(out.stdout.contains("Cleared 2"), "{}", out.stdout);
    let out = mf(&["-u", &repo, "slow"]);
    assert_ok(&out);
    assert!(out.stdout.contains("Nothing has been slow"), "{}", out.stdout);
}

#[test]
fn test_slow_shows_when_in_the_local_time() {
    // The log stores Unix ms; a reader compares it with their own clock.
    use metafolder_core::slowlog::{self, Entry};

    let (repo, root) = init_repo("slow-local-time");
    let dir = slowlog::slow_dir(&root.join(".metafolder").join("internal"));
    // 2025-09-09 14:03:22 UTC, in summer time in Paris (UTC+2).
    let at = 1_757_426_602_000;
    slowlog::Sink::new(&dir, "daemon").append(&Entry::new("daemon", "POST /x", at, 4820));

    let id = Entry::new("daemon", "POST /x", at, 4820).id();
    let out = mf_full(&["-u", &repo, "slow"], None, &[("TZ", "Europe/Paris")], true);
    assert_ok(&out);
    assert!(out.stdout.starts_with(&format!("{id}  2025-09-09 16:03:22 ")), "{}", out.stdout);
    let out = mf_full(&["-u", &repo, "slow"], None, &[("TZ", "UTC")], true);
    assert_ok(&out);
    assert!(out.stdout.starts_with(&format!("{id}  2025-09-09 14:03:22 ")), "{}", out.stdout);
}

#[test]
fn test_slow_replay_runs_a_logged_query_again_and_shows_where_it_goes() {
    use metafolder_core::slowlog::{self, Entry};

    let (repo, root) = init_repo("slow-replay");
    create_metarecord(&repo, &["rating:int=5"]);
    let dir = slowlog::slow_dir(&root.join(".metafolder").join("internal"));
    let logged = |at: i64, query: &str| {
        let mut e = Entry::new("daemon", "POST /repos/:repo/query", at, 4820);
        e.note("query", query);
        e.note("limit", "10");
        e.note("count", "true");
        e
    };
    let older = r#"{"type":"is_present","field":"rating"}"#;
    let newer = r#"{"type":"is_present","field":"nothing_has_this"}"#;
    slowlog::Sink::new(&dir, "daemon").append(&logged(1_757_426_600_000, older));
    slowlog::Sink::new(&dir, "daemon").append(&logged(1_757_426_602_000, newer));

    // The newest logged query by default: its phases, run now.
    let out = mf(&["-u", &repo, "slow", "replay"]);
    assert_ok(&out);
    assert!(out.stdout.contains("index.evaluate"), "{}", out.stdout);
    assert!(out.stdout.contains("results   0"), "the newest: {}", out.stdout);

    // Another one by the id `mf slow` lists it with, or a prefix of it.
    let older_id = logged(1_757_426_600_000, older).id();
    let out = mf(&["-u", &repo, "slow", "replay", &older_id]);
    assert_ok(&out);
    assert!(out.stdout.contains("results   1"), "the older: {}", out.stdout);
    let out = mf(&["-u", &repo, "slow", "replay", &older_id[..4]]);
    assert_ok(&out);
    assert!(out.stdout.contains("results   1"), "by prefix: {}", out.stdout);

    // `show` prints one entry whole: a long query is not cut.
    let long = format!(r#"{{"type":"or","operands":[{}]}}"#, [older; 12].join(","));
    let long_entry = logged(1_757_426_500_000, &long);
    slowlog::Sink::new(&dir, "daemon").append(&long_entry);
    let out = mf(&["-u", &repo, "slow", "show", &long_entry.id()[..6]]);
    assert_ok(&out);
    assert!(out.stdout.starts_with(&long_entry.id()), "{}", out.stdout);
    assert_eq!(out.stdout.matches(r#""field": "rating""#).count(), 12, "{}", out.stdout);
    assert!(!out.stdout.contains('…'), "{}", out.stdout);
    let out = mf(&["-u", &repo, "slow", "show", "ffffffff"]);
    assert_eq!(out.code, 1, "{}", out.stdout);
    assert!(out.stderr.contains("no slow-log entry ffffffff"), "{}", out.stderr);

    // Or any query, typed.
    let out = mf(&["-u", &repo, "slow", "replay", "-q", "rating > 3", "--json"]);
    assert_ok(&out);
    let entry: Entry = serde_json::from_str(out.stdout.trim()).expect(&out.stdout);
    assert!(entry.phases.iter().any(|p| p.name == "index.evaluate"), "{entry:?}");

    // The replays added nothing to the log (the third entry is show's).
    let out = mf(&["-u", &repo, "slow", "list", "--json"]);
    assert_eq!(out.stdout.lines().count(), 3, "{}", out.stdout);
}

// What `mf slow` says when it shows less than the log holds: a filter that
// matched nothing is not an empty log, a window is named, a cut is announced —
// and replay and clear refuse or stop rather than guess.
#[test]
fn test_slow_names_what_it_left_out() {
    use metafolder_core::slowlog::{self, Entry};

    let (repo, root) = init_repo("slow-left-out");
    let dir = slowlog::slow_dir(&root.join(".metafolder").join("internal"));
    // Logged long ago, and not queries.
    for at in [1_757_426_600_000, 1_757_426_602_000] {
        slowlog::Sink::new(&dir, "daemon").append(&Entry::new("daemon", "GET /x", at, 4820));
    }

    let out = mf(&["-u", &repo, "slow", "list", "--op", "query"]);
    assert_ok(&out);
    assert!(out.stdout.contains("No slow operation matches 'query'"), "{}", out.stdout);

    let out = mf(&["-u", &repo, "slow", "list", "-d", "1h"]);
    assert_ok(&out);
    assert!(out.stdout.contains("Nothing has been slow in that window"), "{}", out.stdout);
    let out = mf(&["-u", &repo, "slow", "list", "-d", "soon"]);
    assert_eq!(out.code, 2, "an unparsable window is a usage error");

    let out = mf(&["-u", &repo, "slow", "list", "--limit", "1"]);
    assert_ok(&out);
    assert_eq!(out.stdout.matches("GET /x").count(), 1, "{}", out.stdout);
    assert!(out.stdout.contains("(more entries beyond --limit 1)"), "{}", out.stdout);

    let out = mf(&["-u", &repo, "slow", "replay"]);
    assert_eq!(out.code, 1, "{}", out.stdout);
    assert!(out.stderr.contains("no query in the slow-operation log"), "{}", out.stderr);
    let not_a_query = Entry::new("daemon", "GET /x", 1_757_426_600_000, 4820).id();
    let out = mf(&["-u", &repo, "slow", "replay", &not_a_query]);
    assert_eq!(out.code, 1, "{}", out.stdout);
    assert!(out.stderr.contains(&format!("{not_a_query} is not a query")), "{}", out.stderr);
    let out = mf(&["-u", &repo, "slow", "replay", "ffffffff"]);
    assert_eq!(out.code, 1, "{}", out.stdout);
    assert!(out.stderr.contains("no slow-log entry ffffffff"), "{}", out.stderr);

    let out = mf_full(&["-u", &repo, "slow", "clear"], Some("n\n"), &[], true);
    assert_ok(&out);
    assert!(out.stdout.contains("Cancelled"), "{}", out.stdout);
    let out = mf(&["-u", &repo, "slow", "list", "--json"]);
    assert_eq!(out.stdout.lines().count(), 2, "a cancelled clear keeps the log: {}", out.stdout);
}
