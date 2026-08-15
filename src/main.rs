use clap::{error::ErrorKind, Parser, Subcommand};
use docker_maid::config::{load_config, DEFAULT_CONFIG};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const EXIT_CONFIG: u8 = 3;
const EXIT_INTERNAL: u8 = 7;
const EXIT_USAGE: u8 = 64;

#[derive(Debug, Parser)]
#[command(name = "docker_maid", version, about)]
struct Cli {
    /// Read configuration from this exact path.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate, validate, or normalize configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Validate the selected configuration file.
    Check,
    /// Parse and print the selected configuration in normalized TOML.
    Print,
    /// Print an annotated default configuration.
    Default,
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let successful = matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            );
            let _ = error.print();
            return ExitCode::from(if successful { 0 } else { EXIT_USAGE });
        }
    };

    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(RunError::Output(error)) => {
            let code = output_error_exit_code(&error);
            if code != 0 {
                write_diagnostic(&format!("error: cannot write stdout: {error}"));
            }
            ExitCode::from(code)
        }
        Err(RunError::Config(error)) => {
            write_diagnostic(&format!("error: {error}"));
            ExitCode::from(EXIT_CONFIG)
        }
    }
}

#[derive(Debug)]
enum RunError {
    Config(docker_maid::config::ConfigError),
    Output(io::Error),
}

impl From<docker_maid::config::ConfigError> for RunError {
    fn from(error: docker_maid::config::ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<io::Error> for RunError {
    fn from(error: io::Error) -> Self {
        Self::Output(error)
    }
}

fn run(cli: Cli) -> Result<(), RunError> {
    match cli.command {
        Command::Config {
            command: ConfigCommand::Default,
        } => {
            write_payload(DEFAULT_CONFIG.as_bytes())?;
            Ok(())
        }
        Command::Config { command } => {
            let current_dir = std::env::current_dir().map_err(|source| {
                docker_maid::config::ConfigError::Read {
                    path: PathBuf::from("."),
                    source,
                }
            })?;
            let xdg = config_home();
            let loaded = load_config(cli.config.as_deref(), &current_dir, xdg.as_deref())?;

            match command {
                ConfigCommand::Check => {
                    let message = format!("configuration valid: {}\n", display_path(&loaded.path));
                    write_payload(message.as_bytes())?;
                }
                ConfigCommand::Print => {
                    let normalized = loaded.config.to_normalized_toml()?;
                    write_payload(normalized.as_bytes())?;
                }
                ConfigCommand::Default => unreachable!("handled without loading configuration"),
            }
            Ok(())
        }
    }
}

fn write_payload(payload: &[u8]) -> io::Result<()> {
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    writer.write_all(payload)?;
    writer.flush()
}

fn write_diagnostic(message: &str) {
    let stderr = io::stderr();
    let mut writer = stderr.lock();
    let _ = writeln!(writer, "{message}");
}

fn output_error_exit_code(error: &io::Error) -> u8 {
    if error.kind() == io::ErrorKind::BrokenPipe {
        0
    } else {
        EXIT_INTERNAL
    }
}

fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
}

fn display_path(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broken_pipe_is_success() {
        let error = io::Error::from(io::ErrorKind::BrokenPipe);
        assert_eq!(output_error_exit_code(&error), 0);
    }

    #[test]
    fn other_output_error_is_internal_failure() {
        let error = io::Error::from(io::ErrorKind::StorageFull);
        assert_eq!(output_error_exit_code(&error), EXIT_INTERNAL);
    }
}
