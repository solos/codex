#![cfg(test)]

use clap::Parser;

use super::*;
use crate::ExecServerCli;

fn exec_server_from_args(args: &[&str]) -> ExecServerCommand {
    let cli = ExecServerCli::try_parse_from(
        ["codex-exec-server"]
            .into_iter()
            .chain(args.iter().copied()),
    )
    .expect("parse executor arguments");
    cli.command
}

#[test]
fn exec_server_accepts_standard_listen_modes() {
    for args in [
        vec![],
        vec!["--listen", "stdio"],
        vec!["--listen", "stdio://"],
        vec!["--listen", "ws://127.0.0.1:8765"],
    ] {
        let command = exec_server_from_args(&args);
        assert_eq!(
            command.request_dispatch_mode,
            codex_exec_server::RequestDispatchMode::default()
        );
    }
}

#[test]
fn exec_server_rejects_removed_remote_options() {
    for option in [
        "--remote",
        "--remote-transport",
        "--environment-id",
        "--use-agent-identity-auth",
        "--executor-credential-stdin",
        "--aws-sigv4",
        "--aws-profile",
        "--aws-region",
        "--aws-service",
        "--exit-on-stdin-close",
    ] {
        let error = ExecServerCli::try_parse_from(["codex-exec-server", option, "value"])
            .expect_err("remote option should be removed");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}

#[test]
fn standalone_help_documents_executor_options() {
    use clap::CommandFactory;
    let help = ExecServerCli::command()
        .term_width(80)
        .mut_arg("proxy_private_ips_via_upstream", |arg| {
            arg.hide_env_values(true)
        })
        .render_long_help()
        .to_string()
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(help);
}
