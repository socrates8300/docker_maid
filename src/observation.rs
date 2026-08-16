//! Durable, process-safe observed-unreferenced history.
//!
//! Docker exposes no detach timestamp. Orphan and unused policies therefore
//! measure continuous *observed* unreferenced time: every pass rebuilds this
//! record from the current inventory, so an entry exists only while a resource
//! has been unreferenced without interruption. A resource seen unreferenced for
//! the first time has an age of zero and can never be removed by that pass.

use crate::plan::{InventoryItem, ResourceKind};
use crate::state::{set_private_file_permissions, StateError, StatePaths};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: u32 = 1;
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// The resource kinds whose policies use the observed-unreferenced clock.
///
/// Containers are excluded: Docker reports an exact `FinishedAt`, so their
/// policies keep using state age. Build cache is excluded: its records carry
/// their own last-use timestamp.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ObservedKind {
    Image,
    Volume,
    Network,
}

impl ObservedKind {
    #[must_use]
    pub const fn from_resource_kind(kind: ResourceKind) -> Option<Self> {
        match kind {
            ResourceKind::Image => Some(Self::Image),
            ResourceKind::Volume => Some(Self::Volume),
            ResourceKind::Network => Some(Self::Network),
            ResourceKind::Container | ResourceKind::BuildCache => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct ObservationEntry {
    pub kind: ObservedKind,
    pub id: String,
    /// Docker's creation timestamp, used to detect a reused name or ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    pub first_unreferenced_at: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObservationState {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<ObservationEntry>,
}

impl Default for ObservationState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

impl ObservationState {
    /// An observation record with no entries, usable in a `const` context.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }

    /// Fold an inventory into a new record, carrying forward every clock that
    /// has run without interruption.
    ///
    /// A resource that is referenced again, that no longer exists, or whose
    /// identity was reused does not carry its old clock forward. This is the
    /// pure core of [`ObservationStore::record`].
    #[must_use]
    pub fn folded(&self, inventory: &[InventoryItem], now_epoch_seconds: i64) -> Self {
        let mut entries = inventory
            .iter()
            .filter(|item| !item.referenced)
            .filter_map(|item| {
                let kind = ObservedKind::from_resource_kind(item.kind)?;
                let carried = self
                    .entries
                    .iter()
                    .find(|entry| entry.kind == kind && entry.id == item.id)
                    .filter(|entry| entry.created_at == item.created_at)
                    .map(|entry| entry.first_unreferenced_at);
                Some(ObservationEntry {
                    kind,
                    id: item.id.clone(),
                    created_at: item.created_at,
                    first_unreferenced_at: carried.unwrap_or(now_epoch_seconds),
                })
            })
            .collect::<Vec<_>>();
        entries.sort();
        entries.dedup();
        Self {
            schema_version: SCHEMA_VERSION,
            entries,
        }
    }

    /// Return how long a resource has been continuously observed unreferenced.
    ///
    /// Returns `None` when the resource has no record, when its identity was
    /// reused, or when the recorded time is in the future. Every `None` keeps
    /// the resource: an unmeasured age never satisfies a policy floor.
    #[must_use]
    pub fn unreferenced_age(&self, item: &InventoryItem, now_epoch_seconds: i64) -> Option<u64> {
        let kind = ObservedKind::from_resource_kind(item.kind)?;
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.kind == kind && entry.id == item.id)?;
        if entry.created_at != item.created_at {
            return None;
        }
        now_epoch_seconds
            .checked_sub(entry.first_unreferenced_at)?
            .try_into()
            .ok()
    }
}

#[derive(Debug, Clone)]
pub struct ObservationStore {
    paths: StatePaths,
}

impl ObservationStore {
    #[must_use]
    pub fn new(paths: StatePaths) -> Self {
        Self { paths }
    }

    /// Load the observed-unreferenced record without creating an empty store.
    ///
    /// # Errors
    ///
    /// Returns an error when existing state cannot be locked, read, or parsed.
    pub fn snapshot(&self) -> Result<ObservationState, StateError> {
        let path = self.paths.observation_file();
        if !path.exists() {
            return Ok(ObservationState::default());
        }
        let lock = self.open_lock()?;
        FileExt::lock_shared(&lock)
            .map_err(|source| io_error(self.paths.observation_lock(), source))?;
        read_state(&path)
    }

    /// Fold the current inventory into the durable record and return it.
    ///
    /// The record is rebuilt from this inventory in one exclusive transaction,
    /// so a resource that became referenced again loses its entry, a resource
    /// that no longer exists is dropped, and a reused identity restarts its
    /// clock at `now_epoch_seconds`.
    ///
    /// # Errors
    ///
    /// Returns an error when state cannot be locked, read, parsed, or written.
    pub fn record(
        &self,
        inventory: &[InventoryItem],
        now_epoch_seconds: i64,
    ) -> Result<ObservationState, StateError> {
        self.update(|state| *state = state.folded(inventory, now_epoch_seconds))
    }

    fn update(
        &self,
        operation: impl FnOnce(&mut ObservationState),
    ) -> Result<ObservationState, StateError> {
        self.paths.prepare_root()?;
        let lock = self.open_lock()?;
        FileExt::lock_exclusive(&lock)
            .map_err(|source| io_error(self.paths.observation_lock(), source))?;
        let mut state = read_state(&self.paths.observation_file())?;
        operation(&mut state);
        write_state_atomic(&self.paths, &state)?;
        Ok(state)
    }

    fn open_lock(&self) -> Result<File, StateError> {
        self.paths.prepare_root()?;
        let path = self.paths.observation_lock();
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| io_error(&path, source))?;
        set_private_file_permissions(&path)?;
        Ok(file)
    }
}

fn read_state(path: &Path) -> Result<ObservationState, StateError> {
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ObservationState::default())
        }
        Err(source) => return Err(io_error(path, source)),
    };
    let mut state: ObservationState =
        toml::from_str(&source).map_err(|source| StateError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    if state.schema_version != SCHEMA_VERSION {
        return Err(StateError::Invalid(format!(
            "unsupported observation state schema_version {}; expected {SCHEMA_VERSION}",
            state.schema_version
        )));
    }
    for entry in &state.entries {
        if entry.id.trim().is_empty() {
            return Err(StateError::Invalid(
                "observation entries must not have a blank id".to_owned(),
            ));
        }
    }
    state.entries.sort();
    state.entries.dedup();
    Ok(state)
}

fn write_state_atomic(paths: &StatePaths, state: &ObservationState) -> Result<(), StateError> {
    let serialized = toml::to_string_pretty(state).map_err(StateError::Serialize)?;
    let nonce = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = paths.root().join(format!(
        ".observation.toml.tmp-{}-{timestamp}-{nonce}",
        std::process::id()
    ));
    let destination = paths.observation_file();
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|source| io_error(&temporary, source))?;
        set_private_file_permissions(&temporary)?;
        file.write_all(serialized.as_bytes())
            .map_err(|source| io_error(&temporary, source))?;
        file.sync_all()
            .map_err(|source| io_error(&temporary, source))?;
        fs::rename(&temporary, &destination).map_err(|source| io_error(&destination, source))?;
        File::open(paths.root())
            .and_then(|directory| directory.sync_all())
            .map_err(|source| io_error(paths.root(), source))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn io_error(path: impl AsRef<Path>, source: io::Error) -> StateError {
    StateError::Io {
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::ResourceState;
    use std::collections::BTreeMap;

    fn item(kind: ResourceKind, id: &str, referenced: bool, created_at: i64) -> InventoryItem {
        InventoryItem {
            kind,
            id: id.to_owned(),
            name: id.to_owned(),
            search_names: vec![id.to_owned()],
            parent_ids: Vec::new(),
            labels: BTreeMap::new(),
            mounts: Vec::new(),
            state: ResourceState::Available,
            created_at: Some(created_at),
            state_since: None,
            size: None,
            referenced,
            dangling: false,
            system: false,
        }
    }

    fn store(label: &str) -> (ObservationStore, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "docker-maid-observation-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        (ObservationStore::new(StatePaths::new(root.clone())), root)
    }

    #[test]
    fn first_observation_has_zero_age_and_later_passes_accumulate() {
        let (store, root) = store("first");
        let inventory = vec![item(ResourceKind::Volume, "v1", false, 1)];

        let first = store.record(&inventory, 1_000).expect("record");
        assert_eq!(first.unreferenced_age(&inventory[0], 1_000), Some(0));

        let second = store.record(&inventory, 5_000).expect("record again");
        assert_eq!(second.unreferenced_age(&inventory[0], 5_000), Some(4_000));
        assert_eq!(second.entries.len(), 1);

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn reattachment_clears_the_entry_and_detaching_restarts_the_clock() {
        let (store, root) = store("reattach");
        let detached = vec![item(ResourceKind::Volume, "v1", false, 1)];
        let attached = vec![item(ResourceKind::Volume, "v1", true, 1)];

        store.record(&detached, 1_000).expect("first detach");
        let cleared = store.record(&attached, 2_000).expect("reattached");
        assert!(cleared.entries.is_empty());
        assert_eq!(cleared.unreferenced_age(&attached[0], 2_000), None);

        let again = store.record(&detached, 3_000).expect("detached again");
        assert_eq!(again.unreferenced_age(&detached[0], 9_000), Some(6_000));

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn a_reused_identity_restarts_the_clock_instead_of_inheriting_it() {
        let (store, root) = store("reuse");
        let original = vec![item(ResourceKind::Volume, "shared", false, 1)];
        store.record(&original, 1_000).expect("original");

        let recreated = vec![item(ResourceKind::Volume, "shared", false, 900_000)];
        let state = store.record(&recreated, 1_000_000).expect("recreated");
        assert_eq!(state.unreferenced_age(&recreated[0], 1_000_000), Some(0));

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn vanished_resources_are_compacted_and_containers_are_never_recorded() {
        let (store, root) = store("compaction");
        let present = vec![
            item(ResourceKind::Volume, "v1", false, 1),
            item(ResourceKind::Network, "n1", false, 1),
            item(ResourceKind::Container, "c1", false, 1),
            item(ResourceKind::BuildCache, "b1", false, 1),
        ];
        let state = store.record(&present, 1_000).expect("record");
        assert_eq!(state.entries.len(), 2);

        let remaining = vec![item(ResourceKind::Network, "n1", false, 1)];
        let state = store.record(&remaining, 2_000).expect("compact");
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].id, "n1");

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn state_survives_a_restart_and_stays_private() {
        let (store, root) = store("restart");
        let inventory = vec![item(ResourceKind::Image, "i1", false, 1)];
        store.record(&inventory, 1_000).expect("record");

        let reopened = ObservationStore::new(StatePaths::new(root.clone()));
        let state = reopened.snapshot().expect("snapshot");
        assert_eq!(state.unreferenced_age(&inventory[0], 4_600), Some(3_600));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(store.paths.observation_file())
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        fs::remove_dir_all(root).expect("cleanup");
    }
}
