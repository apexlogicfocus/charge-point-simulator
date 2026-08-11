use std::path::Path;

use super::config::{CapabilitiesConfig, ChargerConfig, EvseConfig, OcppVersion};

/// A charger definition together with where it came from, so callers (e.g. the
/// TUI picker) can distinguish presets from user-provided configs if needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChargerEntry {
    pub config: ChargerConfig,
    pub source: ChargerSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChargerSource {
    BuiltIn,
    /// Loaded from a YAML file discovered in the config directory - `file_name` is that file's
    /// bare name (e.g. `"my-charger.yaml"`, not the full path) so callers like the TUI picker
    /// can show which file a charger came from without leaking the config directory's absolute
    /// path onto screen.
    Configured {
        file_name: String,
    },
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
                capabilities: CapabilitiesConfig::default(),
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
                capabilities: CapabilitiesConfig::default(),
            },
            source: ChargerSource::BuiltIn,
        },
        ChargerEntry {
            config: ChargerConfig {
                id: "demo-ocpp21-full".into(),
                ocpp_version: OcppVersion::V21,
                evses: vec![
                    EvseConfig {
                        id: 1,
                        connectors: 1,
                    },
                    EvseConfig {
                        id: 2,
                        connectors: 1,
                    },
                ],
                has_display: false,
                capabilities: SIMULATED_CAPABILITIES,
            },
            source: ChargerSource::BuiltIn,
        },
    ]
}

/// Every capability this simulator has simulated hardware behind, and no others - the declaration
/// the `demo-ocpp21-full` preset above carries.
///
/// This constant is the roadmap's "never advertise what isn't simulated" principle written down: a
/// flag belongs here only once something in `charger/hardware/` actually implements the behavior and
/// `charger/connect.rs` registers the functional block for it. It is deliberately *not*
/// "everything `CapabilitiesConfig` has a field for" - the fields exist so a config can declare a
/// flag ahead of the hardware, which is exactly what a shipped preset must not do.
///
/// Why each of the omitted ones is omitted, so the next person doesn't have to re-derive it:
/// `can_unlock_under_load` and `has_rtc` have no simulated behavior distinguishing them from their
/// defaults; `firmware_publishing` has no `FirmwarePublisher` fake and `ocsp_checking` no
/// `OcspChecker`, both named in the roadmap's "Known gaps"; `key_storage` has a store but no
/// `ChargePointBuilder` method that registers one (see `ChargerHardware`'s doc comment);
/// `variable_monitoring`, `tariff_and_cost`, `payment`, `battery_swap` and `periodic_event_stream`
/// are CSMS-facing reporting blocks with nothing simulated underneath.
///
/// `der_control` *is* included: `register_der_control` registers it and the charger answers all five
/// of its messages honestly. What that block does not do is actuate - upstream's own scope is
/// "store and report" - so declaring it promises answers, not a connector that changes direction on
/// a CSMS's say-so. Discharge itself is reachable only through
/// [`super::running_charger::RunningCharger::set_discharging`], which is why
/// `supports_bidirectional_power` next to it is about the *meter*, not the protocol.
pub const SIMULATED_CAPABILITIES: CapabilitiesConfig = CapabilitiesConfig {
    // H6a/H6b: `FakeDisplay` records what it was told to show.
    has_display: true,
    // H14a: the meter genuinely discharges, and `exported_energy_wh` accumulates separately from
    // OCPP's import register.
    supports_bidirectional_power: true,
    // H5a/H5b: `FileStorage`, one file per key, atomic writes.
    has_persistent_storage: true,
    // H9: reservations expire locally, and the local list is consulted offline (H3c).
    reservation: true,
    local_auth_list: true,
    // H8: a charging profile computes and applies a current limit the meter actually obeys.
    smart_charging: true,
    // H10a/H10b: `FakeFirmwareInstaller`/`FakeFirmwareVerifier` plus `FakeFileTransfer`.
    firmware_management: true,
    // H10b: the log-upload half of the same file transfer.
    diagnostics: true,
    // H12a/H12b: `FileCertificateStore`, bounded and persistent.
    certificate_management: true,
    // H13b/H14b: `register_der_control` - see this constant's own doc comment for what that means
    // and, just as importantly, what it doesn't.
    der_control: true,

    can_unlock_under_load: false,
    has_rtc: false,
    firmware_publishing: false,
    variable_monitoring: false,
    tariff_and_cost: false,
    payment: false,
    battery_swap: false,
    periodic_event_stream: false,
    certificates: false,
    key_storage: false,
    ocsp_checking: false,
};

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
            let file_name = path.file_name()?.to_string_lossy().into_owned();
            let contents = match std::fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "failed to read charger config");
                    return None;
                }
            };
            match ChargerConfig::from_yaml(&contents) {
                Ok(config) => Some((config, file_name)),
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "skipping invalid charger config");
                    None
                }
            }
        })
        .map(|(config, file_name)| ChargerEntry {
            config,
            source: ChargerSource::Configured { file_name },
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

    /// The point of the `demo-ocpp21-full` preset: every feature this simulator has hardware for is
    /// reachable without anyone hand-writing YAML first. It is 2.1 because that is the only version
    /// `connect_charger` dials, and `der_control` is registered on that path alone.
    #[test]
    fn a_built_in_preset_declares_every_simulated_capability() {
        let demo = built_in_chargers()
            .into_iter()
            .find(|entry| entry.config.id == "demo-ocpp21-full")
            .expect("the full-featured demo preset should ship");

        assert_eq!(demo.config.ocpp_version, OcppVersion::V21);
        assert_eq!(demo.config.capabilities, SIMULATED_CAPABILITIES);
    }

    /// The guard on the constant above: it must stay a description of what is simulated, not drift
    /// into "every field there is". If a wave adds hardware for one of these, move it - deliberately,
    /// in the same change as the hardware, which is exactly what this assertion forces.
    #[test]
    fn the_simulated_capability_set_still_excludes_everything_without_hardware_behind_it() {
        let unsimulated = [
            (
                "can_unlock_under_load",
                SIMULATED_CAPABILITIES.can_unlock_under_load,
            ),
            ("has_rtc", SIMULATED_CAPABILITIES.has_rtc),
            (
                "firmware_publishing",
                SIMULATED_CAPABILITIES.firmware_publishing,
            ),
            (
                "variable_monitoring",
                SIMULATED_CAPABILITIES.variable_monitoring,
            ),
            ("tariff_and_cost", SIMULATED_CAPABILITIES.tariff_and_cost),
            ("payment", SIMULATED_CAPABILITIES.payment),
            ("battery_swap", SIMULATED_CAPABILITIES.battery_swap),
            (
                "periodic_event_stream",
                SIMULATED_CAPABILITIES.periodic_event_stream,
            ),
            ("certificates", SIMULATED_CAPABILITIES.certificates),
            ("key_storage", SIMULATED_CAPABILITIES.key_storage),
            ("ocsp_checking", SIMULATED_CAPABILITIES.ocsp_checking),
        ];

        for (name, declared) in unsimulated {
            assert!(
                !declared,
                "{name} is declared but has no simulated hardware behind it - see \
                 SIMULATED_CAPABILITIES' doc comment"
            );
        }
    }

    /// The two original presets stay bare on purpose: a plain 1.6J charger and a plain 2.0.1 one are
    /// what most people want to start from, and every capability is opt-in.
    #[test]
    fn the_two_plain_presets_declare_nothing() {
        for id in ["demo-ocpp16j-single", "demo-ocpp201-dual"] {
            let entry = built_in_chargers()
                .into_iter()
                .find(|entry| entry.config.id == id)
                .unwrap_or_else(|| panic!("{id} should ship"));
            assert_eq!(entry.config.capabilities, CapabilitiesConfig::default());
        }
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
                .all(|entry| matches!(entry.source, ChargerSource::Configured { .. }))
        );
    }

    #[test]
    fn a_configured_charger_remembers_which_file_it_came_from() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("cp-a.yaml"),
            "id: CP-A\nocpp_version: \"1.6j\"\n",
        )
        .unwrap();

        let entries = discover_configured_chargers(dir.path());

        assert_eq!(
            entries[0].source,
            ChargerSource::Configured {
                file_name: "cp-a.yaml".to_string()
            }
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
        assert_eq!(
            entries[built_in_count].source,
            ChargerSource::Configured {
                file_name: "custom.yaml".to_string()
            }
        );
    }
}
