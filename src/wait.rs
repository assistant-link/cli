use std::pin::{Pin, pin};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::select;
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::tungstenite::Message;

use crate::connection::{self, Connection, Socket};
use crate::deliver::Delivery;
use crate::jsonrpc::{self, Event, Response};
use crate::output::{Exit, Failure, Output, describe};
use crate::server;

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    pub reset_after: Duration,
    pub silence: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            backoff_base: Duration::from_secs(1),
            backoff_cap: Duration::from_secs(30),
            reset_after: Duration::from_secs(60),
            silence: Duration::from_secs(60),
        }
    }
}

struct Backoff {
    base: Duration,
    cap: Duration,
    failures: u32,
}

impl Backoff {
    fn new(timing: Timing) -> Self {
        Self {
            base: timing.backoff_base,
            cap: timing.backoff_cap,
            failures: 0,
        }
    }

    fn next(&mut self) -> Duration {
        let ceiling = self
            .base
            .saturating_mul(2u32.saturating_pow(self.failures))
            .min(self.cap);
        self.failures = self.failures.saturating_add(1);
        ceiling.mul_f64(rand::random_range(0.5..=1.0))
    }

    fn reset(&mut self) {
        self.failures = 0;
    }
}

enum Attempt {
    Shutdown,
    Connected(Box<Connection>),
    Failed(Failure),
}

enum Ended {
    Shutdown,
    Lost(String),
    Fatal(Failure),
}

pub async fn run(
    server: &str,
    key: &str,
    out: &mut Output<'_>,
    timing: Timing,
    shutdown: impl Future<Output = ()>,
    delivery: &mut Delivery,
) -> Exit {
    let mut shutdown = pin!(shutdown);
    let mut backoff = Backoff::new(timing);

    loop {
        let failure = match connect(server, key, &mut shutdown).await {
            Attempt::Shutdown => return stop(out, None),
            Attempt::Connected(mut connection) => {
                let ready = &connection.ready;
                out.log(
                    &json!({
                        "status": "connected",
                        "server": server,
                        "link_id": ready.link_id,
                        "assistant_name": ready.assistant_name,
                    }),
                    &format!(
                        "Connected to {server} as \"{}\" (Link {}).",
                        ready.assistant_name, ready.link_id
                    ),
                );
                let connected_at = Instant::now();
                match hold(
                    server,
                    &mut connection.socket,
                    timing.silence,
                    &mut shutdown,
                    delivery,
                    out,
                )
                .await
                {
                    Ended::Shutdown => {
                        connection::close(&mut connection.socket).await;
                        return stop(out, None);
                    }
                    Ended::Fatal(failure) => {
                        connection::close(&mut connection.socket).await;
                        return stop(out, Some(failure));
                    }
                    Ended::Lost(detail) => {
                        if connected_at.elapsed() >= timing.reset_after {
                            backoff.reset();
                        }
                        Failure::new(
                            Exit::Unreachable,
                            "connection_lost",
                            format!("Lost the connection to {server}: {detail}"),
                        )
                    }
                }
            }
            Attempt::Failed(failure) if failure.exit == Exit::Unreachable => failure,
            Attempt::Failed(failure) => return stop(out, Some(failure)),
        };

        let delay = backoff.next();
        out.log(
            &json!({
                "status": "reconnecting",
                "delay_ms": u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                "error": failure,
            }),
            &format!(
                "{} Reconnecting in {:.1} s.",
                failure.message,
                delay.as_secs_f64()
            ),
        );
        let mut pause = pin!(sleep(delay));
        loop {
            select! {
                () = &mut shutdown => return stop(out, None),
                () = &mut pause => break,
                // Without a connection the acknowledgement is dropped; the service sends the event
                // again and gets it acknowledged without a second write.
                written = delivery.written() => {
                    if let Err(error) = written {
                        return stop(out, Some(write_failure(&error)));
                    }
                }
            }
        }
    }
}

fn write_failure(error: &std::io::Error) -> Failure {
    Failure::new(
        Exit::Unexpected,
        "unexpected",
        format!("Cannot write to standard output: {}", describe(error)),
    )
}

async fn connect(
    server: &str,
    token: &str,
    shutdown: &mut Pin<&mut impl Future<Output = ()>>,
) -> Attempt {
    let deadline = Instant::now() + server::TIMEOUT;
    let mut upgrade = pin!(connection::upgrade(server, token, deadline));
    let upgraded = select! {
        () = &mut *shutdown => None,
        upgraded = &mut upgrade => Some(upgraded),
    };
    let mut socket = match upgraded {
        None => {
            connection::close_when_upgraded(upgrade).await;
            return Attempt::Shutdown;
        }
        Some(Err(failure)) => return Attempt::Failed(failure),
        Some(Ok(socket)) => socket,
    };
    let ready = select! {
        () = &mut *shutdown => None,
        ready = connection::await_ready(server, &mut socket, deadline) => Some(ready),
    };
    match ready {
        None => {
            connection::close(&mut socket).await;
            Attempt::Shutdown
        }
        Some(Err(failure)) => Attempt::Failed(failure),
        Some(Ok(ready)) => Attempt::Connected(Box::new(Connection { socket, ready })),
    }
}

async fn hold(
    server: &str,
    socket: &mut Socket,
    silence: Duration,
    shutdown: &mut Pin<&mut impl Future<Output = ()>>,
    delivery: &mut Delivery,
    out: &mut Output<'_>,
) -> Ended {
    let mut closing_reason = None;
    loop {
        let next = select! {
            biased;
            () = &mut *shutdown => return Ended::Shutdown,
            written = delivery.written() => {
                match written {
                    Ok(acknowledgement) => {
                        if let Err(ended) = reply(socket, &acknowledgement, silence, shutdown).await {
                            return ended;
                        }
                        continue;
                    }
                    Err(error) => return Ended::Fatal(write_failure(&error)),
                }
            }
            next = timeout(silence, socket.next()) => next,
        };
        let message = match next {
            Err(_) => {
                return Ended::Lost(format!(
                    "no message from the service for {} s",
                    silence.as_secs()
                ));
            }
            Ok(None) => {
                return Ended::Lost(closing_reason.map_or_else(
                    || "the connection closed".to_owned(),
                    |reason| format!("the service closed the connection ({reason})"),
                ));
            }
            Ok(Some(Err(error))) => return Ended::Lost(describe(&error)),
            Ok(Some(Ok(message))) => message,
        };
        let Message::Text(text) = message else {
            continue;
        };
        match jsonrpc::parse(&text) {
            Ok(jsonrpc::Message::Request(request)) => {
                let response = if request.method == "event.deliver" {
                    match delivery.deliver(request, out) {
                        Some(response) => response,
                        None => continue,
                    }
                } else {
                    Response::method_not_found(request.id, &request.method)
                };
                if let Err(ended) = reply(socket, &response, silence, shutdown).await {
                    return ended;
                }
            }
            Ok(jsonrpc::Message::Notification(notification)) => {
                if let Some(Event::Closing(closing)) = notification.event() {
                    let failure = connection::closing_failure(server, &closing.reason);
                    if failure.exit == Exit::Rejected {
                        return Ended::Fatal(failure);
                    }
                    closing_reason = Some(closing.reason);
                }
            }
            _ => {}
        }
    }
}

async fn reply(
    socket: &mut Socket,
    response: &Response,
    silence: Duration,
    shutdown: &mut Pin<&mut impl Future<Output = ()>>,
) -> Result<(), Ended> {
    let sent = select! {
        () = &mut *shutdown => return Err(Ended::Shutdown),
        sent = timeout(silence, socket.send(Message::text(response.encode()))) => sent,
    };
    match sent {
        Err(_) => Err(Ended::Lost(format!(
            "could not send to the service for {} s",
            silence.as_secs()
        ))),
        Ok(Err(error)) => Err(Ended::Lost(describe(&error))),
        Ok(Ok(())) => Ok(()),
    }
}

pub fn stop(out: &mut Output, failure: Option<Failure>) -> Exit {
    let Some(failure) = failure else {
        out.log(&json!({"status": "stopped"}), "Stopped.");
        return Exit::Success;
    };
    out.log(
        &json!({"status": "stopped", "error": failure}),
        &format!("Stopped: {}", failure.message),
    );
    failure.exit
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::Message;

    use super::*;
    use crate::testing::{
        self, Blocked, BrokenPipe, MockService, ServerSocket, SharedStdout, Upgrade,
    };

    fn fast() -> Timing {
        Timing {
            backoff_base: Duration::from_millis(10),
            backoff_cap: Duration::from_millis(40),
            reset_after: Duration::from_secs(60),
            silence: Duration::from_secs(5),
        }
    }

    struct Run {
        exit: Exit,
        stdout: String,
        stderr: String,
    }

    impl Run {
        fn lines(&self) -> Vec<Value> {
            self.stderr
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }

        fn statuses(&self) -> Vec<String> {
            self.lines()
                .iter()
                .map(|line| line["status"].as_str().unwrap().to_owned())
                .collect()
        }
    }

    const KEY: &str = "alt_1";

    async fn wait_with(
        server: &str,
        human: bool,
        timing: Timing,
        shutdown: impl Future<Output = ()>,
        mut delivery: Delivery,
    ) -> Run {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let exit = run(
            server,
            KEY,
            &mut Output {
                human,
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
            timing,
            shutdown,
            &mut delivery,
        )
        .await;
        assert!(stdout.is_empty(), "events go through the Delivery");
        Run {
            exit,
            stdout: String::new(),
            stderr: String::from_utf8(stderr).unwrap(),
        }
    }

    async fn wait(
        server: &str,
        human: bool,
        timing: Timing,
        shutdown: impl Future<Output = ()>,
    ) -> Run {
        let stdout = SharedStdout::default();
        let mut run = wait_with(
            server,
            human,
            timing,
            shutdown,
            Delivery::new(stdout.clone()),
        )
        .await;
        run.stdout = stdout.text();
        run
    }

    async fn hold_until_closed(socket: &mut ServerSocket) -> bool {
        while let Some(Ok(message)) = socket.next().await {
            if let Message::Close(_) = message {
                return true;
            }
        }
        false
    }

    async fn holding_service(
        connected: mpsc::UnboundedSender<()>,
        closed: mpsc::UnboundedSender<bool>,
    ) -> MockService {
        MockService::start(Upgrade::Accept, move |mut socket| {
            let (connected, closed) = (connected.clone(), closed.clone());
            async move {
                testing::send_ready_and_await_pong(&mut socket).await;
                let _ = connected.send(());
                let _ = closed.send(hold_until_closed(&mut socket).await);
            }
        })
        .await
    }

    async fn first(receiver: &mut mpsc::UnboundedReceiver<()>) {
        receiver.recv().await.unwrap();
    }

    #[test]
    fn backoff_doubles_from_the_base_up_to_the_cap_with_jitter() {
        let mut backoff = Backoff::new(Timing {
            backoff_base: Duration::from_secs(1),
            backoff_cap: Duration::from_secs(30),
            ..Timing::default()
        });
        for ceiling in [1, 2, 4, 8, 16, 30, 30, 30] {
            let ceiling = Duration::from_secs(ceiling);
            let delay = backoff.next();
            assert!(
                delay >= ceiling / 2 && delay <= ceiling,
                "{delay:?} not in [{:?}, {ceiling:?}]",
                ceiling / 2
            );
        }
    }

    #[test]
    fn backoff_reset_starts_from_the_base_again() {
        let mut backoff = Backoff::new(Timing::default());
        for _ in 0..5 {
            backoff.next();
        }
        backoff.reset();
        assert!(backoff.next() <= Duration::from_secs(1));
    }

    #[test]
    fn backoff_jitters() {
        let mut delays = std::collections::HashSet::new();
        for _ in 0..20 {
            delays.insert(Backoff::new(Timing::default()).next());
        }
        assert!(delays.len() > 1);
    }

    #[test]
    fn default_timing_matches_the_design() {
        let timing = Timing::default();
        assert_eq!(timing.backoff_base, Duration::from_secs(1));
        assert_eq!(timing.backoff_cap, Duration::from_secs(30));
        assert_eq!(timing.reset_after, Duration::from_secs(60));
        assert_eq!(timing.silence, Duration::from_secs(60));
    }

    #[tokio::test]
    async fn holds_the_connection_until_shutdown_then_closes_cleanly() {
        let (connected_tx, mut connected) = mpsc::unbounded_channel();
        let (closed_tx, mut closed) = mpsc::unbounded_channel();
        let service = holding_service(connected_tx, closed_tx).await;

        let run = wait(&service.url, false, fast(), first(&mut connected)).await;

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(run.stdout, "");
        assert_eq!(run.statuses(), ["connected", "stopped"]);
        assert_eq!(
            run.lines()[0],
            json!({"status": "connected", "server": service.url, "link_id": "link-1", "assistant_name": "Hermes"})
        );
        assert_eq!(run.lines()[1], json!({"status": "stopped"}));
        assert_eq!(closed.recv().await, Some(true));
        assert_eq!(service.authorizations(), ["Bearer alt_1"]);
    }

    #[tokio::test]
    async fn human_mode_logs_text_to_stderr() {
        let (connected_tx, mut connected) = mpsc::unbounded_channel();
        let (closed_tx, _closed) = mpsc::unbounded_channel();
        let service = holding_service(connected_tx, closed_tx).await;

        let run = wait(&service.url, true, fast(), first(&mut connected)).await;

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(run.stdout, "");
        assert!(run.stderr.contains("Hermes"), "{}", run.stderr);
        assert!(serde_json::from_str::<Value>(run.stderr.lines().next().unwrap()).is_err());
    }

    #[tokio::test]
    async fn answers_pings() {
        let (pong_tx, mut pong) = mpsc::unbounded_channel();
        let service = MockService::start(Upgrade::Accept, move |mut socket| {
            let pong_tx = pong_tx.clone();
            async move {
                testing::send_ready(&mut socket).await;
                socket
                    .send(Message::Ping(b"hi".to_vec().into()))
                    .await
                    .unwrap();
                while let Some(Ok(message)) = socket.next().await {
                    if let Message::Pong(payload) = message {
                        let _ = pong_tx.send(payload.to_vec());
                    }
                }
            }
        })
        .await;

        let run = wait(&service.url, false, fast(), async {
            assert_eq!(pong.recv().await, Some(b"hi".to_vec()));
        })
        .await;

        assert_eq!(run.exit, Exit::Success);
    }

    #[tokio::test]
    async fn answers_requests_with_method_not_found_and_ignores_unknown_notifications() {
        let (reply_tx, mut reply) = mpsc::unbounded_channel();
        let service = MockService::start(Upgrade::Accept, move |mut socket| {
            let reply_tx = reply_tx.clone();
            async move {
                testing::send_ready(&mut socket).await;
                testing::send(
                    &mut socket,
                    json!({"jsonrpc": "2.0", "method": "message.new", "params": {"id": "m1"}}),
                )
                .await;
                socket.send(Message::text("not json")).await.unwrap();
                testing::send(
                    &mut socket,
                    json!({"jsonrpc": "2.0", "id": 9, "method": "device.ping"}),
                )
                .await;
                while let Some(Ok(message)) = socket.next().await {
                    if let Message::Text(text) = message {
                        let _ = reply_tx.send(serde_json::from_str::<Value>(&text).unwrap());
                    }
                }
            }
        })
        .await;

        let run = wait(&service.url, false, fast(), async {
            let reply = reply.recv().await.unwrap();
            assert_eq!(reply["id"], 9);
            assert_eq!(reply["error"]["code"], -32601);
        })
        .await;

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(run.statuses(), ["connected", "stopped"]);
    }

    #[tokio::test]
    async fn reconnects_after_the_connection_drops() {
        let (connected_tx, mut connected) = mpsc::unbounded_channel();
        let attempts = Arc::new(AtomicUsize::new(0));
        let service = MockService::start(Upgrade::Accept, move |mut socket| {
            let (connected_tx, attempts) = (connected_tx.clone(), Arc::clone(&attempts));
            async move {
                testing::send_ready_and_await_pong(&mut socket).await;
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    testing::send_closing(&mut socket, "shutdown").await;
                } else {
                    let _ = connected_tx.send(());
                    hold_until_closed(&mut socket).await;
                }
            }
        })
        .await;

        let run = wait(&service.url, false, fast(), first(&mut connected)).await;

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(
            run.statuses(),
            ["connected", "reconnecting", "connected", "stopped"]
        );
        let reconnecting = &run.lines()[1];
        assert_eq!(reconnecting["error"]["code"], "connection_lost");
        assert!(reconnecting["delay_ms"].as_u64().unwrap() <= 10);
    }

    #[tokio::test]
    async fn retries_unreachable_and_unavailable_service_with_growing_delays() {
        let (connected_tx, mut connected) = mpsc::unbounded_channel();
        let service = MockService::scripted(
            |attempt| match attempt {
                0..=2 => Upgrade::Reject(503, "starting".into()),
                _ => Upgrade::Accept,
            },
            move |mut socket| {
                let connected_tx = connected_tx.clone();
                async move {
                    testing::send_ready_and_await_pong(&mut socket).await;
                    let _ = connected_tx.send(());
                    hold_until_closed(&mut socket).await;
                }
            },
        )
        .await;

        let run = wait(&service.url, false, fast(), first(&mut connected)).await;

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(
            run.statuses(),
            [
                "reconnecting",
                "reconnecting",
                "reconnecting",
                "connected",
                "stopped"
            ]
        );
        let delays: Vec<u64> = run.lines()[..3]
            .iter()
            .map(|line| {
                assert_eq!(line["error"]["code"], "unreachable");
                line["delay_ms"].as_u64().unwrap()
            })
            .collect();
        assert!(
            delays[0] <= 10 && delays[1] >= 10 && delays[2] >= 20,
            "{delays:?}"
        );
    }

    #[tokio::test]
    async fn treats_a_silent_connection_as_lost() {
        let (connected_tx, mut connected) = mpsc::unbounded_channel();
        let attempts = Arc::new(AtomicUsize::new(0));
        let service = MockService::start(Upgrade::Accept, move |mut socket| {
            let (connected_tx, attempts) = (connected_tx.clone(), Arc::clone(&attempts));
            async move {
                testing::send_ready_and_await_pong(&mut socket).await;
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                } else {
                    let _ = connected_tx.send(());
                    hold_until_closed(&mut socket).await;
                }
            }
        })
        .await;
        let timing = Timing {
            silence: Duration::from_millis(100),
            ..fast()
        };

        let run = wait(&service.url, false, timing, first(&mut connected)).await;

        assert_eq!(
            run.statuses(),
            ["connected", "reconnecting", "connected", "stopped"]
        );
        assert_eq!(run.lines()[1]["error"]["code"], "connection_lost");
    }

    #[tokio::test]
    async fn account_deleted_exits_4_without_reconnecting() {
        let service = MockService::start(Upgrade::Accept, |mut socket| async move {
            testing::send_ready(&mut socket).await;
            testing::send_closing(&mut socket, "account_deleted").await;
        })
        .await;

        let run = wait(&service.url, false, fast(), std::future::pending()).await;

        assert_eq!(run.exit, Exit::Rejected);
        assert_eq!(run.statuses(), ["connected", "stopped"]);
        assert_eq!(run.lines()[1]["error"]["code"], "account_deleted");
        assert_eq!(service.authorizations().len(), 1);
    }

    #[tokio::test]
    async fn rejected_key_exits_4_without_reconnecting() {
        let service = MockService::start(testing::unauthorized(), |_| async {}).await;

        let run = wait(&service.url, false, fast(), std::future::pending()).await;

        assert_eq!(run.exit, Exit::Rejected);
        assert_eq!(run.statuses(), ["stopped"]);
        assert_eq!(run.lines()[0]["error"]["code"], "unauthorized");
        assert_eq!(service.authorizations().len(), 1);
    }

    #[tokio::test]
    async fn non_alp_response_exits_1_without_reconnecting() {
        let service = MockService::start(Upgrade::Reject(404, "{}".into()), |_| async {}).await;

        let run = wait(&service.url, false, fast(), std::future::pending()).await;

        assert_eq!(run.exit, Exit::Unexpected);
        assert_eq!(run.lines()[0]["error"]["code"], "unexpected");
        assert_eq!(service.authorizations().len(), 1);
    }

    #[tokio::test]
    async fn shutdown_while_waiting_to_reconnect_exits_0() {
        let server = testing::closed_port_url();
        let timing = Timing {
            backoff_base: Duration::from_secs(60),
            backoff_cap: Duration::from_secs(60),
            ..fast()
        };

        let run = wait(&server, false, timing, async {
            tokio::time::sleep(Duration::from_millis(100)).await;
        })
        .await;

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(run.statuses(), ["reconnecting", "stopped"]);
    }

    #[tokio::test]
    async fn shutdown_before_connection_ready_closes_cleanly() {
        let (upgraded_tx, mut upgraded) = mpsc::unbounded_channel();
        let (closed_tx, mut closed) = mpsc::unbounded_channel();
        let service = MockService::start(Upgrade::Accept, move |mut socket| {
            let (upgraded_tx, closed_tx) = (upgraded_tx.clone(), closed_tx.clone());
            async move {
                let _ = upgraded_tx.send(());
                let _ = closed_tx.send(hold_until_closed(&mut socket).await);
            }
        })
        .await;

        let run = wait(&service.url, false, fast(), first(&mut upgraded)).await;

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(run.statuses(), ["stopped"]);
        assert_eq!(closed.recv().await, Some(true));
    }

    #[tokio::test]
    async fn shutdown_during_the_upgrade_closes_cleanly() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        let (accepted_tx, mut accepted) = mpsc::unbounded_channel();
        let (closed_tx, mut closed) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ = accepted_tx.send(());
            sleep(Duration::from_millis(200)).await;
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = closed_tx.send(hold_until_closed(&mut socket).await);
        });

        let run = wait(&server, false, fast(), first(&mut accepted)).await;

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(run.statuses(), ["stopped"]);
        assert_eq!(closed.recv().await, Some(true));
    }

    #[tokio::test]
    async fn shutdown_takes_at_most_the_close_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        let (accepted_tx, mut accepted) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ = accepted_tx.send(());
            sleep(Duration::from_millis(1500)).await;
            let _socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            sleep(Duration::from_secs(10)).await;
        });
        let started = Instant::now();

        let run = wait(&server, false, fast(), first(&mut accepted)).await;

        assert_eq!(run.exit, Exit::Success);
        assert!(
            started.elapsed() < Duration::from_millis(2500),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn shutdown_works_while_a_reply_cannot_be_sent() {
        let (connected_tx, mut connected) = mpsc::unbounded_channel();
        let service = MockService::start(Upgrade::Accept, move |mut socket| {
            let connected_tx = connected_tx.clone();
            async move {
                testing::send_ready_and_await_pong(&mut socket).await;
                let _ = connected_tx.send(());
                let method = "x".repeat(4096);
                for id in 0.. {
                    let request = json!({"jsonrpc": "2.0", "id": id, "method": method});
                    if socket
                        .feed(Message::text(request.to_string()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        })
        .await;

        let run = timeout(
            Duration::from_secs(8),
            wait(&service.url, false, fast(), async {
                first(&mut connected).await;
                sleep(Duration::from_secs(1)).await;
            }),
        )
        .await
        .expect("al wait must stop on shutdown while its reply is stuck");

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(run.statuses(), ["connected", "stopped"]);
    }

    const SOON: Duration = Duration::from_millis(300);

    fn event(id: &str, text: &str) -> Value {
        json!({
            "id": id,
            "type": "text_message",
            "created_at": "2026-10-22T09:30:00Z",
            "deleted": false,
            "text_message": {"text": text},
        })
    }

    fn deliver_request(id: Value, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": "event.deliver", "params": params})
    }

    async fn next_text(socket: &mut ServerSocket) -> Value {
        while let Some(Ok(message)) = socket.next().await {
            if let Message::Text(text) = message {
                return serde_json::from_str(&text).unwrap();
            }
        }
        panic!("the connection closed before a message arrived");
    }

    async fn forward_until_closed(
        socket: &mut ServerSocket,
        replies: &mpsc::UnboundedSender<Value>,
    ) -> bool {
        while let Some(Ok(message)) = socket.next().await {
            match message {
                Message::Text(text) => {
                    let _ = replies.send(serde_json::from_str(&text).unwrap());
                }
                Message::Close(_) => return true,
                _ => {}
            }
        }
        false
    }

    #[tokio::test]
    async fn writes_each_event_as_a_json_line_then_acknowledges_it_in_both_modes() {
        for human in [false, true] {
            let (replies_tx, mut replies) = mpsc::unbounded_channel();
            let service = MockService::start(Upgrade::Accept, move |mut socket| {
                let replies_tx = replies_tx.clone();
                async move {
                    testing::send_ready(&mut socket).await;
                    for (id, event) in [
                        (json!(7), event("e1", "Hi")),
                        (json!("srv-2"), event("e2", "Héllo\nthere")),
                    ] {
                        testing::send(&mut socket, deliver_request(id, event)).await;
                        let _ = replies_tx.send(next_text(&mut socket).await);
                    }
                    forward_until_closed(&mut socket, &replies_tx).await;
                }
            })
            .await;
            let mut got = Vec::new();

            let run = wait(&service.url, human, fast(), async {
                got.push(replies.recv().await.unwrap());
                got.push(replies.recv().await.unwrap());
            })
            .await;

            assert_eq!(run.exit, Exit::Success);
            assert_eq!(
                got,
                [
                    json!({"jsonrpc": "2.0", "id": 7, "result": {"event_id": "e1"}}),
                    json!({"jsonrpc": "2.0", "id": "srv-2", "result": {"event_id": "e2"}}),
                ]
            );
            let expected = format!("{}\n{}\n", event("e1", "Hi"), event("e2", "Héllo\nthere"));
            assert_eq!(run.stdout, expected, "human: {human}");
            assert!(run.stdout.lines().all(|line| !line.contains(": ")));
            if human {
                assert!(!run.stderr.contains(r#""status""#), "{}", run.stderr);
            } else {
                assert_eq!(run.statuses(), ["connected", "stopped"]);
            }
        }
    }

    #[tokio::test]
    async fn answers_a_pending_write_with_not_received_and_acknowledges_after_the_write() {
        let (blocked, release, stdout) = Blocked::new();
        let (replies_tx, mut replies) = mpsc::unbounded_channel();
        let service = MockService::start(Upgrade::Accept, move |mut socket| {
            let replies_tx = replies_tx.clone();
            async move {
                testing::send_ready(&mut socket).await;
                testing::send(&mut socket, deliver_request(json!(1), event("e1", "One"))).await;
                testing::send(&mut socket, deliver_request(json!(2), event("e2", "Two"))).await;
                forward_until_closed(&mut socket, &replies_tx).await;
            }
        })
        .await;
        let mut got = Vec::new();

        let run = wait_with(
            &service.url,
            false,
            fast(),
            async {
                got.push(replies.recv().await.unwrap());
                assert!(
                    timeout(SOON, replies.recv()).await.is_err(),
                    "acknowledged before the write finished"
                );
                release.send(()).unwrap();
                got.push(replies.recv().await.unwrap());
            },
            Delivery::new(blocked),
        )
        .await;

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(got[0]["id"], 2);
        assert_eq!(got[0]["error"]["code"], -32001);
        assert_eq!(got[0]["error"]["data"], json!({"reason": "not_received"}));
        assert_eq!(
            got[1],
            json!({"jsonrpc": "2.0", "id": 1, "result": {"event_id": "e1"}})
        );
        assert_eq!(stdout.text(), format!("{}\n", event("e1", "One")));
        assert_eq!(
            run.lines()[1],
            json!({"status": "not_received", "event_id": "e2"})
        );
        assert_eq!(run.statuses(), ["connected", "not_received", "stopped"]);
    }

    #[tokio::test]
    async fn acknowledges_the_same_id_again_after_a_reconnect_without_writing_it() {
        let (replies_tx, mut replies) = mpsc::unbounded_channel();
        let attempts = Arc::new(AtomicUsize::new(0));
        let service = MockService::start(Upgrade::Accept, move |mut socket| {
            let (replies_tx, attempts) = (replies_tx.clone(), Arc::clone(&attempts));
            async move {
                testing::send_ready(&mut socket).await;
                let request = deliver_request(json!(1), event("e1", "One"));
                testing::send(&mut socket, request).await;
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    // Gone before the acknowledgement can be read.
                    testing::send_closing(&mut socket, "shutdown").await;
                } else {
                    forward_until_closed(&mut socket, &replies_tx).await;
                }
            }
        })
        .await;

        let run = wait(&service.url, false, fast(), async {
            let ack = replies.recv().await.unwrap();
            assert_eq!(ack["result"], json!({"event_id": "e1"}));
        })
        .await;

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(run.stdout, format!("{}\n", event("e1", "One")));
        assert!(run.statuses().contains(&"reconnecting".to_owned()));
    }

    #[tokio::test]
    async fn shutdown_does_not_wait_for_a_pending_write_and_leaves_the_event_unacknowledged() {
        let (blocked, release, stdout) = Blocked::new();
        let (replies_tx, mut replies) = mpsc::unbounded_channel();
        let (closed_tx, mut closed) = mpsc::unbounded_channel();
        let service = MockService::start(Upgrade::Accept, move |mut socket| {
            let (replies_tx, closed_tx) = (replies_tx.clone(), closed_tx.clone());
            async move {
                testing::send_ready(&mut socket).await;
                testing::send(&mut socket, deliver_request(json!(1), event("e1", "One"))).await;
                testing::send(&mut socket, deliver_request(json!(2), event("e2", "Two"))).await;
                let _ = closed_tx.send(forward_until_closed(&mut socket, &replies_tx).await);
            }
        })
        .await;

        let run = timeout(
            Duration::from_secs(5),
            wait_with(
                &service.url,
                false,
                fast(),
                async {
                    // Its reply shows the first event is being written.
                    assert_eq!(replies.recv().await.unwrap()["id"], 2);
                },
                Delivery::new(blocked),
            ),
        )
        .await
        .expect("al wait must not wait for the pending write");

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(run.statuses(), ["connected", "not_received", "stopped"]);
        assert_eq!(closed.recv().await, Some(true));
        assert!(replies.try_recv().is_err(), "the event was acknowledged");
        assert_eq!(stdout.text(), "");
        drop(release);
    }

    #[tokio::test]
    async fn a_failing_write_reports_closes_the_connection_and_exits_1() {
        let (replies_tx, mut replies) = mpsc::unbounded_channel();
        let (closed_tx, mut closed) = mpsc::unbounded_channel();
        let service = MockService::start(Upgrade::Accept, move |mut socket| {
            let (replies_tx, closed_tx) = (replies_tx.clone(), closed_tx.clone());
            async move {
                testing::send_ready(&mut socket).await;
                testing::send(&mut socket, deliver_request(json!(1), event("e1", "One"))).await;
                let _ = closed_tx.send(forward_until_closed(&mut socket, &replies_tx).await);
            }
        })
        .await;

        let run = wait_with(
            &service.url,
            false,
            fast(),
            std::future::pending(),
            Delivery::new(BrokenPipe),
        )
        .await;

        assert_eq!(run.exit, Exit::Unexpected);
        assert_eq!(run.statuses(), ["connected", "stopped"]);
        let error = &run.lines()[1]["error"];
        assert_eq!(error["code"], "unexpected");
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("standard output")
        );
        assert_eq!(closed.recv().await, Some(true));
        assert!(replies.try_recv().is_err(), "the event was acknowledged");
    }

    #[tokio::test]
    async fn a_write_failing_while_disconnected_exits_1_without_waiting_to_reconnect() {
        let (blocked, release, _) = Blocked::new();
        let service = MockService::start(Upgrade::Accept, |mut socket| async move {
            testing::send_ready(&mut socket).await;
            testing::send(&mut socket, deliver_request(json!(1), event("e1", "One"))).await;
            testing::send_closing(&mut socket, "shutdown").await;
        })
        .await;
        let timing = Timing {
            backoff_base: Duration::from_secs(60),
            backoff_cap: Duration::from_secs(60),
            ..fast()
        };

        let run = wait_with(
            &service.url,
            false,
            timing,
            async {
                sleep(SOON).await;
                drop(release);
                sleep(Duration::from_secs(2)).await;
            },
            Delivery::new(blocked),
        )
        .await;

        assert_eq!(run.exit, Exit::Unexpected);
        assert_eq!(run.lines().last().unwrap()["error"]["code"], "unexpected");
    }

    #[tokio::test]
    async fn an_event_without_an_id_is_answered_with_invalid_params() {
        let (replies_tx, mut replies) = mpsc::unbounded_channel();
        let service = MockService::start(Upgrade::Accept, move |mut socket| {
            let replies_tx = replies_tx.clone();
            async move {
                testing::send_ready(&mut socket).await;
                let request = deliver_request(json!(4), json!({"type": "text_message"}));
                testing::send(&mut socket, request).await;
                forward_until_closed(&mut socket, &replies_tx).await;
            }
        })
        .await;

        let run = wait(&service.url, false, fast(), async {
            let reply = replies.recv().await.unwrap();
            assert_eq!(reply["id"], 4);
            assert_eq!(reply["error"]["code"], -32602);
        })
        .await;

        assert_eq!(run.exit, Exit::Success);
        assert_eq!(run.stdout, "");
    }
}
