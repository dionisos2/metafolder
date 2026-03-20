use anyhow::bail;
use uuid::Uuid;

use metafolder_core::entry::Value;
use metafolder_core::query::Query;

// ── Public API ────────────────────────────────────────────────────────────────

pub struct CompiledQuery {
    pub sql: String,
    pub params: Vec<rusqlite::types::Value>,
}

/// Compiles a `Query` into a SQL string (WITH CTEs) plus bound parameters.
/// The result can be passed directly to `db::query_entries`.
pub fn compile(q: &Query, db_id: Uuid) -> anyhow::Result<CompiledQuery> {
    let mut compiler = Compiler::new();
    let last_name = compiler.compile_node(q)?;

    // db_id blob for the final isolation filter
    compiler
        .params
        .push(rusqlite::types::Value::Blob(db_id.as_bytes().to_vec()));

    let recursive_kw = if compiler.has_recursive { "RECURSIVE " } else { "" };

    let cte_parts: Vec<String> = compiler
        .ctes
        .iter()
        .map(|(ref_name, body, is_rec)| {
            if *is_rec {
                format!("{ref_name}(uuid) AS ({body})")
            } else {
                format!("{ref_name} AS ({body})")
            }
        })
        .collect();

    let with_clause = cte_parts.join(", ");
    let sql = format!(
        "WITH {recursive_kw}{with_clause} \
         SELECT uuid FROM {last_name} \
         WHERE uuid IN (SELECT uuid FROM metadata WHERE db_id = ?)"
    );

    Ok(CompiledQuery {
        sql,
        params: compiler.params,
    })
}

// ── Compiler internals ────────────────────────────────────────────────────────

struct Compiler {
    /// (ref_name, body, is_recursive_cte)
    ctes: Vec<(String, String, bool)>,
    params: Vec<rusqlite::types::Value>,
    counter: usize,
    has_recursive: bool,
}

impl Compiler {
    fn new() -> Self {
        Self {
            ctes: Vec::new(),
            params: Vec::new(),
            counter: 0,
            has_recursive: false,
        }
    }

    fn fresh_name(&mut self) -> String {
        let name = format!("_q{}", self.counter);
        self.counter += 1;
        name
    }

    fn push_text(&mut self, s: &str) {
        self.params
            .push(rusqlite::types::Value::Text(s.to_string()));
    }

    fn push_int(&mut self, n: i64) {
        self.params.push(rusqlite::types::Value::Integer(n));
    }

    fn push_real(&mut self, f: f64) {
        self.params.push(rusqlite::types::Value::Real(f));
    }

    fn push_blob(&mut self, b: Vec<u8>) {
        self.params.push(rusqlite::types::Value::Blob(b));
    }

    fn add_cte(&mut self, name: String, body: String) {
        self.ctes.push((name, body, false));
    }

    fn add_recursive_cte(&mut self, name: String, body: String) {
        self.has_recursive = true;
        self.ctes.push((name, body, true));
    }

    /// Recursively compiles `q`, appending CTEs and params in order.
    /// Returns the reference name of the CTE representing this node's result.
    fn compile_node(&mut self, q: &Query) -> anyhow::Result<String> {
        match q {
            Query::IsPresent { field } => {
                let name = self.fresh_name();
                self.push_text(field);
                self.add_cte(
                    name.clone(),
                    "SELECT metadata_uuid AS uuid FROM field \
                     WHERE field_name = ? AND value_type != 'nothing'"
                        .to_string(),
                );
                Ok(name)
            }

            Query::IsAbsent { field } => {
                let name = self.fresh_name();
                self.push_text(field);
                self.add_cte(
                    name.clone(),
                    "SELECT metadata_uuid AS uuid FROM field \
                     WHERE field_name = ? AND value_type = 'nothing'"
                        .to_string(),
                );
                Ok(name)
            }

            Query::IsUnknown { field } => {
                let name = self.fresh_name();
                self.push_text(field);
                self.add_cte(
                    name.clone(),
                    "SELECT uuid FROM metadata WHERE uuid NOT IN \
                     (SELECT metadata_uuid FROM field WHERE field_name = ?)"
                        .to_string(),
                );
                Ok(name)
            }

            Query::Eq { field, value } => self.compile_comparison(field, value, "="),
            Query::Neq { field, value } => self.compile_comparison(field, value, "!="),
            Query::Lt { field, value } => self.compile_comparison(field, value, "<"),
            Query::Lte { field, value } => self.compile_comparison(field, value, "<="),
            Query::Gt { field, value } => self.compile_comparison(field, value, ">"),
            Query::Gte { field, value } => self.compile_comparison(field, value, ">="),

            Query::And { operands } => {
                if operands.is_empty() {
                    bail!("And with no operands");
                }
                let child_names: Vec<String> = operands
                    .iter()
                    .map(|op| self.compile_node(op))
                    .collect::<anyhow::Result<_>>()?;
                let name = self.fresh_name();
                let body = child_names
                    .iter()
                    .map(|n| format!("SELECT uuid FROM {n}"))
                    .collect::<Vec<_>>()
                    .join(" INTERSECT ");
                self.add_cte(name.clone(), body);
                Ok(name)
            }

            Query::Or { operands } => {
                if operands.is_empty() {
                    bail!("Or with no operands");
                }
                let child_names: Vec<String> = operands
                    .iter()
                    .map(|op| self.compile_node(op))
                    .collect::<anyhow::Result<_>>()?;
                let name = self.fresh_name();
                let body = child_names
                    .iter()
                    .map(|n| format!("SELECT uuid FROM {n}"))
                    .collect::<Vec<_>>()
                    .join(" UNION ");
                self.add_cte(name.clone(), body);
                Ok(name)
            }

            Query::Not { operand } => {
                let inner = self.compile_node(operand)?;
                let name = self.fresh_name();
                self.add_cte(
                    name.clone(),
                    format!(
                        "SELECT uuid FROM metadata \
                         WHERE uuid NOT IN (SELECT uuid FROM {inner})"
                    ),
                );
                Ok(name)
            }

            Query::Follows { field, condition } => {
                let cond_name = self.compile_node(condition)?;
                let name = self.fresh_name();
                self.push_text(field);
                self.add_cte(
                    name.clone(),
                    format!(
                        "SELECT f.metadata_uuid AS uuid FROM field f \
                         WHERE f.field_name = ? AND f.value_type = 'ref' \
                         AND f.value_ref IN (SELECT uuid FROM {cond_name})"
                    ),
                );
                Ok(name)
            }

            Query::Matches { field, pattern } => {
                let name = self.fresh_name();
                self.push_text(field);
                self.push_text(pattern);
                self.add_cte(
                    name.clone(),
                    "SELECT metadata_uuid AS uuid FROM field \
                     WHERE field_name = ? AND value_type = 'string' AND value_str REGEXP ?"
                        .to_string(),
                );
                Ok(name)
            }

            Query::FollowsTransitive { field, condition } => {
                let cond_name = self.compile_node(condition)?;
                let reach_name = self.fresh_name();
                self.push_text(field);
                // Backward reachability recursive CTE
                let body = format!(
                    "SELECT uuid FROM {cond_name} \
                     UNION \
                     SELECT f.metadata_uuid FROM field f \
                     JOIN {reach_name} r ON f.value_ref = r.uuid \
                     WHERE f.field_name = ? AND f.value_type = 'ref'"
                );
                self.add_recursive_cte(reach_name.clone(), body);
                Ok(reach_name)
            }
        }
    }

    fn compile_comparison(
        &mut self,
        field: &str,
        value: &Value,
        op: &str,
    ) -> anyhow::Result<String> {
        let name = self.fresh_name();
        self.push_text(field);

        let (type_str, col) = match value {
            Value::String(s) => {
                self.push_text(s);
                ("string", "value_str")
            }
            Value::Int(n) => {
                self.push_int(*n);
                ("int", "value_int")
            }
            Value::Float(f) => {
                self.push_real(*f);
                ("float", "value_real")
            }
            Value::Bool(b) => {
                self.push_int(*b as i64);
                ("bool", "value_int")
            }
            Value::Date(s) => {
                self.push_text(s);
                ("date", "value_str")
            }
            Value::DateTime(s) => {
                self.push_text(s);
                ("datetime", "value_str")
            }
            Value::Duration(ms) => {
                self.push_int(*ms);
                ("duration", "value_int")
            }
            Value::Ref(id) => {
                self.push_blob(id.as_bytes().to_vec());
                ("ref", "value_ref")
            }
            Value::Nothing => bail!("Cannot compare with Nothing"),
        };

        self.add_cte(
            name.clone(),
            format!(
                "SELECT metadata_uuid AS uuid FROM field \
                 WHERE field_name = ? AND value_type = '{type_str}' AND {col} {op} ?"
            ),
        );
        Ok(name)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use metafolder_core::entry::Field;
    use rusqlite::Connection;

    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();
        conn
    }

    fn make(conn: &Connection, db_id: Uuid, fields: Vec<(&str, Value)>) -> Uuid {
        let fields: Vec<Field> = fields
            .into_iter()
            .map(|(n, v)| Field { name: n.to_string(), value: v })
            .collect();
        crate::db::create_entry(conn, db_id, fields).unwrap().uuid
    }

    fn run(conn: &Connection, q: &Query, db_id: Uuid) -> Vec<Uuid> {
        let compiled = compile(q, db_id).unwrap();
        let mut result = crate::db::query_entries(conn, &compiled.sql, &compiled.params).unwrap();
        result.sort();
        result
    }

    fn sorted(mut v: Vec<Uuid>) -> Vec<Uuid> {
        v.sort();
        v
    }

    // ── IsPresent / IsAbsent / IsUnknown ─────────────────────────────────────

    #[test]
    fn test_compile_is_present() {
        let conn = test_db();
        let db = Uuid::new_v4();
        let e1 = make(&conn, db, vec![("path", Value::String("/a".into()))]);
        let _e2 = make(&conn, db, vec![("path", Value::Nothing)]);
        let _e3 = make(&conn, db, vec![]);

        let q = Query::IsPresent { field: "path".into() };
        assert_eq!(run(&conn, &q, db), sorted(vec![e1]));
    }

    #[test]
    fn test_compile_is_absent() {
        let conn = test_db();
        let db = Uuid::new_v4();
        let _e1 = make(&conn, db, vec![("path", Value::String("/a".into()))]);
        let e2 = make(&conn, db, vec![("path", Value::Nothing)]);
        let _e3 = make(&conn, db, vec![]);

        let q = Query::IsAbsent { field: "path".into() };
        assert_eq!(run(&conn, &q, db), sorted(vec![e2]));
    }

    #[test]
    fn test_compile_is_unknown() {
        let conn = test_db();
        let db = Uuid::new_v4();
        let _e1 = make(&conn, db, vec![("path", Value::String("/a".into()))]);
        let _e2 = make(&conn, db, vec![("path", Value::Nothing)]);
        let e3 = make(&conn, db, vec![]);

        let q = Query::IsUnknown { field: "path".into() };
        assert_eq!(run(&conn, &q, db), sorted(vec![e3]));
    }

    // ── Eq ───────────────────────────────────────────────────────────────────

    #[test]
    fn test_compile_eq_string() {
        let conn = test_db();
        let db = Uuid::new_v4();
        let e1 = make(&conn, db, vec![("label", Value::String("jazz".into()))]);
        let _e2 = make(&conn, db, vec![("label", Value::String("rock".into()))]);

        let q = Query::Eq { field: "label".into(), value: Value::String("jazz".into()) };
        assert_eq!(run(&conn, &q, db), sorted(vec![e1]));
    }

    #[test]
    fn test_compile_eq_int() {
        let conn = test_db();
        let db = Uuid::new_v4();
        let e1 = make(&conn, db, vec![("rating", Value::Int(5))]);
        let _e2 = make(&conn, db, vec![("rating", Value::Int(3))]);

        let q = Query::Eq { field: "rating".into(), value: Value::Int(5) };
        assert_eq!(run(&conn, &q, db), sorted(vec![e1]));
    }

    // ── Gt ───────────────────────────────────────────────────────────────────

    #[test]
    fn test_compile_gt_int() {
        let conn = test_db();
        let db = Uuid::new_v4();
        let _e1 = make(&conn, db, vec![("rating", Value::Int(2))]);
        let e2 = make(&conn, db, vec![("rating", Value::Int(4))]);
        let e3 = make(&conn, db, vec![("rating", Value::Int(6))]);

        let q = Query::Gt { field: "rating".into(), value: Value::Int(3) };
        assert_eq!(run(&conn, &q, db), sorted(vec![e2, e3]));
    }

    // ── And / Or / Not ───────────────────────────────────────────────────────

    #[test]
    fn test_compile_and() {
        let conn = test_db();
        let db = Uuid::new_v4();
        let e1 = make(&conn, db, vec![
            ("path", Value::String("/a".into())),
            ("rating", Value::Int(5)),
        ]);
        let _e2 = make(&conn, db, vec![("path", Value::String("/b".into()))]);
        let _e3 = make(&conn, db, vec![("rating", Value::Int(5))]);

        let q = Query::And {
            operands: vec![
                Query::IsPresent { field: "path".into() },
                Query::IsPresent { field: "rating".into() },
            ],
        };
        assert_eq!(run(&conn, &q, db), sorted(vec![e1]));
    }

    #[test]
    fn test_compile_or() {
        let conn = test_db();
        let db = Uuid::new_v4();
        let e1 = make(&conn, db, vec![("path", Value::String("/a".into()))]);
        let e2 = make(&conn, db, vec![("rating", Value::Int(5))]);
        let _e3 = make(&conn, db, vec![]);

        let q = Query::Or {
            operands: vec![
                Query::IsPresent { field: "path".into() },
                Query::IsPresent { field: "rating".into() },
            ],
        };
        assert_eq!(run(&conn, &q, db), sorted(vec![e1, e2]));
    }

    #[test]
    fn test_compile_not() {
        let conn = test_db();
        let db = Uuid::new_v4();
        let _e1 = make(&conn, db, vec![("path", Value::String("/a".into()))]);
        let e2 = make(&conn, db, vec![]);

        let q = Query::Not {
            operand: Box::new(Query::IsPresent { field: "path".into() }),
        };
        assert_eq!(run(&conn, &q, db), sorted(vec![e2]));
    }

    // ── Follows ───────────────────────────────────────────────────────────────

    #[test]
    fn test_compile_follows() {
        let conn = test_db();
        let db = Uuid::new_v4();
        let tag_jazz = make(&conn, db, vec![("label", Value::String("jazz".into()))]);
        let file_a = make(&conn, db, vec![("tag", Value::Ref(tag_jazz))]);
        let _file_b = make(&conn, db, vec![]); // no tag

        // Find entries whose "tag" field points to an entry where label = "jazz"
        let q = Query::Follows {
            field: "tag".into(),
            condition: Box::new(Query::Eq {
                field: "label".into(),
                value: Value::String("jazz".into()),
            }),
        };
        assert_eq!(run(&conn, &q, db), sorted(vec![file_a]));
    }

    // ── FollowsTransitive ─────────────────────────────────────────────────────

    #[test]
    fn test_compile_follows_transitive() {
        let conn = test_db();
        let db = Uuid::new_v4();
        // music ← jazz (jazz.parent = music), file.tag = jazz
        let music = make(&conn, db, vec![("label", Value::String("music".into()))]);
        let jazz = make(&conn, db, vec![
            ("label", Value::String("jazz".into())),
            ("parent", Value::Ref(music)),
        ]);
        let file = make(&conn, db, vec![("tag", Value::Ref(jazz))]);

        // tag ->* (label = "music")  — but via one Follows then one FollowsTransitive
        // Simpler: just test FollowsTransitive directly on parent field
        // file.tag = jazz, jazz.parent →* music (label="music")
        // So: tag -> (parent →* (label="music"))
        let q = Query::Follows {
            field: "tag".into(),
            condition: Box::new(Query::FollowsTransitive {
                field: "parent".into(),
                condition: Box::new(Query::Eq {
                    field: "label".into(),
                    value: Value::String("music".into()),
                }),
            }),
        };
        assert_eq!(run(&conn, &q, db), sorted(vec![file]));
    }

    // ── db_id isolation ───────────────────────────────────────────────────────

    #[test]
    fn test_compile_db_id_isolation() {
        let conn = test_db();
        let db1 = Uuid::new_v4();
        let db2 = Uuid::new_v4();
        let e1 = make(&conn, db1, vec![("rating", Value::Int(5))]);
        let _e2 = make(&conn, db2, vec![("rating", Value::Int(5))]);

        let q = Query::IsPresent { field: "rating".into() };
        // Only db1's entry should be returned
        assert_eq!(run(&conn, &q, db1), sorted(vec![e1]));
    }
}
