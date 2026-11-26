//! Where a profile's own key is kept: a file under the app's config folder, or the system
//! credential store (the macOS Keychain, the Secret Service on Linux).

use std::path::PathBuf;

#[cfg(not(target_arch = "wasm32"))]
/// The service the system credential store files keys under; the account is the profile's name.
const SERVICE: &str = "crucible-playbooks";
#[cfg(not(target_arch = "wasm32"))]
const GITHUB: &str = "github.com";
#[cfg(not(target_arch = "wasm32"))]
const GITHUB_FILE: &str = "github.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Store {
    #[cfg(not(target_arch = "wasm32"))]
    Keychain,
    /// One `<profile>.toml` per key in this folder, in crux's config format.
    Dir(PathBuf),
}

impl Store {
    /// `profile`'s key, when one is kept.
    pub fn key(&self, profile: &str) -> Result<Option<String>, String> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Store::Keychain => match entry(profile)?.get_password() {
                Ok(key) => Ok(Some(key)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(error) => Err(format!(
                    "reading {profile}'s key from the keychain: {error}"
                )),
            },
            Store::Dir(dir) => {
                let file = dir.join(format!("{profile}.toml"));
                if !file.exists() {
                    return Ok(None);
                }
                crux::config::FileConfig::load(Some(&file))
                    .map(|config| config.api_token)
                    .map_err(|error| format!("{error:#}"))
            }
        }
    }

    /// Keep `key` as `profile`'s, the controller at `url`.
    pub fn save(&self, profile: &str, url: &str, key: String) -> Result<(), String> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Store::Keychain => entry(profile)?
                .set_password(&key)
                .map_err(|error| format!("keeping {profile}'s key in the keychain: {error}")),
            Store::Dir(dir) => crux::config::FileConfig {
                url: Some(url.to_string()),
                api_token: Some(key),
            }
            .save(Some(&dir.join(format!("{profile}.toml"))))
            .map(drop)
            .map_err(|error| format!("{error:#}")),
        }
    }

    /// Drop `profile`'s key. No key kept is not an error.
    pub fn forget(&self, profile: &str) -> Result<(), String> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Store::Keychain => match entry(profile)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(format!(
                    "removing {profile}'s key from the keychain: {error}"
                )),
            },
            Store::Dir(dir) => match std::fs::remove_file(dir.join(format!("{profile}.toml"))) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(format!("removing {profile}'s key file: {error}")),
            },
        }
    }

    /// Keep `old`'s key as `new`'s, the controller at `url`.
    pub fn rename(&self, old: &str, new: &str, url: &str) -> Result<(), String> {
        match self.key(old)? {
            Some(key) => {
                self.save(new, url, key)?;
                self.forget(old)
            }
            None => Ok(()),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn github(&self) -> Result<Option<crate::github::Tokens>, String> {
        let text = match self {
            #[cfg(not(target_arch = "wasm32"))]
            Store::Keychain => match entry(GITHUB)?.get_password() {
                Ok(text) => text,
                Err(keyring::Error::NoEntry) => return Ok(None),
                Err(error) => {
                    return Err(format!(
                        "reading the GitHub sign-in from the keychain: {error}"
                    ));
                }
            },
            Store::Dir(dir) => match std::fs::read_to_string(dir.join(GITHUB_FILE)) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(format!("reading the GitHub sign-in: {error}")),
            },
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| format!("reading the GitHub sign-in: {error}"))
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn save_github(&self, tokens: &crate::github::Tokens) -> Result<(), String> {
        let text = serde_json::to_string(tokens)
            .map_err(|error| format!("keeping the GitHub sign-in: {error}"))?;
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Store::Keychain => entry(GITHUB)?
                .set_password(&text)
                .map_err(|error| format!("keeping the GitHub sign-in in the keychain: {error}")),
            Store::Dir(dir) => private_write(&dir.join(GITHUB_FILE), &text)
                .map_err(|error| format!("keeping the GitHub sign-in: {error}")),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn forget_github(&self) -> Result<(), String> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Store::Keychain => match entry(GITHUB)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(format!(
                    "removing the GitHub sign-in from the keychain: {error}"
                )),
            },
            Store::Dir(dir) => match std::fs::remove_file(dir.join(GITHUB_FILE)) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(format!("removing the GitHub sign-in: {error}")),
            },
        }
    }

    /// Move the keys of `profiles` (name, URL) to `to`. Nothing leaves this store unless every
    /// key was copied; on a failure the copies already made are dropped again.
    pub fn move_to(&self, to: &Store, profiles: &[(String, String)]) -> Result<(), String> {
        let mut copied = Vec::new();
        for (profile, url) in profiles {
            let copy = self.key(profile).and_then(|key| match key {
                Some(key) => to.save(profile, url, key).map(|()| true),
                None => Ok(false),
            });
            match copy {
                Ok(true) => copied.push(profile),
                Ok(false) => {}
                Err(why) => {
                    for profile in copied {
                        let _ = to.forget(profile);
                    }
                    return Err(why);
                }
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(tokens) = self.github()? {
            if let Err(why) = to.save_github(&tokens) {
                for profile in copied {
                    let _ = to.forget(profile);
                }
                return Err(why);
            }
            self.forget_github()?;
        }
        for profile in copied {
            self.forget(profile)?;
        }
        Ok(())
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn private_write(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    std::io::Write::write_all(&mut options.open(path)?, text.as_bytes())
}

#[cfg(not(target_arch = "wasm32"))]
fn entry(profile: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, profile).map_err(|error| format!("opening the keychain: {error}"))
}

#[cfg(test)]
mod tests {
    use crate::keys::Store;

    fn dir() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::Dir(dir.path().to_path_buf());
        (dir, store)
    }

    #[test]
    fn the_github_sign_in_is_kept_private_and_moves_with_the_keys() {
        let (held, store) = dir();
        let (_other_dir, other) = dir();
        assert_eq!(store.github().unwrap(), None);
        let tokens: crate::github::Tokens = serde_json::from_value(serde_json::json!({
            "login": "wren",
            "access": "ghu_a",
            "refresh": "ghr_b",
            "expires_at": 100,
        }))
        .unwrap();
        store.save_github(&tokens).unwrap();
        assert_eq!(store.github().unwrap(), Some(tokens.clone()));
        let file = held.path().join("github.json");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        store.move_to(&other, &[]).unwrap();
        assert_eq!(store.github().unwrap(), None);
        assert!(!file.exists());
        assert_eq!(other.github().unwrap(), Some(tokens));
        other.forget_github().unwrap();
        other.forget_github().unwrap();
        assert_eq!(other.github().unwrap(), None);
    }

    #[test]
    fn a_folder_keeps_one_key_per_profile() {
        let (dir, store) = dir();
        assert_eq!(store.key("a").unwrap(), None);
        store
            .save("a", "https://a.example.com", "crk_a".into())
            .unwrap();
        assert_eq!(store.key("a").unwrap().as_deref(), Some("crk_a"));
        assert_eq!(store.key("b").unwrap(), None);
        assert!(dir.path().join("a.toml").is_file());
        store.forget("a").unwrap();
        assert_eq!(store.key("a").unwrap(), None);
        store.forget("a").unwrap();
    }

    #[test]
    fn a_renamed_profile_keeps_its_key() {
        let (_dir, store) = dir();
        store
            .save("old", "https://a.example.com", "crk_a".into())
            .unwrap();
        store.rename("old", "new", "https://a.example.com").unwrap();
        assert_eq!(store.key("old").unwrap(), None);
        assert_eq!(store.key("new").unwrap().as_deref(), Some("crk_a"));
        store
            .rename("keyless", "other", "https://a.example.com")
            .unwrap();
        assert_eq!(store.key("other").unwrap(), None);
    }

    #[test]
    fn a_key_file_that_does_not_read_is_an_error_not_a_missing_key() {
        let (dir, store) = dir();
        std::fs::write(dir.path().join("a.toml"), "api_token = [").unwrap();
        assert!(store.key("a").is_err());
    }

    #[test]
    fn moving_keys_empties_the_old_store() {
        let ((_a, from), (_b, to)) = (dir(), dir());
        from.save("one", "https://one.example.com", "crk_1".into())
            .unwrap();
        from.save("two", "https://two.example.com", "crk_2".into())
            .unwrap();
        let profiles = [
            ("one".to_string(), "https://one.example.com".to_string()),
            ("two".to_string(), "https://two.example.com".to_string()),
            (
                "keyless".to_string(),
                "https://none.example.com".to_string(),
            ),
        ];
        from.move_to(&to, &profiles).unwrap();
        assert_eq!(to.key("one").unwrap().as_deref(), Some("crk_1"));
        assert_eq!(to.key("two").unwrap().as_deref(), Some("crk_2"));
        assert_eq!(to.key("keyless").unwrap(), None);
        assert_eq!(from.key("one").unwrap(), None);
        assert_eq!(from.key("two").unwrap(), None);
    }

    #[test]
    fn a_move_that_fails_leaves_every_key_where_it_was() {
        let ((a, from), (_b, to)) = (dir(), dir());
        from.save("one", "https://one.example.com", "crk_1".into())
            .unwrap();
        std::fs::write(a.path().join("two.toml"), "api_token = [").unwrap();
        let profiles = [
            ("one".to_string(), "https://one.example.com".to_string()),
            ("two".to_string(), "https://two.example.com".to_string()),
        ];
        assert!(from.move_to(&to, &profiles).is_err());
        assert_eq!(from.key("one").unwrap().as_deref(), Some("crk_1"));
        assert_eq!(to.key("one").unwrap(), None);
    }

    /// Writes to the real system credential store, which asks for the login password.
    #[test]
    #[ignore]
    fn the_keychain_keeps_and_forgets_a_key() {
        let profile = format!("test-{}", std::process::id());
        let store = Store::Keychain;
        assert_eq!(store.key(&profile).unwrap(), None);
        store
            .save(&profile, "https://a.example.com", "crk_test".into())
            .unwrap();
        assert_eq!(store.key(&profile).unwrap().as_deref(), Some("crk_test"));
        store.forget(&profile).unwrap();
        assert_eq!(store.key(&profile).unwrap(), None);
        store.forget(&profile).unwrap();
    }
}
