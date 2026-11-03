use std::fmt;

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::output::{Exit, Failure, Output};
use crate::server;

const PATH: &str = "/alp/v1/link/webhook";

// Kept apart from clap's own validation, which would echo a rejected value in its error.
#[derive(Clone, PartialEq, Eq)]
pub struct Header(pub String);

impl fmt::Debug for Header {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("Header(..)")
    }
}

fn authorization(header: &Header) -> Result<String, Failure> {
    let invalid = |message| Failure::new(Exit::Usage, "invalid_header", message);
    let (name, value) = header
        .0
        .split_once(':')
        .ok_or_else(|| invalid("Expected the header as \"Authorization: <value>\"."))?;
    if !name.trim().eq_ignore_ascii_case("authorization") {
        return Err(invalid("Only the Authorization header is accepted."));
    }
    match value.trim() {
        "" => Err(invalid("The Authorization header has no value.")),
        value => Ok(value.to_owned()),
    }
}

#[derive(Debug, PartialEq, Eq, clap::Subcommand)]
pub enum Action {
    /// Have the service ping a URL to wake you when the user writes
    #[command(long_about = crate::help::WEBHOOK_SET)]
    Set {
        /// URL the service POSTs to
        url: String,
        /// Authorization header the service sends to the URL, as "Authorization: <value>"
        #[arg(
            long,
            env = "AL_WEBHOOK_HEADER",
            hide_env_values = true,
            value_parser = |header: &str| Ok::<_, String>(Header(header.into())),
        )]
        header: Option<Header>,
    },
    /// Show the webhook, with the Authorization value masked
    #[command(long_about = crate::help::WEBHOOK_SHOW)]
    Show,
    /// Remove the webhook
    #[command(long_about = crate::help::WEBHOOK_CLEAR)]
    Clear,
}

#[derive(Deserialize, Serialize)]
struct Hook {
    url: String,
    authorization: Option<String>,
}

#[derive(Deserialize)]
struct Reply {
    webhook: Option<Hook>,
}

pub async fn run(
    server: &str,
    key: Result<String, Failure>,
    action: Action,
    out: &mut Output<'_>,
) -> Exit {
    let webhook = async {
        let key = key?;
        let reply: Reply = match action {
            Action::Set { url, header } => {
                let header = header.filter(|header| !header.0.is_empty());
                let authorization = header.as_ref().map(authorization).transpose()?;
                let body = json!({"url": url, "authorization": authorization});
                server::call(server, &key, Method::PUT, PATH, Some(&body), false).await?
            }
            Action::Show => server::call(server, &key, Method::GET, PATH, None, false).await?,
            Action::Clear => {
                server::call::<()>(server, &key, Method::DELETE, PATH, None, false).await?;
                Reply { webhook: None }
            }
        };
        Ok(reply.webhook)
    }
    .await;
    match webhook {
        Ok(webhook) => {
            out.report(&json!({"webhook": webhook}), &describe(webhook.as_ref()));
            Exit::Success
        }
        Err(failure) => out.fail(&failure),
    }
}

fn describe(webhook: Option<&Hook>) -> String {
    match webhook {
        None => "No webhook is set.".to_owned(),
        Some(Hook {
            url,
            authorization: None,
        }) => format!("Webhook: {url}"),
        Some(Hook {
            url,
            authorization: Some(value),
        }) => {
            format!("Webhook: {url} (Authorization: {value})")
        }
    }
}
