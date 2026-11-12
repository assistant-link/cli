use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::output::{Exit, Failure, Output};
use crate::server;

const PATH: &str = "/alp/v1/link/wake_email";

#[derive(Debug, PartialEq, Eq, clap::Subcommand)]
pub enum Action {
    /// Have the service email an address to wake you when the user writes
    #[command(long_about = crate::help::WAKE_EMAIL_SET)]
    Set {
        /// Address the service sends the wake email to
        address: String,
    },
    /// Show the wake email and whether it is confirmed
    #[command(long_about = crate::help::WAKE_EMAIL_SHOW)]
    Show,
    /// Remove the wake email
    #[command(long_about = crate::help::WAKE_EMAIL_CLEAR)]
    Clear,
}

#[derive(Deserialize, Serialize)]
struct WakeEmail {
    address: String,
    confirmed: bool,
}

#[derive(Deserialize)]
struct Reply {
    wake_email: Option<WakeEmail>,
}

pub async fn run(
    server: &str,
    key: Result<String, Failure>,
    action: Action,
    out: &mut Output<'_>,
) -> Exit {
    let set = matches!(action, Action::Set { .. });
    let wake_email = async {
        if let Action::Set { address } = &action
            && address.trim().is_empty()
        {
            return Err(Failure::new(
                Exit::Usage,
                "invalid_address",
                "The wake email address is empty.",
            ));
        }
        let key = key?;
        let reply: Reply = match action {
            Action::Set { address } => {
                let body = json!({"address": address});
                server::call(server, &key, Method::PUT, PATH, Some(&body), false).await?
            }
            Action::Show => server::call(server, &key, Method::GET, PATH, None, false).await?,
            Action::Clear => {
                server::call::<()>(server, &key, Method::DELETE, PATH, None, false).await?;
                Reply { wake_email: None }
            }
        };
        Ok(reply.wake_email)
    }
    .await;
    match wake_email {
        Ok(wake_email) => {
            out.report(
                &json!({"wake_email": wake_email}),
                &describe(wake_email.as_ref(), set),
            );
            Exit::Success
        }
        Err(failure) => out.fail(&failure),
    }
}

fn describe(wake_email: Option<&WakeEmail>, set: bool) -> String {
    match wake_email {
        None => "No wake email is set.".to_owned(),
        Some(WakeEmail {
            address,
            confirmed: true,
        }) if set => format!(
            "Wake email: {address} (confirmed). The address was already confirmed, so it stays confirmed and the service sent nothing."
        ),
        Some(WakeEmail {
            address,
            confirmed: true,
        }) => format!("Wake email: {address} (confirmed)"),
        Some(WakeEmail {
            address,
            confirmed: false,
        }) if set => format!(
            "Wake email: {address} (not yet confirmed). A confirmation email was sent to it. No wake email is sent until the link in that email is opened and its Confirm button is pressed; opening the link alone, as a mail scanner does, confirms nothing."
        ),
        Some(WakeEmail {
            address,
            confirmed: false,
        }) => format!(
            "Wake email: {address} (not yet confirmed: no wake email is sent until the link in the confirmation email is confirmed)"
        ),
    }
}
