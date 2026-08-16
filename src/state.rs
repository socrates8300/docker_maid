//! Durable, process-safe runtime protection state.

use crate::plan::{InventoryItem, ResourceKind};
use fs2::FileExt;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: u32 = 1;
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum StateError {
    Environment,
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    Serialize(toml::ser::Error),
    Invalid(String),
}

impl fmt::Display for StateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Environment => {
                formatter.write_str("cannot locate state directory: set XDG_STATE_HOME or HOME")
            }
            Self::Io { path, source } => {
                write!(
                    formatter,
                    "cannot access state path {}: {source}",
                    path.display()
                )
            }
            Self::Parse { path, source } => {
                write!(
                    formatter,
                    "invalid protection state {}: {source}",
                    path.display()
                )
            }
            Self::Serialize(source) => {
                write!(formatter, "cannot serialize protection state: {source}")
            }
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for StateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::Serialize(source) => Some(source),
            Self::Environment | Self::Invalid(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ProtectionKind {
    Container,
    Volume,
    Image,
    Network,
}

impl fmt::Display for ProtectionKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Container => "container",
            Self::Volume => "volume",
            Self::Image => "image",
            Self::Network => "network",
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct ProtectionEntry {
    pub kind: ProtectionKind,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProtectionState {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<ProtectionEntry>,
}

impl Default for ProtectionState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

impl ProtectionState {
    /// A protection set with no entries, usable in a `const` context.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }

    /// Return the runtime protection reason for an inventory item.
    #[must_use]
    pub fn match_reason(&self, item: &InventoryItem) -> Option<String> {
        self.matching_entry(item)
            .map(|entry| format!("matched runtime protection {} {}", entry.kind, entry.value))
    }

    /// Return the first persisted runtime entry protecting an inventory item.
    #[must_use]
    pub fn matching_entry(&self, item: &InventoryItem) -> Option<&ProtectionEntry> {
        self.entries.iter().find(|entry| entry_matches(entry, item))
    }
}

fn entry_matches(entry: &ProtectionEntry, item: &InventoryItem) -> bool {
    let kind_matches = matches!(
        (entry.kind, item.kind),
        (ProtectionKind::Container, ResourceKind::Container)
            | (ProtectionKind::Volume, ResourceKind::Volume)
            | (ProtectionKind::Image, ResourceKind::Image)
            | (ProtectionKind::Network, ResourceKind::Network)
    );
    if !kind_matches {
        return false;
    }

    if item.id == entry.value
        || item.name == entry.value
        || item.search_names.iter().any(|name| name == &entry.value)
    {
        return true;
    }

    entry.kind == ProtectionKind::Container
        && Regex::new(&entry.value).is_ok_and(|pattern| {
            pattern.is_match(&item.name)
                || item.search_names.iter().any(|name| pattern.is_match(name))
        })
}

#[derive(Debug, Clone)]
pub struct StatePaths {
    root: PathBuf,
}

impl StatePaths {
    /// Resolve the application state directory from the process environment.
    ///
    /// # Errors
    ///
    /// Returns [`StateError::Environment`] when neither state base is available.
    pub fn from_env() -> Result<Self, StateError> {
        let base = std::env::var_os("XDG_STATE_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|value| !value.is_empty())
                    .map(|home| PathBuf::from(home).join(".local/state"))
            })
            .ok_or(StateError::Environment)?;
        Ok(Self::new(base.join("docker_maid")))
    }

    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn protection_file(&self) -> PathBuf {
        self.root.join("protection.toml")
    }

    #[must_use]
    pub fn protection_lock(&self) -> PathBuf {
        self.root.join("protection.lock")
    }

    #[must_use]
    pub fn observation_file(&self) -> PathBuf {
        self.root.join("observation.toml")
    }

    #[must_use]
    pub fn observation_lock(&self) -> PathBuf {
        self.root.join("observation.lock")
    }

    #[must_use]
    pub fn activity_file(&self) -> PathBuf {
        self.root.join("activity.jsonl")
    }

    #[must_use]
    pub fn rotated_activity_file(&self) -> PathBuf {
        self.root.join("activity.1.jsonl")
    }

    #[must_use]
    pub fn activity_lock(&self) -> PathBuf {
        self.root.join("activity.lock")
    }

    pub(crate) fn prepare_root(&self) -> Result<(), StateError> {
        fs::create_dir_all(&self.root).map_err(|source| io_error(&self.root, source))?;
        set_private_directory_permissions(&self.root)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ProtectionStore {
    paths: StatePaths,
}

impl ProtectionStore {
    #[must_use]
    pub fn new(paths: StatePaths) -> Self {
        Self { paths }
    }

    /// Resolve the protection store from XDG/HOME state conventions.
    ///
    /// # Errors
    ///
    /// Returns an error when no state base can be resolved.
    pub fn from_env() -> Result<Self, StateError> {
        Ok(Self::new(StatePaths::from_env()?))
    }

    #[must_use]
    pub fn paths(&self) -> &StatePaths {
        &self.paths
    }

    /// Load a consistent protection snapshot without creating an empty store.
    ///
    /// # Errors
    ///
    /// Returns an error when existing state cannot be locked, read, or parsed.
    pub fn snapshot(&self) -> Result<ProtectionState, StateError> {
        let path = self.paths.protection_file();
        if !path.exists() {
            return Ok(ProtectionState::default());
        }
        let lock = self.open_lock()?;
        FileExt::lock_shared(&lock)
            .map_err(|source| io_error(self.paths.protection_lock(), source))?;
        read_state(&path)
    }

    /// Lock and load protection state for a delete-time safety check.
    ///
    /// The shared inter-process lock remains held for the lifetime of the
    /// returned guard.
    ///
    /// # Errors
    ///
    /// Returns an error when state cannot be locked, read, or parsed.
    pub fn locked_snapshot(&self) -> Result<ProtectionReadGuard, StateError> {
        self.paths.prepare_root()?;
        let lock = self.open_lock()?;
        FileExt::lock_shared(&lock)
            .map_err(|source| io_error(self.paths.protection_lock(), source))?;
        let state = read_state(&self.paths.protection_file())?;
        Ok(ProtectionReadGuard { _lock: lock, state })
    }

    /// Add typed entries in one exclusive, durable transaction.
    ///
    /// # Errors
    ///
    /// Returns an error when an entry is invalid or state cannot be updated.
    pub fn add(&self, kind: ProtectionKind, values: &[String]) -> Result<usize, StateError> {
        validate_values(kind, values)?;
        self.update(|state| {
            let before = state.entries.len();
            state.entries.extend(
                values
                    .iter()
                    .cloned()
                    .map(|value| ProtectionEntry { kind, value }),
            );
            state.entries.sort();
            state.entries.dedup();
            state.entries.len() - before
        })
    }

    /// Remove typed runtime entries in one exclusive, durable transaction.
    ///
    /// # Errors
    ///
    /// Returns an error when an entry is invalid or state cannot be updated.
    pub fn remove(&self, kind: ProtectionKind, values: &[String]) -> Result<usize, StateError> {
        validate_values(kind, values)?;
        self.update(|state| {
            let before = state.entries.len();
            state
                .entries
                .retain(|entry| entry.kind != kind || !values.contains(&entry.value));
            before - state.entries.len()
        })
    }

    fn update<T>(
        &self,
        operation: impl FnOnce(&mut ProtectionState) -> T,
    ) -> Result<T, StateError> {
        self.paths.prepare_root()?;
        let lock = self.open_lock()?;
        FileExt::lock_exclusive(&lock)
            .map_err(|source| io_error(self.paths.protection_lock(), source))?;
        let mut state = read_state(&self.paths.protection_file())?;
        let result = operation(&mut state);
        write_state_atomic(&self.paths, &state)?;
        Ok(result)
    }

    fn open_lock(&self) -> Result<File, StateError> {
        self.paths.prepare_root()?;
        let path = self.paths.protection_lock();
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

#[derive(Debug)]
pub struct ProtectionReadGuard {
    _lock: File,
    state: ProtectionState,
}

impl ProtectionReadGuard {
    #[must_use]
    pub fn state(&self) -> &ProtectionState {
        &self.state
    }
}

fn validate_values(kind: ProtectionKind, values: &[String]) -> Result<(), StateError> {
    if values.is_empty() {
        return Err(StateError::Invalid(
            "at least one protection value is required".to_owned(),
        ));
    }
    for value in values {
        if value.trim().is_empty() {
            return Err(StateError::Invalid(
                "protection values must not be blank".to_owned(),
            ));
        }
        if kind == ProtectionKind::Container {
            Regex::new(value).map_err(|error| {
                StateError::Invalid(format!(
                    "container protection value {value:?} is not a valid ID/name regex: {error}"
                ))
            })?;
        }
    }
    Ok(())
}

fn read_state(path: &Path) -> Result<ProtectionState, StateError> {
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ProtectionState::default())
        }
        Err(source) => return Err(io_error(path, source)),
    };
    let mut state: ProtectionState =
        toml::from_str(&source).map_err(|source| StateError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    if state.schema_version != SCHEMA_VERSION {
        return Err(StateError::Invalid(format!(
            "unsupported protection state schema_version {}; expected {SCHEMA_VERSION}",
            state.schema_version
        )));
    }
    for entry in &state.entries {
        validate_values(entry.kind, std::slice::from_ref(&entry.value))?;
    }
    state.entries.sort();
    state.entries.dedup();
    Ok(state)
}

fn write_state_atomic(paths: &StatePaths, state: &ProtectionState) -> Result<(), StateError> {
    let serialized = toml::to_string_pretty(state).map_err(StateError::Serialize)?;
    let nonce = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = paths.root.join(format!(
        ".protection.toml.tmp-{}-{timestamp}-{nonce}",
        std::process::id()
    ));
    let destination = paths.protection_file();
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
        File::open(&paths.root)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| io_error(&paths.root, source))?;
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

#[cfg(unix)]
pub(crate) fn set_private_directory_permissions(path: &Path) -> Result<(), StateError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_error(path, source))
}

#[cfg(not(unix))]
pub(crate) fn set_private_directory_permissions(_path: &Path) -> Result<(), StateError> {
    Ok(())
}

#[cfg(unix)]
pub(crate) fn set_private_file_permissions(path: &Path) -> Result<(), StateError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|source| io_error(path, source))
}

#[cfg(not(unix))]
pub(crate) fn set_private_file_permissions(_path: &Path) -> Result<(), StateError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn item(kind: ResourceKind, id: &str, name: &str) -> InventoryItem {
        InventoryItem {
            kind,
            id: id.to_owned(),
            name: name.to_owned(),
            search_names: vec![name.to_owned()],
            parent_ids: Vec::new(),
            labels: BTreeMap::new(),
            mounts: Vec::new(),
            state: crate::plan::ResourceState::Available,
            created_at: None,
            state_since: None,
            size: None,
            referenced: false,
            dangling: false,
            system: false,
        }
    }

    #[test]
    fn typed_entries_match_only_their_resource_kind() {
        let state = ProtectionState {
            schema_version: 1,
            entries: vec![ProtectionEntry {
                kind: ProtectionKind::Network,
                value: "shared".to_owned(),
            }],
        };
        assert!(state
            .match_reason(&item(ResourceKind::Network, "n1", "shared"))
            .is_some());
        assert!(state
            .match_reason(&item(ResourceKind::Volume, "v1", "shared"))
            .is_none());
    }

    #[test]
    fn atomic_updates_are_sorted_and_idempotent() {
        let root = std::env::temp_dir().join(format!("docker-maid-state-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = ProtectionStore::new(StatePaths::new(root.clone()));
        assert_eq!(
            store
                .add(ProtectionKind::Volume, &["z".to_owned(), "a".to_owned()])
                .unwrap(),
            2
        );
        assert_eq!(
            store
                .add(ProtectionKind::Volume, &["a".to_owned()])
                .unwrap(),
            0
        );
        assert_eq!(store.snapshot().unwrap().entries[0].value, "a");
        assert_eq!(
            store
                .remove(ProtectionKind::Volume, &["a".to_owned()])
                .unwrap(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }
}
