use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::output::{Exit, Failure, Output, describe};
use crate::server;

#[derive(Deserialize, Serialize)]
struct Sent {
    message_id: String,
    created_at: String,
}

pub async fn run(
    server: &str,
    key: Result<String, Failure>,
    text: &str,
    stdin: impl Read,
    out: &mut Output<'_>,
) -> Exit {
    let sent: Result<Sent, Failure> = async {
        let key = key?;
        let body = json!({"text": text_from(text, stdin)?, "idempotency_key": uuid_v7()});
        server::call(server, &key, Method::POST, "/alp/v1/messages", Some(&body)).await
    }
    .await;
    match sent {
        Ok(sent) => {
            out.report(
                &json!(sent),
                &format!("Sent message {} at {}.", sent.message_id, sent.created_at),
            );
            Exit::Success
        }
        Err(failure) => out.fail(&failure),
    }
}

fn text_from(text: &str, mut stdin: impl Read) -> Result<String, Failure> {
    if text != "-" {
        return Ok(text.to_owned());
    }
    let mut input = Vec::new();
    stdin
        .read_to_end(&mut input)
        .map_err(|error| Failure::new(Exit::Unexpected, "unexpected", describe(&error)))?;
    let mut input = String::from_utf8(input).map_err(|_| {
        Failure::new(
            Exit::Usage,
            "invalid_text",
            "Standard input is not valid UTF-8.",
        )
    })?;
    if input.ends_with('\n') {
        input.pop();
    }
    Ok(input)
}

// RFC 9562 UUIDv7: a 48-bit Unix millisecond timestamp, then random bits.
fn uuid_v7() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let mut bytes: [u8; 16] = rand::random();
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[10..]);
    bytes[6] = 0x70 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::key;
    use crate::testing::{self, Buffers};

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

    async fn send_with(
        server: &str,
        key: Result<String, Failure>,
        text: &str,
        stdin: &[u8],
        human: bool,
    ) -> Run {
        let mut buffers = Buffers::default();
        let exit = run(server, key, text, stdin, &mut buffers.output(human)).await;
        Run {
            exit,
            stdout: buffers.stdout(),
            stderr: buffers.stderr(),
        }
    }

    async fn send(server: &str, text: &str) -> Run {
        send_with(server, Ok("alt_1".into()), text, b"", false).await
    }

    async fn messages_service(response: ResponseTemplate) -> MockServer {
        let service = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/alp/v1/messages"))
            .and(header("authorization", "Bearer alt_1"))
            .respond_with(response)
            .mount(&service)
            .await;
        service
    }

    fn stored(status: u16) -> ResponseTemplate {
        ResponseTemplate::new(status)
            .set_body_json(json!({"message_id": "m-1", "created_at": "2026-10-22T09:30:00Z"}))
    }

    fn error(status: u16, code: &str) -> ResponseTemplate {
        ResponseTemplate::new(status)
            .set_body_json(json!({"error": {"code": code, "message": "No."}}))
    }

    async fn bodies(service: &MockServer) -> Vec<Value> {
        service
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| request.body_json().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn sends_the_text_with_an_idempotency_key_and_prints_the_message() {
        let service = messages_service(stored(201)).await;

        let run = send(&service.uri(), "Your reminder is set.").await;

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(
            run.json(),
            json!({"message_id": "m-1", "created_at": "2026-10-22T09:30:00Z"})
        );
        assert_eq!(run.stderr, "");
        let bodies = bodies(&service).await;
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["text"], "Your reminder is set.");
        assert!(bodies[0]["idempotency_key"].is_string());
        assert_eq!(bodies[0].as_object().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn human_output_says_the_same() {
        let service = messages_service(stored(201)).await;

        let run = send_with(&service.uri(), Ok("alt_1".into()), "Hi", b"", true).await;

        assert_eq!(run.exit, Exit::Success);
        for expected in ["m-1", "2026-10-22T09:30:00Z"] {
            assert!(run.stdout.contains(expected), "{}", run.stdout);
        }
    }

    #[tokio::test]
    async fn a_repeat_response_prints_the_same_as_a_new_one() {
        let created = messages_service(stored(201)).await;
        let repeated = messages_service(stored(200)).await;

        let first = send(&created.uri(), "Hi").await;
        let second = send(&repeated.uri(), "Hi").await;

        assert_eq!(second.exit, Exit::Success);
        assert_eq!(first.stdout, second.stdout);
    }

    #[tokio::test]
    async fn each_run_uses_a_fresh_idempotency_key() {
        let service = messages_service(stored(201)).await;

        send(&service.uri(), "Hi").await;
        send(&service.uri(), "Hi").await;

        let bodies = bodies(&service).await;
        assert_ne!(bodies[0]["idempotency_key"], bodies[1]["idempotency_key"]);
    }

    #[tokio::test]
    async fn a_dash_reads_standard_input_and_drops_one_trailing_line_feed() {
        let service = messages_service(stored(201)).await;

        for (stdin, expected) in [
            ("Héllo\nworld\n", "Héllo\nworld"),
            ("no newline", "no newline"),
            ("two\n\n", "two\n"),
            ("", ""),
        ] {
            let run = send_with(
                &service.uri(),
                Ok("alt_1".into()),
                "-",
                stdin.as_bytes(),
                false,
            )
            .await;
            assert_eq!(run.exit, Exit::Success);
            assert_eq!(
                bodies(&service).await.last().unwrap()["text"],
                expected,
                "{stdin:?}"
            );
        }
    }

    #[tokio::test]
    async fn standard_input_that_is_not_utf8_is_a_usage_error_without_a_request() {
        let service = messages_service(stored(201)).await;

        let run = send_with(
            &service.uri(),
            Ok("alt_1".into()),
            "-",
            &[0xff, 0xfe],
            false,
        )
        .await;

        assert_eq!(run.exit, Exit::Usage);
        assert_eq!(run.json()["error"]["code"], "invalid_text");
        assert!(service.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_text_is_only_read_from_standard_input_for_a_dash() {
        let service = messages_service(stored(201)).await;

        send_with(&service.uri(), Ok("alt_1".into()), "Hi", b"ignored", false).await;

        assert_eq!(bodies(&service).await[0]["text"], "Hi");
    }

    #[tokio::test]
    async fn rejections_exit_4_with_the_services_error() {
        for (status, code) in [
            (422, "text_too_long"),
            (409, "idempotency_key_conflict"),
            (401, "unauthorized"),
        ] {
            let service = messages_service(error(status, code)).await;

            let run = send(&service.uri(), "Hi").await;

            assert_eq!(run.exit, Exit::Rejected, "{status}");
            assert_eq!(run.json()["error"]["code"], code);
            assert!(run.json()["error"]["message"].is_string());
        }
    }

    #[tokio::test]
    async fn makes_one_attempt_and_never_retries() {
        for status in [503, 500] {
            let service = messages_service(ResponseTemplate::new(status)).await;

            let run = send(&service.uri(), "Hi").await;

            assert_eq!(run.exit, Exit::Unreachable, "{status}");
            assert_eq!(run.json()["error"]["code"], "unreachable");
            assert_eq!(service.received_requests().await.unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn an_unreachable_service_exits_5() {
        let run = send(&testing::closed_port_url(), "Hi").await;

        assert_eq!(run.exit, Exit::Unreachable);
        assert_eq!(run.json()["error"]["code"], "unreachable");
    }

    #[tokio::test]
    async fn other_statuses_exit_1() {
        let service = MockServer::start().await;

        let run = send(&service.uri(), "Hi").await;

        assert_eq!(run.exit, Exit::Unexpected);
        assert_eq!(run.json()["error"]["code"], "unexpected");
    }

    #[tokio::test]
    async fn no_key_exits_3_without_a_request_or_reading_standard_input() {
        let service = messages_service(stored(201)).await;

        let run = send_with(&service.uri(), key::load(None, None), "Hi", b"", false).await;

        assert_eq!(run.exit, Exit::NoKey);
        assert_eq!(run.json()["error"]["code"], "no_key");
        assert!(service.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_key_that_is_not_a_header_value_is_unexpected_not_unreachable() {
        let service = messages_service(stored(201)).await;

        let run = send_with(&service.uri(), Ok("bad\nkey".into()), "Hi", b"", false).await;

        assert_eq!(run.exit, Exit::Unexpected);
        assert!(service.received_requests().await.unwrap().is_empty());
    }

    #[test]
    fn uuid_v7_has_the_version_variant_and_a_current_timestamp() {
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let uuid = uuid_v7();
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();

        let groups: Vec<&str> = uuid.split('-').collect();
        assert_eq!(
            groups.iter().map(|group| group.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(uuid.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
        assert_eq!(uuid, uuid.to_lowercase());
        assert!(groups[2].starts_with('7'), "{uuid}");
        assert!("89ab".contains(groups[3].chars().next().unwrap()), "{uuid}");
        let millis = u128::from_str_radix(&format!("{}{}", groups[0], groups[1]), 16).unwrap();
        assert!((before..=after).contains(&millis), "{uuid}");
        assert_ne!(uuid, uuid_v7());
    }
}
