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
    #[serde(default)]
    pub has_display: bool,
}

impl ChargerConfig {
    pub fn from_yaml(yaml: &str) -> Result<Self, serde_yaml::Error> {
        serde_yaml::from_str(yaml)
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
}
