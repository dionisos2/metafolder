//! Reserved field names (spec-data-model "Reserved fields"):
//! - `mfr_*` are written by the daemon; user writes require `force`.
//! - `mf_*` are read by the daemon; written freely, but unknown names are
//!   rejected to prevent typos from silently having no effect.

/// `mf_*` fields the daemon knows about. `mf_schema` is defined by the user
/// schema feature (spec-schema).
const KNOWN_MF_FIELDS: &[&str] = &["mf_watch", "mf_ignore", "mf_schema"];

/// Checks whether a user write to `field_name` is allowed.
pub fn check_writable(field_name: &str, force: bool) -> Result<(), String> {
    if field_name.starts_with("mfr_") {
        if force {
            return Ok(());
        }
        return Err(format!(
            "field '{field_name}' is reserved (mfr_*); pass \"force\": true to override"
        ));
    }
    if field_name.starts_with("mf_") && !KNOWN_MF_FIELDS.contains(&field_name) {
        return Err(format!("unknown reserved field '{field_name}' (mf_* names are restricted)"));
    }
    Ok(())
}
