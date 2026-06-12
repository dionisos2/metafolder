//! The query DSL parser lives in core so that both the CLI and the GUI can
//! compile DSL strings to the `Query` IR. The full parser test suite lives
//! next to the module; this test pins the public location.

use metafolder_core::dsl::parse_query;
use metafolder_core::record::Value;
use metafolder_core::query::Query;

#[test]
fn test_parse_query_is_exposed_by_core() {
    assert_eq!(
        parse_query("rating > 3").unwrap(),
        Query::Gt { field: "rating".into(), value: Value::Int(3) }
    );
}
