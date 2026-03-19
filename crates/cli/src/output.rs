use metafolder_core::entry::{Metadata, Value};
use uuid::Uuid;

use crate::http::RepoInfo;

pub fn print_uuids(uuids: &[Uuid]) {
    for u in uuids {
        println!("{u}");
    }
}

pub fn print_metadata(m: &Metadata) {
    println!("uuid:  {}", m.uuid);
    println!("db_id: {}", m.db_id);
    for f in &m.fields {
        println!("  {}: {}", f.name, format_value(&f.value));
    }
}

pub fn print_repos(repos: &[RepoInfo]) {
    for r in repos {
        println!("{} — {}", r.repo_uuid, r.root.display());
    }
}

pub fn print_reconcile(created: usize, cleared: usize) {
    println!("created: {created}  cleared: {cleared}");
}

fn format_value(v: &Value) -> String {
    match v {
        Value::Nothing => "nothing".to_string(),
        Value::String(s) => format!("string({s})"),
        Value::Int(n) => format!("int({n})"),
        Value::Float(f) => format!("float({f})"),
        Value::Bool(b) => format!("bool({b})"),
        Value::Date(s) => format!("date({s})"),
        Value::DateTime(s) => format!("datetime({s})"),
        Value::Duration(ms) => format!("duration({ms}ms)"),
        Value::Ref(id) => format!("ref({id})"),
    }
}
