use std::future;
use std::io::{self, Write};
use std::sync::mpsc as std_mpsc;
use std::thread;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::jsonrpc::{Request, Response};
use crate::output::Output;

struct Pending {
    event_id: String,
    request_id: Value,
}

/// Writes events to standard output on a thread of its own, so a reader that stops reading blocks
/// that thread and nothing else. The thread is never joined or cancelled: a write that cannot
/// finish ends with the process.
pub struct Delivery {
    lines: std_mpsc::Sender<String>,
    written: mpsc::UnboundedReceiver<io::Result<()>>,
    pending: Option<Pending>,
    last: Option<String>,
}

impl Delivery {
    pub fn new(mut stdout: impl Write + Send + 'static) -> Self {
        let (lines, queue) = std_mpsc::channel::<String>();
        let (done, written) = mpsc::unbounded_channel();
        thread::spawn(move || {
            for line in queue {
                let result = stdout
                    .write_all(line.as_bytes())
                    .and_then(|()| stdout.flush());
                let failed = result.is_err();
                if done.send(result).is_err() || failed {
                    break;
                }
            }
        });
        Self {
            lines,
            written,
            pending: None,
            last: None,
        }
    }

    /// Starts writing the event, or answers at once when it cannot be written now. The
    /// acknowledgement of a started write comes from `written`.
    pub fn deliver(&mut self, request: Request, out: &mut Output) -> Option<Response> {
        let Some(event) = request.params.filter(Value::is_object) else {
            return Some(Response::invalid_params(request.id));
        };
        let Some(event_id) = event["id"].as_str().map(str::to_owned) else {
            return Some(Response::invalid_params(request.id));
        };
        if self.pending.is_some() {
            out.log(
                &json!({"status": "not_received", "event_id": event_id}),
                &format!("Not received: still writing an earlier event; event {event_id} will come again."),
            );
            return Some(Response::not_received(request.id));
        }
        if self.last.as_deref() == Some(&event_id) {
            return Some(Response::event_acknowledged(request.id, &event_id));
        }
        let _ = self.lines.send(format!("{event}\n"));
        self.pending = Some(Pending {
            event_id,
            request_id: request.id,
        });
        None
    }

    /// Resolves when the pending write has finished, with the acknowledgement to send for it.
    pub async fn written(&mut self) -> io::Result<Response> {
        if self.pending.is_none() {
            return future::pending().await;
        }
        self.written
            .recv()
            .await
            .unwrap_or_else(|| Err(io::Error::other("the writer stopped")))?;
        match self.pending.take() {
            Some(Pending {
                event_id,
                request_id,
            }) => {
                let response = Response::event_acknowledged(request_id, &event_id);
                self.last = Some(event_id);
                Ok(response)
            }
            None => future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;
    use crate::testing::{Blocked, SharedStdout};

    const SOON: Duration = Duration::from_millis(200);

    fn request(id: Value, event_id: &str) -> Request {
        Request {
            id,
            method: "event.deliver".to_owned(),
            params: Some(json!({
                "id": event_id,
                "type": "text_message",
                "created_at": "2026-10-22T09:30:00Z",
                "deleted": false,
                "text_message": {"text": "Hi"},
            })),
        }
    }

    fn deliver(
        delivery: &mut Delivery,
        request: Request,
        human: bool,
    ) -> (Option<Response>, String) {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let response = delivery.deliver(
            request,
            &mut Output {
                human,
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
        );
        assert!(stdout.is_empty());
        (response, String::from_utf8(stderr).unwrap())
    }

    #[tokio::test]
    async fn writes_one_compact_line_then_acknowledges_the_request_id() {
        for id in [json!(7), json!("s-1"), json!(null)] {
            let stdout = SharedStdout::default();
            let mut delivery = Delivery::new(stdout.clone());

            let (response, _) = deliver(&mut delivery, request(id.clone(), "e1"), false);
            assert_eq!(response, None);
            let ack = delivery.written().await.unwrap();

            assert_eq!(ack, Response::event_acknowledged(id, "e1"));
            let text = stdout.text();
            assert_eq!(text.matches('\n').count(), 1);
            assert!(text.ends_with('\n'));
            assert_eq!(
                serde_json::from_str::<Value>(&text).unwrap(),
                request(json!(0), "e1").params.unwrap()
            );
            assert!(!text.contains(": "));
        }
    }

    #[tokio::test]
    async fn the_line_keeps_fields_this_version_does_not_know() {
        let stdout = SharedStdout::default();
        let mut delivery = Delivery::new(stdout.clone());
        let mut event = request(json!(1), "e1");
        event.params.as_mut().unwrap()["future"] = json!({"x": [1, 2]});

        deliver(&mut delivery, event, false);
        delivery.written().await.unwrap();

        assert_eq!(
            serde_json::from_str::<Value>(&stdout.text()).unwrap()["future"],
            json!({"x": [1, 2]})
        );
    }

    #[tokio::test]
    async fn does_not_acknowledge_before_the_write_finishes() {
        let (blocked, release, stdout) = Blocked::new();
        let mut delivery = Delivery::new(blocked);
        deliver(&mut delivery, request(json!(1), "e1"), false);

        assert!(timeout(SOON, delivery.written()).await.is_err());
        release.send(()).unwrap();

        let ack = timeout(Duration::from_secs(5), delivery.written())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ack, Response::event_acknowledged(json!(1), "e1"));
        assert_eq!(stdout.text().matches('\n').count(), 1);
    }

    #[tokio::test]
    async fn answers_not_received_while_a_write_is_pending_and_reports_it() {
        let (blocked, release, stdout) = Blocked::new();
        let mut delivery = Delivery::new(blocked);
        deliver(&mut delivery, request(json!(1), "e1"), false);

        for (event_id, human) in [("e2", false), ("e1", false), ("e2", true)] {
            let (response, stderr) = deliver(&mut delivery, request(json!(2), event_id), human);
            assert_eq!(
                response,
                Some(Response::not_received(json!(2))),
                "{event_id}"
            );
            assert!(stderr.contains("not_received") || human, "{stderr}");
            assert!(stderr.contains(event_id), "{stderr}");
        }
        release.send(()).unwrap();
        delivery.written().await.unwrap();

        assert_eq!(stdout.text().matches('\n').count(), 1);
    }

    #[tokio::test]
    async fn acknowledges_a_repeat_of_the_last_written_id_without_writing() {
        let stdout = SharedStdout::default();
        let mut delivery = Delivery::new(stdout.clone());
        deliver(&mut delivery, request(json!(1), "e1"), false);
        delivery.written().await.unwrap();

        let (response, stderr) = deliver(&mut delivery, request(json!(2), "e1"), false);

        assert_eq!(response, Some(Response::event_acknowledged(json!(2), "e1")));
        assert_eq!(stderr, "");
        assert_eq!(stdout.text().matches('\n').count(), 1);
    }

    #[tokio::test]
    async fn only_the_last_id_counts_as_a_repeat() {
        let stdout = SharedStdout::default();
        let mut delivery = Delivery::new(stdout.clone());
        for (id, event_id) in [(1, "e1"), (2, "e2"), (3, "e1")] {
            let (response, _) = deliver(&mut delivery, request(json!(id), event_id), false);
            assert_eq!(response, None, "{event_id}");
            delivery.written().await.unwrap();
        }

        assert_eq!(stdout.text().matches('\n').count(), 3);
    }

    #[tokio::test]
    async fn a_failed_write_is_an_error_and_never_an_acknowledgement() {
        let mut delivery = Delivery::new(crate::testing::BrokenPipe);
        deliver(&mut delivery, request(json!(1), "e1"), false);

        let error = delivery.written().await.unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[tokio::test]
    async fn an_event_without_an_id_is_invalid_params() {
        let mut delivery = Delivery::new(SharedStdout::default());
        for params in [
            None,
            Some(json!([])),
            Some(json!({"id": 5})),
            Some(json!({})),
        ] {
            let request = Request {
                id: json!(3),
                method: "event.deliver".to_owned(),
                params,
            };
            let (response, _) = deliver(&mut delivery, request, false);
            assert_eq!(response, Some(Response::invalid_params(json!(3))));
        }
    }

    #[tokio::test]
    async fn written_waits_forever_without_a_pending_write() {
        let mut delivery = Delivery::new(SharedStdout::default());

        assert!(timeout(SOON, delivery.written()).await.is_err());
    }
}
