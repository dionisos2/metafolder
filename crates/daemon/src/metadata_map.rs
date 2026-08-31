//! The per-repository embedded-metadata extraction map (spec-platform
//! "Embedded metadata extraction" → "Configuration").
//!
//! Each [`FieldMapping`] maps an ordered list of native source keys onto one
//! reserved `mfr_meta_<name>` field of a given [`FieldType`]. The map lives at
//! `.metafolder/metadata-map.toml` — ordinary trackable content, following the
//! user-schema model, **not** the git-backed `~/.config/` configs. A default
//! (baked into the daemon with `include_str!`) seeds the file at `mf init` and
//! self-heals it if it is missing at load; a malformed file fails the load of
//! *that* repository only.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// The `Value` type a mapped field is coerced to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    String,
    Int,
    Float,
    DateTime,
}

/// One mapping entry: `from` (ordered source keys) → `mfr_meta_<name>` : `ty`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FieldMapping {
    /// The `mfr_meta_` suffix (the stored field is `mfr_meta_<name>`).
    pub name: String,
    #[serde(rename = "type")]
    pub ty: FieldType,
    /// Native source keys, tried in order; the first present one wins.
    pub from: Vec<String>,
}

impl FieldMapping {
    /// The reserved field name this entry writes (`mfr_meta_<name>`).
    pub fn field_name(&self) -> String {
        format!("mfr_meta_{}", self.name)
    }
}

/// The parsed extraction map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataMap {
    pub fields: Vec<FieldMapping>,
}

/// The file name inside `.metafolder/`.
const FILE_NAME: &str = "metadata-map.toml";

/// The default map baked into the daemon; seeds new repos and self-heals a
/// missing file.
pub const DEFAULT: &str = include_str!("default-metadata-map.toml");

#[derive(Deserialize)]
struct RawMap {
    #[serde(default)]
    field: Vec<FieldMapping>,
}

impl MetadataMap {
    /// Parses the TOML document. Rejects an empty `name` and a `from` with no
    /// sources (both would be silently useless).
    pub fn parse(toml_text: &str) -> Result<Self> {
        let raw: RawMap = toml::from_str(toml_text).context("invalid metadata-map.toml")?;
        for f in &raw.field {
            if f.name.trim().is_empty() {
                bail!("metadata-map.toml: a [[field]] has an empty name");
            }
            if f.from.is_empty() {
                bail!("metadata-map.toml: field '{}' has no source keys (`from` is empty)", f.name);
            }
        }
        Ok(MetadataMap { fields: raw.field })
    }

    /// Reads the map from `<metafolder_dir>/metadata-map.toml`, seeding the file
    /// with [`DEFAULT`] when it is absent (self-heal — the file is
    /// repository-owned data, not user config, so writing a default does not
    /// break spec-config's "No runtime fallback"). A malformed file is an error.
    pub fn load_or_seed(metafolder_dir: &Path) -> Result<Self> {
        let path = metafolder_dir.join(FILE_NAME);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::write(&path, DEFAULT)
                    .with_context(|| format!("failed to seed {path:?}"))?;
                DEFAULT.to_string()
            }
            Err(e) => return Err(e).with_context(|| format!("failed to read {path:?}")),
        };
        Self::parse(&text).with_context(|| format!("in {path:?}"))
    }

    /// Writes [`DEFAULT`] to `<metafolder_dir>/metadata-map.toml` if it does not
    /// already exist (used by repo init). Best-effort: overwriting is never done.
    pub fn seed_file(metafolder_dir: &Path) -> Result<()> {
        let path = metafolder_dir.join(FILE_NAME);
        if !path.exists() {
            std::fs::write(&path, DEFAULT).with_context(|| format!("failed to seed {path:?}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_map() {
        let map = MetadataMap::parse(
            r#"
            [[field]]
            name = "artist"
            type = "string"
            from = ["TrackArtist", "Artist"]

            [[field]]
            name = "duration"
            type = "int"
            from = ["@duration_ms"]
            "#,
        )
        .unwrap();
        assert_eq!(map.fields.len(), 2);
        assert_eq!(map.fields[0].name, "artist");
        assert_eq!(map.fields[0].ty, FieldType::String);
        assert_eq!(map.fields[0].from, vec!["TrackArtist", "Artist"]);
        assert_eq!(map.fields[0].field_name(), "mfr_meta_artist");
        assert_eq!(map.fields[1].ty, FieldType::Int);
    }

    #[test]
    fn rejects_unknown_type() {
        let err = MetadataMap::parse(
            r#"
            [[field]]
            name = "x"
            type = "blob"
            from = ["A"]
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("metadata-map"), "{err}");
    }

    #[test]
    fn rejects_empty_name() {
        let err = MetadataMap::parse(
            r#"
            [[field]]
            name = ""
            type = "string"
            from = ["A"]
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("empty name"), "{err}");
    }

    #[test]
    fn rejects_empty_from() {
        let err = MetadataMap::parse(
            r#"
            [[field]]
            name = "x"
            type = "string"
            from = []
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no source keys"), "{err}");
    }

    #[test]
    fn baked_default_parses_and_covers_known_fields() {
        let map = MetadataMap::parse(DEFAULT).unwrap();
        let names: Vec<&str> = map.fields.iter().map(|f| f.name.as_str()).collect();
        for expected in ["title", "artist", "album_artist", "album", "date", "duration"] {
            assert!(names.contains(&expected), "default map missing {expected}");
        }
    }

    #[test]
    fn load_or_seed_writes_default_when_absent_then_reads_it() {
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("mf-metamap-seed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        assert!(!path.exists());

        let map = MetadataMap::load_or_seed(&dir).unwrap();
        assert!(path.exists(), "file should have been seeded");
        assert_eq!(map, MetadataMap::parse(DEFAULT).unwrap());

        // Second call reads the existing file (does not error, same result).
        let again = MetadataMap::load_or_seed(&dir).unwrap();
        assert_eq!(again, map);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_or_seed_errors_on_malformed_file() {
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("mf-metamap-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE_NAME), "this is : not [ valid toml").unwrap();

        let err = MetadataMap::load_or_seed(&dir).unwrap_err();
        assert!(
            err.to_string().contains("metadata-map.toml") || format!("{err:#}").contains(FILE_NAME),
            "{err:#}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
