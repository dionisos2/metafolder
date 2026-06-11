//! Tests for file fingerprints (spec-file-tracking "File fingerprint") and
//! the stat-derived `mfr_*` field values (spec-platform).

use std::io::Write as _;
use std::path::PathBuf;

use metafolder_core::entry::Value;
use metafolder_daemon::fingerprint;
use metafolder_daemon::fs_meta;
use uuid::Uuid;

fn temp_file(content: &[u8]) -> PathBuf {
    let path = std::env::temp_dir().join(format!("metafolder_fp_{}", Uuid::new_v4()));
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(content).unwrap();
    path
}

// ── Fingerprints ──────────────────────────────────────────────────────────────

#[test]
fn test_full_hash_is_deterministic_and_content_sensitive() {
    let a1 = temp_file(b"hello world");
    let a2 = temp_file(b"hello world");
    let b = temp_file(b"hello worle");

    let h1 = fingerprint::full_hash(&a1).unwrap();
    let h2 = fingerprint::full_hash(&a2).unwrap();
    let h3 = fingerprint::full_hash(&b).unwrap();
    assert_eq!(h1, h2);
    assert_ne!(h1, h3);
    assert!(h1.chars().all(|c| c.is_ascii_hexdigit()), "hex encoding expected: {h1}");

    for p in [a1, a2, b] {
        std::fs::remove_file(p).unwrap();
    }
}

#[test]
fn test_partial_hash_covers_head_and_tail() {
    // Two 1 MiB files differing only in the middle: same partial hash,
    // different full hash.
    let mut content_a = vec![0u8; 1 << 20];
    let mut content_b = content_a.clone();
    content_a[1 << 19] = 1;
    content_b[1 << 19] = 2;
    let a = temp_file(&content_a);
    let b = temp_file(&content_b);

    assert_eq!(fingerprint::partial_hash(&a).unwrap(), fingerprint::partial_hash(&b).unwrap());
    assert_ne!(fingerprint::full_hash(&a).unwrap(), fingerprint::full_hash(&b).unwrap());

    // Differences in the first or last 4 KiB change the partial hash.
    let mut content_c = content_a.clone();
    content_c[100] = 9;
    let c = temp_file(&content_c);
    assert_ne!(fingerprint::partial_hash(&a).unwrap(), fingerprint::partial_hash(&c).unwrap());

    let mut content_d = content_a.clone();
    let len = content_d.len();
    content_d[len - 100] = 9;
    let d = temp_file(&content_d);
    assert_ne!(fingerprint::partial_hash(&a).unwrap(), fingerprint::partial_hash(&d).unwrap());

    for p in [a, b, c, d] {
        std::fs::remove_file(p).unwrap();
    }
}

#[test]
fn test_partial_hash_of_small_file() {
    let small = temp_file(b"tiny");
    assert!(fingerprint::partial_hash(&small).is_ok());
    std::fs::remove_file(small).unwrap();
}

// ── Stat fields ───────────────────────────────────────────────────────────────

#[test]
fn test_stat_fields_for_a_file() {
    let path = temp_file(b"0123456789");
    let fields = fs_meta::stat_fields(&path).unwrap();
    let get = |name: &str| {
        fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("missing field {name}"))
            .value
            .clone()
    };

    assert_eq!(get("mfr_type"), Value::String("file".into()));
    assert_eq!(get("mfr_size"), Value::Int(10));
    match get("mfr_mtime") {
        Value::DateTime(s) => {
            assert!(s.ends_with('Z') && s.contains('T'), "ISO-8601 expected: {s}");
            assert!(s.starts_with("20"), "plausible year expected: {s}");
        }
        other => panic!("mfr_mtime must be a DateTime, got {other:?}"),
    }
    #[cfg(unix)]
    {
        match get("mfr_permissions") {
            Value::String(p) => assert!(
                p.len() == 4 && p.chars().all(|c| c.is_digit(8)),
                "octal string expected: {p}"
            ),
            other => panic!("mfr_permissions must be a String, got {other:?}"),
        }
        assert!(matches!(get("mfr_uid"), Value::Int(_)));
        assert!(matches!(get("mfr_gid"), Value::Int(_)));
    }

    std::fs::remove_file(path).unwrap();
}

#[test]
fn test_stat_fields_for_a_directory() {
    let dir = std::env::temp_dir().join(format!("metafolder_statdir_{}", Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let fields = fs_meta::stat_fields(&dir).unwrap();
    let mfr_type = fields.iter().find(|f| f.name == "mfr_type").unwrap();
    assert_eq!(mfr_type.value, Value::String("dir".into()));
    std::fs::remove_dir(dir).unwrap();
}

#[test]
fn test_iso8601_known_timestamps() {
    use std::time::{Duration, UNIX_EPOCH};
    assert_eq!(fs_meta::iso8601(UNIX_EPOCH), "1970-01-01T00:00:00Z");
    // 2024-02-29T12:34:56Z (leap year) == 1709210096.
    assert_eq!(
        fs_meta::iso8601(UNIX_EPOCH + Duration::from_secs(1_709_210_096)),
        "2024-02-29T12:34:56Z"
    );
    // 2000-01-01T00:00:00Z == 946684800.
    assert_eq!(
        fs_meta::iso8601(UNIX_EPOCH + Duration::from_secs(946_684_800)),
        "2000-01-01T00:00:00Z"
    );
}
