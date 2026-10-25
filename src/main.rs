mod connection;
mod deliver;
mod help;
mod jsonrpc;
mod key;
mod output;
mod pair;
mod send;
mod server;
mod status;
#[cfg(test)]
mod testing;
mod wait;

use std::env;
use std::ffi::OsString;
use std::future;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

use crate::deliver::Delivery;
use crate::output::{Exit, Failure, Output, describe};

const DEFAULT_SERVER: &str = "https://assistant-link.fly.dev";

/// Connect an agent to Assistant Link.
#[derive(Debug, Parser)]
#[command(name = "al", version)]
struct Cli {
    /// Assistant Link service URL
    #[arg(
        long,
        global = true,
        env = "AL_SERVER",
        hide_env_values = true,
        default_value = DEFAULT_SERVER,
        value_parser = parse_server,
    )]
    server: String,

    /// Print human-readable text instead of JSON
    #[arg(long, global = true)]
    human: bool,

    /// File holding the Link key, created by `al pair` [otherwise status, wait and send read AL_LINK_KEY]
    #[arg(long, global = true, value_name = "PATH")]
    key_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum Command {
    /// Exchange a pairing code for the Link key
    Pair {
        /// Pairing code from the app, e.g. K7QM-2XPA
        code: String,
    },
    /// Check that the service accepts the Link key
    #[command(long_about = help::STATUS)]
    Status,
    /// Stay online and print the user's messages, until interrupted
    #[command(long_about = help::WAIT)]
    Wait,
    /// Send a message to the user
    #[command(long_about = help::SEND)]
    Send {
        /// The message, or - to read it from standard input
        text: String,
    },
}

fn command() -> clap::Command {
    Cli::command().after_help(help::footer(env::consts::OS))
}

fn parse() -> Cli {
    Cli::from_arg_matches(&command().get_matches()).unwrap_or_else(|error| error.exit())
}

fn parse_server(value: &str) -> Result<String, String> {
    let (_, rest) = value
        .split_once("://")
        .filter(|(scheme, _)| {
            scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
        })
        .ok_or("expected an http:// or https:// URL")?;
    if value.contains(|c: char| c.is_whitespace() || c == '?' || c == '#') {
        return Err("expected a URL without spaces, a query, or a fragment".into());
    }
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty() || authority.starts_with(':') || authority.ends_with('@') {
        return Err("expected a host after the scheme".into());
    }
    Ok(value.trim_end_matches('/').to_owned())
}

fn main() -> ExitCode {
    let cli = parse();
    let (mut stdout, mut stderr) = (io::stdout(), io::stderr());
    let mut out = Output {
        human: cli.human,
        stdout: &mut stdout,
        stderr: &mut stderr,
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let failure = Failure::new(Exit::Unexpected, "unexpected", describe(&error));
            return fail(&cli.command, &mut out, failure).into();
        }
    };
    let link_key = env::var_os(key::ENV);
    runtime.block_on(run(cli, link_key, &mut out)).into()
}

fn fail(command: &Command, out: &mut Output, failure: Failure) -> Exit {
    match command {
        Command::Wait => wait::stop(out, Some(failure)),
        _ => out.fail(&failure),
    }
}

async fn run(cli: Cli, link_key: Option<OsString>, out: &mut Output<'_>) -> Exit {
    let key_file = cli.key_file.as_deref();
    let load_key = || key::load(key_file, link_key);
    match cli.command {
        Command::Pair { code } => pair::run(&cli.server, &code, key_file, out).await,
        Command::Status => status::run(&cli.server, load_key(), out).await,
        Command::Send { text } => send::run(&cli.server, load_key(), &text, io::stdin(), out).await,
        Command::Wait => {
            let key = match load_key() {
                Ok(key) => key,
                Err(failure) => return wait::stop(out, Some(failure)),
            };
            let signals = count_signals();
            let mut delivery = Delivery::new(io::stdout());
            let finished = tokio::select! {
                exit = wait::run(
                    &cli.server,
                    &key,
                    out,
                    wait::Timing::default(),
                    signaled(signals.clone(), 1),
                    &mut delivery,
                ) => Some(exit),
                () = signaled(signals, 2) => None,
            };
            finished.unwrap_or_else(|| wait::stop(out, None))
        }
    }
}

fn count_signals() -> watch::Receiver<u32> {
    let (sender, receiver) = watch::channel(0);
    if let (Ok(mut interrupt), Ok(mut terminate)) = (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
    ) {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = interrupt.recv() => {}
                    _ = terminate.recv() => {}
                }
                sender.send_modify(|count| *count += 1);
            }
        });
    }
    receiver
}

async fn signaled(mut signals: watch::Receiver<u32>, count: u32) {
    if signals
        .wait_for(|received| *received >= count)
        .await
        .is_err()
    {
        future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;
    use clap::{CommandFactory, FromArgMatches};

    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        let matches = Cli::command()
            .mut_arg("server", |arg| arg.env(None))
            .try_get_matches_from(args)?;
        Cli::from_arg_matches(&matches)
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_pair_with_code() {
        let cli = parse(&["al", "pair", "K7QM-2XPA"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Pair {
                code: "K7QM-2XPA".into()
            }
        );
    }

    #[test]
    fn pair_requires_a_code() {
        assert!(parse(&["al", "pair"]).is_err());
    }

    #[test]
    fn parses_send_with_text_or_a_dash() {
        for text in ["Your reminder is set.", "-"] {
            let cli = parse(&["al", "send", text]).unwrap();
            assert_eq!(cli.command, Command::Send { text: text.into() });
        }
    }

    #[test]
    fn send_without_a_text_is_a_usage_error() {
        let error = parse(&["al", "send"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
        assert_eq!(error.exit_code(), 2);
    }

    fn help_of(subcommand: Option<&str>) -> String {
        let mut command = command();
        match subcommand {
            Some(name) => command.find_subcommand_mut(name).unwrap(),
            None => &mut command,
        }
        .render_long_help()
        .to_string()
    }

    #[test]
    fn general_help_lists_each_command_in_one_line_and_points_to_the_details() {
        let help = help_of(None);
        for command in ["pair", "status", "wait", "send"] {
            assert_eq!(
                help.lines()
                    .filter(|line| line.trim_start().starts_with(&format!("{command} ")))
                    .count(),
                1,
                "{command}: {help}"
            );
        }
        assert!(help.contains("Flow:"));
        assert!(help.contains("Link key:"));
        assert!(
            help.trim_end()
                .ends_with("Run `al <command> --help` for details.")
        );
    }

    #[test]
    fn general_help_shows_the_key_section_of_this_os_only() {
        let help = help_of(None);
        assert!(help.contains(&help::footer(env::consts::OS)));
        assert!(
            help.contains(
                "If your environment has a safer way to guard credentials, use it instead."
            )
        );
    }

    #[test]
    fn command_help_is_specific_and_leaves_out_the_general_part() {
        for command in ["pair", "status", "wait", "send"] {
            let help = help_of(Some(command));
            assert!(!help.contains("Link key:"), "{command}");
            assert!(!help.contains("Flow:"), "{command}");
        }
    }

    #[test]
    fn wait_help_describes_the_event_line_and_missed_events_first() {
        let help = help_of(Some("wait"));
        let line = help.find(r#"{"id":"...","type":"text_message""#).unwrap();
        let missed = help.find("while no `al wait` was running").unwrap();
        let acknowledged = help.find("acknowledged").unwrap();
        let status = help.find("Status messages").unwrap();
        assert!(line < missed && missed < acknowledged && acknowledged < status);
    }

    #[test]
    fn send_help_describes_the_text_rules_and_the_idempotency_key() {
        let help = help_of(Some("send"));
        for expected in [
            "idempotency_key",
            "UUIDv7",
            "never retries",
            "dropping one trailing line feed",
            "4,000",
            "text_too_long",
        ] {
            assert!(help.contains(expected), "{expected}: {help}");
        }
    }

    #[test]
    fn status_help_describes_the_fields_and_the_check_values() {
        let help = help_of(Some("status"));
        for expected in [
            "link_id",
            "assistant_name",
            "unread_count",
            "error",
            "accepted",
            "rejected",
            "unreachable",
            "no_key",
            "unexpected",
        ] {
            assert!(help.contains(expected), "{expected}: {help}");
        }
    }

    #[test]
    fn global_flags_work_after_the_subcommand() {
        let cli = parse(&[
            "al",
            "status",
            "--human",
            "--server",
            "https://example.test",
        ])
        .unwrap();
        assert_eq!(cli.command, Command::Status);
        assert!(cli.human);
        assert_eq!(cli.server, "https://example.test");
    }

    #[test]
    fn key_file_works_before_and_after_the_subcommand() {
        for args in [
            ["al", "--key-file", "/keys/hermes", "wait"],
            ["al", "wait", "--key-file", "/keys/hermes"],
        ] {
            let cli = parse(&args).unwrap();
            assert_eq!(
                cli.key_file,
                Some(PathBuf::from("/keys/hermes")),
                "{args:?}"
            );
        }
    }

    #[test]
    fn json_is_the_default_output() {
        assert!(!parse(&["al", "status"]).unwrap().human);
    }

    fn server_arg() -> clap::Arg {
        Cli::command()
            .get_arguments()
            .find(|arg| arg.get_id() == "server")
            .unwrap()
            .clone()
    }

    #[test]
    fn server_defaults_to_production() {
        assert_eq!(server_arg().get_default_values(), [DEFAULT_SERVER]);
    }

    #[test]
    fn server_is_read_from_al_server() {
        assert_eq!(server_arg().get_env(), Some("AL_SERVER".as_ref()));
    }

    #[test]
    fn server_keeps_valid_urls_without_trailing_slash() {
        for (server, expected) in [
            ("http://localhost:4000/", "http://localhost:4000"),
            (
                "HTTPS://assistant-link.fly.dev",
                "HTTPS://assistant-link.fly.dev",
            ),
            (
                "https://example.test/prefix//",
                "https://example.test/prefix",
            ),
            ("http://[::1]:4000", "http://[::1]:4000"),
        ] {
            let cli = parse(&["al", "wait", "--server", server]).unwrap();
            assert_eq!(cli.server, expected, "{server:?}");
        }
    }

    #[test]
    fn invalid_server_is_a_usage_error() {
        for server in [
            "",
            "assistant-link.fly.dev",
            "ftp://example.test",
            "https://",
            "https:///",
            "https://:4000",
            "https://user@",
            "https://example.test ",
            "https://exa mple.test",
            "https://example.test/?x=1",
            "https://example.test#top",
        ] {
            let error = parse(&["al", "status", "--server", server]).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::ValueValidation, "{server:?}");
            assert_eq!(error.exit_code(), 2, "{server:?}");
        }
    }
}
