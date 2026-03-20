use metafolder_core::entry::Metadata;

/// Returns a copy of the entry with only the requested fields included.
/// If `fields` is None, all fields are kept.
pub fn filter_entry(mut entry: Metadata, fields: Option<&[String]>) -> Metadata {
    if let Some(names) = fields {
        entry.fields.retain(|f| names.contains(&f.name));
    }
    entry
}
