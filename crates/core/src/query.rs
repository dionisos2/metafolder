use serde::{Deserialize, Serialize};

use crate::metarecord::Value;

/// A query predicate. Internal (JSON) representation of queries.
/// The text DSL ("rating > 3 AND tag IS PRESENT") is compiled into this
/// structure by the CLI.
///
/// JSON form: internally tagged with a `"type"` key in snake_case, e.g.
/// `{"type": "is_present", "field": "path"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Query {
    // --- Combinators ---
    And { operands: Vec<Query> },
    Or { operands: Vec<Query> },
    Not { operand: Box<Query> },

    // --- Three-valued logic ---
    /// The field exists with a non-Nothing value.
    IsPresent { field: String },
    /// The field exists with the value Nothing.
    IsAbsent { field: String },
    /// The field does not exist on this metarecord.
    IsUnknown { field: String },

    // --- Comparisons (at least one occurrence of the field matches) ---
    Eq { field: String, value: Value },
    Neq { field: String, value: Value },
    Lt { field: String, value: Value },
    Lte { field: String, value: Value },
    Gt { field: String, value: Value },
    Gte { field: String, value: Value },

    // --- Graph traversal ---
    /// On a `Ref` field, `target` is a sub-query the referent must satisfy.
    /// On a `TreeRef` field, `target` is a path string and matches metarecords
    /// whose direct parent is the metarecord at that path.
    Follows { field: String, target: FollowTarget },
    /// `TreeRef` fields only: the metarecord is a descendant of the metarecord at
    /// the given path (`target` is a path string), or of any metarecord satisfying
    /// the sub-query (`target` is a condition).
    FollowsTransitive { field: String, target: FollowTarget },

    // --- Pattern matching ---
    /// The field has a string value matching the regex. On a `TreeRef`
    /// field, the regex applies to the name component.
    Matches { field: String, pattern: String },
}

/// Right-hand side of `Follows` and `FollowsTransitive`: either a path string
/// (TreeRef semantics) or a sub-query (the referent — direct parent for
/// TreeRef — must satisfy it). JSON: a bare string or a Query object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FollowTarget {
    Path(String),
    Condition(Box<Query>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(q: &Query) -> Query {
        let json = serde_json::to_string(q).expect("serialization failed");
        serde_json::from_str(&json).expect("deserialization failed")
    }

    // ── JSON format (spec-query: "type" key, snake_case) ─────────────────────

    #[test]
    fn test_is_present_json_format() {
        let q = Query::IsPresent { field: "path".into() };
        assert_eq!(serde_json::to_string(&q).unwrap(), r#"{"type":"is_present","field":"path"}"#);
    }

    #[test]
    fn test_eq_json_format() {
        let q = Query::Eq { field: "rating".into(), value: Value::Int(5) };
        assert_eq!(
            serde_json::to_string(&q).unwrap(),
            r#"{"type":"eq","field":"rating","value":{"type":"int","value":5}}"#
        );
    }

    #[test]
    fn test_follows_transitive_json_format() {
        let q = Query::FollowsTransitive {
            field: "mfr_path".into(),
            target: FollowTarget::Path("/music/jazz".into()),
        };
        assert_eq!(
            serde_json::to_string(&q).unwrap(),
            r#"{"type":"follows_transitive","field":"mfr_path","target":"/music/jazz"}"#
        );
    }

    #[test]
    fn test_follows_transitive_with_condition_target_json_format() {
        let q = Query::FollowsTransitive {
            field: "mfr_path".into(),
            target: FollowTarget::Condition(Box::new(Query::Eq {
                field: "mfr_path".into(),
                value: Value::String("2021".into()),
            })),
        };
        assert_eq!(
            serde_json::to_string(&q).unwrap(),
            r#"{"type":"follows_transitive","field":"mfr_path","target":{"type":"eq","field":"mfr_path","value":{"type":"string","value":"2021"}}}"#
        );
        assert_eq!(roundtrip(&q), q);
    }

    #[test]
    fn test_follows_with_path_target() {
        let q = Query::Follows {
            field: "mfr_path".into(),
            target: FollowTarget::Path("/music/jazz".into()),
        };
        assert_eq!(
            serde_json::to_string(&q).unwrap(),
            r#"{"type":"follows","field":"mfr_path","target":"/music/jazz"}"#
        );
    }

    #[test]
    fn test_follows_with_condition_target() {
        let q = Query::Follows {
            field: "author".into(),
            target: FollowTarget::Condition(Box::new(Query::Eq {
                field: "name".into(),
                value: Value::String("Coltrane".into()),
            })),
        };
        assert_eq!(
            serde_json::to_string(&q).unwrap(),
            r#"{"type":"follows","field":"author","target":{"type":"eq","field":"name","value":{"type":"string","value":"Coltrane"}}}"#
        );
    }

    #[test]
    fn test_matches_json_format() {
        let q = Query::Matches { field: "title".into(), pattern: "[Ll]ive".into() };
        assert_eq!(
            serde_json::to_string(&q).unwrap(),
            r#"{"type":"matches","field":"title","pattern":"[Ll]ive"}"#
        );
    }

    // ── Roundtrips ───────────────────────────────────────────────────────────

    #[test]
    fn test_combinator_roundtrip() {
        let q = Query::And {
            operands: vec![
                Query::Or {
                    operands: vec![
                        Query::Eq { field: "tag".into(), value: Value::String("jazz".into()) },
                        Query::Eq { field: "tag".into(), value: Value::String("blues".into()) },
                    ],
                },
                Query::Not {
                    operand: Box::new(Query::IsUnknown { field: "rating".into() }),
                },
                Query::Gte { field: "rating".into(), value: Value::Int(4) },
            ],
        };
        assert_eq!(roundtrip(&q), q);
    }

    #[test]
    fn test_traversal_roundtrip() {
        let q = Query::And {
            operands: vec![
                Query::Follows {
                    field: "tag".into(),
                    target: FollowTarget::Condition(Box::new(Query::Eq {
                        field: "label".into(),
                        value: Value::String("jazz".into()),
                    })),
                },
                Query::Follows {
                    field: "mfr_path".into(),
                    target: FollowTarget::Path("/music".into()),
                },
                Query::FollowsTransitive {
                    field: "mfr_path".into(),
                    target: FollowTarget::Path("/music".into()),
                },
                Query::Matches { field: "title".into(), pattern: "^Live".into() },
            ],
        };
        assert_eq!(roundtrip(&q), q);
    }

    #[test]
    fn test_comparison_roundtrips() {
        let cases = vec![
            Query::IsPresent { field: "a".into() },
            Query::IsAbsent { field: "a".into() },
            Query::IsUnknown { field: "a".into() },
            Query::Eq { field: "a".into(), value: Value::Bool(true) },
            Query::Neq { field: "a".into(), value: Value::Float(1.5) },
            Query::Lt { field: "a".into(), value: Value::Int(1) },
            Query::Lte { field: "a".into(), value: Value::Int(2) },
            Query::Gt {
                field: "a".into(),
                value: Value::DateTime(crate::date::iso_to_ms("2024-01-01T00:00:00Z").unwrap()),
            },
            Query::Gte { field: "a".into(), value: Value::String("x".into()) },
        ];
        for q in cases {
            assert_eq!(roundtrip(&q), q, "roundtrip failed for {q:?}");
        }
    }
}
