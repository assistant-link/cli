use std::io::{BufRead, BufReader, Lines, Read};
use std::process::{Child, ChildStderr, Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn al(server: &str, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_al"));
    command
        .args(args)
        .env("AL_SERVER", server)
        .env_remove("AL_LINK_KEY")
        .env_remove("HOME")
        .env_remove("XDG_CONFIG_HOME");
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

async fn pairing_service(status: u16, body: Value) -> MockServer {
    let service = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/alp/v1/pair"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&service)
        .await;
    service
}

fn paired() -> Value {
    json!({"link_id": "link-1", "assistant_name": "Hermes", "token": "alt_1"})
}

#[tokio::test]
async fn pair_prints_the_key_and_writes_no_file() {
    let service = pairing_service(200, paired()).await;
    let cwd = tempfile::tempdir().unwrap();
    let mut command = al(&service.uri(), &["pair", "K7QM-2XPA"]);
    command.current_dir(cwd.path());

    let output = output(command).await;

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout_json(&output),
        json!({"server": service.uri(), "link_id": "link-1", "assistant_name": "Hermes", "link_key": "alt_1"})
    );
    assert_eq!(std::fs::read_dir(cwd.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn pair_writes_the_key_to_a_private_key_file() {
    use std::os::unix::fs::PermissionsExt;

    let service = pairing_service(200, paired()).await;
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("al-key");

    let output = output(al(
        &service.uri(),
        &[
            "pair",
            "K7QM-2XPA",
            "--key-file",
            key_file.to_str().unwrap(),
        ],
    ))
    .await;

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout_json(&output),
        json!({"server": service.uri(), "link_id": "link-1", "assistant_name": "Hermes"})
    );
    assert_eq!(std::fs::read_to_string(&key_file).unwrap(), "alt_1\n");
    assert_eq!(
        std::fs::metadata(&key_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[tokio::test]
async fn pair_with_a_rejected_code_exits_4_and_writes_no_key_file() {
    let service = pairing_service(
        422,
        json!({"error": {"code": "expired_code", "message": "This pairing code has expired."}}),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("al-key");

    let output = output(al(
        &service.uri(),
        &[
            "pair",
            "K7QM-2XPA",
            "--key-file",
            key_file.to_str().unwrap(),
        ],
    ))
    .await;

    assert_eq!(output.status.code(), Some(4));
    assert_eq!(stdout_json(&output)["error"]["code"], "expired_code");
    assert!(!key_file.exists());
}

#[test]
fn pair_into_an_existing_key_file_exits_2() {
    let key_file = tempfile::NamedTempFile::new().unwrap();
    let output = al(
        "https://example.test",
        &[
            "pair",
            "K7QM-2XPA",
            "--key-file",
            key_file.path().to_str().unwrap(),
        ],
    )
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout_json(&output)["error"]["code"], "key_file_exists");
}

#[test]
fn status_without_a_key_exits_3() {
    let output = al("https://example.test", &["status"]).output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(stdout_json(&output)["check"], "no_key");
}

#[test]
fn wait_without_a_key_exits_3() {
    let output = al("https://example.test", &["wait"]).output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
}

type Authorizations = Arc<Mutex<Vec<String>>>;

async fn holding_service() -> (String, mpsc::UnboundedReceiver<bool>, Authorizations) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (closed_tx, closed) = mpsc::unbounded_channel();
    let authorizations = Authorizations::default();
    let seen = Arc::clone(&authorizations);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (closed_tx, seen) = (closed_tx.clone(), Arc::clone(&seen));
            tokio::spawn(async move {
                // The error type is tungstenite's handshake callback contract.
                #[allow(clippy::result_large_err)]
                let record = |request: &Request, response: Response| {
                    if let Some(value) = request.headers().get("authorization") {
                        seen.lock()
                            .unwrap()
                            .push(value.to_str().unwrap().to_owned());
                    }
                    Ok(response)
                };
                let mut socket = tokio_tungstenite::accept_hdr_async(stream, record)
                    .await
                    .unwrap();
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
    (url, closed, authorizations)
}

fn spawn_wait(mut command: Command) -> (Child, Lines<BufReader<ChildStderr>>) {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let connected = lines.next().unwrap().unwrap();
    assert!(connected.contains("\"connected\""), "{connected}");
    (child, lines)
}

fn kill(signal: &str, child: &Child) {
    let killed = Command::new("kill")
        .args([format!("-{signal}"), child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
}

async fn wait_then_signal(signal: &str) {
    let (server, mut closed, authorizations) = holding_service().await;
    let mut command = al(&server, &["wait"]);
    command.env("AL_LINK_KEY", "alt_1");

    let signal = signal.to_owned();
    let (status, stdout, stderr) = tokio::task::spawn_blocking(move || {
        let (mut child, lines) = spawn_wait(command);
        kill(&signal, &child);
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
    assert_eq!(*authorizations.lock().unwrap(), ["Bearer alt_1"]);
}

#[tokio::test]
async fn wait_closes_cleanly_and_exits_0_on_sigint() {
    wait_then_signal("INT").await;
}

#[tokio::test]
async fn wait_closes_cleanly_and_exits_0_on_sigterm() {
    wait_then_signal("TERM").await;
}

#[tokio::test]
async fn two_links_wait_side_by_side_on_one_host_user() {
    let (server, mut closed, authorizations) = holding_service().await;
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("muse-key");
    std::fs::write(&key_file, "alt_muse\n").unwrap();
    let mut hermes = al(&server, &["wait"]);
    hermes.env("AL_LINK_KEY", "alt_hermes");
    let muse = al(&server, &["wait", "--key-file", key_file.to_str().unwrap()]);

    let (hermes, mut muse) = tokio::task::spawn_blocking(move || {
        let (mut hermes, _) = spawn_wait(hermes);
        let (muse, _) = spawn_wait(muse);
        kill("INT", &hermes);
        assert_eq!(hermes.wait().unwrap().code(), Some(0));
        (hermes, muse)
    })
    .await
    .unwrap();
    drop(hermes);

    assert_eq!(closed.recv().await, Some(true));
    assert_eq!(muse.try_wait().unwrap(), None, "muse stopped with hermes");
    let mut seen = authorizations.lock().unwrap().clone();
    seen.sort();
    assert_eq!(seen, ["Bearer alt_hermes", "Bearer alt_muse"]);

    kill("INT", &muse);
    let status = tokio::task::spawn_blocking(move || muse.wait().unwrap())
        .await
        .unwrap();
    assert_eq!(status.code(), Some(0));
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
    let mut command = al(&server, &["wait"]);
    command
        .env("AL_LINK_KEY", "alt_1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
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
    let output = al("https://example.test", &["pair"]).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
}
