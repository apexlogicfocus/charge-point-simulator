use std::path::Path;

use super::config::{ChargerConfig, EvseConfig, OcppVersion};

/// A charger definition together with where it came from, so callers (e.g. the
/// TUI picker) can distinguish presets from user-provided configs if needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChargerEntry {
    pub config: ChargerConfig,
    pub source: ChargerSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargerSource {
    BuiltIn,
    Configured,
}

/// The small set of presets shipped with the simulator, usable without any YAML.
pub fn built_in_chargers() -> Vec<ChargerEntry> {
    vec![
        ChargerEntry {
            config: ChargerConfig {
                id: "demo-ocpp16j-single".into(),
                ocpp_version: OcppVersion::V16J,
                evses: vec![EvseConfig {
                    id: 1,
                    connectors: 1,
                }],
                has_display: false,
            },
            source: ChargerSource::BuiltIn,
        },
        ChargerEntry {
            config: ChargerConfig {
                id: "demo-ocpp201-dual".into(),
                ocpp_version: OcppVersion::V201,
                evses: vec![
                    EvseConfig {
                        id: 1,
                        connectors: 2,
                    },
                    EvseConfig {
                        id: 2,
                        connectors: 2,
                    },
                ],
                has_display: true,
            },
            source: ChargerSource::BuiltIn,
        },
    ]
}

/// Reads every `*.yaml`/`*.yml` file directly inside `dir` and parses it as a
/// [`ChargerConfig`]. Files that fail to parse are skipped rather than aborting
/// discovery of the rest, since one bad file shouldn't take down the picker.
/// Returns an empty list if `dir` does not exist.
pub fn discover_configured_chargers(dir: &Path) -> Vec<ChargerEntry> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut entries: Vec<ChargerEntry> = read_dir
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|ft| ft.is_file()))
        .filter(|entry| {
            matches!(
                entry.path().extension().and_then(|ext| ext.to_str()),
                Some("yaml") | Some("yml")
            )
        })
        .filter_map(|entry| {
            let path = entry.path();
            let contents = match std::fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "failed to read charger config");
                    return None;
                }
            };
            match ChargerConfig::from_yaml(&contents) {
                Ok(config) => Some(config),
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "skipping invalid charger config");
                    None
                }
            }
        })
        .map(|config| ChargerEntry {
            config,
            source: ChargerSource::Configured,
        })
        .collect();

    entries.sort_by(|a, b| a.config.id.cmp(&b.config.id));
    entries
}

/// Built-in presets followed by every charger configured via YAML in `dir`.
pub fn all_chargers(dir: &Path) -> Vec<ChargerEntry> {
    let mut chargers = built_in_chargers();
    chargers.extend(discover_configured_chargers(dir));
    chargers
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn built_in_chargers_are_not_empty() {
        assert!(!built_in_chargers().is_empty());
        assert!(
            built_in_chargers()
                .iter()
                .all(|entry| entry.source == ChargerSource::BuiltIn)
        );
    }

    #[test]
    fn discovery_returns_empty_for_a_missing_directory() {
        let missing = Path::new("/definitely/does/not/exist/anywhere");
        assert_eq!(discover_configured_chargers(missing), Vec::new());
    }

    #[test]
    fn discovers_valid_yaml_files_and_skips_invalid_ones() {
        let dir = tempfile::tempdir().unwrap();

        fs::write(
            dir.path().join("cp-a.yaml"),
            "id: CP-A\nocpp_version: \"1.6j\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("cp-b.yml"),
            "id: CP-B\nocpp_version: \"2.1\"\n",
        )
        .unwrap();
        fs::write(dir.path().join("broken.yaml"), "not: [valid, charger").unwrap();
        fs::write(dir.path().join("notes.txt"), "ignore me").unwrap();

        let entries = discover_configured_chargers(dir.path());
        let ids: Vec<&str> = entries.iter().map(|e| e.config.id.as_str()).collect();

        assert_eq!(ids, vec!["CP-A", "CP-B"]);
        assert!(
            entries
                .iter()
                .all(|entry| entry.source == ChargerSource::Configured)
        );
    }

    #[test]
    fn all_chargers_lists_built_in_entries_before_configured_ones() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("custom.yaml"),
            "id: custom-charger\nocpp_version: \"2.0.1\"\n",
        )
        .unwrap();

        let entries = all_chargers(dir.path());
        let built_in_count = built_in_chargers().len();

        assert_eq!(entries.len(), built_in_count + 1);
        assert!(
            entries[..built_in_count]
                .iter()
                .all(|e| e.source == ChargerSource::BuiltIn)
        );
        assert_eq!(entries[built_in_count].config.id, "custom-charger");
    }
}
