use std::sync::{Arc, Mutex};
use std::time::Duration;

use ocpp_charge_point::hardware::{
    Capabilities, ChargePoint, Evse, HardwareCommandReceiver, HardwareEventSender,
    execute_hardware_command,
};
use ocpp_charge_point::state::{ChargePointEvent, ConnectorEvent, EvseEvent};

use crate::charger::config::ChargerConfig;

use super::connector::FakeConnector;
use super::evse::FakeEvse;

/// Fake hardware for a charger: an EVSE/connector layout with no physical backing,
/// built to match a [`ChargerConfig`] so the shape the OCPP stack sees lines up with
/// what the picker/dashboard shows.
pub struct FakeChargePoint {
    vendor_name: String,
    model_name: String,
    evses: Vec<FakeEvse>,
    capabilities: Capabilities,
    /// The [`HardwareEventSender`] handed to [`Self::start`], stashed so [`Self::tick`] can push
    /// meter samples after `start` returns. `None` until `start` runs - a charge point that's
    /// never been started (e.g. a unit test that only checks `capabilities()`) simply has nowhere
    /// to send a tick's samples, and `tick` treats that as a no-op rather than a panic.
    events: Mutex<Option<HardwareEventSender>>,
}

impl FakeChargePoint {
    pub fn from_config(config: &ChargerConfig) -> Self {
        Self {
            vendor_name: "Flowion".to_string(),
            model_name: config.id.clone(),
            evses: config
                .evses
                .iter()
                .map(|evse_config| FakeEvse {
                    connectors: (1..=evse_config.connectors)
                        .map(|connector_id| {
                            FakeConnector::new(evse_config.id as usize, connector_id as usize)
                        })
                        .collect(),
                })
                .collect(),
            // Deliberately conservative: only what the simulator actually simulates today is
            // declared, so the CSMS isn't told about functional blocks nothing here implements.
            // `has_display` is the one capability the YAML config already describes.
            capabilities: Capabilities::default().with_has_display(config.has_display),
            events: Mutex::new(None),
        }
    }

    /// Advances every connector's simulated meter by `elapsed` and reports each one's sample to
    /// the charge point's actor as a `ConnectorEvent::MeterValueSampled`, via the
    /// [`HardwareEventSender`] stashed by [`Self::start`].
    ///
    /// Addressed by *position* in [`Self::evses`] and its connectors, not by the
    /// [`crate::charger::config::EvseConfig::id`]/connector-number pair `FakeConnector` carries
    /// for tracing - the runtime's `ChargePointState` is built with one EVSE/connector slot per
    /// position in the `connector_counts` this charge point's shape produced (see
    /// `ChargePointRuntime::new`), so that position is the addressing scheme the actor actually
    /// understands. `charger/ocpp_bridge.rs`'s `meter_sample_events` addresses the same way, for
    /// the same reason.
    ///
    /// Simulated time only, injected by the caller (decision 2 in `docs/hardware-roadmap.md`'s
    /// "Decisions taken") - this type never owns a `tokio::time::interval` or reads a wall clock.
    /// Nothing calls this yet: it exists so the physics-convergence task (the second half of H3)
    /// has a driving entry point to wire up in place of `EvseState::tick`'s accumulator in
    /// `charger/state.rs`, once a local (unconnected) simulation has a hardware layer to route
    /// its meter samples through. Calling this before [`Self::start`] has run is a silent no-op -
    /// there is no sender yet to push a sample through.
    pub async fn tick(&self, elapsed: Duration) {
        let events = self.events.lock().expect("lock poisoned").clone();
        let Some(events) = events else {
            return;
        };

        for (evse_id, evse) in self.evses.iter().enumerate() {
            for (connector_id, connector) in evse.connectors().iter().enumerate() {
                let sample = connector.tick(elapsed);
                if let Err(error) = events
                    .send(ChargePointEvent::Evse {
                        evse_id,
                        event: EvseEvent::Connector {
                            connector_id,
                            event: ConnectorEvent::MeterValueSampled(sample),
                        },
                    })
                    .await
                {
                    tracing::warn!(evse_id, connector_id, ?error, "failed to push meter sample");
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl ChargePoint<FakeEvse, FakeConnector> for FakeChargePoint {
    type StartError = core::convert::Infallible;

    fn vendor_name(&self) -> &str {
        &self.vendor_name
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn evses(&self) -> &[FakeEvse] {
        &self.evses
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    /// `setup()` (in `ocpp-charge-point`) awaits `start()` directly before registering with the
    /// CSMS, so this must return promptly rather than pumping the command loop inline - the
    /// loop is spawned onto its own task instead, owning the `Arc<Self>` the runtime already
    /// holds so it keeps running independent of this call's lifetime.
    async fn start(
        self: Arc<Self>,
        events: HardwareEventSender,
        mut commands: HardwareCommandReceiver,
    ) -> Result<(), Self::StartError> {
        *self.events.lock().expect("lock poisoned") = Some(events.clone());
        tokio::spawn(async move {
            while let Ok(command) = commands.recv().await {
                execute_hardware_command(self.evses(), command, &events).await;
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charger::config::{EvseConfig, OcppVersion};
    use ocpp_charge_point::ChargePointRuntime;
    use ocpp_charge_point::executor::TokioExecutor;
    use ocpp_charge_point::hardware::Evse;
    use std::time::Duration;

    #[tokio::test]
    async fn start_returns_promptly_instead_of_blocking_on_the_command_loop() {
        // `ocpp-charge-point`'s `setup()` awaits `ChargePoint::start` directly before
        // registering with the CSMS - if `start` pumped the command loop inline instead of
        // spawning it, this would hang forever instead of completing within the timeout.
        let config = ChargerConfig {
            id: "CP001".into(),
            ocpp_version: OcppVersion::V21,
            evses: vec![EvseConfig {
                id: 1,
                connectors: 1,
            }],
            has_display: false,
        };

        // A throwaway runtime, just to mint real event/command channel handles - `start` is
        // called directly below, not through this runtime.
        let channel_source =
            ChargePointRuntime::new(FakeChargePoint::from_config(&config), [1], &TokioExecutor);
        let events = channel_source.hardware_events();
        let commands = channel_source.hardware_commands();

        let charge_point = Arc::new(FakeChargePoint::from_config(&config));
        let result = tokio::time::timeout(
            Duration::from_millis(200),
            charge_point.start(events, commands),
        )
        .await;

        assert!(result.is_ok(), "start() did not return within the timeout");
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn tick_before_start_is_a_silent_no_op() {
        // No `HardwareEventSender` has ever been stashed, so there is nowhere to push a sample -
        // this must return without panicking rather than unwrapping a `None`.
        let config = ChargerConfig {
            id: "CP001".into(),
            ocpp_version: OcppVersion::V21,
            evses: vec![EvseConfig {
                id: 1,
                connectors: 1,
            }],
            has_display: false,
        };

        let charge_point = FakeChargePoint::from_config(&config);
        charge_point.tick(Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn tick_pushes_a_meter_sample_per_connector_once_started() {
        let config = ChargerConfig {
            id: "CP001".into(),
            ocpp_version: OcppVersion::V21,
            evses: vec![EvseConfig {
                id: 1,
                connectors: 2,
            }],
            has_display: false,
        };

        let channel_source =
            ChargePointRuntime::new(FakeChargePoint::from_config(&config), [2], &TokioExecutor);
        let events = channel_source.hardware_events();
        let commands = channel_source.hardware_commands();

        let charge_point = Arc::new(FakeChargePoint::from_config(&config));
        charge_point.clone().start(events, commands).await.unwrap();

        charge_point.tick(Duration::from_secs(3600)).await;

        // `latest_meter_samples` is recorded for every sample regardless of whether a
        // transaction is running (see `ocpp-charge-point`'s `apply_connector_event`), so this
        // observes the push without needing to drive a full charging session first.
        let state = channel_source.state();
        assert!(state.evses[0].latest_meter_samples[0].is_some());
        assert!(state.evses[0].latest_meter_samples[1].is_some());
    }

    #[tokio::test]
    async fn from_config_builds_one_fake_evse_per_configured_evse() {
        let config = ChargerConfig {
            id: "CP001".into(),
            ocpp_version: OcppVersion::V21,
            evses: vec![
                EvseConfig {
                    id: 1,
                    connectors: 2,
                },
                EvseConfig {
                    id: 2,
                    connectors: 1,
                },
            ],
            has_display: false,
        };

        let charge_point = FakeChargePoint::from_config(&config);

        assert_eq!(charge_point.evses().len(), 2);
        assert_eq!(charge_point.evses()[0].connectors().len(), 2);
        assert_eq!(charge_point.evses()[1].connectors().len(), 1);
        assert_eq!(charge_point.model_name(), "CP001");
        assert_eq!(charge_point.vendor_name(), "Flowion");
    }

    #[tokio::test]
    async fn from_config_with_no_evses_builds_no_fake_evses() {
        let config = ChargerConfig {
            id: "CP002".into(),
            ocpp_version: OcppVersion::V21,
            evses: vec![],
            has_display: false,
        };

        let charge_point = FakeChargePoint::from_config(&config);
        assert_eq!(charge_point.evses().len(), 0);
    }

    #[tokio::test]
    async fn capabilities_declare_only_what_the_simulator_supports() {
        let config = ChargerConfig {
            id: "CP001".into(),
            ocpp_version: OcppVersion::V21,
            evses: vec![],
            has_display: true,
        };

        let capabilities = FakeChargePoint::from_config(&config).capabilities();

        assert!(capabilities.has_display);
        assert!(!capabilities.smart_charging);
        assert!(!capabilities.reservation);
        assert!(!capabilities.firmware_management);
    }

    #[tokio::test]
    async fn capabilities_follow_the_configs_display_flag() {
        let config = ChargerConfig {
            id: "CP001".into(),
            ocpp_version: OcppVersion::V21,
            evses: vec![],
            has_display: false,
        };

        assert!(
            !FakeChargePoint::from_config(&config)
                .capabilities()
                .has_display
        );
    }
}
