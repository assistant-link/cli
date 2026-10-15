mod connection;
mod jsonrpc;
mod key;
mod output;
mod pair;
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

use clap::{Parser, Subcommand};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

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

    /// File holding the Link key, created by `al pair` [otherwise status and wait read AL_LINK_KEY]
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
    /// Check whether the service accepts a connection with the Link key
    Status,
    /// Hold the connection open so the Link is online, until interrupted
    Wait,
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
    let cli = Cli::parse();
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
        Command::Wait => {
            let key = match load_key() {
                Ok(key) => key,
                Err(failure) => return wait::stop(out, Some(failure)),
            };
            let signals = count_signals();
            let finished = tokio::select! {
                exit = wait::run(
                    &cli.server,
                    &key,
                    out,
                    wait::Timing::default(),
                    signaled(signals.clone(), 1),
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
