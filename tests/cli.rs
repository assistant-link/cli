use std::io::{BufRead, BufReader, Lines, Read, Write};
use std::process::{Child, ChildStderr, Command, ExitStatus, Output, Stdio};
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

fn ready() -> Message {
    Message::text(
        json!({
            "jsonrpc": "2.0",
            "method": "connection.ready",
            "params": {"link_id": "link-1", "assistant_name": "Hermes", "protocol_version": "1"}
        })
        .to_string(),
    )
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
                socket.send(ready()).await.unwrap();
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
    assert!(
        connected.to_lowercase().contains("connected"),
        "{connected}"
    );
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
        socket.send(ready()).await.unwrap();
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
    let stderr = BufReader::new(child.stderr.take().unwrap());

    let (status, elapsed, rest) = tokio::task::spawn_blocking(move || {
        let mut lines = stderr.lines();
        let connected = lines.next().unwrap().unwrap();
        assert!(connected.contains("\"connected\""), "{connected}");
        kill("INT", &child);
        std::thread::sleep(std::time::Duration::from_millis(200));
        let second = std::time::Instant::now();
        kill("INT", &child);
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

fn with_key(mut command: Command) -> Command {
    command.env("AL_LINK_KEY", "alt_1");
    command
}

async fn link_service(status: u16, body: Value) -> MockServer {
    let service = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/alp/v1/link"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&service)
        .await;
    service
}

#[tokio::test]
async fn status_makes_one_get_of_the_link_and_never_upgrades_to_a_websocket() {
    let service = link_service(
        200,
        json!({"link_id": "link-1", "assistant_name": "Hermes", "unread_count": 2}),
    )
    .await;

    let output = output(with_key(al(&service.uri(), &["status"]))).await;

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout_json(&output),
        json!({"server": service.uri(), "link_id": "link-1", "assistant_name": "Hermes", "unread_count": 2, "check": "accepted"})
    );
    let requests = service.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method.as_str(), "GET");
    assert_eq!(requests[0].url.path(), "/alp/v1/link");
    assert_eq!(
        requests[0].headers.get("authorization").unwrap(),
        "Bearer alt_1"
    );
    assert!(!requests[0].headers.contains_key("upgrade"));
}

#[tokio::test]
async fn status_checks_exit_with_their_codes() {
    for (status, body, check, code) in [
        (
            401,
            json!({"error": {"code": "unauthorized", "message": "No."}}),
            "rejected",
            4,
        ),
        (503, json!({}), "unreachable", 5),
        (404, json!({}), "unexpected", 1),
    ] {
        let service = link_service(status, body).await;

        let output = output(with_key(al(&service.uri(), &["status"]))).await;

        assert_eq!(output.status.code(), Some(code), "{check}");
        let printed = stdout_json(&output);
        assert_eq!(printed["check"], check);
        assert!(printed["error"]["code"].is_string());
    }
}

async fn messages_service(status: u16, expected_calls: u64) -> MockServer {
    let service = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/alp/v1/messages"))
        .respond_with(
            ResponseTemplate::new(status)
                .set_body_json(json!({"message_id": "m-1", "created_at": "2026-10-22T09:30:00Z"})),
        )
        .expect(expected_calls)
        .mount(&service)
        .await;
    service
}

fn sent_texts(requests: &[wiremock::Request]) -> Vec<Value> {
    requests
        .iter()
        .map(|request| request.body_json::<Value>().unwrap()["text"].clone())
        .collect()
}

#[tokio::test]
async fn send_posts_once_prints_the_message_and_opens_no_websocket() {
    let service = messages_service(201, 1).await;

    let output = output(with_key(al(
        &service.uri(),
        &["send", "Your reminder is set."],
    )))
    .await;

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout_json(&output),
        json!({"message_id": "m-1", "created_at": "2026-10-22T09:30:00Z"})
    );
    let requests = service.received_requests().await.unwrap();
    assert_eq!(sent_texts(&requests), ["Your reminder is set."]);
    assert!(!requests[0].headers.contains_key("upgrade"));
}

#[tokio::test]
async fn send_reads_standard_input_for_a_dash() {
    let service = messages_service(201, 1).await;
    let mut command = with_key(al(&service.uri(), &["send", "-"]));
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = tokio::task::spawn_blocking(move || {
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all("Line one\nLine two\n".as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();

    assert_eq!(output.status.code(), Some(0));
    let requests = service.received_requests().await.unwrap();
    assert_eq!(sent_texts(&requests), ["Line one\nLine two"]);
}

#[tokio::test]
async fn send_makes_one_attempt_when_the_service_fails() {
    let service = messages_service(503, 1).await;

    let output = output(with_key(al(&service.uri(), &["send", "Hi"]))).await;

    assert_eq!(output.status.code(), Some(5));
    assert_eq!(stdout_json(&output)["error"]["code"], "unreachable");
    drop(service);
}

#[tokio::test]
async fn send_rejections_exit_4() {
    let service = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(422)
                .set_body_json(json!({"error": {"code": "text_too_long", "message": "Too long."}})),
        )
        .mount(&service)
        .await;

    let output = output(with_key(al(&service.uri(), &["send", "Hi"]))).await;

    assert_eq!(output.status.code(), Some(4));
    assert_eq!(
        stdout_json(&output),
        json!({"error": {"code": "text_too_long", "message": "Too long."}})
    );
}

#[test]
fn send_without_a_key_exits_3_and_without_a_text_exits_2() {
    let no_key = al("https://example.test", &["send", "Hi"])
        .output()
        .unwrap();
    assert_eq!(no_key.status.code(), Some(3));
    assert_eq!(stdout_json(&no_key)["error"]["code"], "no_key");

    let no_text = with_key(al("https://example.test", &["send"]))
        .output()
        .unwrap();
    assert_eq!(no_text.status.code(), Some(2));
}

#[test]
fn send_to_an_unreachable_service_exits_5() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);

    let output = with_key(al(&server, &["send", "Hi"])).output().unwrap();

    assert_eq!(output.status.code(), Some(5));
    assert_eq!(stdout_json(&output)["error"]["code"], "unreachable");
}

fn help_of(args: &[&str]) -> String {
    let output = al("https://example.test", args).output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{args:?}");
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn help_is_general_and_shows_the_key_option_of_this_os() {
    let help = help_of(&["help"]);

    for command in ["pair", "status", "wait", "send"] {
        assert!(help.contains(&format!("\n  {command} ")), "{command}");
    }
    assert!(help.contains("Run `al <command> --help` for details."));
    assert!(
        help.contains("If your environment has a safer way to guard credentials, use it instead.")
    );
    let (this_os, other_os) = if cfg!(target_os = "macos") {
        ("add-generic-password", "secret-tool")
    } else if cfg!(target_os = "linux") {
        ("secret-tool", "add-generic-password")
    } else {
        ("--key-file ~/.config/assistant-link", "secret-tool")
    };
    assert!(help.contains(this_os));
    assert!(!help.contains(other_os));
}

#[test]
fn command_help_describes_the_command() {
    for (command, expected) in [
        ("wait", r#"{"id":"...","type":"text_message""#),
        ("send", "idempotency_key"),
        ("status", "unread_count"),
    ] {
        let help = help_of(&[command, "--help"]);
        assert!(help.contains(expected), "{command}");
        assert!(!help.contains("Link key:"), "{command}");
    }
}

struct EventService {
    url: String,
    send: mpsc::UnboundedSender<Value>,
    frames: mpsc::UnboundedReceiver<Value>,
    closed: mpsc::UnboundedReceiver<bool>,
}

async fn event_service() -> EventService {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (send, mut commands) = mpsc::unbounded_channel::<Value>();
    let (frames_tx, frames) = mpsc::unbounded_channel();
    let (closed_tx, closed) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket.send(ready()).await.unwrap();
        let mut closed_cleanly = false;
        loop {
            tokio::select! {
                Some(command) = commands.recv() => {
                    if socket.send(Message::text(command.to_string())).await.is_err() {
                        break;
                    }
                }
                message = socket.next() => match message {
                    Some(Ok(Message::Text(text))) => {
                        let _ = frames_tx.send(serde_json::from_str(&text).unwrap());
                    }
                    Some(Ok(Message::Close(_))) => closed_cleanly = true,
                    Some(Ok(_)) => {}
                    _ => break,
                },
            }
        }
        let _ = closed_tx.send(closed_cleanly);
    });
    EventService {
        url,
        send,
        frames,
        closed,
    }
}

fn deliver_request(id: u64, event_id: &str, text: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "event.deliver", "params": {
        "id": event_id,
        "type": "text_message",
        "created_at": "2026-10-22T09:30:00Z",
        "deleted": false,
        "text_message": {"text": text},
    }})
}

const WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT_LIMIT, future).await.unwrap()
}

// A child that does not exit is killed and reported, not left to hang the test run.
fn finish(mut child: Child, stderr: Lines<BufReader<ChildStderr>>) -> (ExitStatus, Vec<String>) {
    let deadline = std::time::Instant::now() + WAIT_LIMIT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let stderr: Vec<String> = stderr.map(Result::unwrap).collect();
    let status = status.unwrap_or_else(|| panic!("al did not exit; its stderr: {stderr:?}"));
    (status, stderr)
}

#[tokio::test]
async fn wait_prints_each_event_line_on_stdout_then_acknowledges_it() {
    let mut service = event_service().await;
    let command = with_key(al(&service.url, &["wait"]));
    let (mut child, _) = tokio::task::spawn_blocking(move || spawn_wait(command))
        .await
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    service.send.send(deliver_request(41, "e1", "Hi")).unwrap();
    let (line, stdout) = tokio::task::spawn_blocking(move || {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        (line, stdout)
    })
    .await
    .unwrap();
    let ack = within(service.frames.recv()).await.unwrap();

    assert_eq!(
        line,
        format!("{}\n", deliver_request(0, "e1", "Hi")["params"])
    );
    assert_eq!(
        ack,
        json!({"jsonrpc": "2.0", "id": 41, "result": {"event_id": "e1"}})
    );
    kill("INT", &child);
    let status = tokio::task::spawn_blocking(move || {
        drop(stdout);
        child.wait().unwrap()
    })
    .await
    .unwrap();
    assert_eq!(status.code(), Some(0));
}

#[tokio::test]
async fn sigint_with_a_write_pending_exits_without_waiting_or_acknowledging() {
    let mut service = event_service().await;
    let command = with_key(al(&service.url, &["wait"]));
    let (mut child, stderr) = tokio::task::spawn_blocking(move || spawn_wait(command))
        .await
        .unwrap();
    // Nobody reads this stdout, and the first event is far larger than a pipe holds.
    let _unread = child.stdout.take().unwrap();

    service
        .send
        .send(deliver_request(1, "e1", &"x".repeat(1 << 20)))
        .unwrap();
    service.send.send(deliver_request(2, "e2", "Hi")).unwrap();
    let refusal = within(service.frames.recv()).await.unwrap();
    assert_eq!(refusal["id"], 2);
    assert_eq!(refusal["error"]["code"], -32001);
    assert_eq!(refusal["error"]["data"]["reason"], "not_received");

    kill("INT", &child);
    let (status, _) = tokio::task::spawn_blocking(move || finish(child, stderr))
        .await
        .unwrap();

    assert_eq!(status.code(), Some(0));
    assert_eq!(within(service.closed.recv()).await, Some(true));
    assert!(
        service.frames.try_recv().is_err(),
        "the event was acknowledged"
    );
}

#[tokio::test]
async fn wait_exits_1_when_standard_output_is_closed() {
    let mut service = event_service().await;
    let command = with_key(al(&service.url, &["wait"]));
    let (mut child, stderr) = tokio::task::spawn_blocking(move || spawn_wait(command))
        .await
        .unwrap();
    drop(child.stdout.take());

    // A process another test forks at that moment briefly shares the pipe, and a write to it
    // succeeds. So keep sending events until one fails.
    let send = service.send.clone();
    let sender = tokio::spawn(async move {
        for id in 1.. {
            if send
                .send(deliver_request(id, &format!("e{id}"), "Hi"))
                .is_err()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    });
    let (status, stderr) = tokio::task::spawn_blocking(move || finish(child, stderr))
        .await
        .unwrap();
    sender.abort();

    assert_eq!(status.code(), Some(1));
    let stopped: Value = serde_json::from_str(stderr.last().unwrap()).unwrap();
    assert_eq!(stopped["status"], "stopped");
    assert_eq!(stopped["error"]["code"], "unexpected");
    assert!(
        stopped["error"]["message"]
            .as_str()
            .unwrap()
            .contains("standard output")
    );
    assert_eq!(within(service.closed.recv()).await, Some(true));
}
