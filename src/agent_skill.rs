//! Installation of the portable agent skills.
//!
//! A skill is one Markdown document that teaches a coding agent part of this
//! CLI. Every document is compiled into the binary, so an install is a file
//! write and never a download.
//!
//! Two boundaries shape this module.
//!
//! A skill teaches the CLI; it does not reimplement it. Nothing here writes a
//! shell wrapper that creates containers, because that would be a second
//! sandbox launcher to keep in step with `spawn`.
//!
//! Installing touches the skill directories and nothing else. The operator's
//! configuration file stays human-owned, so this never opens it, let alone
//! edits it.

use std::fmt::{Display, Formatter, Result as FmtResult};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// One installable skill: a directory name and the document that fills it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Skill {
    /// The directory this skill occupies, and the name `--skill` selects it by.
    pub name: &'static str,
    /// The document, compiled in so an install needs no network.
    pub document: &'static str,
}

/// Every skill this build installs, in install order.
///
/// This is the single source of truth. The `--skill` flag validates against
/// it, the machine document reports it, and the parity guards iterate it, so
/// adding a third skill is one entry here rather than a sweep of the tree.
pub const SKILLS: &[Skill] = &[
    Skill {
        name: "docker-maid",
        document: include_str!("../assets/agent-skill/SKILL.md"),
    },
    Skill {
        name: "docker-maid-config",
        document: include_str!("../assets/agent-skill-config/SKILL.md"),
    },
    Skill {
        name: "docker-maid-repo",
        document: include_str!("../assets/agent-skill-repo/SKILL.md"),
    },
];

/// The file every supported harness reads.
///
/// This is per-harness, not per-skill: both harnesses look for `SKILL.md`
/// inside each skill's own directory.
pub const SKILL_FILE: &str = "SKILL.md";

/// Look up a skill by the name `--skill` uses.
#[must_use]
pub fn skill_by_name(name: &str) -> Option<&'static Skill> {
    SKILLS.iter().find(|skill| skill.name == name)
}

/// Every installable skill name, for a usage error that lists the real set.
#[must_use]
pub fn skill_names() -> Vec<&'static str> {
    SKILLS.iter().map(|skill| skill.name).collect()
}

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

/// Where one skill ended up and what happened there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    /// Which skill this describes.
    pub name: &'static str,
    /// The `SKILL.md` that was considered.
    pub path: PathBuf,
    /// Whether it was written, replaced, or already correct.
    pub status: InstallStatus,
}

/// Resolve the `SKILL.md` path for one skill under one target.
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
    skill: &Skill,
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
    Ok(base.join(skill.name).join(SKILL_FILE))
}

/// What installing one skill would do, without doing any of it.
///
/// [`install_skills`] runs this over the whole selection before it writes
/// anything, which is how a refusal on the second skill leaves the first one
/// untouched.
fn planned_status(skill: &Skill, path: &Path, force: bool) -> Result<InstallStatus, SkillError> {
    match fs::read_to_string(path) {
        Ok(current) if current == skill.document => Ok(InstallStatus::Unchanged),
        Ok(_) if !force => Err(SkillError::WouldOverwrite {
            path: path.to_path_buf(),
        }),
        Ok(_) => Ok(InstallStatus::Replaced),
        Err(_) => Ok(InstallStatus::Written),
    }
}

/// Install one skill, refusing to replace a different one without `force`.
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
pub fn install_skill(skill: &Skill, path: &Path, force: bool) -> Result<Installation, SkillError> {
    let status = planned_status(skill, path, force)?;
    if status == InstallStatus::Unchanged {
        return Ok(Installation {
            name: skill.name,
            path: path.to_path_buf(),
            status,
        });
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| SkillError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(path, skill.document).map_err(|source| SkillError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Installation {
        name: skill.name,
        path: path.to_path_buf(),
        status,
    })
}

/// Install a selection of skills, all of them or none of them.
///
/// Every path is resolved and every overwrite is checked before the first byte
/// is written. Without that, a refusal on the second skill would leave the
/// first one already installed, and the caller would have no way to tell a
/// clean refusal from a half-finished one.
///
/// A write that fails partway is still partial, because there is no way to
/// undo a file that landed. That is unchanged from the single-skill case: the
/// promise here is about refusals, which are the outcome a caller can provoke.
///
/// # Errors
///
/// Returns the first [`SkillError`] any skill in the selection produces.
pub fn install_skills(
    skills: &[&'static Skill],
    target: InstallTarget,
    destination: Option<&Path>,
    home: Option<&Path>,
    force: bool,
) -> Result<Vec<Installation>, SkillError> {
    let mut planned = Vec::with_capacity(skills.len());
    for skill in skills {
        let path = resolve_skill_path(skill, target, destination, home)?;
        planned_status(skill, &path, force)?;
        planned.push((*skill, path));
    }

    planned
        .iter()
        .map(|(skill, path)| install_skill(skill, path, force))
        .collect()
}

/// Every `docker_maid` subcommand a skill's runnable examples use.
///
/// This exists so a test can hold each document to the real command surface. A
/// skill that teaches a command this build does not have is worse than no
/// skill, because the agent will believe the failure is its own.
///
/// Only fenced code blocks count. Prose says things like "the tool for
/// coding agents", and treating that as an invocation would make the check
/// noisy enough to be turned off.
#[must_use]
pub fn advertised_commands(document: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut inside_code = false;
    for line in document.lines() {
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
        advertised_commands, install_skill, install_skills, resolve_skill_path, skill_by_name,
        skill_names, InstallStatus, InstallTarget, Skill, SkillError, SKILLS,
    };
    use crate::config::{retired_key_names, Config};
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

    /// The skill that teaches resource creation, by name rather than by index.
    fn docker_skill() -> &'static Skill {
        skill_by_name("docker-maid").expect("the resource skill is installed by this build")
    }

    /// The skill that teaches policy authoring.
    fn config_skill() -> &'static Skill {
        skill_by_name("docker-maid-config").expect("the config skill is installed by this build")
    }

    /// The skill that sets per-repo defaults.
    fn repo_skill() -> &'static Skill {
        skill_by_name("docker-maid-repo").expect("the repo skill is installed by this build")
    }

    /// Every fenced block in `document` opened with the given language tag.
    fn fenced_blocks(document: &str, tag: &str) -> Vec<String> {
        let mut blocks = Vec::new();
        let mut buffer: Vec<&str> = Vec::new();
        let mut inside = false;
        let mut collecting = false;
        for line in document.lines() {
            if let Some(rest) = line.trim_start().strip_prefix("```") {
                if inside {
                    if collecting {
                        blocks.push(buffer.join("\n"));
                        buffer.clear();
                    }
                    collecting = false;
                } else {
                    collecting = rest.trim() == tag;
                }
                inside = !inside;
                continue;
            }
            if collecting {
                buffer.push(line);
            }
        }
        blocks
    }

    #[test]
    fn every_document_is_a_skill_a_harness_can_read() {
        // Both supported harnesses read YAML front matter with a name and a
        // description. A document missing either is silently ignored, which
        // looks exactly like a successful install. The name must also equal the
        // directory, or the harness registers the skill under one name while
        // this build reinstalls and reports another.
        for skill in SKILLS {
            assert!(
                skill.document.starts_with("---\n"),
                "{} has no front matter",
                skill.name
            );
            let front_matter = skill
                .document
                .split("\n---\n")
                .next()
                .expect("front matter block");
            assert!(
                front_matter.contains(&format!("\nname: {}", skill.name)),
                "{} does not name itself after its directory",
                skill.name
            );
            assert!(
                front_matter.contains("\ndescription:"),
                "{} has no description",
                skill.name
            );
        }
    }

    #[test]
    fn every_installable_name_is_unique() {
        // The name is the directory, so a duplicate would silently make one
        // skill overwrite the other during a single install.
        let mut names = skill_names();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(before, names.len(), "two skills share a directory name");
        assert!(before >= 2, "this build should install more than one skill");
    }

    #[test]
    fn every_document_teaches_the_commands_this_build_has() {
        // The command surface is the contract between a skill and the binary.
        // If they drift, the agent runs something that does not exist.
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
        for skill in SKILLS {
            let commands = advertised_commands(skill.document);
            assert!(
                !commands.is_empty(),
                "{} shows no runnable example at all",
                skill.name
            );
            for command in commands {
                assert!(
                    known.contains(&command.as_str()),
                    "{} teaches `docker_maid {command}`, which is not a command",
                    skill.name
                );
            }
        }
    }

    #[test]
    fn every_toml_example_parses_and_validates_under_the_real_schema() {
        // This is the whole point of a configuration skill. The schema refuses
        // any unknown key, so one stale example would teach an agent to write a
        // file the tool rejects — the exact failure this document exists to
        // prevent. Ask the real deserializer, not a reviewer's eye.
        let mut fences = fenced_blocks(config_skill().document, "toml");
        fences.extend(fenced_blocks(repo_skill().document, "toml"));
        let examples = fences;
        assert!(
            examples.len() >= 4,
            "the configuration and repo skills should show more than one policy shape each"
        );
        for (index, example) in examples.iter().enumerate() {
            let path = PathBuf::from(format!("#toml[{index}]"));
            let config = Config::parse(example, &path)
                .unwrap_or_else(|error| panic!("example {index} does not parse: {error}"));
            config
                .validate()
                .unwrap_or_else(|error| panic!("example {index} does not validate: {error}"));
        }
    }

    #[test]
    fn no_document_teaches_a_key_this_build_retired() {
        // A retired key parses nowhere. Teaching one would send an agent
        // straight to exit 3 with no idea why, and the retirement note is
        // written for a file that already exists, not for new advice.
        for skill in SKILLS {
            for key in retired_key_names() {
                for form in [format!("{key} ="), format!("[{key}]"), format!(".{key}")] {
                    assert!(
                        !skill.document.contains(&form),
                        "{} still teaches the retired key `{key}` as `{form}`",
                        skill.name
                    );
                }
            }
        }
    }

    #[test]
    fn the_config_skill_names_the_traps_that_produce_a_silent_no_op() {
        // Each of these is a rule that parses, validates, and then matches
        // nothing. Without them the document teaches syntax and leaves the
        // agent to discover the semantics by watching nothing happen.
        let document = config_skill().document;
        for phrase in [
            "allow_unscoped",
            "orphan = true",
            "select.name_parts",
            "observation.toml",
        ] {
            assert!(
                document.contains(phrase),
                "the config skill never mentions {phrase}"
            );
        }
    }

    #[test]
    fn the_resource_skill_sends_agents_to_spawn_and_stamp_rather_than_their_own_wrapper() {
        // The point of that skill is that an agent drives this CLI. A skill
        // that only showed raw Docker would leave every agent inventing its
        // own launcher, which is the sprawl this tool exists to end.
        let document = docker_skill().document;
        assert!(document.contains("docker_maid spawn"));
        assert!(document.contains("docker_maid stamp"));
        assert!(document.contains("--docker-args"));
    }

    #[test]
    fn no_document_names_a_label_key_outside_the_vocabulary() {
        // A skill that told an agent to write some other key would produce
        // resources the survey cannot see.
        for skill in SKILLS {
            for line in skill.document.lines() {
                for word in line.split_whitespace() {
                    let candidate = word.trim_matches(|c: char| !c.is_ascii_graphic());
                    if let Some((key, _)) = candidate.split_once('=') {
                        if key.contains('.') && key.starts_with("dev.") {
                            assert!(
                                labels::is_known(key),
                                "{} names {key}, which is not ownership evidence",
                                skill.name
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_generic_target_will_not_guess_a_location() {
        let error = resolve_skill_path(
            docker_skill(),
            InstallTarget::Generic,
            None,
            Some(Path::new("/home/x")),
        )
        .expect_err("a generic target needs a destination");
        assert!(matches!(error, SkillError::DestinationRequired));
    }

    #[test]
    fn each_harness_target_resolves_under_its_own_directory() {
        let home = Path::new("/home/x");
        assert_eq!(
            resolve_skill_path(docker_skill(), InstallTarget::Claude, None, Some(home))
                .expect("claude path"),
            home.join(".claude/skills/docker-maid/SKILL.md")
        );
        assert_eq!(
            resolve_skill_path(docker_skill(), InstallTarget::Codex, None, Some(home))
                .expect("codex path"),
            home.join(".codex/skills/docker-maid/SKILL.md")
        );
        // Each skill gets its own directory under the same harness, so one
        // never lands on top of another.
        assert_eq!(
            resolve_skill_path(config_skill(), InstallTarget::Claude, None, Some(home))
                .expect("claude path"),
            home.join(".claude/skills/docker-maid-config/SKILL.md")
        );
        // An explicit destination wins, so an unknown harness is reachable
        // without teaching this build about it.
        assert_eq!(
            resolve_skill_path(
                docker_skill(),
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
        let error = resolve_skill_path(docker_skill(), InstallTarget::Claude, None, None)
            .expect_err("no home means no default path");
        assert!(matches!(error, SkillError::HomeUnknown { .. }));
    }

    #[test]
    fn installing_creates_the_directory_and_the_document() {
        let root = temp_dir("write");
        let skill = docker_skill();
        let path = root.join("skills/docker-maid/SKILL.md");
        let installation =
            install_skill(skill, &path, false).expect("install into a new directory");
        assert_eq!(installation.status, InstallStatus::Written);
        assert_eq!(installation.name, "docker-maid");
        assert_eq!(
            fs::read_to_string(&path).expect("read the installed skill"),
            skill.document
        );
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn installing_twice_reports_unchanged_instead_of_rewriting() {
        // A rerun is the normal case, and it must not need --force.
        let root = temp_dir("idempotent");
        let path = root.join("docker-maid/SKILL.md");
        install_skill(docker_skill(), &path, false).expect("first install");
        let second = install_skill(docker_skill(), &path, false).expect("second install");
        assert_eq!(second.status, InstallStatus::Unchanged);
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn a_different_skill_is_not_replaced_without_force() {
        // Someone may have edited the installed skill. Overwriting it silently
        // would discard their work with no way to notice.
        let root = temp_dir("overwrite");
        let skill = docker_skill();
        let path = root.join("docker-maid/SKILL.md");
        install_skill(skill, &path, false).expect("first install");
        fs::write(&path, "---\nname: mine\n---\nlocal edit\n").expect("edit the installed skill");
        let error = install_skill(skill, &path, false).expect_err("an edited skill is protected");
        assert!(matches!(error, SkillError::WouldOverwrite { .. }));
        assert!(fs::read_to_string(&path)
            .expect("read back")
            .contains("local edit"));

        let forced = install_skill(skill, &path, true).expect("force replaces it");
        assert_eq!(forced.status, InstallStatus::Replaced);
        assert_eq!(
            fs::read_to_string(&path).expect("read back"),
            skill.document
        );
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn installing_every_skill_writes_each_one_into_its_own_directory() {
        let root = temp_dir("all");
        let selection = SKILLS.iter().collect::<Vec<_>>();
        let installed =
            install_skills(&selection, InstallTarget::Generic, Some(&root), None, false)
                .expect("install every skill");
        assert_eq!(installed.len(), SKILLS.len());
        for (installation, skill) in installed.iter().zip(SKILLS) {
            assert_eq!(installation.name, skill.name);
            assert_eq!(installation.status, InstallStatus::Written);
            assert_eq!(
                fs::read_to_string(&installation.path).expect("read the installed skill"),
                skill.document
            );
            assert!(installation
                .path
                .ends_with(format!("{}/SKILL.md", skill.name)));
        }
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn a_refusal_on_a_later_skill_leaves_every_earlier_one_untouched() {
        // Without an up-front check the first skill would already be written
        // when the second refused, and the caller could not tell a clean
        // refusal from a half-finished install.
        let root = temp_dir("atomic");
        let selection = SKILLS.iter().collect::<Vec<_>>();
        let last = SKILLS.last().expect("at least one skill");
        let blocked = root.join(last.name).join("SKILL.md");
        fs::create_dir_all(blocked.parent().expect("parent")).expect("create the blocking dir");
        fs::write(&blocked, "---\nname: mine\n---\nlocal edit\n").expect("write a different skill");

        let error = install_skills(&selection, InstallTarget::Generic, Some(&root), None, false)
            .expect_err("a differing later skill refuses the whole install");
        assert!(matches!(error, SkillError::WouldOverwrite { .. }));

        for skill in SKILLS.iter().filter(|skill| skill.name != last.name) {
            assert!(
                !root.join(skill.name).exists(),
                "{} was written despite the refusal",
                skill.name
            );
        }
        assert!(fs::read_to_string(&blocked)
            .expect("read back")
            .contains("local edit"));
        fs::remove_dir_all(root).expect("remove test directory");
    }
}
