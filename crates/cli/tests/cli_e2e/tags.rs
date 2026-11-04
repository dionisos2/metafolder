use super::*;

// ── mf order (folder child numbering) ─────────────────────────────────────────

#[test]
fn test_order_numbers_folder_children() {
    let (repo, root) = init_repo("order");
    let album = root.join("album");
    std::fs::create_dir_all(album.join("extra")).unwrap();
    for f in ["song0.avi", "song1.avi", "song3.avi", "README.md"] {
        std::fs::write(album.join(f), b"x").unwrap();
    }
    // Record each child (the metarecord, with its mfr_type).
    let track = |p: std::path::PathBuf| {
        let out = mf(&["-u", &repo, "metarecord", "add", "--path", p.to_str().unwrap()]);
        assert_ok(&out);
        out.stdout.trim().to_string()
    };
    let album_uuid = track(album.clone());
    let song0 = track(album.join("song0.avi"));
    let song1 = track(album.join("song1.avi"));
    let song3 = track(album.join("song3.avi"));
    let readme = track(album.join("README.md"));
    let extra = track(album.join("extra"));

    // song0 carries an ordering metadata (a custom field, so no reserved-force).
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &song0, "field", "set", "track_no:int=3"]));

    let out = mf(&["-u", &repo, "order", album.to_str().unwrap(), "--meta", "track_no"]);
    assert_ok(&out);

    let pos = |uuid: &str, field: &str| -> String {
        let out = mf(&["-u", &repo, "metarecord", "-i", uuid, "field", "get", field]);
        assert_ok(&out);
        out.stdout.trim().to_string()
    };
    // song0 pinned by its metadata (anchor), then the "song*.avi" name cluster,
    // then README by date; the sub-directory is numbered separately.
    assert_eq!(pos(&song0, "order_file"), "1");
    assert_eq!(pos(&song1, "order_file"), "2");
    assert_eq!(pos(&song3, "order_file"), "4");
    assert_eq!(pos(&readme, "order_file"), "5");
    assert_eq!(pos(&extra, "order_dir"), "1");

    // The folder itself is marked as numbered, so processed folders can be told
    // apart from untreated ones.
    assert_eq!(pos(&album_uuid, "order_numbered"), "true");

    // A second run never overwrites an existing position: nothing is written.
    let again = mf(&["-u", &repo, "order", album.to_str().unwrap(), "--meta", "track_no"]);
    assert_ok(&again);
    assert_eq!(again.stdout.trim(), "0", "second run must write nothing");
    assert_eq!(pos(&song0, "order_file"), "1");
    assert_eq!(pos(&album_uuid, "order_numbered"), "true", "the marker survives a re-run");

    // The numbering was one revision: one undo takes back every position and
    // the marker (it used to take one undo per child).
    assert_ok(&take_back(&repo, "undo", &[]));
    for (uuid, field) in [
        (&song0, "order_file"),
        (&song1, "order_file"),
        (&song3, "order_file"),
        (&readme, "order_file"),
        (&extra, "order_dir"),
        (&album_uuid, "order_numbered"),
    ] {
        let out = mf(&["-u", &repo, "metarecord", "-i", uuid, "field", "get", field]);
        assert!(out.stdout.trim().is_empty(), "{field} of {uuid} survived the undo");
    }
    assert_eq!(pos(&song0, "track_no"), "3", "the undo stops at the numbering");
}

// ── mf tag (hierarchical tags: subsumption) ───────────────────────────────────

#[test]
fn test_mf_tag_subsumption_deny_list() {
    let (repo, _root) = init_repo("tag");
    let rec = create_metarecord(&repo, &["note:string=target"]);
    // Vocabulary as a TreeRef forest on `path`: each tag is a node whose parent
    // is another tag entry. jazz carries the retired `exclusive` flag, which is
    // now an ordinary field with no effect.
    let mk_tag = |specs: &[&str]| create_metarecord(&repo, specs);
    let musique = mk_tag(&["mf_schema:string=tag", "path:tree_ref=/musique"]);
    let jazz = mk_tag(&[
        "mf_schema:string=tag",
        format!("path:tree_ref={musique}/jazz").as_str(),
        "exclusive:bool=true",
    ]);
    let _rock = mk_tag(&["mf_schema:string=tag", format!("path:tree_ref={musique}/rock").as_str()]);
    let _bebop = mk_tag(&["mf_schema:string=tag", format!("path:tree_ref={jazz}/bebop").as_str()]);
    let admin = mk_tag(&["mf_schema:string=tag", "path:tree_ref=/administratif"]);
    let _impots =
        mk_tag(&["mf_schema:string=tag", format!("path:tree_ref={admin}/impots").as_str()]);

    let tag = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["-u", repo.as_str(), "tag"];
        v.extend_from_slice(args);
        assert_ok(&mf(&v));
    };
    // Read the record's tag refs as their resolved hierarchy paths (ref → tag →
    // `path` TreeRef, resolved by the tree-aware `--resolve`).
    let names = |field: &str| -> Vec<String> {
        let out = mf(&[
            "-u",
            &repo,
            "metarecord",
            "-i",
            &rec,
            "field",
            "get",
            field,
            "--resolve",
            "path",
        ]);
        assert_ok(&out);
        let mut v: Vec<String> = out.stdout.lines().map(String::from).collect();
        v.sort();
        v
    };

    tag(&["-i", &rec, "add", "musique/rock"]);
    assert_eq!(names("tag"), vec!["musique/rock"]);

    // Siblings never exclude each other: adding jazz keeps rock, flag or not.
    tag(&["-i", &rec, "add", "musique/jazz"]);
    assert_eq!(names("tag"), vec!["musique/jazz", "musique/rock"]);
    tag(&["-i", &rec, "remove", "musique/rock"]);
    assert_eq!(names("tag"), vec!["musique/jazz"]);

    // The record's single `tag` row id, to tell an untouched row from a rewritten one.
    let tag_row_id = || -> i64 {
        let out = mf(&["-u", &repo, "metarecord", "-i", &rec, "get"]);
        assert_ok(&out);
        let value: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
        let ids: Vec<i64> = value[0]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["name"] == "tag")
            .map(|f| f["id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids.len(), 1, "expected one tag row: {}", out.stdout);
        ids[0]
    };

    // bebop's ancestor jazz is present → dropped on add.
    tag(&["-i", &rec, "add", "musique/jazz/bebop"]);
    assert_eq!(names("tag"), vec!["musique/jazz/bebop"]);

    // Re-adding is idempotent, and writes nothing at all: the daemon refuses to
    // duplicate the row (doc "No duplicate rows"), so the existing
    // row keeps its id and no metarecord is counted as having gained one.
    let before = tag_row_id();
    let again = mf(&["-u", &repo, "tag", "-i", &rec, "add", "musique/jazz/bebop"]);
    assert_ok(&again);
    assert_eq!(again.stdout.trim(), "0", "nothing gained a row");
    assert_eq!(names("tag"), vec!["musique/jazz/bebop"]);
    assert_eq!(tag_row_id(), before, "the row that already held the tag is untouched");

    // A path absent from the vocabulary is auto-created as a node chain
    // (cinema → cinema/thriller) by `ensure_tag_entry`. It is unrelated to bebop,
    // so both tags coexist (add only drops ancestors).
    tag(&["-i", &rec, "add", "cinema/thriller"]);
    assert_eq!(names("tag"), vec!["cinema/thriller", "musique/jazz/bebop"]);

    // deny: a generic negative subsumes (drops) its specific descendant.
    tag(&["-i", &rec, "deny", "administratif/impots"]);
    assert_eq!(names("negative_tag"), vec!["administratif/impots"]);
    tag(&["-i", &rec, "deny", "administratif"]);
    assert_eq!(names("negative_tag"), vec!["administratif"]);

    // list = the vocabulary, path<TAB>uuid<TAB>always<TAB>partition<TAB>sparse
    // (auto-created cinema tags present; the retired `exclusive` is no flag).
    let out = mf(&["-u", &repo, "tag", "list"]);
    assert_ok(&out);
    let listed: Vec<&str> = out.stdout.lines().filter_map(|l| l.split('\t').next()).collect();
    for path in ["musique/jazz", "administratif", "cinema/thriller"] {
        assert!(listed.contains(&path), "{path} in list:\n{}", out.stdout);
    }
    let jazz_line = format!("musique/jazz\t{jazz}\t0\t0\t0");
    assert!(out.stdout.lines().any(|l| l == jazz_line), "list:\n{}", out.stdout);
}

/// A partition's children exclude one another (doc "Tag flags"): adding one
/// drops the others, and what is under them; the flags show in the list.
#[test]
fn test_mf_tag_partition_excludes_the_siblings() {
    let (repo, _root) = init_repo("tag-partition");
    let rec = create_metarecord(&repo, &["note:string=target"]);
    let mk_tag = |specs: &[&str]| create_metarecord(&repo, specs);
    let saison = mk_tag(&[
        "mf_schema:string=tag",
        "path:hierarchy_ref=/saison",
        "partition:bool=true",
        "sparse:bool=false",
    ]);
    let ete = mk_tag(&["mf_schema:string=tag", &format!("path:hierarchy_ref={saison}/été")]);
    let _hiver = mk_tag(&[
        "mf_schema:string=tag",
        &format!("path:hierarchy_ref={saison}/hiver"),
        "always:bool=true",
    ]);
    let _canicule =
        mk_tag(&["mf_schema:string=tag", &format!("path:hierarchy_ref={ete}/canicule")]);
    let tag = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["-u", repo.as_str(), "tag", "-i", rec.as_str()];
        v.extend_from_slice(args);
        assert_ok(&mf(&v));
    };
    let positives = || {
        let out = mf(&[
            "-u",
            &repo,
            "metarecord",
            "-i",
            &rec,
            "field",
            "get",
            "tag",
            "--resolve",
            "path",
        ]);
        assert_ok(&out);
        out.stdout.lines().map(String::from).collect::<Vec<_>>()
    };

    tag(&["add", "saison/été/canicule"]);
    tag(&["add", "saison/hiver"]);
    assert_eq!(positives(), vec!["saison/hiver"], "canicule is été, which hiver excludes");
    tag(&["add", "saison/été"]);
    assert_eq!(positives(), vec!["saison/été"]);

    let out = mf(&["-u", &repo, "tag", "list"]);
    assert_ok(&out);
    for line in [format!("saison\t{saison}\t0\t1\t0"), format!("saison/été\t{ete}\t0\t0\t0")] {
        assert!(out.stdout.lines().any(|l| l == line), "{line} in:\n{}", out.stdout);
    }
    assert!(out
        .stdout
        .lines()
        .any(|l| l.starts_with("saison/hiver\t") && l.ends_with("\t1\t0\t0")));
}

/// A tag vocabulary that is a hierarchy (doc "Hierarchy references"): a tag
/// under two parents has both paths, and subsumption follows every one — its
/// ancestors by either parent are implied, a negative on either parent covers it.
#[test]
fn test_mf_tag_follows_every_parent_of_a_hierarchy() {
    let (repo, _root) = init_repo("tag-hierarchy");
    let rec = create_metarecord(&repo, &["note:string=target"]);
    let mk_tag = |specs: &[&str]| create_metarecord(&repo, specs);
    let sport = mk_tag(&["mf_schema:string=tag", "path:hierarchy_ref=/sport"]);
    let courir =
        mk_tag(&["mf_schema:string=tag", format!("path:hierarchy_ref={sport}/courir").as_str()]);
    let event = mk_tag(&["mf_schema:string=tag", "path:hierarchy_ref=/évènement"]);
    let maraton = mk_tag(&[
        "mf_schema:string=tag",
        format!("path:hierarchy_ref={courir}/maraton").as_str(),
        format!("path:hierarchy_ref={event}/maraton").as_str(),
    ]);

    let tag = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["-u", repo.as_str(), "tag"];
        v.extend_from_slice(args);
        assert_ok(&mf(&v));
    };
    let names = |field: &str| -> Vec<String> {
        let out = mf(&[
            "-u",
            &repo,
            "metarecord",
            "-i",
            &rec,
            "field",
            "get",
            field,
            "--resolve",
            "path",
        ]);
        assert_ok(&out);
        let mut v: Vec<String> = out.stdout.lines().map(String::from).collect();
        v.sort();
        v
    };

    // Named by one path, the tag shows both; its ancestor by the other path goes.
    tag(&["-i", &rec, "add", "sport/courir"]);
    tag(&["-i", &rec, "add", "évènement/maraton"]);
    assert_eq!(names("tag"), vec!["sport/courir/maraton", "évènement/maraton"]);

    // Not an event: not a marathon either, whichever path named it.
    tag(&["-i", &rec, "deny", "évènement"]);
    assert!(names("tag").is_empty(), "{:?}", names("tag"));
    assert_eq!(names("negative_tag"), vec!["évènement"]);

    // A marathon by its sport path is an event: the negative goes.
    tag(&["-i", &rec, "add", "sport/courir/maraton"]);
    assert!(names("negative_tag").is_empty(), "{:?}", names("negative_tag"));

    // The list has one line per path, with the tag's uuid.
    let out = mf(&["-u", &repo, "tag", "list"]);
    assert_ok(&out);
    for path in ["sport/courir/maraton", "évènement/maraton"] {
        let line = format!("{path}\t{maraton}\t0\t0\t0");
        assert!(out.stdout.lines().any(|l| l == line), "{line} in:\n{}", out.stdout);
    }
}

/// A repository created before hierarchies has a forest vocabulary and a schema
/// saying so. Converting it (doc "Hierarchy references") is the schema's
/// constraint changed, reloaded, then the field retyped — after which a tag
/// takes a second parent, and `mf tag` keeps working.
#[test]
fn test_a_tree_vocabulary_converts_to_a_hierarchy() {
    let (repo, root) = init_repo("tag-convert");
    let schema = |ty: &str, max: &str| {
        format!(
            r#"{{"version": 1, "groups": [{{"targets": ["tag"], "constraints": [
                {{"field": "path", "type": "{ty}", "min": 1{max}}}]}}]}}"#
        )
    };
    let file = root.join(".metafolder/config/schema.json");
    std::fs::write(&file, schema("tree_ref", r#", "max": 1"#)).unwrap();
    assert_ok(&mf(&["-u", &repo, "schema", "reload"]));
    let rec = create_metarecord(&repo, &["note:string=target"]);
    assert_ok(&mf(&["-u", &repo, "tag", "-i", &rec, "add", "sport/courir/maraton"]));
    assert_ok(&mf(&["-u", &repo, "tag", "-i", &rec, "add", "évènement"]));
    let out = mf(&["-u", &repo, "field", "list"]);
    assert!(out.stdout.lines().any(|l| l == "path\ttree_ref"), "{}", out.stdout);

    std::fs::write(&file, schema("hierarchy_ref", "")).unwrap();
    assert_ok(&mf(&["-u", &repo, "schema", "reload"]));
    let out = mf(&["-u", &repo, "retype", "path", "hierarchy_ref"]);
    assert_ok(&out);

    let id_of = |q: &str| -> String {
        let out = mf(&["-u", &repo, "metarecord", "-q", q, "get"]);
        assert_ok(&out);
        out.stdout.trim().to_string()
    };
    let maraton = id_of(r#"path = "sport/courir/maraton""#);
    let event = id_of(r#"path = "évènement""#);
    let spec = format!("path:hierarchy_ref={event}/maraton");
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &maraton, "field", "add", &spec]));
    assert_eq!(id_of(r#"path = "évènement/maraton""#), maraton);
    // The marathon is an event now: tagging it drops the event tag it implies.
    assert_ok(&mf(&["-u", &repo, "tag", "-i", &rec, "add", "évènement/maraton"]));
    let out =
        mf(&["-u", &repo, "metarecord", "-i", &rec, "field", "get", "tag", "--resolve", "path"]);
    let mut tags: Vec<&str> = out.stdout.lines().collect();
    tags.sort();
    assert_eq!(tags, vec!["sport/courir/maraton", "évènement/maraton"]);
}

/// A fresh vocabulary is a hierarchy: `mf tag` creates its entries with a
/// `hierarchy_ref`, while a vocabulary already placed by `tree_ref` keeps it.
#[test]
fn test_mf_tag_creates_a_hierarchy() {
    let (repo, _root) = init_repo("tag-fresh");
    let rec = create_metarecord(&repo, &["note:string=target"]);
    assert_ok(&mf(&["-u", &repo, "tag", "-i", &rec, "add", "cinema/thriller"]));
    let out = mf(&["-u", &repo, "field", "list"]);
    assert_ok(&out);
    assert!(
        out.stdout
            .lines()
            .any(|l| l.split_whitespace().collect::<Vec<_>>() == ["path", "hierarchy_ref"]),
        "{}",
        out.stdout
    );
}

/// A positive and a negative never contradict each other: `add X` takes back
/// the negatives of X and of its ancestors (X implies them), `deny X` the
/// positives of X and of its descendants (they imply X). A negative below X,
/// or a positive above it, is compatible and stays.
#[test]
fn test_mf_tag_add_and_deny_take_back_what_they_contradict() {
    let (repo, _root) = init_repo("tagcontra");
    let rec = create_metarecord(&repo, &["note:string=x"]);
    let tag = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["-u", repo.as_str(), "tag", "-i", rec.as_str()];
        v.extend_from_slice(args);
        assert_ok(&mf(&v));
    };
    let paths = |field: &str| -> Vec<String> {
        let out = mf(&[
            "-u",
            &repo,
            "metarecord",
            "-i",
            &rec,
            "field",
            "get",
            field,
            "--resolve",
            "path",
        ]);
        assert_ok(&out);
        let mut v: Vec<String> = out.stdout.lines().map(String::from).collect();
        v.sort();
        v
    };

    tag(&["deny", "musique"]);
    tag(&["deny", "musique/jazz/bebop"]);
    tag(&["add", "musique/jazz"]);
    assert_eq!(paths("tag"), vec!["musique/jazz"]);
    assert_eq!(
        paths("negative_tag"),
        vec!["musique/jazz/bebop"],
        "the ancestor's negative is contradicted, the descendant's is not"
    );

    tag(&["add", "musique/jazz/cool"]);
    tag(&["add", "cinema"]);
    tag(&["deny", "musique/jazz"]);
    assert_eq!(paths("negative_tag"), vec!["musique/jazz"], "bebop's is subsumed");
    assert_eq!(paths("tag"), vec!["cinema"], "jazz/cool implied jazz; cinema is unrelated");

    tag(&["add", "musique"]);
    tag(&["deny", "musique/rock"]);
    assert_eq!(paths("tag"), vec!["cinema", "musique"], "a positive above stays");
}

/// "Mixed X" is "partly X": `add Y` makes the folder wholly Y and so wholly
/// every ancestor of Y, `deny X` wholly not X and so not any descendant — each
/// takes back the mixed marks it makes false, and only those.
#[test]
fn test_mf_tag_add_and_deny_take_back_the_mixed_marks_they_settle() {
    let (repo, _root) = init_repo("tagmixed");
    let rec = create_metarecord(&repo, &["note:string=x"]);
    let tag = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["-u", repo.as_str(), "tag", "-i", rec.as_str()];
        v.extend_from_slice(args);
        assert_ok(&mf(&v));
    };
    let mixed = || -> Vec<String> {
        let out = mf(&[
            "-u",
            &repo,
            "metarecord",
            "-i",
            &rec,
            "field",
            "get",
            "mixed_tag",
            "--resolve",
            "path",
        ]);
        assert_ok(&out);
        let mut v: Vec<String> = out.stdout.lines().map(String::from).collect();
        v.sort();
        v
    };

    for t in ["musique", "musique/jazz", "musique/jazz/bebop", "cinema"] {
        tag(&["mixed", t]);
    }
    tag(&["add", "musique/jazz"]);
    assert_eq!(mixed(), vec!["cinema", "musique/jazz/bebop"], "jazz and music are settled");

    tag(&["mixed", "cinema/noir"]);
    tag(&["deny", "cinema"]);
    assert_eq!(mixed(), vec!["musique/jazz/bebop"], "cinema and all below are settled");
}

/// `mixed X` is "partly X and partly not": it takes back the whole answers it
/// contradicts — a positive on X or below (wholly jazz is wholly music), a
/// negative on X or above (wholly not music is wholly not jazz) — and only those.
#[test]
fn test_mf_tag_mixed_takes_back_the_whole_answers_it_contradicts() {
    let (repo, _root) = init_repo("tagmixed2");
    let rec = create_metarecord(&repo, &["note:string=x"]);
    let tag = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["-u", repo.as_str(), "tag", "-i", rec.as_str()];
        v.extend_from_slice(args);
        assert_ok(&mf(&v));
    };
    let paths = |field: &str| -> Vec<String> {
        let out = mf(&[
            "-u",
            &repo,
            "metarecord",
            "-i",
            &rec,
            "field",
            "get",
            field,
            "--resolve",
            "path",
        ]);
        assert_ok(&out);
        let mut v: Vec<String> = out.stdout.lines().map(String::from).collect();
        v.sort();
        v
    };

    tag(&["add", "musique/jazz/bebop"]);
    tag(&["add", "cinema"]);
    tag(&["mixed", "musique/jazz"]);
    assert_eq!(paths("tag"), vec!["cinema"], "bebop wholly implied jazz wholly");
    assert_eq!(paths("mixed_tag"), vec!["musique/jazz"]);

    tag(&["deny", "livres"]);
    tag(&["deny", "livres/bd/manga"]);
    tag(&["mixed", "livres/bd"]);
    assert_eq!(paths("negative_tag"), vec!["livres/bd/manga"], "not manga stays compatible");
}

/// One `mf tag` is one revision: the entry it creates, the ref it adds and
/// everything it drops come back together with a single `mf log undo`.
#[test]
fn test_mf_tag_is_undone_in_one_step() {
    let (repo, _root) = init_repo("tagundo");
    let rec = create_metarecord(&repo, &["note:string=x"]);
    let tag = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["-u", repo.as_str(), "tag", "-i", rec.as_str()];
        v.extend_from_slice(args);
        assert_ok(&mf(&v));
    };
    let paths = |field: &str| -> Vec<String> {
        let out = mf(&[
            "-u",
            &repo,
            "metarecord",
            "-i",
            &rec,
            "field",
            "get",
            field,
            "--resolve",
            "path",
        ]);
        assert_ok(&out);
        let mut v: Vec<String> = out.stdout.lines().map(String::from).collect();
        v.sort();
        v
    };
    let vocabulary = || mf(&["-u", &repo, "tag", "list"]).stdout;

    tag(&["add", "musique"]);
    tag(&["mixed", "musique/jazz"]);
    tag(&["deny", "cinema"]);
    let vocabulary_before = vocabulary();

    // Creates the bebop entry, adds it, and drops the ancestor positive and
    // the ancestor's mixed mark: four writes.
    tag(&["add", "musique/jazz/bebop"]);
    assert_eq!(paths("tag"), vec!["musique/jazz/bebop"]);
    assert!(paths("mixed_tag").is_empty());
    assert_ne!(vocabulary(), vocabulary_before);

    assert_ok(&take_back(&repo, "undo", &[]));
    assert_eq!(paths("tag"), vec!["musique"]);
    assert_eq!(paths("mixed_tag"), vec!["musique/jazz"]);
    assert_eq!(paths("negative_tag"), vec!["cinema"]);
    assert_eq!(vocabulary(), vocabulary_before, "the entry the add created is gone too");
}

/// `mf tag remove` prints what the other verbs print: how many records the
/// write changed — here, lost the ref — whether or not the tag exists.
#[test]
fn test_mf_tag_remove_prints_how_many_records_lost_the_tag() {
    let (repo, _root) = init_repo("tagrm");
    let x = create_metarecord(&repo, &["note:string=x"]);
    let y = create_metarecord(&repo, &["note:string=y"]);
    let tag = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["-u", repo.as_str(), "tag"];
        v.extend_from_slice(args);
        let out = mf(&v);
        assert_ok(&out);
        out.stdout.trim().to_string()
    };
    tag(&["-i", &x, "add", "music"]);
    let both = format!("uuid_in({x}, {y})");
    assert_eq!(tag(&["-q", &both, "remove", "music"]), "1", "only x carried it");
    assert_eq!(tag(&["-q", &both, "remove", "music"]), "0", "nothing left to remove");
    assert_eq!(tag(&["-q", &both, "remove", "nowhere"]), "0", "an unknown tag removes nothing");
}

#[test]
fn test_watch_status_pause_and_resume() {
    let (repo, _root) = init_repo("watchpause");

    // A freshly loaded repository ingests filesystem events.
    let out = mf(&["-u", &repo, "watch"]);
    assert_ok(&out);
    assert!(out.stdout.starts_with("running"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("0 event(s) waiting"), "stdout: {}", out.stdout);

    let out = mf(&["-u", &repo, "watch", "pause"]);
    assert_ok(&out);
    assert!(out.stdout.starts_with("paused"), "stdout: {}", out.stdout);

    // The pause is repository state, not a property of the call that set it.
    let out = mf(&["-u", &repo, "watch", "status", "--json"]);
    assert_ok(&out);
    assert!(out.stdout.contains("\"paused\": true"), "stdout: {}", out.stdout);

    let out = mf(&["-u", &repo, "watch", "resume"]);
    assert_ok(&out);
    assert!(out.stdout.starts_with("running"), "stdout: {}", out.stdout);
}

/// A repository left on inotify says why, in `watch status` itself: which
/// broker was probed and what it answered (or that none did).
#[test]
fn test_watch_status_says_why_the_broker_is_not_used() {
    let (repo, _root) = init_repo("watchwhy");
    let json = mf(&["-u", &repo, "watch", "status", "--json"]);
    assert_ok(&json);
    let body: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    let out = mf(&["-u", &repo, "watch", "status"]);
    assert_ok(&out);
    if body["backend"] == "fanotify" {
        // A real broker on this machine: nothing to explain.
        assert!(body["backend_reason"].is_null(), "{body}");
        assert!(!out.stdout.contains("broker not used"), "stdout: {}", out.stdout);
    } else {
        let reason = body["backend_reason"].as_str().expect("a reason for the fallback");
        assert!(reason.contains("broker"), "{reason}");
        assert!(out.stdout.contains("fanotify broker not used:"), "stdout: {}", out.stdout);
        assert!(out.stdout.contains(reason), "stdout: {}", out.stdout);
    }
}

#[test]
fn test_mount_list_and_forget() {
    let (repo, root) = init_repo("mount");
    let dir = root.join("photos");
    std::fs::create_dir(&dir).unwrap();
    let uuid = mf(&["-u", &repo, "metarecord", "add", "--path", dir.to_str().unwrap()])
        .stdout
        .trim()
        .to_string();
    assert!(is_hex_uuid(&uuid));

    // A repository with no removable volume declares no mount point.
    let out = mf(&["-u", &repo, "mount"]);
    assert_ok(&out);
    assert!(out.stdout.contains("No mount points"), "stdout: {}", out.stdout);

    // Declare one by hand (an mfr_* write, hence --force): the directory is an
    // ordinary one, so it is exactly what an unplugged volume looks like.
    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &uuid,
        "field",
        "add",
        "mfr_mount:string=label:PHOTOS",
        "--force",
    ]);
    assert_ok(&out);

    let out = mf(&["-u", &repo, "mount", "list"]);
    assert_ok(&out);
    assert!(out.stdout.contains("offline"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("/photos"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("label:PHOTOS"), "stdout: {}", out.stdout);

    // `forget` takes the mount point's uuid, never a query that would
    // un-declare every mount point it matches.
    let out = mf(&["-u", &repo, "mount", "forget", "mfr_mount IS PRESENT", "-y"]);
    assert_eq!(out.code, 2, "{}", out.stderr);
    let out = mf(&["-u", &repo, "mount", "list"]);
    assert!(out.stdout.contains("label:PHOTOS"), "still declared: {}", out.stdout);

    // `forget` un-declares it: the directory becomes an ordinary one again.
    let out = mf(&["-u", &repo, "mount", "forget", &uuid, "-y"]);
    assert_ok(&out);
    let out = mf(&["-u", &repo, "mount"]);
    assert_ok(&out);
    assert!(out.stdout.contains("No mount points"), "stdout: {}", out.stdout);
}
