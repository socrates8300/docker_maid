//! Stable schema-version 1 machine output.

use crate::activity::{CompletedPass, EventData};
use crate::config::Config;
use crate::executor::{ExecutionReport, TargetStatus};
use crate::plan::Plan;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const SCHEMA_VERSION: u32 = 1;

#[must_use]
pub fn plan_document(
    command: &str,
    applied: bool,
    plan: &Plan,
    report: Option<&ExecutionReport>,
) -> Value {
    let mut document = json!({
        "schema_version": SCHEMA_VERSION,
        "command": command,
        "applied": applied,
        "pending_removals": plan.pending_count(),
        "inventory": inventory_summary(plan),
        "items": plan_items(plan),
    });
    if let Some(report) = report {
        document["result"] = execution_result(report);
    }
    document
}

#[must_use]
pub fn status_document(
    config_path: &str,
    config_hash: &str,
    config: &Config,
    plan: &Plan,
    runtime_protection_count: usize,
    last: Option<&CompletedPass>,
) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "command": "status",
        "configuration": config_summary(config_path, config_hash, config),
        "inventory": inventory_summary(plan),
        "items": plan_items(plan),
        "disk_usage": disk_usage(plan),
        "runtime_protection_entries": runtime_protection_count,
        "rule_health": rule_health(plan, last),
        "last_completed_pass": last.map(completed_pass),
    })
}

#[must_use]
pub fn protection_document(command: &str, kind: &str, changed: usize, total: usize) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "command": command,
        "resource_kind": kind,
        "changed": changed,
        "total_runtime_protection_entries": total,
    })
}

#[must_use]
pub fn config_document(command: &str, path: Option<&str>, config: &Config) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "command": command,
        "path": path,
        "configuration": config,
    })
}

#[must_use]
pub fn version_document() -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "command": "version",
        "version": env!("CARGO_PKG_VERSION"),
    })
}

#[must_use]
pub fn error_document(kind: &str, message: &str) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "error": {
            "kind": kind,
            "message": message,
            "details": [],
        }
    })
}

#[must_use]
pub fn warning_document(kind: &str, message: &str) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "warning": {
            "kind": kind,
            "message": message,
        }
    })
}

#[must_use]
pub fn daemon_event(event: &str, timestamp: i64, fields: Value) -> Value {
    let mut value = json!({
        "schema_version": SCHEMA_VERSION,
        "event": event,
        "timestamp": timestamp,
    });
    if let (Some(destination), Value::Object(source)) = (value.as_object_mut(), fields) {
        for (key, field) in source {
            destination.insert(key, field);
        }
    }
    value
}

#[must_use]
pub fn daemon_pass_started_event(
    pass_number: u64,
    trigger: &str,
    applied: bool,
    timestamp: i64,
) -> Value {
    let mode = if applied { "apply" } else { "dry-run" };
    daemon_event(
        "pass_started",
        timestamp,
        json!({"pass_number": pass_number, "trigger": trigger, "mode": mode}),
    )
}

#[must_use]
pub fn daemon_pass_result_events(
    pass_number: u64,
    applied: bool,
    plan: &Plan,
    report: Option<&ExecutionReport>,
    timestamp: i64,
) -> Vec<Value> {
    let mode = if applied { "apply" } else { "dry-run" };
    let mut events = vec![daemon_event(
        "plan",
        timestamp,
        json!({
            "pass_number": pass_number,
            "pending_removals": plan.pending_count(),
            "inventory": inventory_summary(plan),
            "items": plan_items(plan),
        }),
    )];
    if let Some(report) = report {
        for outcome in &report.outcomes {
            events.push(daemon_event(
                "action",
                timestamp,
                json!({
                    "pass_number": pass_number,
                    "action": "remove",
                    "resource_kind": outcome.kind.to_string(),
                    "resource_id": outcome.id,
                    "resource_name": outcome.name,
                    "matched_rule": outcome.matched_rule,
                    "result": outcome.status.to_string(),
                    "detail": outcome.detail,
                }),
            ));
        }
    }
    let result = report.map_or_else(empty_result, execution_result);
    events.push(daemon_event(
        "pass_summary",
        timestamp,
        json!({
            "pass_number": pass_number,
            "mode": mode,
            "pending_removals": plan.pending_count(),
            "result": result,
        }),
    ));
    events
}

/// Serialize one schema document or event as one newline-delimited record.
///
/// # Errors
///
/// Returns an error if the JSON value cannot be serialized.
pub fn to_line(value: &Value) -> Result<Vec<u8>, serde_json::Error> {
    let mut output = serde_json::to_vec(value)?;
    output.push(b'\n');
    Ok(output)
}

fn plan_items(plan: &Plan) -> Vec<Value> {
    plan.decisions
        .iter()
        .map(|decision| {
            json!({
                "resource_kind": decision.resource.kind.to_string(),
                "id": decision.resource.id,
                "name": decision.resource.name,
                "state": decision.resource.state.to_string(),
                "disposition": decision.disposition.to_string(),
                "action": decision.action.to_string(),
                "matched_rule": decision.matched_rule,
                "age_seconds": decision.age_seconds,
                "size_bytes": decision.resource.size,
                "labels": decision.resource.labels,
                "mounts": decision.resource.mounts,
                "referenced": decision.resource.referenced,
                "dangling": decision.resource.dangling,
                "system": decision.resource.system,
                "reason": decision.reason,
            })
        })
        .collect()
}

fn inventory_summary(plan: &Plan) -> Value {
    let mut resource_counts = BTreeMap::<String, usize>::new();
    let mut disposition_counts = BTreeMap::<String, usize>::new();
    for decision in &plan.decisions {
        *resource_counts
            .entry(decision.resource.kind.to_string())
            .or_default() += 1;
        *disposition_counts
            .entry(decision.disposition.to_string())
            .or_default() += 1;
    }
    json!({
        "total": plan.decisions.len(),
        "pending_removals": plan.pending_count(),
        "by_resource_kind": resource_counts,
        "by_disposition": disposition_counts,
    })
}

fn disk_usage(plan: &Plan) -> Value {
    let mut known_bytes = 0u64;
    let mut unknown_size_items = 0usize;
    let mut by_resource_kind = BTreeMap::<String, u64>::new();
    for decision in &plan.decisions {
        if let Some(size) = decision.resource.size {
            known_bytes = known_bytes.saturating_add(size);
            let total = by_resource_kind
                .entry(decision.resource.kind.to_string())
                .or_default();
            *total = total.saturating_add(size);
        } else {
            unknown_size_items = unknown_size_items.saturating_add(1);
        }
    }
    json!({
        "known_bytes": known_bytes,
        "unknown_size_items": unknown_size_items,
        "by_resource_kind": by_resource_kind,
    })
}

fn config_summary(path: &str, config_hash: &str, config: &Config) -> Value {
    json!({
        "path": path,
        "hash": config_hash,
        "interval": config.defaults.interval,
        "rules": {
            "containers": config.rules.containers.len(),
            "images": config.rules.images.len(),
            "volumes": config.rules.volumes.len(),
            "networks": config.rules.networks.len(),
            "build_cache": usize::from(config.rules.build_cache.is_some()),
        },
        "configured_protection": {
            "names": config.protect.names.len(),
            "labels": config.protect.labels.len(),
        },
    })
}

fn rule_health(plan: &Plan, last: Option<&CompletedPass>) -> Vec<Value> {
    let mut current = BTreeMap::<String, u64>::new();
    for decision in &plan.decisions {
        if let Some(rule) = &decision.matched_rule {
            *current.entry(rule.clone()).or_default() += 1;
        }
    }
    let previous = last.map_or_else(BTreeMap::new, |pass| pass.rule_match_counts.clone());
    let names = current
        .keys()
        .chain(previous.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    names
        .into_iter()
        .map(|name| {
            let current_match_count = current.get(&name).copied().unwrap_or(0);
            let previous_match_count = previous.get(&name).copied().unwrap_or(0);
            let health = if previous_match_count != 0 && current_match_count == 0 {
                "regressed"
            } else {
                "healthy"
            };
            json!({
                "rule": name,
                "current_match_count": current_match_count,
                "previous_match_count": previous_match_count,
                "health": health,
            })
        })
        .collect()
}

fn execution_result(report: &ExecutionReport) -> Value {
    let removed = report
        .outcomes
        .iter()
        .filter(|outcome| outcome.status == TargetStatus::Removed)
        .count();
    let skipped = report
        .outcomes
        .iter()
        .filter(|outcome| outcome.status == TargetStatus::Skipped)
        .count();
    let failed = report
        .outcomes
        .iter()
        .filter(|outcome| outcome.status == TargetStatus::Failed)
        .count();
    json!({
        "removed": removed,
        "skipped": skipped,
        "failed": failed,
        "outcomes": report.outcomes.iter().map(|outcome| json!({
            "resource_kind": outcome.kind.to_string(),
            "id": outcome.id,
            "name": outcome.name,
            "matched_rule": outcome.matched_rule,
            "status": outcome.status.to_string(),
            "detail": outcome.detail,
        })).collect::<Vec<_>>(),
    })
}

fn empty_result() -> Value {
    json!({"removed": 0, "skipped": 0, "failed": 0, "outcomes": []})
}

fn completed_pass(pass: &CompletedPass) -> Value {
    json!({
        "pass_id": pass.pass_id,
        "source": pass.source,
        "started_at": pass.started_at,
        "completed_at": pass.completed_at,
        "config_hash": pass.config_hash,
        "removed": pass.removed_count,
        "skipped": pass.skipped_count,
        "failed": pass.failure_count,
        "reclaimed_bytes": pass.reclaimed_bytes,
        "rule_match_counts": pass.rule_match_counts,
        "actions": pass.actions.iter().filter_map(|event| {
            let EventData::Action {
                action,
                resource_kind,
                resource_id,
                resource_name,
                matched_rule,
                age_seconds,
                freed_bytes,
                detail,
            } = &event.data else {
                return None;
            };
            Some(json!({
                "sequence": event.sequence,
                "action": action,
                "resource_kind": resource_kind,
                "resource_id": resource_id,
                "resource_name": resource_name,
                "matched_rule": matched_rule,
                "age_seconds": age_seconds,
                "freed_bytes": freed_bytes,
                "detail": detail,
            }))
        }).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_envelope_is_schema_versioned() {
        let value = error_document("configuration", "bad input");
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["error"]["kind"], "configuration");
        assert_eq!(value["error"]["message"], "bad input");
    }
}
