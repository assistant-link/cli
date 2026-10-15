use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::output::{Exit, Failure};

pub const ENV: &str = "AL_LINK_KEY";

const HOW_TO_SUPPLY: &str = "Set AL_LINK_KEY to the Link key or pass --key-file <path>. To get a key, run `al pair <code>` with the pairing code from the app.";

pub fn load(key_file: Option<&Path>, env: Option<OsString>) -> Result<String, Failure> {
    let (key, source) = match (key_file, env) {
        (Some(path), _) => {
            let source = path.display().to_string();
            match fs::read_to_string(path) {
                Ok(contents) => (contents, source),
                Err(error) => {
                    return Err(no_key(&format!(
                        "Cannot read the Link key from {source}: {error}."
                    )));
                }
            }
        }
        (None, Some(value)) => match value.into_string() {
            Ok(value) => (value, ENV.to_owned()),
            Err(_) => return Err(no_key(&format!("{ENV} is not valid UTF-8."))),
        },
        (None, None) => return Err(no_key("No Link key supplied.")),
    };
    let key = key.trim();
    if key.is_empty() {
        return Err(no_key(&format!("The Link key in {source} is empty.")));
    }
    Ok(key.to_owned())
}

fn no_key(problem: &str) -> Failure {
    Failure::new(Exit::NoKey, "no_key", format!("{problem} {HOW_TO_SUPPLY}"))
}

// Created before pairing, so an existing or unwritable path fails without using up the code.
pub fn create(path: &Path) -> Result<fs::File, Failure> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                Failure::new(
                    Exit::Usage,
                    "key_file_exists",
                    format!(
                        "{} already exists. `al pair` never overwrites a key file: choose a new path.",
                        path.display()
                    ),
                )
            } else {
                Failure::new(
                    Exit::Unexpected,
                    "unexpected",
                    format!("Cannot create the key file {}: {error}", path.display()),
                )
            }
        })
}

pub fn write(mut file: fs::File, key: &str) -> io::Result<()> {
    file.write_all(key.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn file_with(contents: &str) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), contents).unwrap();
        file
    }

    fn no_key_message(result: Result<String, Failure>) -> String {
        let failure = result.unwrap_err();
        assert_eq!(failure.exit, Exit::NoKey);
        assert_eq!(failure.code, "no_key");
        failure.message
    }

    #[test]
    fn reads_the_key_file_trimming_whitespace() {
        let file = file_with("  alt_1\n");
        assert_eq!(load(Some(file.path()), None).unwrap(), "alt_1");
    }

    #[test]
    fn reads_al_link_key_trimming_whitespace() {
        assert_eq!(load(None, Some(" alt_1\n".into())).unwrap(), "alt_1");
    }

    #[test]
    fn the_key_file_takes_precedence_over_al_link_key() {
        let file = file_with("alt_file");
        assert_eq!(
            load(Some(file.path()), Some("alt_env".into())).unwrap(),
            "alt_file"
        );
    }

    #[test]
    fn no_key_names_both_sources_and_al_pair() {
        let message = no_key_message(load(None, None));
        for expected in ["AL_LINK_KEY", "--key-file", "al pair <code>"] {
            assert!(message.contains(expected), "{expected}: {message}");
        }
    }

    #[test]
    fn empty_keys_are_no_key() {
        let file = file_with(" \n");
        assert!(no_key_message(load(Some(file.path()), None)).contains("is empty"));
        assert!(no_key_message(load(None, Some("".into()))).contains("AL_LINK_KEY is empty"));
    }

    #[test]
    fn a_missing_key_file_is_no_key_naming_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing");
        let message = no_key_message(load(Some(&path), Some("alt_env".into())));
        assert!(message.contains(&path.display().to_string()), "{message}");
    }

    #[test]
    fn an_unreadable_key_file_is_no_key() {
        let dir = tempfile::tempdir().unwrap();
        no_key_message(load(Some(dir.path()), None));
    }

    #[test]
    fn create_then_write_makes_a_private_file_holding_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        write(create(&path).unwrap(), "alt_1").unwrap();
        assert_eq!(load(Some(&path), None).unwrap(), "alt_1");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn create_refuses_an_existing_path_and_leaves_it_alone() {
        let file = file_with("alt_other");
        let failure = create(file.path()).unwrap_err();
        assert_eq!(failure.exit, Exit::Usage);
        assert_eq!(failure.code, "key_file_exists");
        assert_eq!(fs::read_to_string(file.path()).unwrap(), "alt_other");
    }

    #[test]
    fn create_in_a_missing_directory_is_unexpected() {
        let dir = tempfile::tempdir().unwrap();
        let failure = create(&dir.path().join("missing/key")).unwrap_err();
        assert_eq!(failure.exit, Exit::Unexpected);
    }
}
