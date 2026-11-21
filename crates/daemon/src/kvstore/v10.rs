//! The value forms of the store's layout 10 and before, frozen: what the
//! conversion to layout 11 reads (doc "Persisted format changes"). Nothing
//! else reads them; they go with the conversions older than layout 11.
//!
//! A value was its column form (`rows::RawValue`): its type as text, then
//! seven optional columns, lengths in `u32`, integers in fixed `i64`.

use anyhow::{bail, Result};
use metafolder_core::metarecord::Value;

use super::mass::{Kind, Listed, Record, Written};
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
/// numbers, tombs and cleared fields, as the fixed values and fields of
/// layout 11 say them: a row's number and a tomb fix that value, a cleared
/// field the field, applied in the order of their numbers.
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
    // (number, field, value: none for a cleared field)
    let mut fixed: Vec<(u64, String, Option<Value>)> = Vec::new();
    for _ in 0..r.int()? {
        match r.int()? {
            0 => {
                let i = r.int()? as usize;
                let op = r.int()? as u64;
                match record.rows.get(i) {
                    Some(row) => fixed.push((op, row.name.clone(), Some(row.value.clone()))),
                    None => bail!("a row number for a row a record does not have"),
                }
            }
            1 => {
                let name = r.string()?;
                let value = get_value(&mut r)?;
                fixed.push((r.int()? as u64, name, Some(value)));
            }
            2 => {
                let name = r.string()?;
                fixed.push((r.int()? as u64, name, None));
            }
            other => bail!("an extra of kind {other} in a record"),
        }
    }
    fixed.sort_by_key(|(n, _, _)| *n);
    for (n, field, value) in fixed {
        match value {
            Some(v) => record.fix_value(&field, &v, n),
            None => record.fix_field(&field, n),
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
    let kind = Written::of(kind, &values);
    Ok(Listed { op, kind, field, values, ids: decode_set(r.bytes()?)?, deletion: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An int in the column form of layout 10.
    fn int(out: &mut Vec<u8>, n: i64) {
        let text = |out: &mut Vec<u8>, s: &str| {
            out.extend_from_slice(&(s.len() as u32).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        };
        text(out, "int");
        out.push(0); // text
        out.push(1);
        out.extend_from_slice(&n.to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 0, 0]); // real, uuid, ref_repo, name, name_bytes
    }

    fn name(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u32).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }

    fn n(out: &mut Vec<u8>, v: i64) {
        out.extend_from_slice(&v.to_be_bytes());
    }

    /// Row numbers and tombs fix a value, a cleared field the field, in the
    /// order of their numbers.
    #[test]
    fn a_records_numbers_tombs_and_cleared_fields_become_what_it_fixed() {
        let mut b = Vec::new();
        n(&mut b, 2);
        name(&mut b, "a");
        int(&mut b, 1);
        name(&mut b, "b");
        int(&mut b, 2);
        n(&mut b, 4);
        // Row 0 (a = 1) written at 5; a tomb b = 9 at 3; a cleared at 4; b
        // cleared at 2.
        n(&mut b, 0);
        n(&mut b, 0);
        n(&mut b, 5);
        n(&mut b, 1);
        name(&mut b, "b");
        int(&mut b, 9);
        n(&mut b, 3);
        n(&mut b, 2);
        name(&mut b, "a");
        n(&mut b, 4);
        n(&mut b, 2);
        name(&mut b, "b");
        n(&mut b, 2);
        let record = dec_record(&b).unwrap();
        assert_eq!(record.rows.len(), 2);
        assert_eq!(record.fixed_fields, vec![("b".to_string(), 2), ("a".to_string(), 4)]);
        assert_eq!(
            record.fixed_values,
            vec![("b".to_string(), Value::Int(9), 3), ("a".to_string(), Value::Int(1), 5)]
        );
    }
}
