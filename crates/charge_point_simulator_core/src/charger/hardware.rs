use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ocpp_charge_point::hardware::{
    Capabilities, ChargePoint, Connector, Evse, HardwareCommandReceiver, HardwareEventSender,
    execute_hardware_command,
};

use super::config::ChargerConfig;

/// A simulated connector actuator: no real hardware behind it, just lock/contactor
/// state tracked in memory. Every action is reported via `tracing` so it flows into
/// whatever is bridging tracing output (e.g. the TUI's log panel) the same way real
/// hardware driver logs would.
#[derive(Debug)]
pub struct FakeConnector {
    evse_id: usize,
    connector_id: usize,
    locked: AtomicBool,
    contactor_closed: AtomicBool,
    /// The most recent current limit applied via [`Connector::set_current_limit`], in mA, or
    /// `None` when no CSMS-imposed limit currently applies. Stored but not yet acted on - the
    /// simulator has no current draw to clamp until metering is simulated.
    current_limit_ma: Mutex<Option<u32>>,
}

impl FakeConnector {
    pub fn new(evse_id: usize, connector_id: usize) -> Self {
        Self {
            evse_id,
            connector_id,
            locked: AtomicBool::new(false),
            contactor_closed: AtomicBool::new(false),
            current_limit_ma: Mutex::new(None),
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    pub fn is_contactor_closed(&self) -> bool {
        self.contactor_closed.load(Ordering::Relaxed)
    }

    /// The current limit last applied to this connector, in mA, or `None` if unlimited.
    pub fn current_limit_ma(&self) -> Option<u32> {
        *self.current_limit_ma.lock().expect("lock poisoned")
    }
}

#[async_trait::async_trait]
impl Connector for FakeConnector {
    type Error = core::convert::Infallible;

    async fn lock(&self) -> Result<(), Self::Error> {
        self.locked.store(true, Ordering::Relaxed);
        tracing::info!(
            evse = self.evse_id,
            connector = self.connector_id,
            "connector locked"
        );
        Ok(())
    }

    async fn unlock(&self) -> Result<(), Self::Error> {
        self.locked.store(false, Ordering::Relaxed);
        tracing::info!(
            evse = self.evse_id,
            connector = self.connector_id,
            "connector unlocked"
        );
        Ok(())
    }

    async fn close_contactor(&self) -> Result<(), Self::Error> {
        self.contactor_closed.store(true, Ordering::Relaxed);
        tracing::info!(
            evse = self.evse_id,
            connector = self.connector_id,
            "contactor closed"
        );
        Ok(())
    }

    async fn open_contactor(&self) -> Result<(), Self::Error> {
        self.contactor_closed.store(false, Ordering::Relaxed);
        tracing::info!(
            evse = self.evse_id,
            connector = self.connector_id,
            "contactor opened"
        );
        Ok(())
    }

    async fn set_current_limit(&self, limit_ma: Option<u32>) -> Result<(), Self::Error> {
        *self.current_limit_ma.lock().expect("lock poisoned") = limit_ma;
        tracing::info!(
            evse = self.evse_id,
            connector = self.connector_id,
            limit_ma = ?limit_ma,
            "current limit set"
        );
        Ok(())
    }
}

pub struct FakeEvse {
    pub connectors: Vec<FakeConnector>,
}

#[async_trait::async_trait]
impl Evse<FakeConnector> for FakeEvse {
    type Error = core::convert::Infallible;

    fn connectors(&self) -> &[FakeConnector] {
        &self.connectors
    }

    /// A simulated reboot: nothing to restart, so this only reports the request. The state
    /// machine has already stopped any transaction and unlocked fail-safely by this point.
    async fn reboot(&self) -> Result<(), Self::Error> {
        tracing::info!("evse reboot requested");
        Ok(())
    }
}

/// Fake hardware for a charger: an EVSE/connector layout with no physical backing,
/// built to match a [`ChargerConfig`] so the shape the OCPP stack sees lines up with
/// what the picker/dashboard shows.
pub struct FakeChargePoint {
    vendor_name: String,
    model_name: String,
    evses: Vec<FakeEvse>,
    capabilities: Capabilities,
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
    async fn lock_and_unlock_flip_the_locked_flag() {
        let connector = FakeConnector::new(1, 1);
        assert!(!connector.is_locked());

        connector.lock().await.unwrap();
        assert!(connector.is_locked());

        connector.unlock().await.unwrap();
        assert!(!connector.is_locked());
    }

    #[tokio::test]
    async fn close_and_open_contactor_flip_the_contactor_flag() {
        let connector = FakeConnector::new(1, 1);
        assert!(!connector.is_contactor_closed());

        connector.close_contactor().await.unwrap();
        assert!(connector.is_contactor_closed());

        connector.open_contactor().await.unwrap();
        assert!(!connector.is_contactor_closed());
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
    async fn set_current_limit_records_the_limit_and_clears_it_again() {
        let connector = FakeConnector::new(1, 1);
        assert_eq!(connector.current_limit_ma(), None);

        connector.set_current_limit(Some(16_000)).await.unwrap();
        assert_eq!(connector.current_limit_ma(), Some(16_000));

        // `Some(0)` is "suspend charging", distinct from `None` - both must round-trip.
        connector.set_current_limit(Some(0)).await.unwrap();
        assert_eq!(connector.current_limit_ma(), Some(0));

        connector.set_current_limit(None).await.unwrap();
        assert_eq!(connector.current_limit_ma(), None);
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
