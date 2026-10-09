use std::path::Path;

use serde::Serialize;
use serde_json::json;

use crate::connection;
use crate::credentials::Credential;
use crate::output::{Exit, Failure, Output};

#[derive(Serialize)]
struct Report<'a> {
    server: &'a str,
    paired: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    link_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    assistant_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stored_server: Option<&'a str>,
    check: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a Failure>,
}

pub async fn run(server: &str, config_dir: &Path, out: &mut Output<'_>) -> Exit {
    let credential = match connection::credential_for(config_dir, server) {
        Ok(credential) => credential,
        Err(refusal) => {
            return report(
                out,
                server,
                refusal.credential.as_ref(),
                Err(&refusal.failure),
            );
        }
    };
    let accepted = match connection::open(server, &credential.token).await {
        Ok(mut connection) => {
            connection::close(&mut connection.socket).await;
            Ok(connection.ready.assistant_name)
        }
        Err(failure) => Err(failure),
    };
    report(out, server, Some(&credential), accepted.as_deref())
}

fn report(
    out: &mut Output,
    server: &str,
    credential: Option<&Credential>,
    accepted: Result<&str, &Failure>,
) -> Exit {
    let (check, error) = match accepted {
        Ok(_) => ("accepted", None),
        Err(failure) => (check_for(failure), Some(failure)),
    };
    let report = Report {
        server,
        paired: credential.is_some(),
        link_id: credential.map(|credential| credential.link_id.as_str()),
        assistant_name: accepted
            .ok()
            .or(credential.map(|credential| credential.assistant_name.as_str())),
        stored_server: credential
            .filter(|_| check == "server_mismatch")
            .map(|credential| credential.server.as_str()),
        check,
        error,
    };
    let subject = match (report.assistant_name, report.link_id) {
        (Some(name), Some(link_id)) => format!("\"{name}\" (Link {link_id}) at {server}: "),
        _ => String::new(),
    };
    let outcome = error.map_or("the service accepted the connection.", |failure| {
        failure.message.as_str()
    });
    out.report(&json!(report), &format!("{subject}{outcome}"));
    error.map_or(Exit::Success, |failure| failure.exit)
}

fn check_for(failure: &Failure) -> &'static str {
    match failure.exit {
        Exit::NotPaired => "not_paired",
        Exit::ServerMismatch => "server_mismatch",
        Exit::Rejected => "rejected",
        Exit::Unreachable => "unreachable",
        Exit::Success | Exit::Unexpected => "unexpected",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use futures_util::StreamExt;
    use serde_json::{Value, json};
    use tokio_tungstenite::tungstenite::Message;

    use super::*;
    use crate::credentials::{self, Credential};
    use crate::testing::{self, MockService, Upgrade};

    struct Run {
        exit: Exit,
        stdout: String,
    }

    impl Run {
        fn json(&self) -> Value {
            serde_json::from_str(&self.stdout).unwrap()
        }
    }

    async fn status(server: &str, config_dir: &Path, human: bool) -> Run {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let exit = run(
            server,
            config_dir,
            &mut Output {
                human,
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
        )
        .await;
        Run {
            exit,
            stdout: String::from_utf8(stdout).unwrap(),
        }
    }

    fn paired_with(server: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        credentials::save(
            dir.path(),
            &Credential {
                link_id: "link-1".into(),
                assistant_name: "Hermes".into(),
                server: server.into(),
                token: "alt_1".into(),
            },
        )
        .unwrap();
        dir
    }

    async fn ready_then_wait_for_close(closed_cleanly: Arc<AtomicBool>) -> MockService {
        MockService::start(Upgrade::Accept, move |mut socket| {
            let closed_cleanly = Arc::clone(&closed_cleanly);
            async move {
                testing::send_ready(&mut socket).await;
                while let Some(Ok(message)) = socket.next().await {
                    if let Message::Close(_) = message {
                        closed_cleanly.store(true, Ordering::SeqCst);
                    }
                }
            }
        })
        .await
    }

    #[tokio::test]
    async fn accepted_connection_reports_the_link_and_closes_cleanly() {
        let closed_cleanly = Arc::new(AtomicBool::new(false));
        let service = ready_then_wait_for_close(Arc::clone(&closed_cleanly)).await;
        let dir = paired_with(&service.url);

        let run = status(&service.url, dir.path(), false).await;

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(
            run.json(),
            json!({
                "server": service.url,
                "paired": true,
                "link_id": "link-1",
                "assistant_name": "Hermes",
                "check": "accepted",
            })
        );
        assert_eq!(service.authorizations(), ["Bearer alt_1"]);
        assert!(closed_cleanly.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn human_output_names_the_assistant_and_the_check() {
        let service = ready_then_wait_for_close(Arc::default()).await;
        let dir = paired_with(&service.url);

        let run = status(&service.url, dir.path(), true).await;

        assert_eq!(run.exit, Exit::Success);
        for expected in ["Hermes", "link-1", &service.url, "accepted"] {
            assert!(run.stdout.contains(expected), "{expected}: {}", run.stdout);
        }
    }

    #[tokio::test]
    async fn not_paired_exits_3_and_suggests_al_pair() {
        let dir = tempfile::tempdir().unwrap();
        let server = testing::closed_port_url();

        let run = status(&server, dir.path(), false).await;

        assert_eq!(run.exit, Exit::NotPaired);
        let printed = run.json();
        assert_eq!(printed["server"], server.as_str());
        assert_eq!(printed["paired"], false);
        assert_eq!(printed["check"], "not_paired");
        assert_eq!(printed["error"]["code"], "not_paired");
        assert!(
            printed["error"]["message"]
                .as_str()
                .unwrap()
                .contains("al pair <code>")
        );
        assert_eq!(printed.get("link_id"), None);
    }

    #[tokio::test]
    async fn server_mismatch_exits_6_without_sending_the_token() {
        let service = ready_then_wait_for_close(Arc::default()).await;
        let dir = paired_with("https://other.example");

        let run = status(&service.url, dir.path(), false).await;

        assert_eq!(run.exit, Exit::ServerMismatch);
        let printed = run.json();
        assert_eq!(printed["server"], service.url.as_str());
        assert_eq!(printed["stored_server"], "https://other.example");
        assert_eq!(printed["paired"], true);
        assert_eq!(printed["link_id"], "link-1");
        assert_eq!(printed["check"], "server_mismatch");
        assert_eq!(printed["error"]["code"], "server_mismatch");
        let message = printed["error"]["message"].as_str().unwrap();
        assert!(message.contains("https://other.example"), "{message}");
        assert!(message.contains(&service.url), "{message}");
        assert!(service.authorizations().is_empty());
    }

    #[tokio::test]
    async fn server_matching_ignores_trailing_slash_and_case() {
        let service = ready_then_wait_for_close(Arc::default()).await;
        let dir = paired_with(&format!("{}/", service.url.to_uppercase()));

        let run = status(&service.url, dir.path(), false).await;

        assert_eq!(run.exit, Exit::Success);
    }

    #[tokio::test]
    async fn rejected_credential_exits_4() {
        let service = MockService::start(testing::unauthorized(), |_| async {}).await;
        let dir = paired_with(&service.url);

        let run = status(&service.url, dir.path(), false).await;

        assert_eq!(run.exit, Exit::Rejected);
        let printed = run.json();
        assert_eq!(printed["check"], "rejected");
        assert_eq!(printed["error"]["code"], "unauthorized");
        assert_eq!(printed["link_id"], "link-1");
    }

    #[tokio::test]
    async fn unreachable_service_exits_5_naming_the_url() {
        let server = testing::closed_port_url();
        let dir = paired_with(&server);

        let run = status(&server, dir.path(), false).await;

        assert_eq!(run.exit, Exit::Unreachable);
        let printed = run.json();
        assert_eq!(printed["check"], "unreachable");
        assert_eq!(printed["error"]["code"], "unreachable");
        assert!(
            printed["error"]["message"]
                .as_str()
                .unwrap()
                .contains(&server)
        );
    }

    #[tokio::test]
    async fn server_errors_exit_5() {
        let service =
            MockService::start(Upgrade::Reject(503, "unavailable".into()), |_| async {}).await;
        let dir = paired_with(&service.url);

        let run = status(&service.url, dir.path(), false).await;

        assert_eq!(run.exit, Exit::Unreachable);
        assert_eq!(run.json()["check"], "unreachable");
    }

    #[tokio::test]
    async fn connection_closed_before_ready_exits_5() {
        let service = MockService::start(Upgrade::Accept, |mut socket| async move {
            let _ = socket.close(None).await;
        })
        .await;
        let dir = paired_with(&service.url);

        let run = status(&service.url, dir.path(), false).await;

        assert_eq!(run.exit, Exit::Unreachable);
        assert_eq!(run.json()["check"], "unreachable");
    }

    #[tokio::test]
    async fn account_deleted_before_ready_exits_4() {
        let service = MockService::start(Upgrade::Accept, |mut socket| async move {
            testing::send_closing(&mut socket, "account_deleted").await;
        })
        .await;
        let dir = paired_with(&service.url);

        let run = status(&service.url, dir.path(), false).await;

        assert_eq!(run.exit, Exit::Rejected);
        assert_eq!(run.json()["error"]["code"], "account_deleted");
    }

    #[tokio::test]
    async fn non_alp_responses_exit_1() {
        let service = MockService::start(Upgrade::Reject(404, "{}".into()), |_| async {}).await;
        let dir = paired_with(&service.url);

        let run = status(&service.url, dir.path(), false).await;

        assert_eq!(run.exit, Exit::Unexpected);
        let printed = run.json();
        assert_eq!(printed["server"], service.url.as_str());
        assert_eq!(printed["paired"], true);
        assert_eq!(printed["link_id"], "link-1");
        assert_eq!(printed["assistant_name"], "Hermes");
        assert_eq!(printed["check"], "unexpected");
        assert_eq!(printed["error"]["code"], "unexpected");
    }

    #[tokio::test]
    async fn corrupt_credential_file_exits_1() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(credentials::path(dir.path()), "not json").unwrap();

        let run = status("https://example.test", dir.path(), false).await;

        assert_eq!(run.exit, Exit::Unexpected);
        let printed = run.json();
        assert_eq!(printed["server"], "https://example.test");
        assert_eq!(printed["paired"], false);
        assert_eq!(printed["check"], "unexpected");
        assert_eq!(printed["error"]["code"], "unexpected");
    }

    #[tokio::test]
    async fn closes_the_connection_when_it_fails_before_ready() {
        let closed_cleanly = Arc::new(AtomicBool::new(false));
        let closed = Arc::clone(&closed_cleanly);
        let service = MockService::start(Upgrade::Accept, move |mut socket| {
            let closed = Arc::clone(&closed);
            async move {
                testing::send(
                    &mut socket,
                    json!({"jsonrpc": "2.0", "method": "connection.closing", "params": {"reason": "maintenance"}}),
                )
                .await;
                while let Some(Ok(message)) = socket.next().await {
                    if let Message::Close(_) = message {
                        closed.store(true, Ordering::SeqCst);
                    }
                }
            }
        })
        .await;
        let dir = paired_with(&service.url);

        let run = status(&service.url, dir.path(), false).await;

        assert_eq!(run.exit, Exit::Unreachable);
        assert!(closed_cleanly.load(Ordering::SeqCst));
    }
}
