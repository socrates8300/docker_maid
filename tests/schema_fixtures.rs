use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/schema")
}

fn parse_document(path: &Path) -> Value {
    let source = fs::read_to_string(path).expect("read schema fixture");
    let value: Value = serde_json::from_str(&source).expect("parse schema fixture");
    assert_eq!(value["schema_version"], 1, "fixture: {}", path.display());
    value
}

#[test]
fn one_shot_schema_fixtures_are_valid() {
    let plan = parse_document(&fixtures().join("plan-v1.json"));
    assert_eq!(plan["command"], "plan");
    assert!(plan["inventory"].is_object());
    assert!(plan["items"].is_array());
    assert!(plan["pending_removals"].is_u64());

    let error = parse_document(&fixtures().join("error-v1.json"));
    assert!(error["error"]["kind"].is_string());
    assert!(error["error"]["message"].is_string());
    assert!(error["error"]["details"].is_array());
}

#[test]
fn daemon_ndjson_fixture_is_valid() {
    let source = fs::read_to_string(fixtures().join("daemon-v1.ndjson"))
        .expect("read daemon schema fixture");
    let events = source
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("parse daemon event"))
        .collect::<Vec<_>>();
    assert!(!events.is_empty());
    assert!(events.iter().all(|event| event["schema_version"] == 1));
    assert!(events.iter().all(|event| event["event"].is_string()));
    assert!(events.iter().all(|event| event["timestamp"].is_i64()));
    assert!(events.iter().any(|event| event["event"] == "action"));
    assert!(events.iter().any(|event| event["event"] == "pass_summary"));
    assert_eq!(
        events.first().expect("first event")["event"],
        "daemon_started"
    );
    assert_eq!(
        events.last().expect("last event")["event"],
        "daemon_stopped"
    );
}
