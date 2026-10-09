use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "credentials.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    pub link_id: String,
    pub assistant_name: String,
    pub server: String,
    pub token: String,
}

pub fn config_dir(xdg_config_home: Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    // The XDG spec says a relative $XDG_CONFIG_HOME is invalid and must be ignored.
    let base = xdg_config_home
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| {
            home.filter(|home| !home.as_os_str().is_empty())
                .map(|home| home.join(".config"))
        })?;
    Some(base.join("assistant-link"))
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join(FILE_NAME)
}

pub fn load(dir: &Path) -> io::Result<Option<Credential>> {
    let contents = match fs::read(path(dir)) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    serde_json::from_slice(&contents)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn save(dir: &Path, credential: &Credential) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;

    let temp = dir.join(format!(".{FILE_NAME}.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&temp);
    let written = write_private(&temp, &serde_json::to_vec_pretty(credential)?)
        .and_then(|()| fs::rename(&temp, path(dir)));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
        return written;
    }
    // Makes the rename survive a crash. Best effort: some filesystems cannot sync a directory, and
    // the credential is already in place.
    let _ = fs::File::open(dir).and_then(|dir| dir.sync_all());
    Ok(())
}

fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn credential(link_id: &str) -> Credential {
        Credential {
            link_id: link_id.into(),
            assistant_name: "Hermes".into(),
            server: "https://example.test".into(),
            token: format!("alt_{link_id}"),
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn config_dir_uses_an_absolute_xdg_config_home() {
        assert_eq!(
            config_dir(Some("/xdg".into()), Some("/home/me".into())),
            Some(PathBuf::from("/xdg/assistant-link"))
        );
    }

    #[test]
    fn config_dir_falls_back_to_dot_config_in_home() {
        for xdg in [None, Some("".into()), Some("relative/dir".into())] {
            assert_eq!(
                config_dir(xdg.clone(), Some("/home/me".into())),
                Some(PathBuf::from("/home/me/.config/assistant-link")),
                "{xdg:?}"
            );
        }
    }

    #[test]
    fn config_dir_is_unknown_without_xdg_config_home_or_home() {
        assert_eq!(config_dir(None, None), None);
        assert_eq!(config_dir(None, Some("".into())), None);
    }

    #[test]
    fn load_without_a_stored_credential_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(&dir.path().join("assistant-link")).unwrap(), None);
    }

    #[test]
    fn save_then_load_returns_the_credential() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().join("assistant-link");
        save(&dir, &credential("a")).unwrap();
        assert_eq!(load(&dir).unwrap(), Some(credential("a")));
    }

    #[test]
    fn saved_file_and_new_directory_are_private_to_the_user() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().join("assistant-link");
        save(&dir, &credential("a")).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path(&dir)), 0o600);
    }

    #[test]
    fn save_replaces_the_stored_credential_and_keeps_it_private() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().to_path_buf();
        fs::write(path(&dir), "{}").unwrap();
        fs::set_permissions(path(&dir), fs::Permissions::from_mode(0o644)).unwrap();

        save(&dir, &credential("a")).unwrap();
        save(&dir, &credential("b")).unwrap();

        assert_eq!(load(&dir).unwrap(), Some(credential("b")));
        assert_eq!(mode(&path(&dir)), 0o600);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn load_of_a_corrupt_file_is_invalid_data() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(path(dir.path()), "not json").unwrap();
        let error = load(dir.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
