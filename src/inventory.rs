//! Read-only Docker inventory adapter.

use crate::config::Config;
use crate::plan::{InventoryItem, ResourceKind, ResourceState};
use bollard::errors::Error as BollardError;
use bollard::models::{ContainerInspectResponse, ContainerSummary, ImageSummary, Network, Volume};
use bollard::query_parameters::{
    ListContainersOptionsBuilder, ListImagesOptionsBuilder, ListVolumesOptions,
};
use bollard::Docker;
use futures_util::{stream, StreamExt, TryStreamExt};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::time::UNIX_EPOCH;

#[derive(Debug)]
pub enum InventoryError {
    Docker {
        operation: String,
        source: BollardError,
    },
    InvalidData(String),
}

impl fmt::Display for InventoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Docker { operation, source } => {
                write!(formatter, "Docker {operation} failed: {source}")
            }
            Self::InvalidData(message) => write!(formatter, "invalid Docker response: {message}"),
        }
    }
}

impl std::error::Error for InventoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Docker { source, .. } => Some(source),
            Self::InvalidData(_) => None,
        }
    }
}

/// Inventory the Docker resource types supported by the M0 planner.
///
/// All daemon requests are GET requests. Container inspection is enabled only
/// when a configured container age policy needs start or finish timestamps.
///
/// # Errors
///
/// Returns an error if Docker cannot be reached, a read request fails, or the
/// daemon omits an identifier required to construct a safe plan target.
pub async fn collect_inventory(
    inspect_container_state: bool,
) -> Result<Vec<InventoryItem>, InventoryError> {
    let docker = Docker::connect_with_defaults().map_err(|source| InventoryError::Docker {
        operation: "connection setup".to_owned(),
        source,
    })?;
    let (containers, images, volumes, networks) = read_docker_lists(&docker).await?;

    let state_snapshots = if inspect_container_state {
        inspect_container_states(&docker, &containers).await?
    } else {
        HashMap::new()
    };

    let references = References::from_containers(&containers);
    let mut inventory = container_items(&containers, &state_snapshots, inspect_container_state)?;
    inventory.extend(image_items(images, &references.images));
    inventory.extend(volume_items(volumes, &references.volumes));
    inventory.extend(network_items(networks, &references.networks)?);
    inventory.sort_by(|left, right| {
        (left.kind, &left.name, &left.id).cmp(&(right.kind, &right.name, &right.id))
    });
    Ok(inventory)
}

/// Return whether container inspection is required for configured state-age rules.
#[must_use]
pub fn needs_container_state(config: &Config) -> bool {
    config
        .rules
        .containers
        .iter()
        .any(|rule| rule.stopped_ttl.is_some() || rule.running_ttl.is_some())
}

async fn read_docker_lists(
    docker: &Docker,
) -> Result<
    (
        Vec<ContainerSummary>,
        Vec<ImageSummary>,
        Vec<Volume>,
        Vec<Network>,
    ),
    InventoryError,
> {
    let container_options = ListContainersOptionsBuilder::default()
        .all(true)
        .size(false)
        .build();
    let image_options = ListImagesOptionsBuilder::default().all(true).build();
    let containers = async {
        docker
            .list_containers(Some(container_options))
            .await
            .map_err(|source| docker_error("container list", source))
    };
    let images = async {
        docker
            .list_images(Some(image_options))
            .await
            .map_err(|source| docker_error("image list", source))
    };
    let volumes = async {
        docker
            .list_volumes(None::<ListVolumesOptions>)
            .await
            .map(|response| response.volumes.unwrap_or_default())
            .map_err(|source| docker_error("volume list", source))
    };
    let networks = async {
        docker
            .list_networks(None)
            .await
            .map_err(|source| docker_error("network list", source))
    };
    tokio::try_join!(containers, images, volumes, networks)
}

struct References {
    images: HashSet<String>,
    volumes: HashSet<String>,
    networks: HashSet<String>,
}

impl References {
    fn from_containers(containers: &[ContainerSummary]) -> Self {
        let images = containers
            .iter()
            .filter_map(|container| container.image_id.clone())
            .collect();
        let volumes = containers
            .iter()
            .flat_map(|container| container.mounts.iter().flatten())
            .filter_map(|mount| mount.name.clone())
            .collect();
        let networks = containers
            .iter()
            .filter_map(|container| container.network_settings.as_ref())
            .filter_map(|settings| settings.networks.as_ref())
            .flat_map(|networks| networks.keys().cloned())
            .collect();
        Self {
            images,
            volumes,
            networks,
        }
    }
}

fn container_items(
    containers: &[ContainerSummary],
    state_snapshots: &HashMap<String, ContainerStateSnapshot>,
    state_required: bool,
) -> Result<Vec<InventoryItem>, InventoryError> {
    let mut inventory = Vec::with_capacity(containers.len());
    for container in containers {
        let id = container.id.clone().ok_or_else(|| {
            InventoryError::InvalidData("container list entry has no ID".to_owned())
        })?;
        if state_required && !state_snapshots.contains_key(&id) {
            continue;
        }
        let mut names = container
            .names
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|name| name.trim_start_matches('/').to_owned())
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        let name = names.first().cloned().unwrap_or_else(|| short_id(&id));
        let mut search_names = names;
        search_names.push(id.clone());
        let summary_state = container
            .state
            .map_or_else(|| "unknown".to_owned(), |state| state.to_string());
        let snapshot = state_snapshots.get(&id);
        let state = snapshot.map_or_else(
            || resource_state(&summary_state),
            |snapshot| resource_state(&snapshot.state),
        );
        let state_since = snapshot.and_then(|snapshot| snapshot.since);

        inventory.push(InventoryItem {
            kind: ResourceKind::Container,
            id,
            name,
            search_names,
            labels: to_btree(container.labels.clone().unwrap_or_default()),
            state,
            created_at: container.created,
            state_since,
            size: nonnegative(container.size_root_fs),
            referenced: false,
            dangling: false,
            system: false,
        });
    }
    Ok(inventory)
}

fn image_items(
    images: Vec<ImageSummary>,
    referenced_images: &HashSet<String>,
) -> Vec<InventoryItem> {
    let mut inventory = Vec::with_capacity(images.len());
    for image in images {
        let mut tags = image
            .repo_tags
            .into_iter()
            .filter(|tag| tag != "<none>:<none>")
            .collect::<Vec<_>>();
        tags.sort();
        tags.dedup();
        let dangling = tags.is_empty();
        let name = tags.first().cloned().unwrap_or_else(|| short_id(&image.id));
        let mut search_names = tags;
        search_names.push(image.id.clone());
        inventory.push(InventoryItem {
            kind: ResourceKind::Image,
            id: image.id.clone(),
            name,
            search_names,
            labels: to_btree(image.labels),
            state: ResourceState::Available,
            created_at: Some(image.created),
            state_since: None,
            size: nonnegative(Some(image.size)),
            referenced: referenced_images.contains(&image.id),
            dangling,
            system: false,
        });
    }
    inventory
}

fn volume_items(volumes: Vec<Volume>, referenced_volumes: &HashSet<String>) -> Vec<InventoryItem> {
    let mut inventory = Vec::with_capacity(volumes.len());
    for volume in volumes {
        let name = volume.name;
        inventory.push(InventoryItem {
            kind: ResourceKind::Volume,
            id: name.clone(),
            name: name.clone(),
            search_names: vec![name.clone()],
            labels: to_btree(volume.labels),
            state: ResourceState::Available,
            created_at: volume.created_at.as_deref().and_then(rfc3339_epoch),
            state_since: None,
            size: volume
                .usage_data
                .and_then(|usage| nonnegative(Some(usage.size))),
            referenced: referenced_volumes.contains(&name),
            dangling: false,
            system: false,
        });
    }
    inventory
}

fn network_items(
    networks: Vec<Network>,
    referenced_networks: &HashSet<String>,
) -> Result<Vec<InventoryItem>, InventoryError> {
    let mut inventory = Vec::with_capacity(networks.len());
    for network in networks {
        let id = network.id.ok_or_else(|| {
            InventoryError::InvalidData("network list entry has no ID".to_owned())
        })?;
        let name = network.name.unwrap_or_else(|| short_id(&id));
        let mut search_names = vec![name.clone(), id.clone()];
        search_names.sort();
        search_names.dedup();
        let system = matches!(name.as_str(), "bridge" | "host" | "none")
            || network.ingress.unwrap_or(false)
            || network.config_only.unwrap_or(false);
        inventory.push(InventoryItem {
            kind: ResourceKind::Network,
            id,
            name: name.clone(),
            search_names,
            labels: to_btree(network.labels.unwrap_or_default()),
            state: ResourceState::Available,
            created_at: network.created.as_deref().and_then(rfc3339_epoch),
            state_since: None,
            size: None,
            referenced: referenced_networks.contains(&name),
            dangling: false,
            system,
        });
    }
    Ok(inventory)
}

fn docker_error(operation: &str, source: BollardError) -> InventoryError {
    InventoryError::Docker {
        operation: operation.to_owned(),
        source,
    }
}

#[derive(Debug)]
struct ContainerStateSnapshot {
    state: String,
    since: Option<i64>,
}

async fn inspect_container_states(
    docker: &Docker,
    containers: &[ContainerSummary],
) -> Result<HashMap<String, ContainerStateSnapshot>, InventoryError> {
    let ids = containers
        .iter()
        .filter_map(|container| container.id.clone())
        .collect::<Vec<_>>();
    let snapshots = stream::iter(ids)
        .map(|id| async move {
            match docker.inspect_container(&id, None).await {
                Ok(response) => Ok(Some((id, state_snapshot(&response)))),
                Err(BollardError::DockerResponseServerError {
                    status_code: 404, ..
                }) => Ok(None),
                Err(source) => Err(InventoryError::Docker {
                    operation: format!("container inspect {id}"),
                    source,
                }),
            }
        })
        .buffer_unordered(32)
        .try_collect::<Vec<_>>()
        .await?;
    Ok(snapshots.into_iter().flatten().collect())
}

fn state_snapshot(response: &ContainerInspectResponse) -> ContainerStateSnapshot {
    let state = response
        .state
        .as_ref()
        .and_then(|state| state.status)
        .map_or_else(|| "unknown".to_owned(), |status| status.to_string());
    let since = response.state.as_ref().and_then(|details| {
        if matches!(state.as_str(), "running" | "paused" | "restarting") {
            details.started_at.as_deref().and_then(rfc3339_epoch)
        } else if matches!(state.as_str(), "exited" | "dead") {
            details.finished_at.as_deref().and_then(rfc3339_epoch)
        } else {
            None
        }
    });
    ContainerStateSnapshot { state, since }
}

fn resource_state(value: &str) -> ResourceState {
    match value {
        "running" | "paused" | "restarting" => ResourceState::Running,
        "exited" | "dead" => ResourceState::Stopped,
        other => ResourceState::Other(other.to_owned()),
    }
}

fn rfc3339_epoch(value: &str) -> Option<i64> {
    let (normalized, offset) = normalize_rfc3339_offset(value)?;
    let local: i64 = humantime::parse_rfc3339(&normalized)
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs()
        .try_into()
        .ok()?;
    local.checked_sub(offset)
}

fn normalize_rfc3339_offset(value: &str) -> Option<(String, i64)> {
    if value.ends_with('Z') {
        return Some((value.to_owned(), 0));
    }
    let offset_start = value.len().checked_sub(6)?;
    let sign = match value.as_bytes().get(offset_start)? {
        b'+' => 1_i64,
        b'-' => -1_i64,
        _ => return None,
    };
    if value.as_bytes().get(offset_start + 3) != Some(&b':') {
        return None;
    }
    let hours = value
        .get(offset_start + 1..offset_start + 3)?
        .parse::<i64>()
        .ok()?;
    let minutes = value.get(offset_start + 4..)?.parse::<i64>().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    let local = value.get(..offset_start)?;
    Some((format!("{local}Z"), sign * (hours * 3_600 + minutes * 60)))
}

fn short_id(id: &str) -> String {
    id.strip_prefix("sha256:")
        .unwrap_or(id)
        .chars()
        .take(12)
        .collect()
}

fn nonnegative(value: Option<i64>) -> Option<u64> {
    value?.try_into().ok()
}

fn to_btree(values: HashMap<String, String>) -> BTreeMap<String, String> {
    values.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_timestamp_without_panicking_on_zero_value() {
        assert_eq!(rfc3339_epoch("1970-01-01T00:00:01Z"), Some(1));
        assert_eq!(rfc3339_epoch("1970-01-01T01:00:01+01:00"), Some(1));
        assert_eq!(rfc3339_epoch("1970-01-01T00:00:01-01:00"), Some(3_601));
        assert_eq!(rfc3339_epoch("0001-01-01T00:00:00Z"), None);
        assert_eq!(rfc3339_epoch("not-a-time"), None);
    }

    #[test]
    fn normalizes_container_states_for_policy_evaluation() {
        assert_eq!(resource_state("running"), ResourceState::Running);
        assert_eq!(resource_state("paused"), ResourceState::Running);
        assert_eq!(resource_state("exited"), ResourceState::Stopped);
        assert_eq!(
            resource_state("created"),
            ResourceState::Other("created".to_owned())
        );
    }

    #[test]
    fn short_ids_are_stable_for_digest_and_plain_ids() {
        assert_eq!(short_id("sha256:1234567890abcdef"), "1234567890ab");
        assert_eq!(short_id("abcdef"), "abcdef");
    }
}
