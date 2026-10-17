//! The per-repository embedded-metadata extraction map (doc "The metadata map").
//!
//! Each [`FieldMapping`] maps an ordered list of native source keys onto one
//! reserved `mfr_meta_<name>` field of a given [`FieldType`]. The map lives at
//! `.metafolder/config/metadata-map.toml`, in the repository's configuration
//! (doc "Repository configuration"), which the daemon only reads: `mf repo
//! init` installs it from the defaults. A missing or malformed file fails the
//! load of *that* repository only — no copy of the default is built in (doc
//! "No runtime fallback").

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

/// The parsed extraction map. The empty one (`Default`) extracts nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetadataMap {
    pub fields: Vec<FieldMapping>,
}

/// The file name inside `.metafolder/config/`.
const FILE_NAME: &str = "metadata-map.toml";

/// The shipped default, for the tests that need a realistic map.
#[cfg(test)]
pub const DEFAULT: &str = include_str!("../default-config/repo-default/metadata-map.toml");

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

    /// Reads the map from `<metafolder_dir>/config/metadata-map.toml`. A
    /// missing file is an error naming the cure: it is installed with the
    /// repository's configuration, and nothing writes it back.
    pub fn load(metafolder_dir: &Path) -> Result<Self> {
        let path = metafolder_dir.join(crate::repo::CONFIG_DIR).join(FILE_NAME);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
                "{} is missing: a repository's configuration is installed by `mf repo init`, \
                 and brought up to date by `mf repo update-config {}`",
                path.display(),
                metafolder_dir.display()
            ),
            Err(e) => return Err(e).with_context(|| format!("failed to read {path:?}")),
        };
        Self::parse(&text).with_context(|| format!("in {path:?}"))
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

    /// A `.metafolder/` under the tests' parent, removed on drop.
    struct Dir(std::path::PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join("metafolder-tests")
                .join(format!("mf-metamap-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join(crate::repo::CONFIG_DIR)).unwrap();
            Dir(dir)
        }
        fn write(&self, text: &str) {
            std::fs::write(self.0.join(crate::repo::CONFIG_DIR).join(FILE_NAME), text).unwrap();
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn load_reads_the_file_in_the_configuration_directory() {
        let dir = Dir::new("read");
        dir.write(DEFAULT);
        assert_eq!(MetadataMap::load(&dir.0).unwrap(), MetadataMap::parse(DEFAULT).unwrap());
    }

    #[test]
    fn load_of_a_missing_file_is_an_error_and_writes_nothing() {
        let dir = Dir::new("missing");
        let err = format!("{:#}", MetadataMap::load(&dir.0).unwrap_err());
        assert!(err.contains("mf repo update-config"), "{err}");
        assert!(!dir.0.join(crate::repo::CONFIG_DIR).join(FILE_NAME).exists());
    }

    #[test]
    fn load_errors_on_malformed_file() {
        let dir = Dir::new("bad");
        dir.write("this is : not [ valid toml");
        let err = MetadataMap::load(&dir.0).unwrap_err();
        assert!(format!("{err:#}").contains(FILE_NAME), "{err:#}");
    }
}
