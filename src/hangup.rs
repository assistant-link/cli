use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::output::{Exit, Failure, Output};
use crate::{send, server};

#[derive(Deserialize, Serialize)]
struct HungUp {
    event_id: String,
    created_at: String,
}

pub async fn run(server: &str, key: Result<String, Failure>, out: &mut Output<'_>) -> Exit {
    let hung_up: Result<HungUp, Failure> = async {
        let key = key?;
        let body = json!({"idempotency_key": send::uuid_v7()});
        server::call(
            server,
            &key,
            Method::POST,
            "/alp/v1/hangup",
            Some(&body),
            true,
        )
        .await
    }
    .await;
    match hung_up {
        Ok(hung_up) => {
            out.report(
                &json!(hung_up),
                &format!(
                    "Ended the call at {} (event {}).",
                    hung_up.created_at, hung_up.event_id
                ),
            );
            Exit::Success
        }
        Err(failure) => out.fail(&failure),
    }
}
