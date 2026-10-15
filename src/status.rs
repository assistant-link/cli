use serde::Serialize;
use serde_json::json;

use crate::connection;
use crate::jsonrpc::Ready;
use crate::output::{Exit, Failure, Output};

#[derive(Serialize)]
struct Report<'a> {
    server: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    link_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    assistant_name: Option<&'a str>,
    check: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a Failure>,
}

pub async fn run(server: &str, key: Result<String, Failure>, out: &mut Output<'_>) -> Exit {
    let accepted = match key {
        Ok(key) => match connection::open(server, &key).await {
            Ok(mut connection) => {
                connection::close(&mut connection.socket).await;
                Ok(connection.ready)
            }
            Err(failure) => Err(failure),
        },
        Err(failure) => Err(failure),
    };
    report(out, server, accepted.as_ref())
}

fn report(out: &mut Output, server: &str, accepted: Result<&Ready, &Failure>) -> Exit {
    let (report, text) = match accepted {
        Ok(ready) => (
            Report {
                server,
                link_id: Some(&ready.link_id),
                assistant_name: Some(&ready.assistant_name),
                check: "accepted",
                error: None,
            },
            format!(
                "\"{}\" (Link {}) at {server}: the service accepted the connection.",
                ready.assistant_name, ready.link_id
            ),
        ),
        Err(failure) => (
            Report {
                server,
                link_id: None,
                assistant_name: None,
                check: check_for(failure),
                error: Some(failure),
            },
            failure.message.clone(),
        ),
    };
    out.report(&json!(report), &text);
    accepted.map_or_else(|failure| failure.exit, |_| Exit::Success)
}

fn check_for(failure: &Failure) -> &'static str {
    match failure.exit {
        Exit::NoKey => "no_key",
        Exit::Rejected => "rejected",
        Exit::Unreachable => "unreachable",
        Exit::Success | Exit::Unexpected | Exit::Usage => "unexpected",
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
    use crate::key;
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

    async fn status_with(server: &str, key: Result<String, Failure>, human: bool) -> Run {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let exit = run(
            server,
            key,
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

    async fn status(server: &str, human: bool) -> Run {
        status_with(server, Ok("alt_1".into()), human).await
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

        let run = status(&service.url, false).await;

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(
            run.json(),
            json!({
                "server": service.url,
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

        let run = status(&service.url, true).await;

        assert_eq!(run.exit, Exit::Success);
        for expected in ["Hermes", "link-1", &service.url, "accepted"] {
            assert!(run.stdout.contains(expected), "{expected}: {}", run.stdout);
        }
    }

    #[tokio::test]
    async fn no_key_exits_3_without_connecting() {
        let service = ready_then_wait_for_close(Arc::default()).await;

        let run = status_with(&service.url, key::load(None, None), false).await;

        assert_eq!(run.exit, Exit::NoKey);
        let printed = run.json();
        assert_eq!(printed["server"], service.url.as_str());
        assert_eq!(printed["check"], "no_key");
        assert_eq!(printed["error"]["code"], "no_key");
        assert!(
            printed["error"]["message"]
                .as_str()
                .unwrap()
                .contains("AL_LINK_KEY")
        );
        assert_eq!(printed.get("link_id"), None);
        assert!(service.authorizations().is_empty());
    }

    #[tokio::test]
    async fn rejected_key_exits_4_naming_the_service() {
        let service = MockService::start(testing::unauthorized(), |_| async {}).await;

        let run = status(&service.url, false).await;

        assert_eq!(run.exit, Exit::Rejected);
        let printed = run.json();
        assert_eq!(printed["check"], "rejected");
        assert_eq!(printed["error"]["code"], "unauthorized");
        assert_eq!(printed.get("link_id"), None);
        assert!(
            printed["error"]["message"]
                .as_str()
                .unwrap()
                .contains(&service.url)
        );
    }

    #[tokio::test]
    async fn unreachable_service_exits_5_naming_the_url() {
        let server = testing::closed_port_url();

        let run = status(&server, false).await;

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

        let run = status(&service.url, false).await;

        assert_eq!(run.exit, Exit::Unreachable);
        assert_eq!(run.json()["check"], "unreachable");
    }

    #[tokio::test]
    async fn connection_closed_before_ready_exits_5() {
        let service = MockService::start(Upgrade::Accept, |mut socket| async move {
            let _ = socket.close(None).await;
        })
        .await;

        let run = status(&service.url, false).await;

        assert_eq!(run.exit, Exit::Unreachable);
        assert_eq!(run.json()["check"], "unreachable");
    }

    #[tokio::test]
    async fn account_deleted_before_ready_exits_4() {
        let service = MockService::start(Upgrade::Accept, |mut socket| async move {
            testing::send_closing(&mut socket, "account_deleted").await;
        })
        .await;

        let run = status(&service.url, false).await;

        assert_eq!(run.exit, Exit::Rejected);
        assert_eq!(run.json()["error"]["code"], "account_deleted");
    }

    #[tokio::test]
    async fn non_alp_responses_exit_1() {
        let service = MockService::start(Upgrade::Reject(404, "{}".into()), |_| async {}).await;

        let run = status(&service.url, false).await;

        assert_eq!(run.exit, Exit::Unexpected);
        let printed = run.json();
        assert_eq!(printed["server"], service.url.as_str());
        assert_eq!(printed.get("link_id"), None);
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

        let run = status(&service.url, false).await;

        assert_eq!(run.exit, Exit::Unreachable);
        assert!(closed_cleanly.load(Ordering::SeqCst));
    }
}
