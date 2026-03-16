use serde::{Deserialize, Serialize};

use crate::entry::Value;

/// A query predicate. Internal (JSON) representation of queries.
/// The text DSL ("rating > 3 AND tag IS PRESENT") is compiled into this structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Query {
    // --- Combinators ---
    And { operands: Vec<Query> },
    Or  { operands: Vec<Query> },
    Not { operand: Box<Query> },

    // --- Three-valued logic ---
    /// The field does not exist on this entry.
    IsUnknown { field: String },
    /// The field exists with the value Nothing.
    IsAbsent  { field: String },
    /// The field exists with a non-Nothing value.
    IsPresent { field: String },

    // --- Comparisons (at least one occurrence of the field satisfies the condition) ---
    Eq  { field: String, value: Value },
    Neq { field: String, value: Value },
    Lt  { field: String, value: Value },
    Lte { field: String, value: Value },
    Gt  { field: String, value: Value },
    Gte { field: String, value: Value },

    // --- Reference traversal ---
    /// `field → condition`: the field points to an entry that satisfies `condition`.
    Follows {
        field: String,
        condition: Box<Query>,
    },
    /// `field →* condition`: following `field` zero or more times reaches
    /// an entry that satisfies `condition`.
    FollowsTransitive {
        field: String,
        condition: Box<Query>,
    },
}
