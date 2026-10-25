use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::output::{Exit, Failure, Output};
use crate::server;

#[derive(Deserialize)]
struct Link {
    link_id: String,
    assistant_name: String,
    unread_count: u64,
}

#[derive(Serialize)]
struct Report<'a> {
    server: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    link_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    assistant_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unread_count: Option<u64>,
    check: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a Failure>,
}

pub async fn run(server: &str, key: Result<String, Failure>, out: &mut Output<'_>) -> Exit {
    let accepted =
        async { server::call(server, &key?, Method::GET, "/alp/v1/link", None, false).await }.await;
    report(out, server, accepted.as_ref())
}

fn report(out: &mut Output, server: &str, accepted: Result<&Link, &Failure>) -> Exit {
    let (report, text) = match accepted {
        Ok(link) => (
            Report {
                server,
                link_id: Some(&link.link_id),
                assistant_name: Some(&link.assistant_name),
                unread_count: Some(link.unread_count),
                check: "accepted",
                error: None,
            },
            format!(
                "\"{}\" (Link {}) at {server}: the service accepted the Link key. Unread messages: {}.",
                link.assistant_name, link.link_id, link.unread_count
            ),
        ),
        Err(failure) => (
            Report {
                server,
                link_id: None,
                assistant_name: None,
                unread_count: None,
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
    use serde_json::{Value, json};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::key;
    use crate::testing::{self, Buffers};

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
        let mut buffers = Buffers::default();
        let exit = run(server, key, &mut buffers.output(human)).await;
        Run {
            exit,
            stdout: buffers.stdout(),
        }
    }

    async fn status(server: &str, human: bool) -> Run {
        status_with(server, Ok("alt_1".into()), human).await
    }

    async fn link_service(response: ResponseTemplate) -> MockServer {
        let service = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/alp/v1/link"))
            .and(header("authorization", "Bearer alt_1"))
            .respond_with(response)
            .mount(&service)
            .await;
        service
    }

    fn accepted() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(
            json!({"link_id": "link-1", "assistant_name": "Hermes", "unread_count": 3}),
        )
    }

    #[tokio::test]
    async fn accepted_reports_the_link_and_the_unread_count_over_one_plain_get() {
        let service = link_service(accepted()).await;

        let run = status(&service.uri(), false).await;

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(
            run.json(),
            json!({
                "server": service.uri(),
                "link_id": "link-1",
                "assistant_name": "Hermes",
                "unread_count": 3,
                "check": "accepted",
            })
        );
        let requests = service.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/alp/v1/link");
        assert!(!requests[0].headers.contains_key("upgrade"));
    }

    #[tokio::test]
    async fn human_output_names_the_assistant_the_check_and_the_unread_count() {
        let service = link_service(accepted()).await;

        let run = status(&service.uri(), true).await;

        assert_eq!(run.exit, Exit::Success);
        for expected in ["Hermes", "link-1", &service.uri(), "accepted", "3"] {
            assert!(run.stdout.contains(expected), "{expected}: {}", run.stdout);
        }
    }

    #[tokio::test]
    async fn no_key_exits_3_without_calling_the_service() {
        let service = link_service(accepted()).await;

        let run = status_with(&service.uri(), key::load(None, None), false).await;

        assert_eq!(run.exit, Exit::NoKey);
        let printed = run.json();
        assert_eq!(printed["server"], service.uri());
        assert_eq!(printed["check"], "no_key");
        assert_eq!(printed["error"]["code"], "no_key");
        let message = printed["error"]["message"].as_str().unwrap();
        for expected in ["AL_LINK_KEY", "--key-file", "al pair <code>"] {
            assert!(message.contains(expected), "{expected}: {message}");
        }
        assert_eq!(printed.get("link_id"), None);
        assert!(service.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejected_key_exits_4_naming_the_service() {
        let service = link_service(ResponseTemplate::new(401).set_body_json(
            json!({"error": {"code": "unauthorized", "message": "Missing or invalid bearer token."}}),
        ))
        .await;

        let run = status(&service.uri(), false).await;

        assert_eq!(run.exit, Exit::Rejected);
        let printed = run.json();
        assert_eq!(printed["check"], "rejected");
        assert_eq!(printed["error"]["code"], "unauthorized");
        assert_eq!(printed.get("link_id"), None);
        assert_eq!(printed.get("unread_count"), None);
        assert!(
            printed["error"]["message"]
                .as_str()
                .unwrap()
                .contains(&service.uri())
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
        let service = link_service(ResponseTemplate::new(503)).await;

        let run = status(&service.uri(), false).await;

        assert_eq!(run.exit, Exit::Unreachable);
        assert_eq!(run.json()["check"], "unreachable");
    }

    #[tokio::test]
    async fn other_statuses_are_unexpected_and_exit_1() {
        let service = MockServer::start().await;

        let run = status(&service.uri(), false).await;

        assert_eq!(run.exit, Exit::Unexpected);
        let printed = run.json();
        assert_eq!(printed["server"], service.uri());
        assert_eq!(printed.get("link_id"), None);
        assert_eq!(printed["check"], "unexpected");
        assert_eq!(printed["error"]["code"], "unexpected");
    }

    #[tokio::test]
    async fn a_body_that_is_not_a_link_is_unexpected() {
        let service = link_service(ResponseTemplate::new(200).set_body_string("<html>")).await;

        let run = status(&service.uri(), false).await;

        assert_eq!(run.exit, Exit::Unexpected);
        assert_eq!(run.json()["check"], "unexpected");
    }
}
