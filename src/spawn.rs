//! Creation of one labelled sandbox container.
//!
//! `spawn` exists so an agent inherits ownership without having to remember
//! the stamp. It creates one container carrying [`crate::stamp::Stamp`] and
//! then gets out of the way.
//!
//! Two boundaries shape everything here.
//!
//! It does not parent the agent. The container is always created detached with
//! `AutoRemove` off, and the caller returns as soon as Docker has started it.
//! Nothing attaches to the container's streams, waits for it, or watches it
//! die, so the sandbox outlives the process that asked for it and no cleanup
//! is tied to this program exiting.
//!
//! It does not proxy Docker. The surface is an image, a name, a workspace, a
//! working directory, and a command; there is no route here for ports,
//! networks, environment variables, users, capabilities, or limits. A caller
//! that needs those runs Docker directly and applies
//! `docker_maid stamp --docker-args` at creation, which reaches the same
//! ownership from the other side.

use crate::stamp::Stamp;
use bollard::errors::Error as BollardError;
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{CreateContainerOptionsBuilder, StartContainerOptions};
use bollard::Docker;
use std::collections::HashMap;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::path::{Path, PathBuf};

/// Where a bound workspace appears inside the sandbox.
///
/// The destination is fixed rather than configurable. A single documented path
/// is one less thing for an agent to get wrong, and `--workdir` still moves
/// the starting directory anywhere the image supports.
pub const WORKSPACE_MOUNT_PATH: &str = "/workspace";

/// Why a sandbox cannot be created.
#[derive(Debug)]
pub enum SpawnError {
    /// The image reference was empty.
    BlankImage,
    /// The container name was empty or only whitespace.
    BlankName,
    /// The workspace path was relative, so Docker would read it as a volume name.
    RelativeWorkspace {
        /// The path as given.
        path: PathBuf,
    },
    /// The workspace path is not an existing directory on this host.
    MissingWorkspace {
        /// The path as given.
        path: PathBuf,
    },
    /// The working directory was not absolute.
    RelativeWorkdir {
        /// The path as given.
        path: String,
    },
    /// The image is not present locally and this command never pulls.
    ImageAbsent {
        /// The image reference the caller asked for.
        image: String,
    },
    /// Docker refused the request.
    Docker {
        /// What was attempted, for the message.
        operation: String,
        /// The underlying failure.
        source: BollardError,
    },
}

impl Display for SpawnError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> FmtResult {
        match self {
            Self::BlankImage => formatter.write_str("image reference is empty"),
            Self::BlankName => formatter.write_str("container name is empty"),
            Self::RelativeWorkspace { path } => write!(
                formatter,
                "workspace {} is relative; Docker would read it as a volume name, so give an \
                 absolute host path",
                path.display()
            ),
            Self::MissingWorkspace { path } => write!(
                formatter,
                "workspace {} is not an existing directory; create it first, because Docker would \
                 otherwise create it as root",
                path.display()
            ),
            Self::RelativeWorkdir { path } => {
                write!(formatter, "working directory {path} is not absolute")
            }
            Self::ImageAbsent { image } => write!(
                formatter,
                "image {image} is not present locally and spawn never pulls; run `docker pull \
                 {image}` first"
            ),
            Self::Docker { operation, source } => {
                write!(formatter, "Docker {operation} failed: {source}")
            }
        }
    }
}

impl std::error::Error for SpawnError {}

/// A validated, daemon-free description of the sandbox to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    image: String,
    name: Option<String>,
    workspace: Option<PathBuf>,
    workdir: Option<String>,
    command: Vec<String>,
    stamp: Stamp,
}

impl SpawnRequest {
    /// Validate one sandbox description without contacting Docker.
    ///
    /// # Errors
    ///
    /// Returns [`SpawnError`] for a blank image or name, a workspace that is
    /// not an existing absolute directory, or a relative working directory.
    pub fn new(
        image: &str,
        name: Option<&str>,
        workspace: Option<&Path>,
        workdir: Option<&str>,
        command: Vec<String>,
        stamp: Stamp,
    ) -> Result<Self, SpawnError> {
        if image.trim().is_empty() {
            return Err(SpawnError::BlankImage);
        }
        if let Some(name) = name {
            if name.trim().is_empty() {
                return Err(SpawnError::BlankName);
            }
        }
        if let Some(workspace) = workspace {
            if !workspace.is_absolute() {
                return Err(SpawnError::RelativeWorkspace {
                    path: workspace.to_path_buf(),
                });
            }
            if !workspace.is_dir() {
                return Err(SpawnError::MissingWorkspace {
                    path: workspace.to_path_buf(),
                });
            }
        }
        if let Some(workdir) = workdir {
            if !Path::new(workdir).is_absolute() {
                return Err(SpawnError::RelativeWorkdir {
                    path: workdir.to_owned(),
                });
            }
        }
        Ok(Self {
            image: image.to_owned(),
            name: name.map(str::to_owned),
            workspace: workspace.map(Path::to_path_buf),
            workdir: workdir.map(str::to_owned),
            command,
            stamp,
        })
    }

    /// The image reference the sandbox is created from.
    #[must_use]
    pub fn image(&self) -> &str {
        &self.image
    }

    /// The requested container name, if the caller chose one.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The host directory bound into the sandbox, if any.
    #[must_use]
    pub fn workspace(&self) -> Option<&Path> {
        self.workspace.as_deref()
    }

    /// The directory the sandbox starts in.
    ///
    /// An explicit `--workdir` wins. Otherwise a bound workspace supplies its
    /// own mount path, because starting anywhere else would surprise a caller
    /// who just handed over a project directory.
    #[must_use]
    pub fn workdir(&self) -> Option<&str> {
        match (&self.workdir, &self.workspace) {
            (Some(workdir), _) => Some(workdir),
            (None, Some(_)) => Some(WORKSPACE_MOUNT_PATH),
            (None, None) => None,
        }
    }

    /// The command the sandbox runs, empty when the image default is used.
    #[must_use]
    pub fn command(&self) -> &[String] {
        &self.command
    }

    /// The ownership labels the container is created with.
    #[must_use]
    pub fn stamp(&self) -> &Stamp {
        &self.stamp
    }

    /// Build the exact container Docker is asked to create.
    ///
    /// The non-parenting guarantees live here rather than at the call site, so
    /// they hold for every caller: no stream is attached, no TTY is allocated,
    /// and `AutoRemove` is explicitly off so the sandbox is still there to be
    /// inventoried after it exits.
    #[must_use]
    pub fn container_body(&self) -> ContainerCreateBody {
        let binds = self.workspace.as_ref().map(|workspace| {
            vec![format!(
                "{}:{WORKSPACE_MOUNT_PATH}",
                workspace.to_string_lossy()
            )]
        });
        ContainerCreateBody {
            image: Some(self.image.clone()),
            cmd: (!self.command.is_empty()).then(|| self.command.clone()),
            working_dir: self.workdir().map(str::to_owned),
            labels: Some(
                self.stamp
                    .labels()
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<HashMap<_, _>>(),
            ),
            attach_stdin: Some(false),
            attach_stdout: Some(false),
            attach_stderr: Some(false),
            open_stdin: Some(false),
            tty: Some(false),
            host_config: Some(HostConfig {
                binds,
                auto_remove: Some(false),
                ..HostConfig::default()
            }),
            ..ContainerCreateBody::default()
        }
    }
}

/// What Docker created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnOutcome {
    /// The full container ID.
    pub id: String,
    /// The name Docker settled on, without its leading slash.
    pub name: String,
    /// Any warnings Docker returned about the creation request.
    pub warnings: Vec<String>,
}

/// Create and start one labelled sandbox, then return without watching it.
///
/// # Errors
///
/// Returns [`SpawnError::Docker`] when the daemon cannot be reached, and
/// whatever [`create_and_start`] reports otherwise.
pub async fn spawn_sandbox(request: &SpawnRequest) -> Result<SpawnOutcome, SpawnError> {
    let docker = Docker::connect_with_defaults().map_err(|source| SpawnError::Docker {
        operation: "connection setup".to_owned(),
        source,
    })?;
    create_and_start(&docker, request).await
}

/// Create and start the sandbox on an already-connected daemon.
///
/// # Errors
///
/// Returns [`SpawnError::ImageAbsent`] when the image is not present locally,
/// and [`SpawnError::Docker`] for any other Docker failure. A container that
/// was created but could not be started is left in place rather than removed,
/// so the caller can inspect why.
pub async fn create_and_start(
    docker: &Docker,
    request: &SpawnRequest,
) -> Result<SpawnOutcome, SpawnError> {
    // Checking first turns the common mistake into one clear sentence instead
    // of a raw 404 from the create call, and it keeps the promise that this
    // command never reaches the network to fix it. Only a 404 means the image
    // is absent: treating every failure that way would tell an operator whose
    // daemon is down to go and pull an image they already have.
    if let Err(error) = docker.inspect_image(request.image()).await {
        return Err(match error {
            BollardError::DockerResponseServerError {
                status_code: 404, ..
            } => SpawnError::ImageAbsent {
                image: request.image().to_owned(),
            },
            source => SpawnError::Docker {
                operation: "image inspect".to_owned(),
                source,
            },
        });
    }

    let mut options = CreateContainerOptionsBuilder::default();
    if let Some(name) = request.name() {
        options = options.name(name);
    }
    let created = docker
        .create_container(Some(options.build()), request.container_body())
        .await
        .map_err(|source| SpawnError::Docker {
            operation: "container create".to_owned(),
            source,
        })?;

    docker
        .start_container(&created.id, None::<StartContainerOptions>)
        .await
        .map_err(|source| SpawnError::Docker {
            operation: "container start".to_owned(),
            source,
        })?;

    // Ask Docker for the settled name rather than echoing the request, so an
    // unnamed sandbox still reports something a human can use.
    let name = docker
        .inspect_container(&created.id, None)
        .await
        .ok()
        .and_then(|inspected| inspected.name)
        .map_or_else(
            || created.id.clone(),
            |name| name.trim_start_matches('/').to_owned(),
        );

    Ok(SpawnOutcome {
        id: created.id,
        name,
        warnings: created.warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::{SpawnError, SpawnRequest, WORKSPACE_MOUNT_PATH};
    use crate::stamp::Stamp;
    use std::path::{Path, PathBuf};

    fn stamp() -> Stamp {
        Stamp::new(Some("agent-7")).expect("named stamp")
    }

    fn request(workspace: Option<&Path>) -> SpawnRequest {
        SpawnRequest::new("alpine:latest", None, workspace, None, Vec::new(), stamp())
            .expect("a plain request is valid")
    }

    /// A directory that exists on every host running these tests.
    fn existing_directory() -> PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn the_container_carries_exactly_the_stamp() {
        // If spawn wrote its own labels the survey would see a family the
        // `labels` table never advertised, which is the drift this contract
        // exists to prevent.
        let request = request(None);
        let body = request.container_body();
        let labels = body.labels.expect("a spawned container is labelled");
        assert_eq!(labels.len(), request.stamp().labels().len());
        for (key, value) in request.stamp().labels() {
            assert_eq!(labels.get(key), Some(value));
        }
    }

    #[test]
    fn the_sandbox_is_never_removed_or_attached_on_our_behalf() {
        // These four settings are the whole non-parenting promise. Attaching a
        // stream would make this process the sandbox's console, and AutoRemove
        // would delete the container the moment it exited, so nothing could
        // ever inventory or adopt it.
        let body = request(None).container_body();
        let host_config = body.host_config.expect("a host config is always sent");
        assert_eq!(host_config.auto_remove, Some(false));
        assert_eq!(body.attach_stdin, Some(false));
        assert_eq!(body.attach_stdout, Some(false));
        assert_eq!(body.attach_stderr, Some(false));
        assert_eq!(body.open_stdin, Some(false));
        assert_eq!(body.tty, Some(false));
    }

    #[test]
    fn no_workspace_means_no_bind_mount() {
        let body = request(None).container_body();
        let host_config = body.host_config.expect("host config");
        assert_eq!(host_config.binds, None);
        assert_eq!(body.working_dir, None);
    }

    #[test]
    fn a_workspace_is_bound_at_the_documented_path_and_becomes_the_workdir() {
        let directory = existing_directory();
        let body = request(Some(&directory)).container_body();
        let host_config = body.host_config.expect("host config");
        assert_eq!(
            host_config.binds,
            Some(vec![format!(
                "{}:{WORKSPACE_MOUNT_PATH}",
                directory.display()
            )])
        );
        assert_eq!(body.working_dir.as_deref(), Some(WORKSPACE_MOUNT_PATH));
    }

    #[test]
    fn an_explicit_workdir_wins_over_the_workspace_default() {
        let directory = existing_directory();
        let request = SpawnRequest::new(
            "alpine:latest",
            None,
            Some(&directory),
            Some("/srv"),
            Vec::new(),
            stamp(),
        )
        .expect("an absolute workdir is valid");
        assert_eq!(request.workdir(), Some("/srv"));
        assert_eq!(
            request.container_body().working_dir.as_deref(),
            Some("/srv")
        );
    }

    #[test]
    fn an_empty_command_leaves_the_image_default_alone() {
        // Sending an empty Cmd would blank the image's own entry point rather
        // than inherit it, so the sandbox would start and immediately stop.
        assert_eq!(request(None).container_body().cmd, None);
        let with_command = SpawnRequest::new(
            "alpine:latest",
            None,
            None,
            None,
            vec!["sleep".to_owned(), "600".to_owned()],
            stamp(),
        )
        .expect("a command is valid");
        assert_eq!(
            with_command.container_body().cmd,
            Some(vec!["sleep".to_owned(), "600".to_owned()])
        );
    }

    #[test]
    fn a_relative_workspace_is_refused_rather_than_becoming_a_volume() {
        // Docker reads a relative bind source as a named volume, so passing it
        // through would silently mount the wrong thing.
        let error = SpawnRequest::new(
            "alpine:latest",
            None,
            Some(Path::new("relative/path")),
            None,
            Vec::new(),
            stamp(),
        )
        .expect_err("a relative workspace is refused");
        assert!(matches!(error, SpawnError::RelativeWorkspace { .. }));
    }

    #[test]
    fn a_workspace_that_does_not_exist_is_refused() {
        // Docker would create the directory as root and hand the agent a
        // workspace it cannot write to.
        let missing = existing_directory().join("docker-maid-no-such-workspace-6f21");
        assert!(!missing.exists());
        let error = SpawnRequest::new(
            "alpine:latest",
            None,
            Some(&missing),
            None,
            Vec::new(),
            stamp(),
        )
        .expect_err("a missing workspace is refused");
        assert!(matches!(error, SpawnError::MissingWorkspace { .. }));
    }

    #[test]
    fn blank_identifiers_are_refused() {
        assert!(matches!(
            SpawnRequest::new("", None, None, None, Vec::new(), stamp())
                .expect_err("a blank image is refused"),
            SpawnError::BlankImage
        ));
        assert!(matches!(
            SpawnRequest::new("alpine:latest", Some("  "), None, None, Vec::new(), stamp())
                .expect_err("a blank name is refused"),
            SpawnError::BlankName
        ));
        assert!(matches!(
            SpawnRequest::new(
                "alpine:latest",
                None,
                None,
                Some("relative"),
                Vec::new(),
                stamp()
            )
            .expect_err("a relative workdir is refused"),
            SpawnError::RelativeWorkdir { .. }
        ));
    }
}
