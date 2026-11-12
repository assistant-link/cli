use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest_websocket::{Message, Upgrade, WebSocket};
use tokio::time::{Instant, timeout_at};

use crate::jsonrpc::{self, Event, Ready};
use crate::output::{Exit, Failure, describe};
use crate::server::{self, ErrorBody};

pub type Socket = WebSocket;

const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

pub struct Connection {
    pub socket: Socket,
    pub ready: Ready,
}

pub fn account_deleted() -> Failure {
    Failure::new(
        Exit::Rejected,
        "account_deleted",
        "The user's account was deleted, so this Link no longer exists. Ask the user for new pairing text.",
    )
}

pub async fn upgrade(server: &str, token: &str, deadline: Instant) -> Result<Socket, Failure> {
    timeout_at(deadline, connect(server, token))
        .await
        .unwrap_or_else(|_| Err(timed_out(server)))
}

pub async fn await_ready(
    server: &str,
    socket: &mut Socket,
    deadline: Instant,
) -> Result<Ready, Failure> {
    let ready = timeout_at(deadline, read_ready(server, socket))
        .await
        .unwrap_or_else(|_| Err(timed_out(server)));
    if ready.is_err() {
        close(socket).await;
    }
    ready
}

pub async fn close(socket: &mut Socket) {
    close_by(socket, Instant::now() + CLOSE_TIMEOUT).await;
}

// The service may already have accepted an upgrade that is still in flight; dropping it would end
// that WebSocket without a close frame.
pub async fn close_when_upgraded(upgrade: impl Future<Output = Result<Socket, Failure>>) {
    let deadline = Instant::now() + CLOSE_TIMEOUT;
    if let Ok(Ok(mut socket)) = timeout_at(deadline, upgrade).await {
        close_by(&mut socket, deadline).await;
    }
}

async fn close_by(socket: &mut Socket, deadline: Instant) {
    let _ = timeout_at(deadline, async {
        if SinkExt::close(socket).await.is_ok() {
            while let Some(Ok(_)) = socket.next().await {}
        }
    })
    .await;
}

fn timed_out(server: &str) -> Failure {
    server::unreachable(server, "timed out waiting for connection.ready")
}

async fn connect(server: &str, token: &str) -> Result<Socket, Failure> {
    let authorization =
        HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| server::invalid_key())?;
    let response = server::client()?
        .get(server::endpoint(server, "/alp/v1/connect"))
        .header(AUTHORIZATION, authorization)
        .upgrade()
        .send()
        .await
        .map_err(|error| server::unreachable(server, describe(&error)))?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Err(upgrade_failure(server, response.into_inner()).await);
    }
    response
        .into_websocket()
        .await
        .map_err(|error| server::unreachable(server, describe(&error)))
}

async fn read_ready(server: &str, socket: &mut Socket) -> Result<Ready, Failure> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => match event(&text) {
                Some(Event::Ready(ready)) => return Ok(ready),
                Some(Event::Closing(closing)) => {
                    return Err(closing_failure(server, &closing.reason));
                }
                Some(Event::CaughtUp) | None => {}
            },
            Some(Ok(Message::Close { .. })) | None => {
                return Err(server::unreachable(
                    server,
                    "the connection closed before connection.ready",
                ));
            }
            Some(Ok(_)) => {}
            Some(Err(error)) => return Err(server::unreachable(server, describe(&error))),
        }
    }
}

pub fn event(text: &str) -> Option<Event> {
    match jsonrpc::parse(text) {
        Ok(jsonrpc::Message::Notification(notification)) => notification.event(),
        _ => None,
    }
}

pub fn closing_failure(server: &str, reason: &str) -> Failure {
    if reason == "account_deleted" {
        account_deleted()
    } else {
        server::unreachable(
            server,
            format!("the service closed the connection ({reason})"),
        )
    }
}

async fn upgrade_failure(server: &str, response: reqwest::Response) -> Failure {
    let status = response.status();
    if status.is_server_error() {
        return server::unavailable(server, status);
    }
    if status != StatusCode::UNAUTHORIZED {
        return server::not_alp(server, status);
    }
    let error = response
        .json::<ErrorBody>()
        .await
        .ok()
        .map(|body| body.error);
    server::rejected_key(server, error)
}
