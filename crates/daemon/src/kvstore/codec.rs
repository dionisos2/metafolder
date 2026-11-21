//! The compact encoding of the store's values (doc "Value encoding in the
//! store"): varints, a type byte, each value in a form of its type. The same
//! encoding is used everywhere a value is stored — a record, a pending write,
//! a pending index write, the log.

use anyhow::{bail, Result};
use metafolder_core::metarecord::{TreeName, Value};
use uuid::Uuid;

use crate::rows::FieldRow;

/// The name of the type of `code` (`rows::encode_value`'s `value_type`),
/// as `field_types` keys a field by `code` (doc "Store tables").
pub(crate) fn type_name(code: u8) -> Option<&'static str> {
    Some(match code {
        NOTHING => "nothing",
        1 => "bool",
        2 => "int",
        3 => "float",
        4 => "string",
        5 => "datetime",
        6 => "ref",
        7 => "refbase",
        8 => "externalref",
        9 => "tree_ref",
        10 => "hierarchy_ref",
        _ => return None,
    })
}

/// The type code of a `field_types` key's last byte, as its name: the type
/// a field holds rows of.
pub(crate) fn field_type(key_tail: &[u8]) -> Result<&'static str> {
    match key_tail {
        [code] => type_name(*code).ok_or_else(|| anyhow::anyhow!("a type code of {code}")),
        other => bail!("a field type of {} bytes", other.len()),
    }
}

/// Whether a `field_types` key's type is a position's.
pub(crate) fn is_position_type(key_tail: &[u8]) -> bool {
    matches!(key_tail, [9] | [10])
}

/// The code of a value's type, the byte written before a field's values.
pub(crate) fn type_code(v: &Value) -> u8 {
    match v {
        Value::Nothing => NOTHING,
        Value::Bool(_) => 1,
        Value::Int(_) => 2,
        Value::Float(_) => 3,
        Value::String(_) => 4,
        Value::DateTime(_) => 5,
        Value::Ref(_) => 6,
        Value::RefBase(_) => 7,
        Value::ExternalRef { .. } => 8,
        Value::TreeRef { .. } => 9,
        Value::HierarchyRef { .. } => 10,
    }
}

/// The type code of `nothing`: a field of it holds no other value.
pub(crate) const NOTHING: u8 = 0;

/// The zero uuid: the parent of a root position.
const ROOT: [u8; 16] = [0; 16];

/// A growable byte buffer, in the compact encoding.
#[derive(Default)]
pub(crate) struct Enc(pub(crate) Vec<u8>);

impl Enc {
    pub(crate) fn varint(&mut self, mut n: u64) -> &mut Self {
        while n >= 0x80 {
            self.0.push((n as u8) | 0x80);
            n >>= 7;
        }
        self.0.push(n as u8);
        self
    }
    pub(crate) fn zigzag(&mut self, n: i64) -> &mut Self {
        self.varint(((n << 1) ^ (n >> 63)) as u64)
    }
    /// `varint len · bytes`.
    pub(crate) fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.varint(b.len() as u64);
        self.0.extend_from_slice(b);
        self
    }
    pub(crate) fn raw(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(b);
        self
    }
    /// A value without its type: the field's type is written once before.
    pub(crate) fn bare(&mut self, v: &Value) -> &mut Self {
        let parent = |p: &Option<Uuid>| p.map_or(ROOT, |p| *p.as_bytes());
        match v {
            Value::Nothing => self,
            Value::Bool(b) => self.raw(&[*b as u8]),
            Value::Int(n) | Value::DateTime(n) => self.zigzag(*n),
            Value::Float(f) => self.raw(&f.to_bits().to_be_bytes()),
            Value::String(t) => self.bytes(t.as_bytes()),
            Value::Ref(u) | Value::RefBase(u) => self.raw(u.as_bytes()),
            Value::ExternalRef { repo, metarecord } => {
                self.raw(repo.as_bytes()).raw(metarecord.as_bytes())
            }
            Value::TreeRef { parent: p, name } | Value::HierarchyRef { parent: p, name } => {
                self.raw(&parent(p)).bytes(name.as_bytes())
            }
        }
    }
    /// A value with its type.
    pub(crate) fn value(&mut self, v: &Value) -> &mut Self {
        self.raw(&[type_code(v)]).bare(v)
    }
}

/// A reader of the compact encoding.
pub(crate) struct Dec<'a>(pub(crate) &'a [u8]);

impl<'a> Dec<'a> {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            bail!("a truncated value in the key-value store");
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }
    pub(crate) fn varint(&mut self) -> Result<u64> {
        let mut n = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.take(1)?[0];
            n |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(n);
            }
        }
        bail!("a varint over 64 bits in the key-value store")
    }
    pub(crate) fn zigzag(&mut self) -> Result<i64> {
        let n = self.varint()?;
        Ok(((n >> 1) as i64) ^ -((n & 1) as i64))
    }
    pub(crate) fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.varint()?;
        self.take(usize::try_from(n)?)
    }
    pub(crate) fn uuid(&mut self) -> Result<Uuid> {
        Ok(Uuid::from_slice(self.take(16)?)?)
    }
    fn parent(&mut self) -> Result<Option<Uuid>> {
        let p = self.take(16)?;
        Ok((p != ROOT).then(|| Uuid::from_slice(p).expect("16 bytes")))
    }
    pub(crate) fn string(&mut self) -> Result<String> {
        Ok(String::from_utf8(self.bytes()?.to_vec())?)
    }
    /// A value of the type `code`, written by [`Enc::bare`].
    pub(crate) fn bare(&mut self, code: u8) -> Result<Value> {
        Ok(match code {
            NOTHING => Value::Nothing,
            1 => Value::Bool(match self.take(1)?[0] {
                0 => false,
                1 => true,
                b => bail!("a bool of {b} in the key-value store"),
            }),
            2 => Value::Int(self.zigzag()?),
            3 => Value::Float(f64::from_bits(u64::from_be_bytes(self.take(8)?.try_into()?))),
            4 => Value::String(self.string()?),
            5 => Value::DateTime(self.zigzag()?),
            6 => Value::Ref(self.uuid()?),
            7 => Value::RefBase(self.uuid()?),
            8 => Value::ExternalRef { repo: self.uuid()?, metarecord: self.uuid()? },
            9 => {
                let parent = self.parent()?;
                Value::TreeRef { parent, name: TreeName::from_bytes(self.bytes()?.to_vec()) }
            }
            10 => {
                let parent = self.parent()?;
                Value::HierarchyRef { parent, name: TreeName::from_bytes(self.bytes()?.to_vec()) }
            }
            other => bail!("a value of type code {other} in the key-value store"),
        })
    }
    /// A value written by [`Enc::value`].
    pub(crate) fn value(&mut self) -> Result<Value> {
        let code = self.take(1)?[0];
        self.bare(code)
    }
}

/// A record's rows: `varint groups · { varint len · name · type · [varint
/// count · { value }] }`, a group being the rows of one name and one type (a
/// `nothing` group holds no count: it is the one row). The rows come back
/// grouped: `order` says, for each row written, where it is in the rows read.
pub(crate) fn put_rows(out: &mut Enc, rows: &[FieldRow]) -> Vec<usize> {
    // The groups in the order their first row comes, found through a map: a
    // search of the list made each encoding cost rows × fields, and a write to
    // a record of thousands of fields took seconds.
    let mut groups: Vec<((&str, u8), Vec<usize>)> = Vec::new();
    let mut index: std::collections::HashMap<(&str, u8), usize> = Default::default();
    for (i, row) in rows.iter().enumerate() {
        let group = (row.name.as_str(), type_code(&row.value));
        match index.entry(group) {
            std::collections::hash_map::Entry::Occupied(at) => groups[*at.get()].1.push(i),
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(groups.len());
                groups.push((group, vec![i]));
            }
        }
    }
    let mut order = vec![0; rows.len()];
    let mut at = 0;
    out.varint(groups.len() as u64);
    for ((name, code), members) in &groups {
        out.bytes(name.as_bytes()).raw(&[*code]);
        if *code != NOTHING {
            out.varint(members.len() as u64);
        }
        for (k, &i) in members.iter().enumerate() {
            // A field holds one nothing at most; a second is not written.
            if *code == NOTHING && k > 0 {
                order[i] = at - 1;
                continue;
            }
            out.bare(&rows[i].value);
            order[i] = at;
            at += 1;
        }
    }
    order
}

/// The rows [`put_rows`] wrote, in its order.
pub(crate) fn get_rows(r: &mut Dec) -> Result<Vec<FieldRow>> {
    let groups = r.varint()?;
    let mut rows = Vec::new();
    for _ in 0..groups {
        let name = r.string()?;
        let code = r.take(1)?[0];
        let count = if code == NOTHING { 1 } else { r.varint()? };
        for _ in 0..count {
            rows.push(FieldRow { name: name.clone(), value: r.bare(code)? });
        }
    }
    Ok(rows)
}

/// A log context, the value of its id in `log_contexts` (doc "Store
/// tables"): `varint len · name · flags · [varint len · description] ·
/// [varint parent] · varint created (ms)`, the flags saying which of the two
/// optional parts follow.
pub(crate) fn enc_context(m: &crate::store::LabelMeta) -> Vec<u8> {
    const DESCRIBED: u8 = 1;
    const CHILD: u8 = 2;
    let flags = (m.description.is_some() as u8 * DESCRIBED) | (m.parent.is_some() as u8 * CHILD);
    let mut out = Enc::default();
    out.bytes(m.name.as_bytes()).raw(&[flags]);
    if let Some(d) = &m.description {
        out.bytes(d.as_bytes());
    }
    if let Some(p) = m.parent {
        out.varint(p as u64);
    }
    out.varint(m.created as u64);
    out.0
}

/// What [`enc_context`] wrote.
pub(crate) fn dec_context(b: &[u8]) -> Result<crate::store::LabelMeta> {
    let mut r = Dec(b);
    let name = r.string()?;
    let flags = r.take(1)?[0];
    let description = if flags & 1 != 0 { Some(r.string()?) } else { None };
    let parent = if flags & 2 != 0 { Some(r.varint()? as i64) } else { None };
    let created = r.varint()? as i64;
    Ok(crate::store::LabelMeta { name, description, parent, created })
}

/// An operation's origin, one byte: a client's (none), the watcher's, the
/// trash's (doc "Operations and their origin").
pub(crate) fn origin_byte(origin: Option<&str>) -> Result<u8> {
    Ok(match origin {
        None => 0,
        Some(crate::log::ORIGIN_WATCHER) => 1,
        Some(crate::log::ORIGIN_TRASH) => 2,
        Some(other) => bail!("an operation of origin {other:?}"),
    })
}

pub(crate) fn origin_of(byte: u8) -> Result<Option<String>> {
    Ok(match byte {
        0 => None,
        1 => Some(crate::log::ORIGIN_WATCHER.to_string()),
        2 => Some(crate::log::ORIGIN_TRASH.to_string()),
        other => bail!("an operation of origin byte {other}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_origin_is_one_byte() {
        for origin in [None, Some("watcher"), Some("trash")] {
            assert_eq!(origin_of(origin_byte(origin).unwrap()).unwrap().as_deref(), origin);
        }
    }

    #[test]
    fn a_log_context_comes_back_compact() {
        use crate::store::LabelMeta;
        let full = LabelMeta {
            name: "tag run".into(),
            description: Some("the 2024 photos".into()),
            parent: Some(300),
            created: 1_763_700_000_000,
        };
        let bare = LabelMeta { name: "x".into(), description: None, parent: None, created: 5 };
        for m in [&full, &bare] {
            let back = dec_context(&enc_context(m)).unwrap();
            assert_eq!(
                (back.name, back.description, back.parent, back.created),
                (m.name.clone(), m.description.clone(), m.parent, m.created)
            );
        }
        // varint len · name · flags · varint created: 1 + 1 + 1 + 1.
        assert_eq!(enc_context(&bare).len(), 4);
    }

    fn every_kind() -> Vec<Value> {
        let u = Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
        vec![
            Value::Nothing,
            Value::Bool(true),
            Value::Bool(false),
            Value::Int(0),
            Value::Int(-1),
            Value::Int(i64::MIN),
            Value::Int(i64::MAX),
            Value::Float(-2.5),
            Value::Float(f64::NAN),
            Value::String(String::new()),
            Value::String("héllo".into()),
            Value::DateTime(-86_400_000),
            Value::Ref(u),
            Value::RefBase(u),
            Value::ExternalRef { repo: Uuid::from_u128(7), metarecord: u },
            Value::TreeRef { parent: None, name: TreeName::from_bytes(b"".to_vec()) },
            Value::TreeRef { parent: Some(u), name: TreeName::from_bytes(vec![0xff, 0, b'a']) },
            Value::HierarchyRef { parent: Some(u), name: "tags".into() },
        ]
    }

    /// Equality that a NaN satisfies too.
    fn same(a: &Value, b: &Value) -> bool {
        format!("{a:?}") == format!("{b:?}")
    }

    #[test]
    fn every_value_comes_back() {
        for v in every_kind() {
            let mut out = Enc::default();
            out.value(&v);
            let mut r = Dec(&out.0);
            let back = r.value().unwrap();
            assert!(same(&v, &back), "{v:?} came back as {back:?}");
            assert!(r.is_empty(), "{v:?} left bytes");
        }
    }

    #[test]
    fn varints_and_zigzags_come_back_at_their_bounds() {
        for n in [0, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut out = Enc::default();
            out.varint(n);
            assert_eq!(Dec(&out.0).varint().unwrap(), n);
        }
        for n in [0, -1, 1, -64, 64, i64::MIN, i64::MAX] {
            let mut out = Enc::default();
            out.zigzag(n);
            assert_eq!(Dec(&out.0).zigzag().unwrap(), n);
        }
        let mut out = Enc::default();
        out.varint(127).zigzag(-1);
        assert_eq!(out.0, vec![127, 1], "one byte each");
    }

    #[test]
    fn a_short_text_field_is_twelve_bytes_and_ten_tags_forty_eight() {
        let row = |v: &str| FieldRow { name: "tags".into(), value: Value::String(v.into()) };
        let mut out = Enc::default();
        put_rows(&mut out, &[row("foo")]);
        assert_eq!(out.0.len(), 12, "1 + 5 + 1 + 1 + 4");
        let ten: Vec<_> = (0..10).map(|i| row(&format!("t{i:02}"))).collect();
        let mut out = Enc::default();
        put_rows(&mut out, &ten);
        assert_eq!(out.0.len(), 48, "1 + 5 + 1 + 1 + 10 × 4");
    }

    #[test]
    fn rows_come_back_grouped_by_name_and_type() {
        let row = |n: &str, v: Value| FieldRow { name: n.into(), value: v };
        // A retype passes through a field of two types; names interleave.
        let rows = vec![
            row("a", Value::Int(1)),
            row("b", Value::Nothing),
            row("a", Value::String("x".into())),
            row("a", Value::Int(2)),
        ];
        let mut out = Enc::default();
        let order = put_rows(&mut out, &rows);
        let back = get_rows(&mut Dec(&out.0)).unwrap();
        assert_eq!(back.len(), rows.len());
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(&back[order[i]], row, "row {i}");
        }
        assert_eq!(back[0].name, "a");
        assert_eq!(back[1].value, Value::Int(2), "the ints of a together");
    }

    #[test]
    fn a_truncated_value_is_an_error_not_a_panic() {
        let mut out = Enc::default();
        out.value(&Value::String("truncated".into()));
        for cut in 0..out.0.len() {
            assert!(Dec(&out.0[..cut]).value().is_err(), "cut at {cut}");
        }
    }
}
