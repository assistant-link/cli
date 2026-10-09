use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Output, Stdio};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn al(server: &str, config_home: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_al"));
    command
        .args(args)
        .env("AL_SERVER", server)
        .env("XDG_CONFIG_HOME", config_home)
        .env_remove("HOME");
    command
}

async fn output(mut command: Command) -> Output {
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn pair_stores_a_private_credential_under_xdg_config_home() {
    use std::os::unix::fs::PermissionsExt;

    let service = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/alp/v1/pair"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"link_id": "link-1", "assistant_name": "Hermes", "token": "alt_1"}),
        ))
        .mount(&service)
        .await;
    let config_home = tempfile::tempdir().unwrap();

    let output = output(al(
        &service.uri(),
        config_home.path(),
        &["pair", "K7QM-2XPA"],
    ))
    .await;

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout_json(&output),
        json!({"server": service.uri(), "link_id": "link-1", "assistant_name": "Hermes"})
    );
    let file = config_home.path().join("assistant-link/credentials.json");
    let stored: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(stored["token"], "alt_1");
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[tokio::test]
async fn pair_with_a_rejected_code_exits_4() {
    let service = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/alp/v1/pair"))
        .respond_with(ResponseTemplate::new(422).set_body_json(
            json!({"error": {"code": "expired_code", "message": "This pairing code has expired."}}),
        ))
        .mount(&service)
        .await;
    let config_home = tempfile::tempdir().unwrap();

    let output = output(al(
        &service.uri(),
        config_home.path(),
        &["pair", "K7QM-2XPA"],
    ))
    .await;

    assert_eq!(output.status.code(), Some(4));
    assert_eq!(stdout_json(&output)["error"]["code"], "expired_code");
    assert!(!config_home.path().join("assistant-link").exists());
}

fn store_credential(config_home: &Path, server: &str) {
    let dir = config_home.join("assistant-link");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("credentials.json"),
        json!({"link_id": "link-1", "assistant_name": "Hermes", "server": server, "token": "alt_1"})
            .to_string(),
    )
    .unwrap();
}

#[test]
fn status_without_a_credential_exits_3() {
    let config_home = tempfile::tempdir().unwrap();
    let output = al("https://example.test", config_home.path(), &["status"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(stdout_json(&output)["check"], "not_paired");
}

#[test]
fn status_for_another_server_exits_6() {
    let config_home = tempfile::tempdir().unwrap();
    store_credential(config_home.path(), "https://other.example");
    let output = al("https://example.test", config_home.path(), &["status"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(6));
    assert_eq!(stdout_json(&output)["check"], "server_mismatch");
}

#[test]
fn wait_without_a_credential_exits_3() {
    let config_home = tempfile::tempdir().unwrap();
    let output = al("https://example.test", config_home.path(), &["wait"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
}

#[test]
fn wait_for_another_server_exits_6() {
    let config_home = tempfile::tempdir().unwrap();
    store_credential(config_home.path(), "https://other.example");
    let output = al("https://example.test", config_home.path(), &["wait"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(6));
    assert!(output.stdout.is_empty());
}

async fn holding_service() -> (String, mpsc::UnboundedReceiver<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (closed_tx, closed) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let closed_tx = closed_tx.clone();
            tokio::spawn(async move {
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                let ready = json!({
                    "jsonrpc": "2.0",
                    "method": "connection.ready",
                    "params": {"link_id": "link-1", "assistant_name": "Hermes", "protocol_version": "1"}
                });
                socket.send(Message::text(ready.to_string())).await.unwrap();
                let mut closed_cleanly = false;
                while let Some(Ok(message)) = socket.next().await {
                    closed_cleanly |= matches!(message, Message::Close(_));
                }
                let _ = closed_tx.send(closed_cleanly);
            });
        }
    });
    (url, closed)
}

async fn wait_then_signal(signal: &str) {
    let (server, mut closed) = holding_service().await;
    let config_home = tempfile::tempdir().unwrap();
    store_credential(config_home.path(), &server);
    let mut command = al(&server, config_home.path(), &["wait"]);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let pid = child.id().to_string();
    let stderr = BufReader::new(child.stderr.take().unwrap());

    let signal = signal.to_owned();
    let (status, stdout, stderr) = tokio::task::spawn_blocking(move || {
        let mut lines = stderr.lines();
        let connected = lines.next().unwrap().unwrap();
        assert!(connected.contains("\"connected\""), "{connected}");
        let killed = Command::new("kill")
            .args([format!("-{signal}"), pid])
            .status()
            .unwrap();
        assert!(killed.success());
        let rest: Vec<String> = lines.map(Result::unwrap).collect();
        let mut stdout = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        (child.wait().unwrap(), stdout, rest)
    })
    .await
    .unwrap();

    assert_eq!(status.code(), Some(0));
    assert_eq!(stdout, "");
    assert_eq!(stderr, [r#"{"status":"stopped"}"#]);
    assert_eq!(closed.recv().await, Some(true));
}

#[tokio::test]
async fn wait_closes_cleanly_and_exits_0_on_sigint() {
    wait_then_signal("INT").await;
}

#[tokio::test]
async fn wait_closes_cleanly_and_exits_0_on_sigterm() {
    wait_then_signal("TERM").await;
}

async fn service_that_never_answers_close() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let ready = json!({
            "jsonrpc": "2.0",
            "method": "connection.ready",
            "params": {"link_id": "link-1", "assistant_name": "Hermes", "protocol_version": "1"}
        });
        socket.send(Message::text(ready.to_string())).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });
    url
}

#[tokio::test]
async fn wait_exits_at_once_on_a_second_signal() {
    let server = service_that_never_answers_close().await;
    let config_home = tempfile::tempdir().unwrap();
    store_credential(config_home.path(), &server);
    let mut command = al(&server, config_home.path(), &["wait"]);
    command.stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let pid = child.id().to_string();
    let stderr = BufReader::new(child.stderr.take().unwrap());

    let (status, elapsed, rest) = tokio::task::spawn_blocking(move || {
        let interrupt = || {
            assert!(
                Command::new("kill")
                    .args(["-INT", &pid])
                    .status()
                    .unwrap()
                    .success()
            );
        };
        let mut lines = stderr.lines();
        let connected = lines.next().unwrap().unwrap();
        assert!(connected.contains("\"connected\""), "{connected}");
        interrupt();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let second = std::time::Instant::now();
        interrupt();
        let status = child.wait().unwrap();
        let rest: Vec<String> = lines.map(Result::unwrap).collect();
        (status, second.elapsed(), rest)
    })
    .await
    .unwrap();

    assert_eq!(status.code(), Some(0));
    assert!(elapsed < std::time::Duration::from_secs(1), "{elapsed:?}");
    assert_eq!(rest, [r#"{"status":"stopped"}"#]);
}

#[test]
fn usage_errors_exit_2() {
    let config_home = tempfile::tempdir().unwrap();
    let output = al("https://example.test", config_home.path(), &["pair"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}
