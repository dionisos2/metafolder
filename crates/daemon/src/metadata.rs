//! Embedded-metadata extraction (doc "Embedded metadata").
//!
//! Reads the *payload* metadata of media files — an audio file's
//! artist/album/title, an image's capture date/camera/GPS — into reserved
//! `mfr_meta_*` fields, driven by the per-repo [`MetadataMap`].
//!
//! The parsers are **pure-Rust and memory-safe** (`lofty` for audio,
//! `kamadak-exif` for images): a crafted file can at worst make a parser return an error or
//! wrong values (which we treat as "no metadata"), never the passive
//! code-execution vector that the C media decoders behind `ffmpeg` / the WebView
//! are. That is precisely why this needs no subprocess sandbox in the daemon.
//!
//! Memory-safe is not the same as well-behaved, though, and the two remaining
//! ways a crafted file bites are bounded here: a *panic* is caught per backend
//! ([`guarded`]), and an *allocation* sized by the file is capped by lofty's
//! global allocation limit (16 MiB by default) and, for kamadak-exif, by what
//! the file really holds (a segment or box *claiming* 2 GiB in a small file ends
//! at the end of the file, not in a 2 GiB buffer). A failed allocation aborts
//! the daemon, past any `catch_unwind`: `tests/metadata_exif.rs` measures what
//! damaged and crafted images make the parser hold.

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
/// but non-coercible source is skipped and the next one tried (doc "The metadata map").
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
    let mut out = guarded(abs, "audio", |out| collect_audio(abs, out));
    out.extend(guarded(abs, "exif", |out| collect_exif(abs, out)));
    out
}

/// Runs one backend, turning a panic into "no metadata". Memory-safe is not
/// panic-free: a parser may `assert!` on sizes a crafted file controls, and a
/// panic here would fail the whole reconcile — on every run, since the file
/// stays in the tree. What a panicked backend had collected is dropped (it
/// stopped half-way); the file's path is reported.
fn guarded(
    abs: &Path,
    backend: &str,
    collect: impl FnOnce(&mut HashMap<String, SourceValue>),
) -> HashMap<String, SourceValue> {
    let mut out = HashMap::new();
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| collect(&mut out)));
    if ran.is_err() {
        crate::diagnostics::warn(
            "metadata",
            format!("the {backend} metadata parser failed on {}: skipped", abs.display()),
        );
        return HashMap::new();
    }
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

/// Image EXIF via `kamadak-exif` (JPEG, TIFF and the RAW formats built on it,
/// HEIF, PNG, WebP). A capture date with its offset is kept as an exact
/// instant, one without as text; GPS is exposed as signed decimal degrees under
/// `GPSLatitude`/`GPSLongitude`.
fn collect_exif(abs: &Path, out: &mut HashMap<String, SourceValue>) {
    use exif::{In, Tag, Value as V};

    let Ok(file) = std::fs::File::open(abs) else {
        return;
    };
    let Ok(exif) = exif::Reader::new().read_from_container(&mut std::io::BufReader::new(file))
    else {
        return;
    };
    let field = |tag| exif.get_field(tag, In::PRIMARY).map(|f| &f.value);
    let ascii = |tag| match field(tag)? {
        V::Ascii(parts) => parts.first().map(|p| p.as_slice()),
        _ => None,
    };

    for (tag, name) in [(Tag::Make, "Make"), (Tag::Model, "Model")] {
        if let Some(bytes) = ascii(tag) {
            let s = String::from_utf8_lossy(bytes);
            let s = s.trim_end_matches(['\0', ' ']);
            if !s.is_empty() {
                out.insert(name.to_string(), SourceValue::Text(s.to_string()));
            }
        }
    }
    for (tag, name) in [
        (Tag::Orientation, "Orientation"),
        (Tag::ImageWidth, "ImageWidth"),
        (Tag::ImageLength, "ImageHeight"),
    ] {
        if let Some(n) = field(tag).and_then(|v| v.get_uint(0)) {
            out.insert(name.to_string(), SourceValue::Text(n.to_string()));
        }
    }
    if let Some(sv) = ascii(Tag::DateTimeOriginal)
        .and_then(|raw| capture_date(raw, ascii(Tag::OffsetTimeOriginal)))
    {
        out.insert("DateTimeOriginal".to_string(), sv);
    }
    for (tag, sign_tag, negative, name) in [
        (Tag::GPSLatitude, Tag::GPSLatitudeRef, b'S', "GPSLatitude"),
        (Tag::GPSLongitude, Tag::GPSLongitudeRef, b'W', "GPSLongitude"),
    ] {
        let (Some(V::Rational(dms)), Some(sign)) = (field(tag), ascii(sign_tag)) else {
            continue;
        };
        let Some(deg) = dms
            .iter()
            .zip([1.0, 60.0, 3600.0])
            .map(|(r, div)| (r.denom != 0).then(|| r.to_f64() / div))
            .sum::<Option<f64>>()
        else {
            continue;
        };
        let deg = if sign.first() == Some(&negative) { -deg } else { deg };
        out.insert(name.to_string(), SourceValue::Float(deg));
    }
}

/// An EXIF capture date: an instant when `offset` names its time zone, the
/// text `YYYY-MM-DD HH:MM:SS` when it does not (a wall clock time names no
/// instant), nothing when it is blank or not a date.
fn capture_date(raw: &[u8], offset: Option<&[u8]>) -> Option<SourceValue> {
    let mut dt = exif::DateTime::from_ascii(raw).ok()?;
    let valid = (1..=12).contains(&dt.month)
        && (1..=31).contains(&dt.day)
        && dt.hour < 24
        && dt.minute < 60
        && dt.second < 61;
    if !valid {
        return None;
    }
    if offset.is_some_and(|o| dt.parse_offset(o).is_ok()) {
        let days = date::days_from_civil(dt.year.into(), dt.month.into(), dt.day.into());
        let secs = days * 86_400
            + i64::from(dt.hour) * 3600
            + i64::from(dt.minute) * 60
            + i64::from(dt.second)
            - i64::from(dt.offset?) * 60;
        return Some(SourceValue::DateTimeMs(secs * 1000));
    }
    Some(SourceValue::Text(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(toml: &str) -> MetadataMap {
        MetadataMap::parse(toml).unwrap()
    }

    /// A parser that panics on a crafted file (one that `assert!`s on box
    /// sizes) must cost that file its metadata, not the whole reconcile — else
    /// one file in the tree makes every reconcile of the repository fail.
    #[test]
    fn test_a_panicking_backend_contributes_nothing_and_the_others_still_run() {
        let got = guarded(Path::new("/r/evil.heic"), "exif", |out| {
            out.insert("Make".to_string(), SourceValue::Text("half".into()));
            panic!("crafted box size");
        });
        assert!(got.is_empty(), "a panicked backend's partial output must be dropped");
        let got = guarded(Path::new("/r/fine.mp3"), "audio", |out| {
            out.insert("Artist".to_string(), SourceValue::Text("x".into()));
        });
        assert_eq!(got.len(), 1);
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
        let fields =
            apply_map(&HashMap::new(), &MetadataMap::parse(crate::metadata_map::DEFAULT).unwrap());
        assert!(fields.is_empty());
    }

    #[test]
    fn collecting_a_non_media_file_yields_nothing() {
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("mf-meta-nonmedia-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("plain.txt");
        std::fs::write(&f, b"just some text, no tags").unwrap();
        assert!(collect_sources(&f).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
