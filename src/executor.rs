//! Safety-critical application of an immutable cleanup plan.

use crate::config::{load_config, Config, LoadedConfig};
use crate::inventory::{collect_inventory, InventoryError};
use crate::plan::{build_plan, Action, Decision, Plan, ResourceKind, ResourceState};
use bollard::errors::Error as BollardError;
use bollard::models::BuildPruneResponse;
use bollard::query_parameters::{
    PruneBuildOptionsBuilder, RemoveContainerOptionsBuilder, RemoveImageOptionsBuilder,
    RemoveVolumeOptionsBuilder,
};
use bollard::Docker;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fmt::Write as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub enum ExecutionError {
    DockerSetup(BollardError),
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DockerSetup(source) => {
                write!(
                    formatter,
                    "Docker deletion connection setup failed: {source}"
                )
            }
        }
    }
}

impl std::error::Error for ExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::DockerSetup(source) => Some(source),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetStatus {
    Removed,
    Skipped,
    Failed,
}

impl fmt::Display for TargetStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Removed => formatter.write_str("removed"),
            Self::Skipped => formatter.write_str("skipped"),
            Self::Failed => formatter.write_str("failed"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetOutcome {
    pub kind: ResourceKind,
    pub id: String,
    pub name: String,
    pub matched_rule: String,
    pub status: TargetStatus,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionReport {
    pub outcomes: Vec<TargetOutcome>,
}

impl ExecutionReport {
    #[must_use]
    pub fn has_partial_failure(&self) -> bool {
        self.outcomes
            .iter()
            .any(|outcome| outcome.status != TargetStatus::Removed)
    }

    #[must_use]
    pub fn render_table(&self) -> String {
        if self.outcomes.is_empty() {
            return "No removals pending.\n".to_owned();
        }

        let mut rows = vec![vec![
            "TYPE".to_owned(),
            "NAME".to_owned(),
            "RULE".to_owned(),
            "RESULT".to_owned(),
            "DETAIL".to_owned(),
        ]];
        for outcome in &self.outcomes {
            rows.push(vec![
                outcome.kind.to_string(),
                outcome.name.clone(),
                outcome.matched_rule.clone(),
                outcome.status.to_string(),
                one_line(&outcome.detail),
            ]);
        }

        let widths = (0..rows[0].len())
            .map(|column| rows.iter().map(|row| row[column].len()).max().unwrap_or(0))
            .collect::<Vec<_>>();
        let mut output = String::new();
        for row in rows {
            for (column, value) in row.iter().enumerate() {
                output.push_str(value);
                if column + 1 != row.len() {
                    output.extend(std::iter::repeat_n(
                        ' ',
                        widths[column].saturating_sub(value.len()) + 2,
                    ));
                }
            }
            output.push('\n');
        }

        let removed = self.count(TargetStatus::Removed);
        let skipped = self.count(TargetStatus::Skipped);
        let failed = self.count(TargetStatus::Failed);
        output.push('\n');
        writeln!(
            output,
            "Cleanup result: removed {removed}, skipped {skipped}, failed {failed}."
        )
        .expect("writing to a String cannot fail");
        output
    }

    fn count(&self, status: TargetStatus) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| outcome.status == status)
            .count()
    }
}

/// Apply only removal targets already present in `initial_plan`.
///
/// Before each delete, this reloads the exact configuration file, rejects a
/// changed configuration, rebuilds policy from a fresh Docker inventory, and
/// requires the same target ID, disposition, and rule to remain eligible.
/// Revalidation can shrink the plan but cannot add a target.
///
/// # Errors
///
/// Returns an error only when the deletion client cannot be constructed. Once
/// target processing starts, individual revalidation and deletion failures are
/// retained in the report so one failure cannot hide successful deletions.
pub async fn execute_plan(
    config_path: &Path,
    initial_config: &Config,
    initial_source: &str,
    initial_plan: &Plan,
) -> Result<ExecutionReport, ExecutionError> {
    let docker = Docker::connect_with_defaults().map_err(ExecutionError::DockerSetup)?;
    let targets = ordered_removal_targets(initial_plan);
    let mut outcomes = Vec::with_capacity(targets.len());

    for target in targets {
        let revalidated = revalidate(config_path, initial_config, initial_source, &target).await;
        let outcome = match revalidated {
            Ok(current) => match delete_target(&docker, &current).await {
                Ok(DeleteEffect::Removed(detail)) => {
                    target_outcome(&target, TargetStatus::Removed, detail)
                }
                Ok(DeleteEffect::Skipped(detail)) => {
                    target_outcome(&target, TargetStatus::Skipped, detail)
                }
                Ok(DeleteEffect::Failed(detail)) => {
                    target_outcome(&target, TargetStatus::Failed, detail)
                }
                Err(source) => {
                    let (status, detail) = classify_delete_error(&source);
                    target_outcome(&target, status, detail)
                }
            },
            Err(detail) => target_outcome(&target, TargetStatus::Skipped, detail),
        };
        outcomes.push(outcome);
    }

    Ok(ExecutionReport { outcomes })
}

fn ordered_removal_targets(plan: &Plan) -> Vec<Decision> {
    let cache_parents = plan
        .decisions
        .iter()
        .filter(|decision| decision.resource.kind == ResourceKind::BuildCache)
        .map(|decision| {
            (
                decision.resource.id.clone(),
                decision.resource.parent_ids.clone(),
            )
        })
        .collect::<HashMap<_, _>>();
    let mut cache_depths = HashMap::new();
    for id in cache_parents.keys() {
        cache_depth(id, &cache_parents, &mut cache_depths, &mut HashSet::new());
    }

    let mut targets = plan
        .decisions
        .iter()
        .filter(|decision| decision.action == Action::Remove)
        .cloned()
        .collect::<Vec<_>>();
    targets.sort_by(|left, right| {
        left.resource
            .kind
            .cmp(&right.resource.kind)
            .then_with(|| {
                if left.resource.kind == ResourceKind::BuildCache {
                    cache_depths
                        .get(&right.resource.id)
                        .unwrap_or(&0)
                        .cmp(cache_depths.get(&left.resource.id).unwrap_or(&0))
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then_with(|| left.resource.name.cmp(&right.resource.name))
            .then_with(|| left.resource.id.cmp(&right.resource.id))
    });
    targets
}

fn cache_depth(
    id: &str,
    parents: &HashMap<String, Vec<String>>,
    memo: &mut HashMap<String, usize>,
    visiting: &mut HashSet<String>,
) -> usize {
    if let Some(depth) = memo.get(id) {
        return *depth;
    }
    if !visiting.insert(id.to_owned()) {
        return 0;
    }
    let depth = parents.get(id).map_or(0, |ids| {
        ids.iter()
            .filter(|parent| parents.contains_key(parent.as_str()))
            .map(|parent| cache_depth(parent, parents, memo, visiting).saturating_add(1))
            .max()
            .unwrap_or(0)
    });
    visiting.remove(id);
    memo.insert(id.to_owned(), depth);
    depth
}

async fn revalidate(
    config_path: &Path,
    initial_config: &Config,
    initial_source: &str,
    target: &Decision,
) -> Result<Decision, String> {
    let loaded = load_config(Some(config_path), Path::new("."), None)
        .map_err(|error| format!("revalidation could not load configuration: {error}"))?;
    if !configuration_is_unchanged(initial_config, initial_source, &loaded) {
        return Err("configuration changed after the plan was created".to_owned());
    }

    let inventory = collect_inventory(&loaded.config)
        .await
        .map_err(|error| revalidation_inventory_error(&error))?;
    let now = epoch_seconds().map_err(|error| format!("revalidation failed: {error}"))?;
    let current_plan = build_plan(&loaded.config, inventory, now)
        .map_err(|error| format!("revalidation could not rebuild the plan: {error}"))?;
    select_revalidated_target(target, &current_plan).cloned()
}

fn configuration_is_unchanged(
    initial_config: &Config,
    initial_source: &str,
    current: &LoadedConfig,
) -> bool {
    current.source == initial_source && current.config == *initial_config
}

fn select_revalidated_target<'a>(
    target: &Decision,
    current_plan: &'a Plan,
) -> Result<&'a Decision, String> {
    let Some(current) = current_plan.decisions.iter().find(|decision| {
        decision.resource.kind == target.resource.kind && decision.resource.id == target.resource.id
    }) else {
        return Err("resource disappeared before deletion".to_owned());
    };

    if current.action != Action::Remove {
        return Err(format!("resource became ineligible: {}", current.reason));
    }
    if current.disposition != target.disposition || current.matched_rule != target.matched_rule {
        return Err("resource no longer matches the same authorized rule".to_owned());
    }
    Ok(current)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DeleteEffect {
    Removed(String),
    Skipped(String),
    Failed(String),
}

async fn delete_target(docker: &Docker, target: &Decision) -> Result<DeleteEffect, BollardError> {
    match target.resource.kind {
        ResourceKind::Container => {
            let options = RemoveContainerOptionsBuilder::default()
                .force(target.resource.state == ResourceState::Running)
                .v(false)
                .link(false)
                .build();
            docker
                .remove_container(&target.resource.id, Some(options))
                .await
                .map(|()| DeleteEffect::Removed("removed".to_owned()))
        }
        ResourceKind::Image => {
            let options = RemoveImageOptionsBuilder::default()
                .force(false)
                .noprune(true)
                .build();
            docker
                .remove_image(&target.resource.id, Some(options), None)
                .await
                .map(|_| DeleteEffect::Removed("removed".to_owned()))
        }
        ResourceKind::Volume => {
            let options = RemoveVolumeOptionsBuilder::default().force(false).build();
            docker
                .remove_volume(&target.resource.id, Some(options))
                .await
                .map(|()| DeleteEffect::Removed("removed".to_owned()))
        }
        ResourceKind::Network => docker
            .remove_network(&target.resource.id)
            .await
            .map(|()| DeleteEffect::Removed("removed".to_owned())),
        ResourceKind::BuildCache => {
            let filters = HashMap::from([("id", vec![target.resource.id.clone()])]);
            let options = PruneBuildOptionsBuilder::default()
                .all(true)
                .filters(&filters)
                .build();
            docker
                .prune_build(Some(options))
                .await
                .map(|response| classify_prune_response(&target.resource.id, &response))
        }
    }
}

fn classify_prune_response(target_id: &str, response: &BuildPruneResponse) -> DeleteEffect {
    let deleted = response.caches_deleted.as_deref().unwrap_or_default();
    let unexpected = deleted
        .iter()
        .filter(|id| id.as_str() != target_id)
        .cloned()
        .collect::<Vec<_>>();
    if !unexpected.is_empty() {
        return DeleteEffect::Failed(format!(
            "Docker reported unexpected build-cache deletions: {}",
            unexpected.join(", ")
        ));
    }
    if deleted.iter().any(|id| id == target_id) {
        return DeleteEffect::Removed(format!(
            "removed; reclaimed {} bytes",
            response.space_reclaimed.unwrap_or(0).max(0)
        ));
    }
    DeleteEffect::Skipped("Docker did not delete the build-cache record".to_owned())
}

fn classify_delete_error(source: &BollardError) -> (TargetStatus, String) {
    let status = match &source {
        BollardError::DockerResponseServerError {
            status_code: 404 | 409,
            ..
        } => TargetStatus::Skipped,
        _ => TargetStatus::Failed,
    };
    (status, format!("Docker deletion failed: {source}"))
}

fn target_outcome(
    target: &Decision,
    status: TargetStatus,
    detail: impl Into<String>,
) -> TargetOutcome {
    TargetOutcome {
        kind: target.resource.kind,
        id: target.resource.id.clone(),
        name: target.resource.name.clone(),
        matched_rule: target
            .matched_rule
            .clone()
            .unwrap_or_else(|| "-".to_owned()),
        status,
        detail: detail.into(),
    }
}

fn revalidation_inventory_error(error: &InventoryError) -> String {
    format!("revalidation inventory failed: {error}")
}

fn epoch_seconds() -> Result<i64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before 1970: {error}"))?
        .as_secs()
        .try_into()
        .map_err(|error| format!("system clock is out of range: {error}"))
}

fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Disposition, InventoryItem, ResourceState};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn decision(action: Action, referenced: bool, rule: &str) -> Decision {
        Decision {
            resource: InventoryItem {
                kind: ResourceKind::Volume,
                id: "volume-1".to_owned(),
                name: "volume-1".to_owned(),
                search_names: vec!["volume-1".to_owned()],
                parent_ids: Vec::new(),
                labels: BTreeMap::new(),
                state: ResourceState::Available,
                created_at: Some(1),
                state_since: None,
                size: None,
                referenced,
                dangling: false,
                system: false,
            },
            disposition: Disposition::Owned,
            matched_rule: Some(rule.to_owned()),
            action,
            age_seconds: Some(10),
            reason: "test decision".to_owned(),
        }
    }

    #[test]
    fn exact_target_remains_eligible() {
        let original = decision(Action::Remove, false, "workspaces");
        let current = original.clone();
        let plan = Plan {
            decisions: vec![current],
        };

        let selected = select_revalidated_target(&original, &plan).expect("eligible target");
        assert_eq!(selected.resource.id, "volume-1");
    }

    #[test]
    fn ineligible_target_is_skipped() {
        let original = decision(Action::Remove, false, "workspaces");
        let mut current = decision(Action::Keep, true, "workspaces");
        current.reason = "volume is referenced by a container".to_owned();
        let error = select_revalidated_target(
            &original,
            &Plan {
                decisions: vec![current],
            },
        )
        .expect_err("referenced target must be skipped");

        assert!(error.contains("became ineligible"));
        assert!(error.contains("referenced"));
    }

    #[test]
    fn changed_rule_does_not_authorize_original_target() {
        let original = decision(Action::Remove, false, "workspaces");
        let current = decision(Action::Remove, false, "fallback");
        let error = select_revalidated_target(
            &original,
            &Plan {
                decisions: vec![current],
            },
        )
        .expect_err("a different rule must not authorize the immutable target");

        assert!(error.contains("same authorized rule"));
    }

    #[test]
    fn report_marks_skips_as_partial_failure() {
        let report = ExecutionReport {
            outcomes: vec![
                target_outcome(
                    &decision(Action::Remove, false, "workspaces"),
                    TargetStatus::Removed,
                    "removed",
                ),
                TargetOutcome {
                    kind: ResourceKind::Network,
                    id: "network-1".to_owned(),
                    name: "network-1".to_owned(),
                    matched_rule: "networks".to_owned(),
                    status: TargetStatus::Skipped,
                    detail: "resource became referenced".to_owned(),
                },
            ],
        };

        assert!(report.has_partial_failure());
        let output = report.render_table();
        assert!(output.contains("Cleanup result: removed 1, skipped 1, failed 0."));
    }

    #[test]
    fn comment_only_config_change_invalidates_the_plan() {
        let initial_config = Config::default();
        let current = LoadedConfig {
            path: PathBuf::from("config.toml"),
            config: initial_config.clone(),
            source: "# comment added after planning\n".to_owned(),
        };

        assert!(!configuration_is_unchanged(&initial_config, "", &current));
    }

    #[test]
    fn build_cache_prune_response_must_name_only_the_exact_target() {
        let removed = classify_prune_response(
            "cache-1",
            &BuildPruneResponse {
                caches_deleted: Some(vec!["cache-1".to_owned()]),
                space_reclaimed: Some(42),
            },
        );
        assert_eq!(
            removed,
            DeleteEffect::Removed("removed; reclaimed 42 bytes".to_owned())
        );

        let none = classify_prune_response("cache-1", &BuildPruneResponse::default());
        assert!(matches!(none, DeleteEffect::Skipped(_)));

        let unexpected = classify_prune_response(
            "cache-1",
            &BuildPruneResponse {
                caches_deleted: Some(vec!["cache-1".to_owned(), "cache-2".to_owned()]),
                space_reclaimed: Some(42),
            },
        );
        assert!(matches!(unexpected, DeleteEffect::Failed(_)));
    }

    #[test]
    fn build_cache_targets_are_ordered_child_before_parent() {
        let mut parent = decision(Action::Remove, false, "build-cache");
        parent.resource.kind = ResourceKind::BuildCache;
        parent.resource.id = "parent".to_owned();
        parent.resource.name = "parent".to_owned();
        let mut child = decision(Action::Remove, false, "build-cache");
        child.resource.kind = ResourceKind::BuildCache;
        child.resource.id = "child".to_owned();
        child.resource.name = "child".to_owned();
        child.resource.parent_ids = vec!["parent".to_owned()];

        let targets = ordered_removal_targets(&Plan {
            decisions: vec![parent, child],
        });
        let ids = targets
            .iter()
            .map(|target| target.resource.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["child", "parent"]);
    }
}
