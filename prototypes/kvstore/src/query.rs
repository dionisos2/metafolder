//! The query shapes the prototype measures — a subset of the daemon's IR,
//! chosen to cover each access path of docs/spec-storage.org once.

use uuid::Uuid;

use crate::model::Value;

#[derive(Debug, Clone)]
pub enum Q {
    All,
    /// ≥1 non-`Nothing` row of the field.
    Present(String),
    /// ≥1 `Nothing` row of the field.
    Absent(String),
    /// Some row equal to the value (never a `Tree`: use `Child`/`Under`).
    Eq(String, Value),
    /// Some row of the bounds' type within `[lo, hi]` (either may be open;
    /// at least one is given, and both have the same type).
    Range {
        field: String,
        lo: Option<Value>,
        hi: Option<Value>,
    },
    /// Records whose position in the field's forest is directly under `node`
    /// (`model::ROOT` for the roots).
    Child {
        field: String,
        node: Uuid,
    },
    /// Records strictly below `node` in the field's forest (`ROOT`: every
    /// record placed in it).
    Under {
        field: String,
        node: Uuid,
    },
    /// Some string (or tree name) of the field contains `text`, ignoring case.
    Contains {
        field: String,
        text: String,
    },
    /// Some string (or tree name) of the field matches the regex.
    Regex {
        field: String,
        pattern: String,
    },
    And(Vec<Q>),
    Or(Vec<Q>),
    Not(Box<Q>),
}

#[derive(Debug, Clone)]
pub enum Sort {
    /// Creation order.
    None,
    /// On the field's ordered values: ascending by each record's smallest
    /// value, descending by its largest; ties by creation order (reversed when
    /// descending). Records with no sortable value come last, in creation
    /// order.
    Field { field: String, desc: bool },
    /// On the whole path in the field's forest, component by component (a
    /// directory before its contents). Records not in the forest come last.
    Path { field: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub uuids: Vec<Uuid>,
    pub count: u64,
}
