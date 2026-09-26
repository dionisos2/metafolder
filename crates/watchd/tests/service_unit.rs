//! The systemd unit is part of the broker's security: it is what grants the two
//! capabilities, and what takes everything else away. These tests hold it to
//! that (docs/watcher-fanotify.md "The broker").

use std::collections::HashMap;
use std::path::PathBuf;

/// `key → values` of the `[Service]` section (a key may repeat).
fn service() -> HashMap<String, Vec<String>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/metafolder-watchd.service");
    let text = std::fs::read_to_string(path).expect("the unit file");
    let mut section = String::new();
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            section = line.to_string();
            continue;
        }
        if section == "[Service]" {
            if let Some((key, value)) = line.split_once('=') {
                out.entry(key.to_string()).or_default().push(value.to_string());
            }
        }
    }
    out
}

fn one(unit: &HashMap<String, Vec<String>>, key: &str) -> String {
    unit.get(key).and_then(|v| v.last()).cloned().unwrap_or_else(|| panic!("no {key}="))
}

#[test]
fn test_the_broker_runs_as_its_own_user_with_two_capabilities_and_no_more() {
    let unit = service();
    // Not root: the two capabilities are the whole grant.
    let user = one(&unit, "User");
    assert!(!user.is_empty() && user != "root" && user != "0");
    assert_eq!(one(&unit, "AmbientCapabilities"), "CAP_SYS_ADMIN CAP_DAC_READ_SEARCH");
    assert_eq!(one(&unit, "CapabilityBoundingSet"), "CAP_SYS_ADMIN CAP_DAC_READ_SEARCH");
    assert_eq!(one(&unit, "NoNewPrivileges"), "true");
}

#[test]
fn test_the_broker_has_no_network() {
    let unit = service();
    assert_eq!(one(&unit, "RestrictAddressFamilies"), "AF_UNIX");
    assert_eq!(one(&unit, "PrivateNetwork"), "true");
    assert_eq!(one(&unit, "IPAddressDeny"), "any");
}

#[test]
fn test_the_broker_is_confined_and_bounded() {
    let unit = service();
    for (key, value) in [
        ("ProtectSystem", "strict"),
        ("ProtectHome", "read-only"),
        ("ProtectKernelModules", "true"),
        ("ProtectKernelTunables", "true"),
        ("ProtectKernelLogs", "true"),
        ("ProtectControlGroups", "true"),
        ("ProtectClock", "true"),
        ("RestrictNamespaces", "true"),
        ("RestrictSUIDSGID", "true"),
        ("LockPersonality", "true"),
        ("MemoryDenyWriteExecute", "true"),
        ("SystemCallArchitectures", "native"),
    ] {
        assert_eq!(one(&unit, key), value, "{key}");
    }
    assert!(one(&unit, "SystemCallFilter").contains("@system-service"));
    // Resource ceilings on what any local user can make it hold.
    for key in ["MemoryMax", "TasksMax", "LimitNOFILE"] {
        one(&unit, key);
    }
}

/// The broker must see paths as the host does: a private /tmp would hide the
/// real one, and a repository under /tmp would be resolved to nothing (or to
/// the wrong filesystem).
#[test]
fn test_the_broker_sees_the_hosts_tmp() {
    let unit = service();
    assert_ne!(unit.get("PrivateTmp").and_then(|v| v.last()).map(String::as_str), Some("true"));
    assert!(unit.get("DynamicUser").is_none(), "DynamicUser implies PrivateTmp");
}
