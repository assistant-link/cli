use std::fmt::Display;
use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::output::{Exit, Failure, describe};

pub const USER_AGENT: &str = concat!("al/", env!("CARGO_PKG_VERSION"));

// Generous, because the first request after an idle period waits for the service to cold start.
pub const TIMEOUT: Duration = Duration::from_secs(30);

pub fn unreachable(server: &str, detail: impl Display) -> Failure {
    Failure::new(
        Exit::Unreachable,
        "unreachable",
        format!("Could not reach the service at {server}: {detail}"),
    )
}

pub fn invalid_key() -> Failure {
    Failure::new(
        Exit::Unexpected,
        "unexpected",
        "The Link key is not a valid header value. Check that it is the key `al pair` printed.",
    )
}

pub fn unavailable(server: &str, status: impl Display) -> Failure {
    Failure::new(
        Exit::Unreachable,
        "unreachable",
        format!("The service at {server} is unavailable ({status})."),
    )
}

pub fn not_alp(server: &str, status: impl Display) -> Failure {
    Failure::new(
        Exit::Unexpected,
        "unexpected",
        format!(
            "The service at {server} answered with {status}. Check that it is an Assistant Link service."
        ),
    )
}

pub fn rejected_key(server: &str, error: Option<ServiceError>) -> Failure {
    let (code, detail) = error.map_or_else(
        || ("unauthorized".to_owned(), ".".to_owned()),
        |error| (error.code, format!(": {}", error.message)),
    );
    Failure::new(
        Exit::Rejected,
        &code,
        format!(
            "The service at {server} rejected the Link key{detail} Check that {server} is the service named in the pairing text; if it is, ask the user for new pairing text."
        ),
    )
}

#[derive(Deserialize)]
pub struct ErrorBody {
    pub error: ServiceError,
}

#[derive(Deserialize)]
pub struct ServiceError {
    pub code: String,
    pub message: String,
}

pub fn endpoint(server: &str, path: &str) -> String {
    format!("{}{path}", server.trim_end_matches('/'))
}

pub fn client() -> Result<reqwest::Client, Failure> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| Failure::new(Exit::Unexpected, "unexpected", describe(&error)))
}

// One attempt, no retry. 401 is a key the service does not accept; 409 and 422 are the service
// rejecting the request.
pub async fn call<T: DeserializeOwned>(
    server: &str,
    key: &str,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<T, Failure> {
    let mut request = client()?
        .request(method, endpoint(server, path))
        .bearer_auth(key);
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request.send().await.map_err(|error| {
        if error.is_builder() {
            invalid_key()
        } else {
            unreachable(server, describe(&error))
        }
    })?;

    let status = response.status();
    if status.is_success() {
        return response.json().await.map_err(|error| {
            Failure::new(
                Exit::Unexpected,
                "unexpected",
                format!(
                    "The service at {server} answered {path} with an unexpected body: {}",
                    describe(&error)
                ),
            )
        });
    }
    if status.is_server_error() {
        return Err(unavailable(server, status));
    }
    let error = response
        .json::<ErrorBody>()
        .await
        .ok()
        .map(|body| body.error);
    match (status, error) {
        (StatusCode::UNAUTHORIZED, error) => Err(rejected_key(server, error)),
        (StatusCode::CONFLICT | StatusCode::UNPROCESSABLE_ENTITY, Some(error)) => {
            Err(Failure::new(Exit::Rejected, &error.code, error.message))
        }
        _ => Err(not_alp(server, status)),
    }
}

pub fn websocket_endpoint(server: &str, path: &str) -> String {
    let (scheme, authority, prefix) = split(server);
    let scheme = if scheme.eq_ignore_ascii_case("https") {
        "wss"
    } else {
        "ws"
    };
    format!("{scheme}://{authority}{prefix}{path}")
}

fn split(server: &str) -> (&str, &str, &str) {
    let server = server.trim_end_matches('/');
    let (scheme, rest) = server.split_once("://").unwrap_or(("", server));
    let (authority, path) = rest.find('/').map_or((rest, ""), |at| rest.split_at(at));
    (scheme, authority, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_joins_the_server_and_path() {
        assert_eq!(
            endpoint("https://example.test/prefix", "/alp/v1/pair"),
            "https://example.test/prefix/alp/v1/pair"
        );
    }

    #[test]
    fn websocket_endpoint_switches_the_scheme() {
        assert_eq!(
            websocket_endpoint("https://example.test", "/alp/v1/connect"),
            "wss://example.test/alp/v1/connect"
        );
        assert_eq!(
            websocket_endpoint("HTTP://localhost:4000", "/alp/v1/connect"),
            "ws://localhost:4000/alp/v1/connect"
        );
    }
}
