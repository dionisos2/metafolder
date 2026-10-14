//! The query planner (doc "Query planner"): the cost choices the evaluator
//! makes before reading, kept in one place. It is deliberately minimal — no
//! statistics, no cost model of the whole query: each choice is a fixed rule
//! on what the query's shape says, or on exact counts the store already
//! holds (a field's holders, a candidate set's size).
//!
//! Two choices live here:
//!
//! * the order in which the operands of an `and` are evaluated — the ones
//!   answered from a bitmap or a posting first, the *scans* last, each
//!   handed the running intersection as its candidates (and none evaluated
//!   once that intersection is empty);
//! * whether a scan reads its candidates' own rows (a *probe*) or walks the
//!   field's partition.
//!
//! The choices of the page — fetch or walk, how far a walk may go — are made
//! with the sort, in `Eval::walk_page` (doc "Sorting and postings").

use metafolder_core::query::{OsmMode, Query};

/// What probing one candidate costs, in keys of a partition walk: its uuid,
/// its rows of the field, and the miss of a candidate that holds none.
const PROBE_COST: u64 = 4;

/// Whether `candidates` records are checked on their own rows rather than by
/// walking a field `holders` records hold a value of — a walk reads about a
/// key per holder, a probe a few per candidate.
pub(crate) fn probes(candidates: u64, holders: u64) -> bool {
    candidates.saturating_mul(PROBE_COST) < holders
}

/// Whether an operand is a *scan*: answered by walking a field's values,
/// which costs the field's holders — unless candidates are handed in, which a
/// scan may check instead. A text predicate (`matches`, `osm direct`) runs
/// its regex over the distinct values; a `!=` or an ordered comparison walks
/// a range of them. An equality reads one value's posting or run, and every
/// other leaf a bitmap: candidates would not make them cheaper.
pub(crate) fn is_scan(q: &Query) -> bool {
    matches!(
        q,
        Query::Matches { .. }
            | Query::Osm { mode: OsmMode::Direct, .. }
            | Query::Neq { .. }
            | Query::Lt { .. }
            | Query::Lte { .. }
            | Query::Gt { .. }
            | Query::Gte { .. }
    )
}

/// The operands of an `and` in the order they are evaluated: the others
/// first, the scans last, each part in the query's own order — a property of
/// the query's shape, so every page of a paginated session is served the
/// same way.
pub(crate) fn and_order<'q>(operands: impl IntoIterator<Item = &'q Query>) -> Vec<&'q Query> {
    let (scans, rest): (Vec<&Query>, Vec<&Query>) = operands.into_iter().partition(|o| is_scan(o));
    rest.into_iter().chain(scans).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use metafolder_core::metarecord::Value;
    use metafolder_core::query::Aspect;

    fn gt() -> Query {
        Query::Gt { field: "size".into(), value: Value::Int(1), aspect: Aspect::Raw }
    }

    fn eq() -> Query {
        Query::Eq { field: "kind".into(), value: Value::String("a".into()), aspect: Aspect::Raw }
    }

    fn text() -> Query {
        Query::Matches { field: "name".into(), pattern: "x".into(), aspect: Aspect::Raw }
    }

    #[test]
    fn scans_come_last_in_their_own_order() {
        let ops = [gt(), eq(), text(), Query::IsPresent { field: "a".into(), aspect: Aspect::Raw }];
        let order = and_order(&ops);
        assert_eq!(order, vec![&ops[1], &ops[3], &ops[0], &ops[2]]);
    }

    #[test]
    fn an_equality_is_no_scan() {
        assert!(!is_scan(&eq()));
        assert!(is_scan(&gt()));
        assert!(is_scan(&text()));
    }

    #[test]
    fn a_few_candidates_are_probed() {
        assert!(probes(100, 16_000));
        assert!(!probes(4_000, 16_000));
        assert!(!probes(u64::MAX, u64::MAX));
    }
}
