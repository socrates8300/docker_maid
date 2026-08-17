//! Installation of the portable agent skill.
//!
//! The skill is one Markdown document that teaches a coding agent to drive
//! this CLI. It is compiled into the binary, so an install is a file write and
//! never a download.
//!
//! Two boundaries shape this module.
//!
//! The skill teaches the CLI; it does not reimplement it. Nothing here writes
//! a shell wrapper that creates containers, because that would be a second
//! sandbox launcher to keep in step with `spawn`.
//!
//! Installing touches the skill directory and nothing else. The operator's
//! configuration file stays human-owned, so this never opens it, let alone
//! edits it.

use std::fmt::{Display, Formatter, Result as FmtResult};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The skill document, compiled in so an install needs no network.
pub const SKILL_DOCUMENT: &str = include_str!("../assets/agent-skill/SKILL.md");

/// The directory one skill occupies inside a harness's skills directory.
pub const SKILL_DIRECTORY: &str = "docker-maid";

/// The file every supported harness reads.
pub const SKILL_FILE: &str = "SKILL.md";

/// Which harness's skills directory to install into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallTarget {
    /// `~/.claude/skills`.
    Claude,
    /// `~/.codex/skills`.
    Codex,
    /// A directory the caller names explicitly.
    Generic,
}

impl InstallTarget {
    /// The path under the home directory this target installs into.
    ///
    /// [`InstallTarget::Generic`] has none, because a generic harness is
    /// exactly the case where guessing a location would be wrong.
    #[must_use]
    pub fn home_relative_directory(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some(".claude/skills"),
            Self::Codex => Some(".codex/skills"),
            Self::Generic => None,
        }
    }
}

impl Display for InstallTarget {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> FmtResult {
        let text = match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Generic => "generic",
        };
        formatter.write_str(text)
    }
}

/// Why the skill cannot be installed.
#[derive(Debug)]
pub enum SkillError {
    /// A generic target was chosen without saying where to install.
    DestinationRequired,
    /// A home-relative target was chosen but no home directory is known.
    HomeUnknown {
        /// The target that needed one.
        target: InstallTarget,
    },
    /// A different skill document is already installed.
    WouldOverwrite {
        /// The file that would have been replaced.
        path: PathBuf,
    },
    /// The install could not be written.
    Write {
        /// The path being written.
        path: PathBuf,
        /// The underlying failure.
        source: io::Error,
    },
}

impl Display for SkillError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> FmtResult {
        match self {
            Self::DestinationRequired => formatter.write_str(
                "a generic target needs --dest to say which skills directory to install into",
            ),
            Self::HomeUnknown { target } => write!(
                formatter,
                "cannot locate a home directory for the {target} target; pass --dest instead"
            ),
            Self::WouldOverwrite { path } => write!(
                formatter,
                "{} already holds a different skill; pass --force to replace it",
                path.display()
            ),
            Self::Write { path, source } => {
                write!(formatter, "cannot write {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for SkillError {}

/// What an install did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStatus {
    /// No skill was there before.
    Written,
    /// The same document was already installed, so nothing changed.
    Unchanged,
    /// A different document was replaced, which `--force` allowed.
    Replaced,
}

impl Display for InstallStatus {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> FmtResult {
        let text = match self {
            Self::Written => "written",
            Self::Unchanged => "unchanged",
            Self::Replaced => "replaced",
        };
        formatter.write_str(text)
    }
}

/// Where the skill ended up and what happened there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    /// The `SKILL.md` that was considered.
    pub path: PathBuf,
    /// Whether it was written, replaced, or already correct.
    pub status: InstallStatus,
}

/// Resolve the `SKILL.md` path for one target.
///
/// `destination` overrides the target's own directory, which is how a test
/// installs somewhere harmless and how an operator installs into a harness
/// this build has never heard of.
///
/// # Errors
///
/// Returns [`SkillError::DestinationRequired`] for a generic target with no
/// destination, and [`SkillError::HomeUnknown`] when a home-relative target
/// has no home directory to resolve against.
pub fn resolve_skill_path(
    target: InstallTarget,
    destination: Option<&Path>,
    home: Option<&Path>,
) -> Result<PathBuf, SkillError> {
    let base = match (destination, target.home_relative_directory()) {
        (Some(destination), _) => destination.to_path_buf(),
        (None, Some(relative)) => {
            let home = home.ok_or(SkillError::HomeUnknown { target })?;
            home.join(relative)
        }
        (None, None) => return Err(SkillError::DestinationRequired),
    };
    Ok(base.join(SKILL_DIRECTORY).join(SKILL_FILE))
}

/// Install the skill, refusing to replace a different one without `force`.
///
/// An identical document already in place is reported as
/// [`InstallStatus::Unchanged`] rather than rewritten, so repeated installs
/// are safe and say so.
///
/// # Errors
///
/// Returns [`SkillError::WouldOverwrite`] when a different document is present
/// and `force` is false, and [`SkillError::Write`] when the directory or file
/// cannot be written.
pub fn install_skill(path: &Path, force: bool) -> Result<Installation, SkillError> {
    let existing = fs::read_to_string(path).ok();
    let status = match existing {
        Some(current) if current == SKILL_DOCUMENT => {
            return Ok(Installation {
                path: path.to_path_buf(),
                status: InstallStatus::Unchanged,
            })
        }
        Some(_) if !force => {
            return Err(SkillError::WouldOverwrite {
                path: path.to_path_buf(),
            })
        }
        Some(_) => InstallStatus::Replaced,
        None => InstallStatus::Written,
    };

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| SkillError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(path, SKILL_DOCUMENT).map_err(|source| SkillError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Installation {
        path: path.to_path_buf(),
        status,
    })
}

/// Every `docker_maid` subcommand the skill's runnable examples use.
///
/// This exists so a test can hold the document to the real command surface. A
/// skill that teaches a command this build does not have is worse than no
/// skill, because the agent will believe the failure is its own.
///
/// Only fenced code blocks count. Prose says things like "the tool for
/// coding agents", and treating that as an invocation would make the check
/// noisy enough to be turned off.
#[must_use]
pub fn advertised_commands() -> Vec<String> {
    let mut commands = Vec::new();
    let mut inside_code = false;
    for line in SKILL_DOCUMENT.lines() {
        if line.trim_start().starts_with("```") {
            inside_code = !inside_code;
            continue;
        }
        if !inside_code {
            continue;
        }
        // An invocation is either the line itself or a `$(...)` substitution
        // inside it, and both read the same way once split on the binary name.
        for tail in line.split("docker_maid ").skip(1) {
            if let Some(word) = tail
                .split_whitespace()
                // Global flags may precede the subcommand.
                .find(|word| !word.starts_with("--"))
            {
                commands.push(word.trim_end_matches(')').to_owned());
            }
        }
    }
    commands.sort_unstable();
    commands.dedup();
    commands
}

#[cfg(test)]
mod tests {
    use super::{
        advertised_commands, install_skill, resolve_skill_path, InstallStatus, InstallTarget,
        SkillError, SKILL_DOCUMENT,
    };
    use crate::labels;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "docker-maid-skill-{label}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        path
    }

    #[test]
    fn the_document_is_a_skill_a_harness_can_read() {
        // Both supported harnesses read YAML front matter with a name and a
        // description. A document missing either is silently ignored, which
        // looks exactly like a successful install.
        assert!(SKILL_DOCUMENT.starts_with("---\n"));
        let front_matter = SKILL_DOCUMENT
            .split("\n---\n")
            .next()
            .expect("front matter block");
        assert!(front_matter.contains("\nname: docker-maid"));
        assert!(front_matter.contains("\ndescription:"));
    }

    #[test]
    fn the_document_teaches_the_commands_this_build_has() {
        // The command surface is the contract between the skill and the
        // binary. If they drift, the agent runs something that does not exist.
        let known = [
            "clean",
            "config",
            "daemon",
            "init",
            "labels",
            "plan",
            "protect",
            "spawn",
            "stamp",
            "status",
            "tui",
            "unprotect",
        ];
        for command in advertised_commands() {
            assert!(
                known.contains(&command.as_str()),
                "the skill teaches `docker_maid {command}`, which is not a command"
            );
        }
    }

    #[test]
    fn the_document_sends_agents_to_spawn_and_stamp_rather_than_their_own_wrapper() {
        // The point of the skill is that an agent drives this CLI. A skill
        // that only showed raw Docker would leave every agent inventing its
        // own launcher, which is the sprawl this tool exists to end.
        assert!(SKILL_DOCUMENT.contains("docker_maid spawn"));
        assert!(SKILL_DOCUMENT.contains("docker_maid stamp"));
        assert!(SKILL_DOCUMENT.contains("--docker-args"));
    }

    #[test]
    fn the_document_never_names_a_label_key_outside_the_vocabulary() {
        // A skill that told an agent to write some other key would produce
        // resources the survey cannot see.
        for line in SKILL_DOCUMENT.lines() {
            for word in line.split_whitespace() {
                let candidate = word.trim_matches(|c: char| !c.is_ascii_graphic());
                if let Some((key, _)) = candidate.split_once('=') {
                    if key.contains('.') && key.starts_with("dev.") {
                        assert!(
                            labels::is_known(key),
                            "the skill names {key}, which is not ownership evidence"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_generic_target_will_not_guess_a_location() {
        let error = resolve_skill_path(InstallTarget::Generic, None, Some(Path::new("/home/x")))
            .expect_err("a generic target needs a destination");
        assert!(matches!(error, SkillError::DestinationRequired));
    }

    #[test]
    fn each_harness_target_resolves_under_its_own_directory() {
        let home = Path::new("/home/x");
        assert_eq!(
            resolve_skill_path(InstallTarget::Claude, None, Some(home)).expect("claude path"),
            home.join(".claude/skills/docker-maid/SKILL.md")
        );
        assert_eq!(
            resolve_skill_path(InstallTarget::Codex, None, Some(home)).expect("codex path"),
            home.join(".codex/skills/docker-maid/SKILL.md")
        );
        // An explicit destination wins, so an unknown harness is reachable
        // without teaching this build about it.
        assert_eq!(
            resolve_skill_path(
                InstallTarget::Claude,
                Some(Path::new("/opt/skills")),
                Some(home)
            )
            .expect("explicit path"),
            Path::new("/opt/skills/docker-maid/SKILL.md")
        );
    }

    #[test]
    fn a_missing_home_is_refused_rather_than_installed_somewhere_odd() {
        let error = resolve_skill_path(InstallTarget::Claude, None, None)
            .expect_err("no home means no default path");
        assert!(matches!(error, SkillError::HomeUnknown { .. }));
    }

    #[test]
    fn installing_creates_the_directory_and_the_document() {
        let root = temp_dir("write");
        let path = root.join("skills/docker-maid/SKILL.md");
        let installation = install_skill(&path, false).expect("install into a new directory");
        assert_eq!(installation.status, InstallStatus::Written);
        assert_eq!(
            fs::read_to_string(&path).expect("read the installed skill"),
            SKILL_DOCUMENT
        );
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn installing_twice_reports_unchanged_instead_of_rewriting() {
        // A rerun is the normal case, and it must not need --force.
        let root = temp_dir("idempotent");
        let path = root.join("docker-maid/SKILL.md");
        install_skill(&path, false).expect("first install");
        let second = install_skill(&path, false).expect("second install");
        assert_eq!(second.status, InstallStatus::Unchanged);
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn a_different_skill_is_not_replaced_without_force() {
        // Someone may have edited the installed skill. Overwriting it silently
        // would discard their work with no way to notice.
        let root = temp_dir("overwrite");
        let path = root.join("docker-maid/SKILL.md");
        install_skill(&path, false).expect("first install");
        fs::write(&path, "---\nname: mine\n---\nlocal edit\n").expect("edit the installed skill");
        let error = install_skill(&path, false).expect_err("an edited skill is protected");
        assert!(matches!(error, SkillError::WouldOverwrite { .. }));
        assert!(fs::read_to_string(&path)
            .expect("read back")
            .contains("local edit"));

        let forced = install_skill(&path, true).expect("force replaces it");
        assert_eq!(forced.status, InstallStatus::Replaced);
        assert_eq!(
            fs::read_to_string(&path).expect("read back"),
            SKILL_DOCUMENT
        );
        fs::remove_dir_all(root).expect("remove test directory");
    }
}
