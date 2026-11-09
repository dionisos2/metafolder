//! The query planner (doc "Query planner"): the cost choices the evaluator
//! makes before reading, kept in one place. It is deliberately minimal — no
//! cost model of the whole query: each choice is a fixed rule on what the
//! query's shape says, or on exact counts the store already holds (a field's
//! holders, a candidate set's size, a range's rows by its histogram).
//!
//! Two choices live here:
//!
//! * the order in which the operands of an `and` are evaluated — the ones
//!   answered from a bitmap or a posting first, the *scans* last, the
//!   narrowest first, each handed the running intersection as its candidates
//!   (and none evaluated once that intersection is empty);
//! * whether a scan reads its candidates' own rows (a *probe*) or walks the
//!   field's partition.
//!
//! The choices of the page — fetch or walk, how far a walk may go — are made
//! with the sort, in `Eval::walk_page` (doc "Sorting and postings").

use metafolder_core::query::{OsmMode, Query};

/// The keys probing one candidate reads: its uuid, its rows of the field, and
/// the miss of a candidate that holds none.
const PROBE_KEYS: u64 = 4;

/// What a scattered key read costs against a contiguous one (doc "Planner
/// statistics", the cost model): a probe's keys are a candidate's rows, then
/// the next one's, far apart in the file; a walk's are a hundred to a page.
/// A first value, from the profile of November 2026 — about 13 walked keys
/// per probed candidate cold, 16 warm —, to be calibrated with `--cold`.
const SCATTERED: u64 = 4;

/// Whether `candidates` records are checked on their own rows rather than by
/// a walk reading `walked` contiguous keys — a key per row walked, where a
/// probe reads a few scattered ones per candidate.
pub(crate) fn probes(candidates: u64, walked: u64) -> bool {
    candidates.saturating_mul(PROBE_KEYS * SCATTERED) < walked
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
/// first, in the query's order — a bitmap or a posting, which candidates
/// would not make cheaper —, then the scans, the narrowest by `estimate` (the
/// rows it may answer) first, ties in the query's order: each scan is handed
/// the intersection so far, so the narrow ones leave the wide ones few
/// candidates to check. `estimate` is asked only when there are two scans to
/// order. The order depends on the data, the answer does not: an `and`
/// commutes (doc "Query planner").
pub(crate) fn and_order<'q>(
    operands: impl IntoIterator<Item = &'q Query>,
    estimate: impl Fn(&Query) -> u64,
) -> Vec<&'q Query> {
    let (mut scans, rest): (Vec<&Query>, Vec<&Query>) =
        operands.into_iter().partition(|o| is_scan(o));
    if scans.len() > 1 {
        // A stable sort: ties keep the query's order.
        scans.sort_by_cached_key(|q| estimate(q));
    }
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
        let order = and_order(&ops, |_| 0);
        assert_eq!(order, vec![&ops[1], &ops[3], &ops[0], &ops[2]]);
    }

    fn gt_on(field: &str) -> Query {
        Query::Gt { field: field.into(), value: Value::Int(1), aspect: Aspect::Raw }
    }

    #[test]
    fn scans_go_narrowest_first_ties_in_their_own_order() {
        let ops = [text(), gt_on("a"), eq(), gt_on("b"), gt_on("c")];
        let estimate = |q: &Query| match q {
            Query::Gt { field, .. } if field == "a" => 500,
            Query::Gt { field, .. } if field == "b" => 10,
            _ => 500,
        };
        let order = and_order(&ops, estimate);
        assert_eq!(order, vec![&ops[2], &ops[3], &ops[0], &ops[1], &ops[4]]);
    }

    #[test]
    fn a_single_scan_needs_no_estimate() {
        let ops = [gt(), eq()];
        let order = and_order(&ops, |_| panic!("no estimate for a single scan"));
        assert_eq!(order, vec![&ops[1], &ops[0]]);
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
        assert!(!probes(1_000, 16_000));
        assert!(!probes(u64::MAX, u64::MAX));
    }
}
