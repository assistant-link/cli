use std::io::Write;
use std::process::ExitCode;

use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Success,
    Unexpected,
    NotPaired,
    Rejected,
    Unreachable,
    ServerMismatch,
}

impl From<Exit> for ExitCode {
    fn from(exit: Exit) -> Self {
        Self::from(match exit {
            Exit::Success => 0,
            Exit::Unexpected => 1,
            Exit::NotPaired => 3,
            Exit::Rejected => 4,
            Exit::Unreachable => 5,
            Exit::ServerMismatch => 6,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Failure {
    #[serde(skip)]
    pub exit: Exit,
    pub code: String,
    pub message: String,
}

impl Failure {
    pub fn new(exit: Exit, code: &str, message: impl Into<String>) -> Self {
        Self {
            exit,
            code: code.to_owned(),
            message: message.into(),
        }
    }
}

pub fn describe(error: &dyn std::error::Error) -> String {
    let mut description = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        description.push_str(": ");
        description.push_str(&cause.to_string());
        source = cause.source();
    }
    description
}

pub struct Output<'a> {
    pub human: bool,
    pub stdout: &'a mut dyn Write,
    pub stderr: &'a mut dyn Write,
}

impl Output<'_> {
    pub fn report(&mut self, json: &Value, text: &str) {
        let line = if self.human {
            text.to_owned()
        } else {
            json.to_string()
        };
        let _ = writeln!(self.stdout, "{line}");
    }

    pub fn fail(&mut self, failure: &Failure) -> Exit {
        if self.human {
            let _ = writeln!(self.stderr, "al: {}", failure.message);
        } else {
            let _ = writeln!(self.stdout, "{}", json!({ "error": failure }));
        }
        failure.exit
    }

    pub fn log(&mut self, json: &Value, text: &str) {
        let _ = if self.human {
            writeln!(self.stderr, "al: {text}")
        } else {
            writeln!(self.stderr, "{json}")
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(human: bool, run: impl FnOnce(&mut Output)) -> (String, String) {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        run(&mut Output {
            human,
            stdout: &mut stdout,
            stderr: &mut stderr,
        });
        (
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    }

    #[test]
    fn exit_codes_match_the_spec() {
        for (exit, code) in [
            (Exit::Success, 0),
            (Exit::Unexpected, 1),
            (Exit::NotPaired, 3),
            (Exit::Rejected, 4),
            (Exit::Unreachable, 5),
            (Exit::ServerMismatch, 6),
        ] {
            assert_eq!(ExitCode::from(exit), ExitCode::from(code), "{exit:?}");
        }
    }

    #[test]
    fn report_prints_one_json_line_on_stdout_by_default() {
        let (stdout, stderr) = output(false, |out| out.report(&json!({"a": 1}), "text"));
        assert_eq!(stdout, "{\"a\":1}\n");
        assert_eq!(stderr, "");
    }

    #[test]
    fn report_prints_text_on_stdout_when_human() {
        let (stdout, stderr) = output(true, |out| out.report(&json!({"a": 1}), "text"));
        assert_eq!(stdout, "text\n");
        assert_eq!(stderr, "");
    }

    #[test]
    fn fail_prints_the_error_object_on_stdout_by_default() {
        let failure = Failure::new(Exit::Rejected, "invalid_code", "Bad code.");
        let (stdout, _) = output(false, |out| assert_eq!(out.fail(&failure), Exit::Rejected));
        let printed: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(
            printed,
            json!({"error": {"code": "invalid_code", "message": "Bad code."}})
        );
    }

    #[test]
    fn fail_prints_the_message_on_stderr_when_human() {
        let failure = Failure::new(Exit::Rejected, "invalid_code", "Bad code.");
        let (stdout, stderr) = output(true, |out| {
            out.fail(&failure);
        });
        assert_eq!(stdout, "");
        assert_eq!(stderr, "al: Bad code.\n");
    }

    #[test]
    fn log_prints_json_lines_on_stderr_by_default() {
        let (stdout, stderr) = output(false, |out| {
            out.log(&json!({"status": "connected"}), "connected");
            out.log(&json!({"status": "stopped"}), "stopped");
        });
        assert_eq!(stdout, "");
        assert_eq!(
            stderr,
            "{\"status\":\"connected\"}\n{\"status\":\"stopped\"}\n"
        );
    }

    #[test]
    fn log_prints_text_lines_on_stderr_when_human() {
        let (stdout, stderr) = output(true, |out| out.log(&json!({}), "connected"));
        assert_eq!(stdout, "");
        assert_eq!(stderr, "al: connected\n");
    }
}
