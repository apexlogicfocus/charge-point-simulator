use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::connection::ConnectionProfile;

/// The last-used [`ConnectionProfile`] per charger id, persisted to disk so a charger's CSMS
/// connection details don't need re-entering every session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionStore {
    chargers: HashMap<String, ConnectionProfile>,
}

impl ConnectionStore {
    /// Loads the store from `path`. Missing or corrupt files are treated the same as an empty
    /// store (logging a warning for corruption) rather than failing - losing remembered
    /// connection details shouldn't take down the picker.
    pub fn load(path: &Path) -> Self {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_yaml::from_str(&contents) {
            Ok(store) => store,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "ignoring unreadable connection store");
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let yaml = serde_yaml::to_string(self)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        std::fs::write(path, yaml)
    }

    pub fn get(&self, charger_id: &str) -> Option<&ConnectionProfile> {
        self.chargers.get(charger_id)
    }

    pub fn remember(&mut self, charger_id: impl Into<String>, profile: ConnectionProfile) {
        self.chargers.insert(charger_id.into(), profile);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charger::connection::SecurityProfile;

    fn profile(url: &str) -> ConnectionProfile {
        ConnectionProfile {
            csms_url: url.to_string(),
            ocpp_identity: "CP001".to_string(),
            security: SecurityProfile::basic("secret").unwrap(),
        }
    }

    #[test]
    fn loading_a_missing_file_returns_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConnectionStore::load(&dir.path().join("connections.yaml"));
        assert_eq!(store.get("CP001"), None);
    }

    #[test]
    fn loading_a_corrupt_file_returns_an_empty_store_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.yaml");
        std::fs::write(&path, "not: [valid, store").unwrap();

        let store = ConnectionStore::load(&path);
        assert_eq!(store.get("CP001"), None);
    }

    #[test]
    fn remember_then_save_then_load_round_trips_the_profile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.yaml");

        let mut store = ConnectionStore::default();
        store.remember("CP001", profile("wss://csms.example.com/dev"));
        store.save(&path).unwrap();

        let loaded = ConnectionStore::load(&path);
        assert_eq!(
            loaded.get("CP001"),
            Some(&profile("wss://csms.example.com/dev"))
        );
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/dir/connections.yaml");

        let mut store = ConnectionStore::default();
        store.remember("CP001", profile("wss://csms.example.com/dev"));
        store.save(&path).unwrap();

        assert!(path.exists());
    }

    #[test]
    fn get_on_an_unknown_charger_returns_none() {
        let store = ConnectionStore::default();
        assert_eq!(store.get("unknown"), None);
    }

    #[test]
    fn remembering_a_charger_again_overwrites_its_previous_profile() {
        let mut store = ConnectionStore::default();
        store.remember("CP001", profile("wss://old.example.com"));
        store.remember("CP001", profile("wss://new.example.com"));

        assert_eq!(store.get("CP001"), Some(&profile("wss://new.example.com")));
    }
}
