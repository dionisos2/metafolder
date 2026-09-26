//! The prototype's data model: a reduced copy of the daemon's, keeping the
//! shapes whose storage cost differs — strings, ordered numbers, the explicit
//! absence `Nothing`, and a `tree_ref` forest.

use anyhow::{bail, Result};
use uuid::Uuid;

/// The forest root sentinel: a `tree_ref` whose parent is this is a root.
pub const ROOT: Uuid = Uuid::nil();

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Nothing,
    Str(String),
    Int(i64),
    /// Unix milliseconds.
    Time(i64),
    /// A position in the field's forest. At most one per record and field in
    /// the prototype (the daemon allows several).
    Tree {
        parent: Uuid,
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub uuid: Uuid,
    pub fields: Vec<(String, Value)>,
}

impl Record {
    pub fn values<'a>(&'a self, field: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
        self.fields.iter().filter(move |(n, _)| n == field).map(|(_, v)| v)
    }

    pub fn tree(&self, field: &str) -> Option<(Uuid, &str)> {
        self.fields.iter().filter(|(n, _)| n == field).find_map(|(_, v)| match v {
            Value::Tree { parent, name } => Some((*parent, name.as_str())),
            _ => None,
        })
    }
}

const T_NOTHING: u8 = 0;
const T_STR: u8 = 1;
const T_INT: u8 = 2;
const T_TIME: u8 = 3;
const T_TREE: u8 = 4;

/// The order-preserving key of a value: byte comparison of two keys is the
/// comparison of the values, and values of different types never interleave
/// (the type tag comes first). `None` for a `Tree`, which sorts on its path.
pub fn ordered_key(v: &Value) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    match v {
        Value::Nothing => out.push(T_NOTHING),
        Value::Str(s) => {
            out.push(T_STR);
            push_terminated(&mut out, s.as_bytes());
        }
        Value::Int(i) => {
            out.push(T_INT);
            out.extend_from_slice(&flip(*i));
        }
        Value::Time(t) => {
            out.push(T_TIME);
            out.extend_from_slice(&flip(*t));
        }
        Value::Tree { .. } => return None,
    }
    Some(out)
}

/// A signed integer as big-endian bytes that sort in numeric order.
fn flip(i: i64) -> [u8; 8] {
    ((i as u64) ^ (1 << 63)).to_be_bytes()
}

/// Appends `bytes` so that no encoded string is a prefix of another: `0x00`
/// is escaped as `00 FF`, and the string ends with `00 00`. Order is kept.
fn push_terminated(out: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 0]);
}

/// The text a substring or regex predicate reads: a string, or a tree name.
pub fn text_of(v: &Value) -> Option<&str> {
    match v {
        Value::Str(s) => Some(s),
        Value::Tree { name, .. } => Some(name),
        _ => None,
    }
}

// ── Record serialisation ────────────────────────────────────────────────────

pub fn encode_record(r: &Record) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(r.uuid.as_bytes());
    for (name, v) in &r.fields {
        put_bytes(&mut out, name.as_bytes());
        match v {
            Value::Nothing => out.push(T_NOTHING),
            Value::Str(s) => {
                out.push(T_STR);
                put_bytes(&mut out, s.as_bytes());
            }
            Value::Int(i) => {
                out.push(T_INT);
                out.extend_from_slice(&i.to_le_bytes());
            }
            Value::Time(t) => {
                out.push(T_TIME);
                out.extend_from_slice(&t.to_le_bytes());
            }
            Value::Tree { parent, name } => {
                out.push(T_TREE);
                out.extend_from_slice(parent.as_bytes());
                put_bytes(&mut out, name.as_bytes());
            }
        }
    }
    out
}

pub fn decode_uuid(bytes: &[u8]) -> Uuid {
    Uuid::from_slice(&bytes[..16]).expect("16-byte record header")
}

pub fn decode_record(bytes: &[u8]) -> Result<Record> {
    let uuid = decode_uuid(bytes);
    let mut r = Reader { b: bytes, at: 16 };
    let mut fields = Vec::new();
    while r.at < bytes.len() {
        let name = r.string()?;
        let v = match r.byte()? {
            T_NOTHING => Value::Nothing,
            T_STR => Value::Str(r.string()?),
            T_INT => Value::Int(r.i64()?),
            T_TIME => Value::Time(r.i64()?),
            T_TREE => {
                let parent = Uuid::from_slice(r.take(16)?)?;
                Value::Tree { parent, name: r.string()? }
            }
            t => bail!("unknown value tag {t}"),
        };
        fields.push((name, v));
    }
    Ok(Record { uuid, fields })
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.at + n > self.b.len() {
            bail!("truncated record");
        }
        let s = &self.b[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into()?))
    }
    fn string(&mut self) -> Result<String> {
        let n = u32::from_le_bytes(self.take(4)?.try_into()?) as usize;
        Ok(String::from_utf8(self.take(n)?.to_vec())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips() {
        let r = Record {
            uuid: Uuid::from_u128(7),
            fields: vec![
                ("a".into(), Value::Str("x\0y".into())),
                ("b".into(), Value::Int(-3)),
                ("c".into(), Value::Time(1_700_000_000_000)),
                ("d".into(), Value::Nothing),
                ("p".into(), Value::Tree { parent: ROOT, name: "n".into() }),
            ],
        };
        assert_eq!(decode_record(&encode_record(&r)).unwrap(), r);
    }

    #[test]
    fn ordered_keys_sort_like_their_values() {
        let ints = [-5i64, -1, 0, 1, 1 << 40];
        for w in ints.windows(2) {
            assert!(ordered_key(&Value::Int(w[0])) < ordered_key(&Value::Int(w[1])));
        }
        let strs = ["", "\0", "a", "a\0", "ab", "b"];
        for w in strs.windows(2) {
            let (a, b) = (Value::Str(w[0].into()), Value::Str(w[1].into()));
            assert!(ordered_key(&a) < ordered_key(&b), "{:?} < {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn no_string_key_is_a_prefix_of_another() {
        let a = ordered_key(&Value::Str("ab".into())).unwrap();
        let b = ordered_key(&Value::Str("abc".into())).unwrap();
        assert!(!b.starts_with(&a));
    }
}
