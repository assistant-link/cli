use std::path::Path;

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;

use crate::credentials::{self, Credential};
use crate::output::{Exit, Failure, Output, describe};
use crate::server::{self, ErrorBody};

#[derive(Deserialize)]
struct Paired {
    link_id: String,
    assistant_name: String,
    token: String,
}

pub async fn run(server: &str, code: &str, config_dir: &Path, out: &mut Output<'_>) -> Exit {
    let credential = match request(server, code).await {
        Ok(paired) => Credential {
            link_id: paired.link_id,
            assistant_name: paired.assistant_name,
            server: server.to_owned(),
            token: paired.token,
        },
        Err(failure) => return out.fail(&failure),
    };
    if let Err(error) = credentials::save(config_dir, &credential) {
        return out.fail(&Failure::new(
            Exit::Unexpected,
            "unexpected",
            format!(
                "Paired Link {}, but could not store its credential in {}: {error}. Ask the user for a new pairing code.",
                credential.link_id,
                credentials::path(config_dir).display()
            ),
        ));
    }
    out.report(
        &json!({
            "server": credential.server,
            "link_id": credential.link_id,
            "assistant_name": credential.assistant_name,
        }),
        &format!(
            "Paired with \"{}\" (Link {}) at {}.",
            credential.assistant_name, credential.link_id, credential.server
        ),
    );
    Exit::Success
}

async fn request(server: &str, code: &str) -> Result<Paired, Failure> {
    let unexpected = |message: String| Failure::new(Exit::Unexpected, "unexpected", message);
    let client = reqwest::Client::builder()
        .user_agent(server::USER_AGENT)
        .timeout(server::TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| unexpected(describe(&error)))?;
    let response = client
        .post(server::endpoint(server, "/alp/v1/pair"))
        .json(&json!({ "code": code }))
        .send()
        .await
        .map_err(|error| server::unreachable(server, describe(&error)))?;

    let status = response.status();
    if status.is_success() {
        return response.json().await.map_err(|error| {
            unexpected(format!(
                "The service at {server} answered pairing with an unexpected body: {}",
                describe(&error)
            ))
        });
    }
    if status.is_server_error() {
        return Err(server::unavailable(server, status));
    }
    if status.is_redirection() {
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|location| location.to_str().ok())
            .unwrap_or("an unknown location");
        return Err(unexpected(format!(
            "The service at {server} redirected pairing to {location}. If that is the Assistant Link service, use its base URL as --server."
        )));
    }
    match (status, response.json::<ErrorBody>().await) {
        (StatusCode::UNPROCESSABLE_ENTITY, Ok(ErrorBody { error })) => {
            Err(Failure::new(Exit::Rejected, &error.code, error.message))
        }
        _ => Err(server::not_alp(server, status)),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::credentials::{self, Credential};

    struct Run {
        exit: Exit,
        stdout: String,
        stderr: String,
    }

    impl Run {
        fn json(&self) -> Value {
            serde_json::from_str(&self.stdout).unwrap()
        }
    }

    async fn pair(server: &str, code: &str, config_dir: &Path, human: bool) -> Run {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let exit = run(
            server,
            code,
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
            stderr: String::from_utf8(stderr).unwrap(),
        }
    }

    async fn service(status: u16, body: Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/alp/v1/pair"))
            .and(body_json(json!({"code": "K7QM-2XPA"})))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        server
    }

    fn paired() -> Value {
        json!({"link_id": "link-b", "assistant_name": "Hermes", "token": "alt_b"})
    }

    fn stored(link_id: &str) -> Credential {
        Credential {
            link_id: link_id.into(),
            assistant_name: "Muse".into(),
            server: "https://other.example".into(),
            token: format!("alt_{link_id}"),
        }
    }

    #[tokio::test]
    async fn stores_the_credential_and_prints_the_link() {
        let service = service(200, paired()).await;
        let dir = tempfile::tempdir().unwrap();

        let run = pair(&service.uri(), "K7QM-2XPA", dir.path(), false).await;

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(
            run.json(),
            json!({"server": service.uri(), "link_id": "link-b", "assistant_name": "Hermes"})
        );
        assert_eq!(run.stderr, "");
        assert_eq!(
            credentials::load(dir.path()).unwrap(),
            Some(Credential {
                link_id: "link-b".into(),
                assistant_name: "Hermes".into(),
                server: service.uri(),
                token: "alt_b".into(),
            })
        );
    }

    #[tokio::test]
    async fn replaces_a_stored_credential() {
        let service = service(200, paired()).await;
        let dir = tempfile::tempdir().unwrap();
        credentials::save(dir.path(), &stored("link-a")).unwrap();

        let run = pair(&service.uri(), "K7QM-2XPA", dir.path(), false).await;

        assert_eq!(run.exit, Exit::Success);
        let credential = credentials::load(dir.path()).unwrap().unwrap();
        assert_eq!(credential.link_id, "link-b");
    }

    #[tokio::test]
    async fn prints_text_when_human() {
        let service = service(200, paired()).await;
        let dir = tempfile::tempdir().unwrap();

        let run = pair(&service.uri(), "K7QM-2XPA", dir.path(), true).await;

        assert_eq!(run.exit, Exit::Success);
        assert!(run.stdout.contains("Hermes"), "{}", run.stdout);
        assert!(run.stdout.contains("link-b"), "{}", run.stdout);
    }

    #[tokio::test]
    async fn rejected_codes_exit_4_and_keep_the_stored_credential() {
        for reason in ["invalid_code", "expired_code", "invalid_params"] {
            let service =
                service(422, json!({"error": {"code": reason, "message": "Nope."}})).await;
            let dir = tempfile::tempdir().unwrap();
            credentials::save(dir.path(), &stored("link-a")).unwrap();

            let run = pair(&service.uri(), "K7QM-2XPA", dir.path(), false).await;

            assert_eq!(run.exit, Exit::Rejected, "{reason}");
            assert_eq!(
                run.json(),
                json!({"error": {"code": reason, "message": "Nope."}})
            );
            assert_eq!(
                credentials::load(dir.path()).unwrap(),
                Some(stored("link-a"))
            );
        }
    }

    #[tokio::test]
    async fn rejected_code_in_human_mode_prints_the_service_message_on_stderr() {
        let service = service(
            422,
            json!({"error": {"code": "expired_code", "message": "This pairing code has expired."}}),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();

        let run = pair(&service.uri(), "K7QM-2XPA", dir.path(), true).await;

        assert_eq!(run.exit, Exit::Rejected);
        assert_eq!(run.stdout, "");
        assert!(
            run.stderr.contains("This pairing code has expired."),
            "{}",
            run.stderr
        );
    }

    #[tokio::test]
    async fn server_errors_exit_5_and_store_nothing() {
        let service = service(503, json!({})).await;
        let dir = tempfile::tempdir().unwrap();

        let run = pair(&service.uri(), "K7QM-2XPA", dir.path(), false).await;

        assert_eq!(run.exit, Exit::Unreachable);
        assert_eq!(run.json()["error"]["code"], "unreachable");
        assert_eq!(credentials::load(dir.path()).unwrap(), None);
    }

    #[tokio::test]
    async fn unreachable_service_exits_5_naming_the_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let dir = tempfile::tempdir().unwrap();

        let run = pair(&server, "K7QM-2XPA", dir.path(), false).await;

        assert_eq!(run.exit, Exit::Unreachable);
        let printed = run.json();
        assert_eq!(printed["error"]["code"], "unreachable");
        assert!(
            printed["error"]["message"]
                .as_str()
                .unwrap()
                .contains(&server),
            "{printed}"
        );
    }

    #[tokio::test]
    async fn redirects_are_not_followed_and_name_the_target() {
        let service = MockServer::start().await;
        let target = format!("{}/moved/alp/v1/pair", service.uri());
        Mock::given(method("POST"))
            .and(path("/alp/v1/pair"))
            .respond_with(ResponseTemplate::new(301).insert_header("location", target.as_str()))
            .mount(&service)
            .await;
        Mock::given(path("/moved/alp/v1/pair"))
            .respond_with(ResponseTemplate::new(200).set_body_json(paired()))
            .expect(0)
            .mount(&service)
            .await;
        let dir = tempfile::tempdir().unwrap();

        let run = pair(&service.uri(), "K7QM-2XPA", dir.path(), false).await;

        assert_eq!(run.exit, Exit::Unexpected);
        let printed = run.json();
        assert_eq!(printed["error"]["code"], "unexpected");
        assert!(
            printed["error"]["message"]
                .as_str()
                .unwrap()
                .contains(&target),
            "{printed}"
        );
        assert_eq!(credentials::load(dir.path()).unwrap(), None);
    }

    #[tokio::test]
    async fn other_responses_are_unexpected() {
        for (status, body) in [
            (
                404,
                json!({"error": {"code": "not_found", "message": "Not found."}}),
            ),
            (200, json!({"link_id": "link-b"})),
        ] {
            let service = service(status, body).await;
            let dir = tempfile::tempdir().unwrap();

            let run = pair(&service.uri(), "K7QM-2XPA", dir.path(), false).await;

            assert_eq!(run.exit, Exit::Unexpected, "{status}");
            assert_eq!(run.json()["error"]["code"], "unexpected", "{status}");
            assert_eq!(credentials::load(dir.path()).unwrap(), None);
        }
    }
}
