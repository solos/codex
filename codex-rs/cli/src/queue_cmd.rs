use clap::Arg;
use clap::ArgMatches;
use clap::Command;
use clap::CommandFactory;
use clap::Parser;
use clap::builder::NonEmptyStringValueParser;
use clap::error::ErrorKind;
use clap::parser::ValueSource;
use codex_arg0::Arg0DispatchPaths;
use codex_tui::Cli as TuiCli;
use codex_utils_cli::CliConfigOverrides;

use crate::SessionArchiveConfigOverrides;
use crate::finalize_session_archive_interactive;

#[derive(Debug, Parser)]
#[command(defer(configure_help))]
pub(crate) struct QueueCommand {
    /// Session UUID or exact session name.
    #[arg(long, value_name = "THREAD")]
    thread: String,

    /// Message text to queue.
    #[arg(long, value_name = "TEXT", value_parser = NonEmptyStringValueParser::new())]
    message: String,

    #[clap(flatten)]
    pub(crate) config_overrides: SessionArchiveConfigOverrides,
}

/// Only inherited options need restrictions; queue owns its other arguments.
fn restricted_options(command: &Command) -> impl Iterator<Item = &Arg> {
    command.get_arguments().filter(|arg| {
        !matches!(
            arg.get_long(),
            Some("config" | "enable" | "disable" | "profile" | "strict-config")
        )
    })
}

fn configure_help(command: Command) -> Command {
    let interactive = TuiCli::command();
    command.mut_args(|arg| {
        if arg.get_long() == Some("config") {
            arg.long_help(None).help("Override a startup configuration value (key=value). Queued messages use the thread's model.")
        } else if !restricted_options(&interactive).any(|shared| shared.get_id() == arg.get_id()) {
            arg
        } else {
            arg.hide(true)
        }
    })
}

/// Check both scopes before root flags such as `--approve-for-me` become config overrides.
pub(super) fn validate_options(command: &Command, root: &ArgMatches) -> Result<(), clap::Error> {
    let Some(queue) = root.subcommand_matches("queue") else {
        return Ok(());
    };
    let supplied = [root, queue].into_iter().flat_map(|scope| {
        scope
            .ids()
            .filter(|id| scope.value_source(id.as_str()) == Some(ValueSource::CommandLine))
    });
    for id in supplied {
        let Some(arg) = restricted_options(command).find(|arg| arg.get_id() == id) else {
            continue;
        };
        let message = match arg.get_long() {
            Some("cd") => continue,
            // These flags already have dedicated diagnostics in the CLI and session startup.
            Some("worktree") => continue,
            Some("model") => {
                "`codex queue` does not support `--model`; queued messages use the thread's model"
                    .to_string()
            }
            Some("image") => "`codex queue` does not support image attachments".to_string(),
            Some(name) => format!("`codex queue` does not support `--{name}`"),
            None => {
                "`codex queue` does not accept a positional prompt; use `--message`".to_string()
            }
        };
        return Err(clap::Error::raw(ErrorKind::ArgumentConflict, message));
    }
    Ok(())
}

pub(crate) async fn run_queue_command(
    command: QueueCommand,
    interactive: TuiCli,
    root_config_overrides: CliConfigOverrides,
    arg0_paths: Arg0DispatchPaths,
) -> anyhow::Result<String> {
    let QueueCommand {
        thread,
        message,
        config_overrides,
    } = command;
    let cli =
        finalize_session_archive_interactive(interactive, root_config_overrides, config_overrides);
    if cli.config_overrides.raw_overrides.iter().any(|entry| {
        entry
            .split_once('=')
            .is_some_and(|(key, _)| key.trim() == "model")
    }) {
        anyhow::bail!(
            "`codex queue` does not support a `model` config override; queued messages use the thread's model"
        );
    }
    codex_tui::run_session_queue_command(
        thread,
        message,
        codex_tui::SessionArchiveCommandOptions { cli, arg0_paths },
    )
    .await
    .map_err(|error| anyhow::anyhow!("{error:#}"))
}
