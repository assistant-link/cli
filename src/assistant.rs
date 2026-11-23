use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::output::{Exit, Failure, Output};
use crate::server;

const PATH: &str = "/alp/v1/assistant";

#[derive(Debug, PartialEq, Eq, clap::Subcommand)]
pub enum Action {
    /// Say which platform you run on, or rename the Assistant
    #[command(long_about = crate::help::ASSISTANT_SET)]
    #[command(group = clap::ArgGroup::new("change").required(true).multiple(true))]
    Set {
        /// New name of the Assistant, shown to the user
        #[arg(long, group = "change", display_order = 0)]
        name: Option<String>,
        /// Platform you run on, e.g. openclaw; "" clears it
        #[arg(long, group = "change", display_order = 0)]
        platform: Option<String>,
    },
    /// Show the Assistant's name and platform
    #[command(long_about = crate::help::ASSISTANT_SHOW)]
    Show,
}

#[derive(Serialize)]
struct Changes {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    platform: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct Assistant {
    name: String,
    platform: Option<String>,
}

pub async fn run(
    server: &str,
    key: Result<String, Failure>,
    action: Action,
    out: &mut Output<'_>,
) -> Exit {
    let assistant = async {
        let key = key?;
        match action {
            Action::Set { name, platform } => {
                let body = json!(Changes { name, platform });
                server::call(server, &key, Method::PATCH, PATH, Some(&body), false).await
            }
            Action::Show => server::call(server, &key, Method::GET, PATH, None, false).await,
        }
    }
    .await;
    match assistant {
        Ok(assistant) => {
            out.report(&json!(assistant), &describe(&assistant));
            Exit::Success
        }
        Err(failure) => out.fail(&failure),
    }
}

fn describe(Assistant { name, platform }: &Assistant) -> String {
    match platform {
        Some(platform) => format!("Assistant: {name} (platform: {platform})"),
        None => format!("Assistant: {name} (no platform set)"),
    }
}
