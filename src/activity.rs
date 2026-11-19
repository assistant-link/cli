use reqwest::Method;
use serde::Deserialize;
use serde_json::json;

use crate::output::{Exit, Failure, Output};
use crate::server;

const PATH: &str = "/alp/v1/link/activity";

#[derive(Deserialize)]
struct Reply {
    activity: Option<String>,
}

/// `text` is `None` for `--clear`.
pub async fn run(
    server: &str,
    key: Result<String, Failure>,
    text: Option<String>,
    out: &mut Output<'_>,
) -> Exit {
    let activity = async {
        let key = key?;
        let Some(text) = text else {
            server::call::<()>(server, &key, Method::DELETE, PATH, None, false).await?;
            return Ok(None);
        };
        let body = json!({"text": text});
        let reply: Reply = server::call(server, &key, Method::PUT, PATH, Some(&body), false)
            .await
            .map_err(explain_not_connected)?;
        Ok(reply.activity)
    }
    .await;
    match activity {
        Ok(activity) => {
            let text = activity.as_ref().map_or_else(
                || "No activity is shown.".to_owned(),
                |activity| format!("Activity: {activity}"),
            );
            out.report(&json!({"activity": activity}), &text);
            Exit::Success
        }
        Err(failure) => out.fail(&failure),
    }
}

fn explain_not_connected(failure: Failure) -> Failure {
    if failure.code != "not_connected" {
        return failure;
    }
    Failure {
        message: "The activity shows only while `al wait` or `al call` is connected, so the service did not set it. Set it while one of them runs.".to_owned(),
        ..failure
    }
}
