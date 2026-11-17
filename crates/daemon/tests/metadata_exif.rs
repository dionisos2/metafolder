//! Image EXIF extraction on real files (doc "Embedded metadata"): what a JPEG's
//! EXIF gives, and what a crafted one cannot do — panic the parser, or make it
//! allocate out of proportion to the file. A failed allocation aborts the whole
//! daemon, which no `catch_unwind` stops, so the allocations are measured: this
//! binary counts them, per thread, with its own global allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::Path;

use metafolder_core::date;
use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::metadata::extract;
use metafolder_daemon::metadata_map::MetadataMap;

// ---- allocation peak, per thread --------------------------------------------

struct Counting;

thread_local! {
    static LIVE: Cell<usize> = const { Cell::new(0) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
}

fn grew(n: usize) {
    let _ = LIVE.try_with(|live| {
        let now = live.get() + n;
        live.set(now);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
    });
}

fn shrank(n: usize) {
    let _ = LIVE.try_with(|live| live.set(live.get().saturating_sub(n)));
}

// SAFETY: every call is forwarded unchanged to `System`; the counting
// beside it only touches const-initialised thread locals, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        // SAFETY: the caller upholds `GlobalAlloc`\'s contract, passed on as is.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        shrank(layout.size());
        // SAFETY: the caller upholds `GlobalAlloc`\'s contract, passed on as is.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        // SAFETY: the caller upholds `GlobalAlloc`\'s contract, passed on as is.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        grew(new_size);
        shrank(layout.size());
        // SAFETY: the caller upholds `GlobalAlloc`\'s contract, passed on as is.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

thread_local! {
    /// Panics seen by this thread. `extract` guards each backend, so a parser's
    /// panic never reaches the test: it is counted by a hook instead, per
    /// thread so that another test's failing assertion is not counted.
    static PANICS: Cell<usize> = const { Cell::new(0) };
}

fn count_panics() {
    static HOOKED: std::sync::Once = std::sync::Once::new();
    HOOKED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = PANICS.try_with(|n| n.set(n.get() + 1));
            previous(info);
        }));
    });
}

/// Extracts `abs` with `map`, returning the fields and the peak of bytes this
/// thread held during the call, above what it held before.
fn measured(abs: &Path, map: &MetadataMap) -> (Vec<Field>, usize) {
    let base = LIVE.with(Cell::get);
    PEAK.with(|p| p.set(base));
    let fields = extract(abs, map);
    (fields, PEAK.with(Cell::get) - base)
}

// ---- a TIFF builder ----------------------------------------------------------

const ASCII: u16 = 2;
const SHORT: u16 = 3;
const LONG: u16 = 4;
const RATIONAL: u16 = 5;

enum Payload {
    /// At most four bytes, in the entry itself.
    Inline(u32),
    /// Out of line, after the IFDs.
    Data(Vec<u8>),
    /// The offset of another IFD of the same file, by its index.
    Ifd(usize),
}

struct Entry(u16, u16, u32, Payload);

fn ascii(tag: u16, s: &str) -> Entry {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    let n = bytes.len() as u32;
    if bytes.len() <= 4 {
        bytes.resize(4, 0);
        Entry(tag, ASCII, n, Payload::Inline(u32::from_le_bytes(bytes.try_into().unwrap())))
    } else {
        Entry(tag, ASCII, n, Payload::Data(bytes))
    }
}

fn rationals(tag: u16, parts: &[(u32, u32)]) -> Entry {
    let bytes =
        parts.iter().flat_map(|(n, d)| [n.to_le_bytes(), d.to_le_bytes()].concat()).collect();
    Entry(tag, RATIONAL, parts.len() as u32, Payload::Data(bytes))
}

/// A little-endian TIFF of `ifds`, IFD 0 first, each IFD with no next one.
fn tiff(ifds: &[Vec<Entry>]) -> Vec<u8> {
    let mut at = Vec::new();
    let mut next = 8u32;
    for ifd in ifds {
        at.push(next);
        next += 2 + 12 * ifd.len() as u32 + 4;
    }
    let mut out = b"II*\0".to_vec();
    out.extend_from_slice(&8u32.to_le_bytes());
    let mut data = Vec::new();
    for ifd in ifds {
        out.extend_from_slice(&(ifd.len() as u16).to_le_bytes());
        for Entry(tag, ty, count, payload) in ifd {
            let value = match payload {
                Payload::Inline(v) => *v,
                Payload::Ifd(i) => at[*i],
                Payload::Data(bytes) => {
                    let offset = next + data.len() as u32;
                    data.extend_from_slice(bytes);
                    offset
                }
            };
            for part in [
                &tag.to_le_bytes()[..],
                &ty.to_le_bytes(),
                &count.to_le_bytes(),
                &value.to_le_bytes(),
            ] {
                out.extend_from_slice(part);
            }
        }
        out.extend_from_slice(&0u32.to_le_bytes());
    }
    out.extend_from_slice(&data);
    out
}

/// A JPEG holding only an EXIF segment.
fn jpeg(tiff: &[u8]) -> Vec<u8> {
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
    out.extend_from_slice(&((2 + 6 + tiff.len()) as u16).to_be_bytes());
    out.extend_from_slice(b"Exif\0\0");
    out.extend_from_slice(tiff);
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

/// A photo: make, model, orientation, size in IFD 0; the capture date (and
/// its offset when given) in the Exif IFD; 48°30' N, 2°15' W in the GPS IFD.
fn photo(date: &str, offset: Option<&str>) -> Vec<u8> {
    let mut exif = vec![ascii(0x9003, date)];
    if let Some(o) = offset {
        exif.push(ascii(0x9011, o));
    }
    jpeg(&tiff(&[
        vec![
            Entry(0x0100, LONG, 1, Payload::Inline(640)),
            Entry(0x0101, LONG, 1, Payload::Inline(480)),
            ascii(0x010F, "Acme"),
            ascii(0x0110, "Shooter 9"),
            Entry(0x0112, SHORT, 1, Payload::Inline(6)),
            Entry(0x8769, LONG, 1, Payload::Ifd(1)),
            Entry(0x8825, LONG, 1, Payload::Ifd(2)),
        ],
        exif,
        vec![
            ascii(0x0001, "N"),
            rationals(0x0002, &[(48, 1), (30, 1), (0, 1)]),
            ascii(0x0003, "W"),
            rationals(0x0004, &[(2, 1), (15, 1), (0, 1)]),
        ],
    ]))
}

// ---- fixtures ----------------------------------------------------------------

struct Dir(std::path::PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("mf-exif-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Dir(dir)
    }
    fn file(&self, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let f = self.0.join(name);
        std::fs::write(&f, bytes).unwrap();
        f
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Every image source, the date both as an instant and as text.
fn map() -> MetadataMap {
    let entry = |name: &str, ty: &str, from: &str| {
        format!("[[field]]\nname = \"{name}\"\ntype = \"{ty}\"\nfrom = [\"{from}\"]\n")
    };
    MetadataMap::parse(
        &[
            entry("make", "string", "Make"),
            entry("model", "string", "Model"),
            entry("orientation", "int", "Orientation"),
            entry("width", "int", "ImageWidth"),
            entry("height", "int", "ImageHeight"),
            entry("lat", "float", "GPSLatitude"),
            entry("lon", "float", "GPSLongitude"),
            entry("date", "datetime", "DateTimeOriginal"),
            entry("date_text", "string", "DateTimeOriginal"),
        ]
        .concat(),
    )
    .unwrap()
}

fn get<'a>(fields: &'a [Field], name: &str) -> Option<&'a Value> {
    fields.iter().find(|f| f.name == format!("mfr_meta_{name}")).map(|f| &f.value)
}

// ---- what a photo gives ------------------------------------------------------

#[test]
fn the_exif_of_a_jpeg_gives_its_camera_its_size_and_its_position() {
    let dir = Dir::new("photo");
    let fields =
        extract(&dir.file("photo.jpg", &photo("2024:05:06 07:08:09", Some("+02:00"))), &map());
    assert_eq!(get(&fields, "make"), Some(&Value::String("Acme".into())));
    assert_eq!(get(&fields, "model"), Some(&Value::String("Shooter 9".into())));
    assert_eq!(get(&fields, "orientation"), Some(&Value::Int(6)));
    assert_eq!(get(&fields, "width"), Some(&Value::Int(640)));
    assert_eq!(get(&fields, "height"), Some(&Value::Int(480)));
    let Some(Value::Float(lat)) = get(&fields, "lat") else { panic!("{fields:?}") };
    let Some(Value::Float(lon)) = get(&fields, "lon") else { panic!("{fields:?}") };
    assert!((lat - 48.5).abs() < 1e-9 && (lon + 2.25).abs() < 1e-9, "{lat} {lon}");
}

/// With its offset the capture date is an instant: 07:08:09 at +02:00 is
/// 05:08:09 UTC.
#[test]
fn a_capture_date_with_its_offset_is_an_exact_instant() {
    let dir = Dir::new("dated");
    let fields =
        extract(&dir.file("dated.jpg", &photo("2024:05:06 07:08:09", Some("+02:00"))), &map());
    let want = date::iso_to_ms("2024-05-06T05:08:09Z").unwrap();
    assert_eq!(get(&fields, "date"), Some(&Value::DateTime(want)));
}

/// Without an offset the date names no instant: it reaches the map as the
/// text `YYYY-MM-DD HH:MM:SS`, for its date parser.
#[test]
fn a_capture_date_without_an_offset_is_kept_as_text() {
    let dir = Dir::new("naive");
    let fields = extract(&dir.file("naive.jpg", &photo("2024:05:06 07:08:09", None)), &map());
    assert_eq!(get(&fields, "date_text"), Some(&Value::String("2024-05-06 07:08:09".into())));
}

/// A blank date, or one that is no date, gives no date — not a wrong one.
#[test]
fn a_blank_or_impossible_capture_date_gives_no_date() {
    let dir = Dir::new("blank");
    for (i, raw) in ["    :  :     :  :  ", "2024:13:06 07:08:09", "2024:05:06 25:08:09", "garbage"]
        .into_iter()
        .enumerate()
    {
        let fields = extract(&dir.file(&format!("{i}.jpg"), &photo(raw, Some("+02:00"))), &map());
        assert_eq!(get(&fields, "date"), None, "{raw:?}");
        assert_eq!(get(&fields, "date_text"), None, "{raw:?}");
        assert_eq!(get(&fields, "make"), Some(&Value::String("Acme".into())), "{raw:?}");
    }
}

// ---- what a crafted file cannot do -------------------------------------------

/// What extracting a file of `len` bytes may hold at most: the buffers of the
/// readers and the parsed fields of a file that small fit well within it.
fn budget(len: usize) -> usize {
    (1 << 20) + 64 * len
}

/// GHSA-53gf-vqf6-qvp8 (kamadak-exif ≤ 0.6.1): pointers repeated at each of
/// three levels were each followed, so n of them gave n³ fields — a few KB
/// that grew into gigabytes, and an abort.
#[test]
fn repeated_ifd_pointers_are_not_followed_into_a_cubic_blow_up() {
    const N: usize = 60;
    let interop: Vec<Entry> = (0..N).map(|_| ascii(0x0001, "R98")).collect();
    let exif: Vec<Entry> = (0..N).map(|_| Entry(0xA005, LONG, 1, Payload::Ifd(2))).collect();
    let ifd0: Vec<Entry> = (0..N).map(|_| Entry(0x8769, LONG, 1, Payload::Ifd(1))).collect();
    let bytes = jpeg(&tiff(&[ifd0, exif, interop]));
    let dir = Dir::new("cubic");
    let (_, peak) = measured(&dir.file("cubic.jpg", &bytes), &map());
    assert!(peak < budget(bytes.len()), "{peak} bytes held for a {}-byte file", bytes.len());
}

/// A small xorshift: the cases are drawn the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Damages a photo the ways a crafted one is: random bytes, extreme counts and
/// offsets, entries repeated, a cut.
fn damaged(rng: &mut Rng) -> Vec<u8> {
    let mut b = photo("2024:05:06 07:08:09", Some("+02:00"));
    for _ in 0..1 + rng.below(6) {
        let at = rng.below(b.len());
        match rng.below(5) {
            0 => b[at] = rng.next() as u8,
            1 => {
                let v = [0u32, 1, 8, 0xFFFF, 0x7FFF_FFFF, u32::MAX][rng.below(6)].to_le_bytes();
                for (i, x) in v.iter().enumerate() {
                    if let Some(y) = b.get_mut(at + i) {
                        *y = *x;
                    }
                }
            }
            2 => {
                let end = (at + 12 * (1 + rng.below(4))).min(b.len());
                let chunk = b[at..end].to_vec();
                for _ in 0..1 + rng.below(16) {
                    b.splice(at..at, chunk.iter().copied());
                }
            }
            3 => b.truncate(at.max(4)),
            _ => b[at] ^= 1 << rng.below(8),
        }
    }
    b
}

#[test]
fn a_damaged_exif_never_panics_the_parser_nor_makes_it_allocate_out_of_proportion() {
    count_panics();
    let cases: usize =
        std::env::var("EXIF_DAMAGE_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(3000);
    let dir = Dir::new("damaged");
    let map = map();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let before = PANICS.with(Cell::get);
    for case in 0..cases {
        let bytes = damaged(&mut rng);
        let f = dir.file("damaged.jpg", &bytes);
        let (_, peak) = measured(&f, &map);
        assert!(
            peak < budget(bytes.len()),
            "case {case}: {peak} bytes held for {} bytes",
            bytes.len()
        );
        assert_eq!(PANICS.with(Cell::get), before, "case {case} panicked");
    }
}
