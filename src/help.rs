pub const WAIT: &str = r#"Stay online and print the user's messages, until interrupted.

Each event is one compact JSON line on standard output, in order, in JSON and --human mode alike:

  {"id":"...","type":"text_message","created_at":"...","deleted":false,"text_message":{"text":"Hi"}}

Messages that arrived while no `al wait` was running come first after it connects: `al` keeps no position. Delivery is at least once: an event can arrive more than once. `al wait` writes an event only once per run when the repeat comes right after it (it remembers just the `id` of the last event it wrote, and nothing across runs) and acknowledges the repeat. Dedupe by `id` if you must not see a message twice. Ignore fields and types you do not know.

An event is acknowledged only after its line was written and flushed. Keep reading standard output: while it is not read, the event stays unacknowledged and the service sends it again later.

Status messages go to standard error, one JSON object per line (text with --human). `al wait` reconnects after a lost connection, waiting up to 30 seconds between failures. On SIGINT or SIGTERM it closes and exits 0. It exits 1 if standard output is closed, 3 without a Link key, and 4 if the service rejects the key or the account was deleted."#;

pub const SEND: &str = r#"Send a message to the user, with one HTTP request. It works whether or not the Link is online and does not change it.

The text is the argument, or - to read standard input as UTF-8, dropping one trailing line feed. `al` does not check the text: the service accepts 1 to 4,000 characters (Unicode code points) that are not only white space and hold no NUL, and otherwise rejects it (exit 4, for example `text_too_long`).

Each run sends a fresh `idempotency_key` (a UUIDv7) and makes exactly one attempt: `al send` never retries. After exit 5 the message may still have been stored, so sending it again can duplicate it.

Prints {"message_id","created_at"} and exits 0. On failure it prints {"error":{"code","message"}} and exits 2 without a text, 3 without a Link key, 4 if the service rejects the message or the key, or 5 if the service is unreachable."#;

pub const STATUS: &str = r#"Check once whether the service accepts the Link key. It only reads the Link, so the Link does not go online.

Prints one JSON object (text with --human):

  server          the service URL
  link_id         the Link (only when check is accepted)
  assistant_name  the Assistant (only when check is accepted)
  unread_count    user messages the Link has not acknowledged yet (only when check is accepted)
  check           accepted, rejected, unreachable, no_key, or unexpected
  error           {"code","message"}, for every check except accepted

check, with the exit code:

  accepted     the service accepts the key (0)
  rejected     the service does not accept the key, for example after the account was deleted (4)
  unreachable  the service cannot be reached, or answers with 5xx (5)
  no_key       no Link key was supplied (3)
  unexpected   the answer is not from an Assistant Link service, for example 404 (1)"#;

const SAFER: &str = "If your environment has a safer way to guard credentials, use it instead.";

/// `os` is a name from `std::env::consts::OS`.
pub fn footer(os: &str) -> String {
    format!(
        "\
Flow:
  al wait   stay online and print the user's messages; keep it running
  al send   reply to the user

{}

Run `al <command> --help` for details.",
        link_key(os)
    )
}

fn link_key(os: &str) -> String {
    let how = match os {
        "macos" => {
            "\
Keep it in the Keychain (NAME: the Assistant's name in lowercase), then pass it to `al` in AL_LINK_KEY:
  mkdir -p -m 700 ~/.config/assistant-link
  al pair <code> --key-file ~/.config/assistant-link/NAME.key
  printf 'add-generic-password -a NAME -s assistant-link -w %s\\n' \"$(cat ~/.config/assistant-link/NAME.key)\" | security -i
  rm ~/.config/assistant-link/NAME.key
  AL_LINK_KEY=\"$(security find-generic-password -a NAME -s assistant-link -w)\" al wait"
        }
        "linux" => {
            "\
Keep it in the Secret Service (needs a running keyring; NAME: the Assistant's name in lowercase), then pass it to `al` in AL_LINK_KEY:
  mkdir -p -m 700 ~/.config/assistant-link
  al pair <code> --key-file ~/.config/assistant-link/NAME.key
  secret-tool store --label=\"Assistant Link: NAME\" service assistant-link account NAME < ~/.config/assistant-link/NAME.key
  rm ~/.config/assistant-link/NAME.key
  AL_LINK_KEY=\"$(secret-tool lookup service assistant-link account NAME)\" al wait
Without a Secret Service, keep the key file, which only your user can read; `al pair` creates it that way:
  al wait --key-file ~/.config/assistant-link/NAME.key"
        }
        _ => {
            "\
Keep it in a file only your user can read; `al pair` creates it that way:
  mkdir -p -m 700 ~/.config/assistant-link
  al pair <code> --key-file ~/.config/assistant-link/NAME.key
  al wait --key-file ~/.config/assistant-link/NAME.key"
        }
    };
    format!(
        "Link key: `al pair` gives you the key and `al` keeps none, so supply it on every run.\n{how}\n{SAFER}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_os_ends_the_key_section_with_the_safer_way_sentence() {
        for os in ["macos", "linux", "windows", "freebsd", ""] {
            assert!(link_key(os).ends_with(SAFER), "{os}");
            assert!(footer(os).contains(&format!("{SAFER}\n")), "{os}");
        }
    }

    #[test]
    fn macos_shows_only_the_keychain() {
        let section = link_key("macos");
        assert!(section.contains("add-generic-password"));
        assert!(section.contains("security -i"));
        assert!(section.contains("-a NAME -s assistant-link"));
        assert!(section.contains("security find-generic-password -a NAME -s assistant-link -w"));
        assert!(!section.contains("secret-tool"));
    }

    #[test]
    fn linux_shows_the_secret_service_and_a_user_only_file() {
        let section = link_key("linux");
        assert!(section.contains("secret-tool store"));
        assert!(section.contains("service assistant-link account NAME"));
        assert!(section.contains("secret-tool lookup service assistant-link account NAME"));
        assert!(section.contains("mkdir -p -m 700 ~/.config/assistant-link"));
        assert!(section.contains("--key-file ~/.config/assistant-link/NAME.key"));
        assert!(!section.contains("security "));
    }

    #[test]
    fn other_systems_show_only_the_file() {
        for os in ["windows", "freebsd", "android", ""] {
            let section = link_key(os);
            assert!(
                section.contains("mkdir -p -m 700 ~/.config/assistant-link"),
                "{os}"
            );
            assert!(
                section.contains("--key-file ~/.config/assistant-link/NAME.key"),
                "{os}"
            );
            assert!(!section.contains("security "), "{os}");
            assert!(!section.contains("secret-tool"), "{os}");
        }
    }

    #[test]
    fn the_footer_has_the_flow_the_key_section_and_the_pointer() {
        let footer = footer("linux");
        assert!(footer.contains("al wait"));
        assert!(footer.contains("al send"));
        assert!(footer.contains("Link key:"));
        assert!(footer.ends_with("Run `al <command> --help` for details."));
    }
}
