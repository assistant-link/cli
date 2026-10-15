use std::fs;
use std::path::Path;

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;

use crate::key;
use crate::output::{Exit, Failure, Output, describe};
use crate::server::{self, ErrorBody};

#[derive(Deserialize)]
struct Paired {
    link_id: String,
    assistant_name: String,
    token: String,
}

pub async fn run(server: &str, code: &str, key_file: Option<&Path>, out: &mut Output<'_>) -> Exit {
    let key_file = match key_file
        .map(|path| key::create(path).map(|file| (path, file)))
        .transpose()
    {
        Ok(key_file) => key_file,
        Err(failure) => return out.fail(&failure),
    };
    let paired = match request(server, code).await {
        Ok(paired) => paired,
        Err(failure) => {
            if let Some((path, _)) = key_file {
                let _ = fs::remove_file(path);
            }
            return out.fail(&failure);
        }
    };
    let mut report = json!({
        "server": server,
        "link_id": paired.link_id,
        "assistant_name": paired.assistant_name,
    });
    let mut text = format!(
        "Paired with \"{}\" (Link {}) at {server}.",
        paired.assistant_name, paired.link_id
    );
    if let Some((path, file)) = key_file {
        if let Err(error) = key::write(file, &paired.token) {
            let _ = fs::remove_file(path);
            return out.fail(&Failure::new(
                Exit::Unexpected,
                "unexpected",
                format!(
                    "Paired Link {}, but could not write its key to {}: {error}. Ask the user to create a new Link in the app and send you its pairing text.",
                    paired.link_id,
                    path.display()
                ),
            ));
        }
        text.push_str(&format!(" Link key written to {}.", path.display()));
    } else {
        report["link_key"] = json!(paired.token);
        text.push_str(&format!("\nLink key: {}", paired.token));
    }
    out.report(&report, &text);
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
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use serde_json::{Value, json};
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

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

    async fn pair(server: &str, key_file: Option<&Path>, human: bool) -> Run {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let exit = run(
            server,
            "K7QM-2XPA",
            key_file,
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

    async fn service_expecting(calls: u64, status: u16, body: Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/alp/v1/pair"))
            .and(body_json(json!({"code": "K7QM-2XPA"})))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .expect(calls)
            .mount(&server)
            .await;
        server
    }

    async fn service(status: u16, body: Value) -> MockServer {
        service_expecting(1, status, body).await
    }

    fn paired() -> Value {
        json!({"link_id": "link-b", "assistant_name": "Hermes", "token": "alt_b"})
    }

    fn key_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("al-key");
        (dir, path)
    }

    #[tokio::test]
    async fn prints_the_link_and_its_key() {
        let service = service(200, paired()).await;

        let run = pair(&service.uri(), None, false).await;

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(
            run.json(),
            json!({
                "server": service.uri(),
                "link_id": "link-b",
                "assistant_name": "Hermes",
                "link_key": "alt_b",
            })
        );
        assert_eq!(run.stderr, "");
    }

    #[tokio::test]
    async fn writes_the_key_to_a_new_private_file_instead_of_printing_it() {
        let service = service(200, paired()).await;
        let (_dir, path) = key_path();

        let run = pair(&service.uri(), Some(&path), false).await;

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(
            run.json(),
            json!({"server": service.uri(), "link_id": "link-b", "assistant_name": "Hermes"})
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "alt_b\n");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn an_existing_key_file_exits_2_without_sending_the_code() {
        let service = service_expecting(0, 200, paired()).await;
        let (_dir, path) = key_path();
        fs::write(&path, "alt_a\n").unwrap();

        let run = pair(&service.uri(), Some(&path), false).await;

        assert_eq!(run.exit, Exit::Usage);
        assert_eq!(run.json()["error"]["code"], "key_file_exists");
        assert_eq!(fs::read_to_string(&path).unwrap(), "alt_a\n");
    }

    #[tokio::test]
    async fn prints_text_when_human() {
        let service = service(200, paired()).await;

        let run = pair(&service.uri(), None, true).await;

        assert_eq!(run.exit, Exit::Success);
        for expected in ["Hermes", "link-b", "alt_b"] {
            assert!(run.stdout.contains(expected), "{expected}: {}", run.stdout);
        }
    }

    #[tokio::test]
    async fn human_text_names_the_key_file_without_the_key() {
        let service = service(200, paired()).await;
        let (_dir, path) = key_path();

        let run = pair(&service.uri(), Some(&path), true).await;

        assert_eq!(run.exit, Exit::Success);
        assert!(
            run.stdout.contains(&path.display().to_string()),
            "{}",
            run.stdout
        );
        assert!(!run.stdout.contains("alt_b"), "{}", run.stdout);
    }

    #[tokio::test]
    async fn rejected_codes_exit_4_and_leave_no_key_file() {
        for reason in ["invalid_code", "expired_code", "invalid_params"] {
            let service =
                service(422, json!({"error": {"code": reason, "message": "Nope."}})).await;
            let (_dir, path) = key_path();

            let run = pair(&service.uri(), Some(&path), false).await;

            assert_eq!(run.exit, Exit::Rejected, "{reason}");
            assert_eq!(
                run.json(),
                json!({"error": {"code": reason, "message": "Nope."}})
            );
            assert!(!path.exists(), "{reason}");
        }
    }

    #[tokio::test]
    async fn rejected_code_in_human_mode_prints_the_service_message_on_stderr() {
        let service = service(
            422,
            json!({"error": {"code": "expired_code", "message": "This pairing code has expired."}}),
        )
        .await;

        let run = pair(&service.uri(), None, true).await;

        assert_eq!(run.exit, Exit::Rejected);
        assert_eq!(run.stdout, "");
        assert!(
            run.stderr.contains("This pairing code has expired."),
            "{}",
            run.stderr
        );
    }

    #[tokio::test]
    async fn server_errors_exit_5_and_leave_no_key_file() {
        let service = service(503, json!({})).await;
        let (_dir, path) = key_path();

        let run = pair(&service.uri(), Some(&path), false).await;

        assert_eq!(run.exit, Exit::Unreachable);
        assert_eq!(run.json()["error"]["code"], "unreachable");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn unreachable_service_exits_5_naming_the_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);

        let run = pair(&server, None, false).await;

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

        let run = pair(&service.uri(), None, false).await;

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

            let run = pair(&service.uri(), None, false).await;

            assert_eq!(run.exit, Exit::Unexpected, "{status}");
            assert_eq!(run.json()["error"]["code"], "unexpected", "{status}");
        }
    }
}
