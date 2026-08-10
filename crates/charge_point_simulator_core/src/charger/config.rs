use ocpp_charge_point::hardware::Capabilities;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum OcppVersion {
    #[serde(rename = "1.6j")]
    V16J,
    #[serde(rename = "2.0.1")]
    V201,
    #[serde(rename = "2.1")]
    V21,
}

impl std::fmt::Display for OcppVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OcppVersion::V16J => write!(f, "OCPP 1.6J"),
            OcppVersion::V201 => write!(f, "OCPP 2.0.1"),
            OcppVersion::V21 => write!(f, "OCPP 2.1"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EvseConfig {
    pub id: u32,
    pub connectors: u32,
}

/// The `capabilities:` block of a charger YAML config: which of
/// [`ocpp_charge_point::hardware::Capabilities`]'s flags this charger's simulated hardware
/// declares to the CSMS.
///
/// **This is plumbing, not a feature list.** Setting a flag here only reaches the CSMS as `true`
/// (see [`ChargerConfig::capabilities`]) - it does not by itself make the simulator *do* anything
/// that flag implies. Per the hardware roadmap's "never advertise what isn't simulated" principle,
/// a capability flag going `true` must land in the same commit as the hardware behind it. Most of
/// the fields below currently have no simulated hardware backing them at all; they exist so that
/// later work can flip them on without another config format change. Setting one prematurely
/// tells a CSMS under test that this charger supports something the simulator will silently no-op.
///
/// Every field defaults to `false`, and an unknown key under `capabilities:` is a parse error
/// (`#[serde(deny_unknown_fields)]`) rather than being silently ignored - a typo'd capability name
/// that quietly stays `false` is exactly the class of bug this format exists to prevent, and a
/// declaration that's silently dropped is just as bad as one that's silently accepted.
///
/// Two of upstream's `Capabilities` fields aren't represented here yet:
/// `max_current_per_connector_amps` (an `Option<u16>`) and `iso15118_support` (an enum) - both
/// need a YAML shape richer than a bare bool, left for whichever task actually wires up hardware
/// that needs them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapabilitiesConfig {
    /// See [`Capabilities::has_display`]. Prefer the legacy top-level `has_display:` key or this
    /// one, not both - see [`ChargerConfig::capabilities`] for how they combine.
    pub has_display: bool,
    pub supports_bidirectional_power: bool,
    pub can_unlock_under_load: bool,
    pub has_rtc: bool,
    pub has_persistent_storage: bool,
    pub reservation: bool,
    pub local_auth_list: bool,
    pub smart_charging: bool,
    pub firmware_management: bool,
    pub firmware_publishing: bool,
    pub diagnostics: bool,
    pub certificate_management: bool,
    pub variable_monitoring: bool,
    pub tariff_and_cost: bool,
    pub payment: bool,
    pub der_control: bool,
    pub battery_swap: bool,
    pub periodic_event_stream: bool,
    pub certificates: bool,
    pub key_storage: bool,
    pub ocsp_checking: bool,
}

/// A charger's hardware definition: OCPP version and EVSE/connector layout. Deliberately
/// carries no CSMS connection details - the same definition can be dialed against different
/// CSMS endpoints (see [`crate::charger::connection`]).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ChargerConfig {
    pub id: String,
    pub ocpp_version: OcppVersion,
    #[serde(default)]
    pub evses: Vec<EvseConfig>,
    /// Whether this charger has a physical display (OCPP 2.x's DisplayMessage functional
    /// block: `SetDisplayMessage`/`ClearDisplayMessage`). Defaults to `false` - most chargers
    /// in the wild are display-less.
    ///
    /// Kept as its own top-level key for backwards compatibility with configs written before
    /// `capabilities:` existed; `capabilities.has_display` is the equivalent key inside the new
    /// block. [`Self::capabilities`] ORs the two together, so either spelling (or both) works.
    #[serde(default)]
    pub has_display: bool,
    /// Which [`Capabilities`] flags this charger's simulated hardware declares - see
    /// [`CapabilitiesConfig`]. Absent from the YAML, it defaults to all-`false`.
    #[serde(default)]
    pub capabilities: CapabilitiesConfig,
}

impl ChargerConfig {
    pub fn from_yaml(yaml: &str) -> Result<Self, serde_yaml::Error> {
        serde_yaml::from_str(yaml)
    }

    /// Builds the [`Capabilities`] this charger's simulated hardware declares to the CSMS, from
    /// the legacy top-level `has_display` field and the `capabilities:` block combined.
    ///
    /// This is a declaration, not a promise beyond what's already true elsewhere in `core`: a
    /// flag here reads `true` only because the YAML said so, and the hardware roadmap's "never
    /// advertise what isn't simulated" principle means nothing sets one of these `true` without
    /// simulated behavior to back it landing in the same change.
    pub fn capabilities(&self) -> Capabilities {
        let c = &self.capabilities;
        Capabilities::default()
            .with_has_display(self.has_display || c.has_display)
            .with_supports_bidirectional_power(c.supports_bidirectional_power)
            .with_can_unlock_under_load(c.can_unlock_under_load)
            .with_has_rtc(c.has_rtc)
            .with_has_persistent_storage(c.has_persistent_storage)
            .with_reservation(c.reservation)
            .with_local_auth_list(c.local_auth_list)
            .with_smart_charging(c.smart_charging)
            .with_firmware_management(c.firmware_management)
            .with_firmware_publishing(c.firmware_publishing)
            .with_diagnostics(c.diagnostics)
            .with_certificate_management(c.certificate_management)
            .with_variable_monitoring(c.variable_monitoring)
            .with_tariff_and_cost(c.tariff_and_cost)
            .with_payment(c.payment)
            .with_der_control(c.der_control)
            .with_battery_swap(c.battery_swap)
            .with_periodic_event_stream(c.periodic_event_stream)
            .with_certificates(c.certificates)
            .with_key_storage(c.key_storage)
            .with_ocsp_checking(c.ocsp_checking)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_charger_definition() {
        let config = ChargerConfig::from_yaml(
            r#"
            id: CP001
            ocpp_version: "1.6j"
            evses:
              - id: 1
                connectors: 1
            "#,
        )
        .unwrap();

        assert_eq!(
            config,
            ChargerConfig {
                id: "CP001".into(),
                ocpp_version: OcppVersion::V16J,
                evses: vec![EvseConfig {
                    id: 1,
                    connectors: 1
                }],
                has_display: false,
                capabilities: CapabilitiesConfig::default(),
            }
        );
    }

    #[test]
    fn defaults_to_no_display_when_omitted() {
        let config = ChargerConfig::from_yaml(
            r#"
            id: CP005
            ocpp_version: "2.1"
            "#,
        )
        .unwrap();

        assert!(!config.has_display);
    }

    #[test]
    fn parses_a_charger_with_a_display() {
        let config = ChargerConfig::from_yaml(
            r#"
            id: CP006
            ocpp_version: "2.1"
            has_display: true
            "#,
        )
        .unwrap();

        assert!(config.has_display);
    }

    #[test]
    fn defaults_to_no_evses_when_omitted() {
        let config = ChargerConfig::from_yaml(
            r#"
            id: CP002
            ocpp_version: "2.0.1"
            "#,
        )
        .unwrap();

        assert_eq!(config.evses, Vec::new());
    }

    #[test]
    fn parses_every_supported_ocpp_version() {
        for (raw, expected) in [
            ("1.6j", OcppVersion::V16J),
            ("2.0.1", OcppVersion::V201),
            ("2.1", OcppVersion::V21),
        ] {
            let config =
                ChargerConfig::from_yaml(&format!("id: CP\nocpp_version: \"{raw}\"\n")).unwrap();
            assert_eq!(config.ocpp_version, expected);
        }
    }

    #[test]
    fn rejects_an_unknown_ocpp_version() {
        let result = ChargerConfig::from_yaml(
            r#"
            id: CP003
            ocpp_version: "3.0"
            "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_yaml_missing_required_fields() {
        let result = ChargerConfig::from_yaml("id: CP004\n");
        assert!(result.is_err());
    }

    #[test]
    fn a_capabilities_block_setting_three_flags_round_trips_into_the_right_capabilities() {
        let config = ChargerConfig::from_yaml(
            r#"
            id: CP007
            ocpp_version: "2.1"
            capabilities:
              smart_charging: true
              reservation: true
              diagnostics: true
            "#,
        )
        .unwrap();

        let expected = Capabilities::default()
            .with_smart_charging(true)
            .with_reservation(true)
            .with_diagnostics(true);
        assert_eq!(config.capabilities(), expected);
    }

    #[test]
    fn an_absent_capabilities_block_yields_all_false_plus_the_legacy_has_display_flag() {
        let without_display = ChargerConfig::from_yaml(
            r#"
            id: CP008
            ocpp_version: "2.1"
            "#,
        )
        .unwrap();
        assert_eq!(without_display.capabilities(), Capabilities::default());

        let with_display = ChargerConfig::from_yaml(
            r#"
            id: CP009
            ocpp_version: "2.1"
            has_display: true
            "#,
        )
        .unwrap();
        assert_eq!(
            with_display.capabilities(),
            Capabilities::default().with_has_display(true)
        );
    }

    #[test]
    fn an_unknown_key_inside_capabilities_is_a_parse_error() {
        let result = ChargerConfig::from_yaml(
            r#"
            id: CP010
            ocpp_version: "2.1"
            capabilities:
              smart_charing: true
            "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn the_legacy_top_level_has_display_flag_still_reaches_capabilities() {
        let config = ChargerConfig::from_yaml(
            r#"
            id: CP011
            ocpp_version: "2.1"
            has_display: true
            "#,
        )
        .unwrap();

        assert!(config.capabilities().has_display);
    }

    #[test]
    fn capabilities_block_has_display_also_reaches_capabilities() {
        let config = ChargerConfig::from_yaml(
            r#"
            id: CP012
            ocpp_version: "2.1"
            capabilities:
              has_display: true
            "#,
        )
        .unwrap();

        assert!(config.capabilities().has_display);
    }
}
