use std::fmt::Display;
use std::time::Duration;

use serde::Deserialize;

use crate::output::{Exit, Failure};

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
