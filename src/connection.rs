use std::time::Duration;

use futures_util::StreamExt;
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout_at};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, USER_AGENT};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::jsonrpc::{self, Event, Ready};
use crate::output::{Exit, Failure, describe};
use crate::server::{self, ErrorBody};

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

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

pub async fn open(server: &str, token: &str) -> Result<Connection, Failure> {
    let deadline = Instant::now() + server::TIMEOUT;
    let mut socket = upgrade(server, token, deadline).await?;
    let ready = await_ready(server, &mut socket, deadline).await?;
    Ok(Connection { socket, ready })
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
        if socket.close(None).await.is_ok() {
            while let Some(Ok(_)) = socket.next().await {}
        }
    })
    .await;
}

fn timed_out(server: &str) -> Failure {
    server::unreachable(server, "timed out waiting for connection.ready")
}

async fn connect(server: &str, token: &str) -> Result<Socket, Failure> {
    let mut request = server::websocket_endpoint(server, "/alp/v1/connect")
        .into_client_request()
        .map_err(|error| server::unreachable(server, describe(&error)))?;
    let authorization = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
        Failure::new(
            Exit::Unexpected,
            "unexpected",
            "The Link key is not a valid header value. Check that it is the key `al pair` printed.",
        )
    })?;
    request.headers_mut().insert(AUTHORIZATION, authorization);
    request
        .headers_mut()
        .insert(USER_AGENT, HeaderValue::from_static(server::USER_AGENT));

    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|error| upgrade_failure(server, error))?;
    Ok(socket)
}

async fn read_ready(server: &str, socket: &mut Socket) -> Result<Ready, Failure> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => match event(&text) {
                Some(Event::Ready(ready)) => return Ok(ready),
                Some(Event::Closing(closing)) => {
                    return Err(closing_failure(server, &closing.reason));
                }
                None => {}
            },
            Some(Ok(Message::Close(_))) | None => {
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

fn upgrade_failure(server: &str, error: tungstenite::Error) -> Failure {
    let tungstenite::Error::Http(response) = error else {
        return server::unreachable(server, describe(&error));
    };
    let status = response.status();
    if status.is_server_error() {
        return server::unavailable(server, status);
    }
    if status != StatusCode::UNAUTHORIZED {
        return server::not_alp(server, status);
    }
    let error = response
        .body()
        .as_deref()
        .and_then(|body| serde_json::from_slice::<ErrorBody>(body).ok())
        .map(|body| body.error);
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
