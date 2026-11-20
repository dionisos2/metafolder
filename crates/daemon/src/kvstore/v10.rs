//! The value forms of the store's layout 10 and before, frozen: what the
//! conversion to layout 11 reads (doc "Persisted format changes"). Nothing
//! else reads them; they go with the conversions older than layout 11.
//!
//! A value was its column form (`rows::RawValue`): its type as text, then
//! seven optional columns, lengths in `u32`, integers in fixed `i64`.

use anyhow::{bail, Result};
use metafolder_core::metarecord::Value;

use super::mass::{Kind, Listed, Record};
use super::{derived::decode_set, In};
use crate::rows::{self, FieldRow, RawValue};

/// A value as layout 10 wrote it.
pub(super) fn get_value(r: &mut In) -> Result<Value> {
    let raw = RawValue {
        value_type: r.string()?,
        text: r.opt_string()?,
        int: r.opt_int()?,
        real: r.opt_int()?.map(|b| f64::from_bits(b as u64)),
        uuid: r.opt_bytes()?.map(<[u8]>::to_vec),
        ref_repo: r.opt_bytes()?.map(<[u8]>::to_vec),
        name: r.opt_string()?,
        name_bytes: r.opt_bytes()?.map(<[u8]>::to_vec),
    };
    rows::decode_value(raw)
}

/// A row of the older tables `cells` and `field_cells`: a row id, the name,
/// the value.
pub(super) fn dec_row(b: &[u8]) -> Result<FieldRow> {
    let mut r = In(b);
    r.int()?;
    Ok(FieldRow { name: r.string()?, value: get_value(&mut r)? })
}

/// A record of `records`: its rows, then — when it has any — its row
/// numbers, tombs and cleared fields.
pub(super) fn dec_record(b: &[u8]) -> Result<Record> {
    let mut r = In(b);
    let n = r.int()?;
    let mut rows = Vec::with_capacity(n.max(0) as usize);
    for _ in 0..n {
        rows.push(FieldRow { name: r.string()?, value: get_value(&mut r)? });
    }
    let mut record = Record::plain(rows);
    if r.0.is_empty() {
        return Ok(record);
    }
    for _ in 0..r.int()? {
        match r.int()? {
            0 => {
                let i = r.int()? as usize;
                let op = r.int()? as u64;
                match record.ops.get_mut(i) {
                    Some(slot) => *slot = op,
                    None => bail!("a row number for a row a record does not have"),
                }
            }
            1 => {
                let name = r.string()?;
                let value = get_value(&mut r)?;
                record.tombs.push((name, value, r.int()? as u64));
            }
            2 => {
                let name = r.string()?;
                record.cleared.push((name, r.int()? as u64));
            }
            other => bail!("an extra of kind {other} in a record"),
        }
    }
    Ok(record)
}

/// An entry of `mass`.
pub(super) fn dec_listed(op: u64, bytes: &[u8]) -> Result<Listed> {
    let mut r = In(bytes);
    let kind = match r.int()? {
        0 => Kind::Add,
        1 => Kind::Remove,
        2 => Kind::Replace,
        other => bail!("an entry of kind {other}"),
    };
    let field = r.string()?;
    let n = r.int()?;
    let mut values = Vec::new();
    for _ in 0..n {
        values.push(get_value(&mut r)?);
    }
    Ok(Listed { op, kind, field, values, ids: decode_set(r.bytes()?)? })
}
