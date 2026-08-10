use core::convert::Infallible;

use ocpp_charge_point::ChargePointRuntime;
use ocpp_charge_point::ConnectAndSetupError;
use ocpp_charge_point::executor::TokioExecutor;
use ocpp_charge_point::provisioning::TokioBackoff;
use ocpp_client::ConnectOptions;

use super::config::ChargerConfig;
use super::connection::{ConnectionProfile, SecurityProfile};
use super::hardware::FakeChargePoint;

/// Builds the WebSocket URL to dial for `ocpp_identity`, given the CSMS's configured base
/// address: normalizes an `http(s)://` scheme to `ws(s)://` (so [`ConnectionProfile::csms_url`]
/// can be written either way) and appends the identity as the final path segment.
pub fn websocket_url(base_url: &str, ocpp_identity: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    let base_url = if let Some(rest) = base_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base_url.to_string()
    };
    format!("{base_url}/{ocpp_identity}")
}

/// Dials `profile`'s CSMS as an OCPP 2.1 CSMS and runs the fake hardware built from `config`
/// against it. Only meaningful for OCPP 2.1 chargers - `ocpp-charge-point`'s own
/// `setup`/`connect_and_setup` don't cover 1.6J/2.0.1 yet, so callers must check
/// `config.ocpp_version` themselves before calling this.
pub async fn connect_charger(
    config: &ChargerConfig,
    profile: &ConnectionProfile,
) -> Result<ChargePointRuntime<FakeChargePoint>, ConnectAndSetupError<Infallible>> {
    let charge_point = FakeChargePoint::from_config(config);
    let url = websocket_url(&profile.csms_url, &profile.ocpp_identity);
    tracing::info!(
        charger = %config.id,
        identity = %profile.ocpp_identity,
        url = %url,
        "connecting to CSMS"
    );

    let SecurityProfile::Basic { password } = &profile.security;
    let options = ConnectOptions {
        username: Some(profile.ocpp_identity.as_str()),
        password: Some(password.as_str()),
        ..Default::default()
    };

    ocpp_charge_point::connect_and_setup(
        charge_point,
        &url,
        // No explicit version list or payload limits: offer whatever the client supports and
        // take the crate's default frame ceiling.
        None,
        Some(options),
        None,
        TokioExecutor,
        TokioBackoff,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charger::config::OcppVersion;

    #[tokio::test]
    #[ignore = "hits a real local CSMS, run manually with --ignored"]
    async fn can_connect_to_the_local_dev_csms() {
        let config = ChargerConfig {
            id: "sim-test".into(),
            ocpp_version: OcppVersion::V21,
            evses: vec![],
            has_display: false,
        };
        let profile = ConnectionProfile {
            csms_url: std::env::var("OCPP_TEST_URL")
                .unwrap_or_else(|_| "http://localhost:8082/flowion/dev".into()),
            ocpp_identity: std::env::var("OCPP_TEST_IDENTITY")
                .unwrap_or_else(|_| "sim-test".into()),
            security: SecurityProfile::basic(
                std::env::var("OCPP_TEST_PASSWORD").unwrap_or_default(),
            )
            .unwrap(),
        };

        match connect_charger(&config, &profile).await {
            Ok(_runtime) => println!("connected successfully"),
            Err(error) => panic!("connect failed: {error}"),
        }
    }

    #[test]
    fn converts_http_to_ws_and_appends_the_identity() {
        assert_eq!(
            websocket_url("http://localhost:8082/flowion/dev", "CP001"),
            "ws://localhost:8082/flowion/dev/CP001"
        );
    }

    #[test]
    fn converts_https_to_wss() {
        assert_eq!(
            websocket_url("https://csms.example.com/dev", "CP001"),
            "wss://csms.example.com/dev/CP001"
        );
    }

    #[test]
    fn leaves_an_already_websocket_scheme_untouched() {
        assert_eq!(
            websocket_url("ws://localhost:8082/flowion/dev", "CP001"),
            "ws://localhost:8082/flowion/dev/CP001"
        );
        assert_eq!(
            websocket_url("wss://localhost:8082/flowion/dev", "CP001"),
            "wss://localhost:8082/flowion/dev/CP001"
        );
    }

    #[test]
    fn does_not_produce_a_double_slash_when_the_base_url_has_a_trailing_slash() {
        assert_eq!(
            websocket_url("http://localhost:8082/flowion/dev/", "CP001"),
            "ws://localhost:8082/flowion/dev/CP001"
        );
    }
}
