//! Embedded-metadata extraction (spec-platform "Embedded metadata extraction").
//!
//! Reads the *payload* metadata of media files — an audio file's
//! artist/album/title, an image's capture date/camera/GPS — into reserved
//! `mfr_meta_*` fields, driven by the per-repo [`MetadataMap`].
//!
//! The parsers are **pure-Rust and memory-safe** (`lofty` for audio, `nom-exif`
//! for images): a crafted file can at worst make a parser return an error or
//! wrong values (which we treat as "no metadata"), never the passive
//! code-execution vector that the C media decoders behind `ffmpeg` / the WebView
//! are. That is precisely why this needs no subprocess sandbox in the daemon.

use std::collections::HashMap;
use std::path::Path;

use metafolder_core::date;
use metafolder_core::metarecord::{Field, Value};

use crate::metadata_map::{FieldType, MetadataMap};

/// A value read from a native source key, before coercion to the mapped type.
#[derive(Debug, Clone)]
enum SourceValue {
    Text(String),
    Int(i64),
    Float(f64),
    /// An exact instant already resolved to Unix-ms (EXIF `DateTimeOriginal`).
    DateTimeMs(i64),
}

/// Extracts the mapped `mfr_meta_*` fields of `abs` using `map`.
pub fn extract(abs: &Path, map: &MetadataMap) -> Vec<Field> {
    apply_map(&collect_sources(abs), map)
}

/// Applies the mapping to an already-collected source set. For each entry the
/// **first** present source that coerces into the entry's type wins; a present
/// but non-coercible source is skipped and the next one tried (spec-platform).
fn apply_map(sources: &HashMap<String, SourceValue>, map: &MetadataMap) -> Vec<Field> {
    let mut fields = Vec::new();
    for m in &map.fields {
        for key in &m.from {
            let Some(sv) = sources.get(key) else { continue };
            if let Some(value) = coerce(sv, m.ty) {
                fields.push(Field::new(m.field_name(), value));
                break;
            }
        }
    }
    fields
}

/// Coerces a source value into the mapped [`FieldType`], or `None` when it does
/// not represent a value of that type.
fn coerce(sv: &SourceValue, ty: FieldType) -> Option<Value> {
    match ty {
        FieldType::String => Some(Value::String(match sv {
            SourceValue::Text(s) => s.clone(),
            SourceValue::Int(n) => n.to_string(),
            SourceValue::Float(f) => f.to_string(),
            SourceValue::DateTimeMs(ms) => date::iso8601_from_ms(*ms),
        })),
        FieldType::Int => match sv {
            SourceValue::Int(n) => Some(Value::Int(*n)),
            SourceValue::Float(f) => Some(Value::Int(*f as i64)),
            SourceValue::Text(s) => parse_leading_int(s).map(Value::Int),
            SourceValue::DateTimeMs(_) => None,
        },
        FieldType::Float => match sv {
            SourceValue::Float(f) => Some(Value::Float(*f)),
            SourceValue::Int(n) => Some(Value::Float(*n as f64)),
            SourceValue::Text(s) => s.trim().parse::<f64>().ok().map(Value::Float),
            SourceValue::DateTimeMs(_) => None,
        },
        FieldType::DateTime => match sv {
            SourceValue::DateTimeMs(ms) => Some(Value::DateTime(*ms)),
            SourceValue::Text(s) => date::iso_to_ms(s.trim()).map(Value::DateTime),
            _ => None,
        },
    }
}

/// Parses the leading integer of a string, tolerating a trailing `/total`
/// (audio track/disc tags are often `"3/10"`).
fn parse_leading_int(s: &str) -> Option<i64> {
    s.trim().split('/').next()?.trim().parse().ok()
}

/// Collects `(source key → value)` for a file from every backend. A backend
/// that cannot parse the file contributes nothing (treated as "no metadata").
fn collect_sources(abs: &Path) -> HashMap<String, SourceValue> {
    let mut out = HashMap::new();
    collect_audio(abs, &mut out);
    collect_exif(abs, &mut out);
    out
}

/// Audio tags and stream properties via `lofty` (ID3 / Vorbis / MP4).
fn collect_audio(abs: &Path, out: &mut HashMap<String, SourceValue>) {
    use lofty::prelude::*;

    let Ok(tagged) = lofty::read_from_path(abs) else {
        return;
    };
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        // The lofty normalised item keys we expose under their canonical name;
        // these already unify the per-format spellings (ID3 TPE1 / Vorbis
        // ARTIST / MP4 ©ART all map to `ItemKey::TrackArtist`).
        const TAGS: &[(ItemKey, &str)] = &[
            (ItemKey::TrackTitle, "TrackTitle"),
            (ItemKey::TrackArtist, "TrackArtist"),
            (ItemKey::AlbumTitle, "AlbumTitle"),
            (ItemKey::AlbumArtist, "AlbumArtist"),
            (ItemKey::Genre, "Genre"),
            (ItemKey::RecordingDate, "RecordingDate"),
            (ItemKey::Year, "Year"),
            (ItemKey::TrackNumber, "TrackNumber"),
            (ItemKey::DiscNumber, "DiscNumber"),
            (ItemKey::Comment, "Comment"),
        ];
        for (key, name) in TAGS {
            if let Some(v) = tag.get_string(*key) {
                if !v.is_empty() {
                    out.insert((*name).to_string(), SourceValue::Text(v.to_string()));
                }
            }
        }
    }

    let props = tagged.properties();
    let ms = props.duration().as_millis();
    if ms > 0 {
        out.insert("@duration_ms".to_string(), SourceValue::Int(ms as i64));
    }
    if let Some(b) = props.overall_bitrate() {
        out.insert("@bitrate".to_string(), SourceValue::Int(b as i64));
    }
    if let Some(sr) = props.sample_rate() {
        out.insert("@sample_rate".to_string(), SourceValue::Int(sr as i64));
    }
    if let Some(ch) = props.channels() {
        out.insert("@channels".to_string(), SourceValue::Int(ch as i64));
    }
}

/// Image EXIF via `nom-exif`. Dates are kept as exact instants; GPS is exposed
/// as signed decimal degrees under `GPSLatitude`/`GPSLongitude`.
fn collect_exif(abs: &Path, out: &mut HashMap<String, SourceValue>) {
    use nom_exif::{EntryValue, ExifTag};

    let Ok(exif) = nom_exif::read_exif(abs) else {
        return;
    };
    const TAGS: &[(ExifTag, &str)] = &[
        (ExifTag::DateTimeOriginal, "DateTimeOriginal"),
        (ExifTag::Make, "Make"),
        (ExifTag::Model, "Model"),
        (ExifTag::Orientation, "Orientation"),
        (ExifTag::ImageWidth, "ImageWidth"),
        (ExifTag::ImageHeight, "ImageHeight"),
    ];
    for (tag, name) in TAGS {
        if let Some(v) = exif.get(*tag) {
            let sv = match v {
                // Keep the exact instant; its `Display` is rfc3339-with-offset,
                // which the text date parser cannot read back.
                EntryValue::DateTime(dt) => SourceValue::DateTimeMs(dt.timestamp_millis()),
                other => SourceValue::Text(other.to_string()),
            };
            out.insert((*name).to_string(), sv);
        }
    }
    if let Some(gps) = exif.gps_info() {
        if let Some(lat) = gps.latitude_decimal() {
            out.insert("GPSLatitude".to_string(), SourceValue::Float(lat));
        }
        if let Some(lon) = gps.longitude_decimal() {
            out.insert("GPSLongitude".to_string(), SourceValue::Float(lon));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(toml: &str) -> MetadataMap {
        MetadataMap::parse(toml).unwrap()
    }

    fn find<'a>(fields: &'a [Field], name: &str) -> Option<&'a Value> {
        fields.iter().find(|f| f.name == name).map(|f| &f.value)
    }

    #[test]
    fn first_present_source_wins_and_maps_to_prefixed_name() {
        let mut sources = HashMap::new();
        sources.insert("Artist".to_string(), SourceValue::Text("fallback".into()));
        sources.insert("TrackArtist".to_string(), SourceValue::Text("primary".into()));
        let fields = apply_map(
            &sources,
            &map(r#"[[field]]
                name = "artist"
                type = "string"
                from = ["TrackArtist", "Artist"]"#),
        );
        assert_eq!(find(&fields, "mfr_meta_artist"), Some(&Value::String("primary".into())));
    }

    #[test]
    fn non_coercible_source_is_skipped_for_the_next_one() {
        // First source present but not an int; second source is a valid int.
        let mut sources = HashMap::new();
        sources.insert("A".to_string(), SourceValue::Text("not a number".into()));
        sources.insert("B".to_string(), SourceValue::Int(7));
        let fields = apply_map(
            &sources,
            &map(r#"[[field]]
                name = "track"
                type = "int"
                from = ["A", "B"]"#),
        );
        assert_eq!(find(&fields, "mfr_meta_track"), Some(&Value::Int(7)));
    }

    #[test]
    fn track_tag_with_total_parses_the_leading_int() {
        let mut sources = HashMap::new();
        sources.insert("TrackNumber".to_string(), SourceValue::Text("3/10".into()));
        let fields = apply_map(
            &sources,
            &map(r#"[[field]]
                name = "track"
                type = "int"
                from = ["TrackNumber"]"#),
        );
        assert_eq!(find(&fields, "mfr_meta_track"), Some(&Value::Int(3)));
    }

    #[test]
    fn bare_year_text_becomes_january_first() {
        let mut sources = HashMap::new();
        sources.insert("Year".to_string(), SourceValue::Text("1998".into()));
        let fields = apply_map(
            &sources,
            &map(r#"[[field]]
                name = "date"
                type = "datetime"
                from = ["Year"]"#),
        );
        // 1998-01-01T00:00:00Z
        assert_eq!(find(&fields, "mfr_meta_date"), Some(&Value::DateTime(883_612_800_000)));
    }

    #[test]
    fn datetime_ms_source_passes_through() {
        let mut sources = HashMap::new();
        sources.insert("DateTimeOriginal".to_string(), SourceValue::DateTimeMs(1_234_567_890_000));
        let fields = apply_map(
            &sources,
            &map(r#"[[field]]
                name = "date"
                type = "datetime"
                from = ["DateTimeOriginal", "Year"]"#),
        );
        assert_eq!(find(&fields, "mfr_meta_date"), Some(&Value::DateTime(1_234_567_890_000)));
    }

    #[test]
    fn absent_sources_produce_no_fields() {
        let fields = apply_map(&HashMap::new(), &MetadataMap::parse(crate::metadata_map::DEFAULT).unwrap());
        assert!(fields.is_empty());
    }

    #[test]
    fn collecting_a_non_media_file_yields_nothing() {
        let dir = std::env::temp_dir().join(format!("mf-meta-nonmedia-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("plain.txt");
        std::fs::write(&f, b"just some text, no tags").unwrap();
        assert!(collect_sources(&f).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
