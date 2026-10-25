use std::io;
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http;
use tokio_tungstenite::{WebSocketStream, accept_hdr_async};

pub type ServerSocket = WebSocketStream<TcpStream>;

#[derive(Debug, Clone)]
pub enum Upgrade {
    Accept,
    Reject(u16, String),
}

pub struct MockService {
    pub url: String,
    authorizations: Arc<Mutex<Vec<String>>>,
}

impl MockService {
    pub async fn start<F, Fut>(upgrade: Upgrade, session: F) -> Self
    where
        F: Fn(ServerSocket) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self::scripted(move |_| upgrade.clone(), session).await
    }

    pub async fn scripted<U, F, Fut>(upgrade: U, session: F) -> Self
    where
        U: Fn(usize) -> Upgrade + Send + Sync + 'static,
        F: Fn(ServerSocket) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let authorizations = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&authorizations);
        let (upgrade, session) = (Arc::new(upgrade), Arc::new(session));
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (upgrade, session, seen) = (
                    Arc::clone(&upgrade),
                    Arc::clone(&session),
                    Arc::clone(&seen),
                );
                tokio::spawn(async move {
                    // tungstenite's handshake callback fixes this Result type.
                    #[allow(clippy::result_large_err)]
                    let callback = |request: &Request, response: Response| {
                        let mut seen = seen.lock().unwrap();
                        let attempt = seen.len();
                        seen.push(
                            request
                                .headers()
                                .get("authorization")
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default()
                                .to_owned(),
                        );
                        if request.uri().path() != "/alp/v1/connect" {
                            return Err(reject(404, "{}".into()));
                        }
                        match upgrade(attempt) {
                            Upgrade::Accept => Ok(response),
                            Upgrade::Reject(status, body) => Err(reject(status, body)),
                        }
                    };
                    if let Ok(socket) = accept_hdr_async(stream, callback).await {
                        session(socket).await;
                    }
                });
            }
        });
        Self {
            url,
            authorizations,
        }
    }

    pub fn authorizations(&self) -> Vec<String> {
        self.authorizations.lock().unwrap().clone()
    }
}

fn reject(status: u16, body: String) -> ErrorResponse {
    http::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Some(body))
        .unwrap()
}

pub fn unauthorized() -> Upgrade {
    Upgrade::Reject(
        401,
        json!({"error": {"code": "unauthorized", "message": "Missing or invalid bearer token."}})
            .to_string(),
    )
}

pub async fn send(socket: &mut ServerSocket, message: Value) {
    socket
        .send(Message::text(message.to_string()))
        .await
        .unwrap();
}

pub async fn send_ready(socket: &mut ServerSocket) {
    send(
        socket,
        json!({
            "jsonrpc": "2.0",
            "method": "connection.ready",
            "params": {"link_id": "link-1", "assistant_name": "Hermes", "protocol_version": "1"}
        }),
    )
    .await;
}

// The pong can only come from the client's read loop after it handled connection.ready, so the
// client has logged "connected" by the time this returns.
pub async fn send_ready_and_await_pong(socket: &mut ServerSocket) {
    send_ready(socket).await;
    socket.send(Message::Ping(Vec::new().into())).await.unwrap();
    while let Some(Ok(message)) = socket.next().await {
        if let Message::Pong(_) = message {
            return;
        }
    }
}

pub async fn send_closing(socket: &mut ServerSocket, reason: &str) {
    send(
        socket,
        json!({"jsonrpc": "2.0", "method": "connection.closing", "params": {"reason": reason}}),
    )
    .await;
    let _ = socket.close(None).await;
}

pub fn closed_port_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}", listener.local_addr().unwrap())
}

/// Standard output that tests can read back.
#[derive(Clone, Default)]
pub struct SharedStdout(Arc<Mutex<Vec<u8>>>);

impl SharedStdout {
    pub fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl io::Write for SharedStdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Standard output whose every write waits for a release, like a pipe nobody reads. Dropping the
/// release sender fails the waiting write.
pub struct Blocked {
    release: std::sync::mpsc::Receiver<()>,
    stdout: SharedStdout,
}

impl Blocked {
    pub fn new() -> (Self, std::sync::mpsc::Sender<()>, SharedStdout) {
        let (release_tx, release) = std::sync::mpsc::channel();
        let stdout = SharedStdout::default();
        let blocked = Self {
            release,
            stdout: stdout.clone(),
        };
        (blocked, release_tx, stdout)
    }
}

impl io::Write for Blocked {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.release.recv().map_err(|_| io::ErrorKind::BrokenPipe)?;
        self.stdout.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Standard output whose reader has gone away.
pub struct BrokenPipe;

impl io::Write for BrokenPipe {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::BrokenPipe.into())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
