use std::sync::atomic::{AtomicBool, Ordering};

use ocpp_charge_point::hardware::{
    ChargePoint, Connector, Evse, HardwareCommandReceiver, HardwareEventSender,
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
}

impl FakeConnector {
    pub fn new(evse_id: usize, connector_id: usize) -> Self {
        Self {
            evse_id,
            connector_id,
            locked: AtomicBool::new(false),
            contactor_closed: AtomicBool::new(false),
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    pub fn is_contactor_closed(&self) -> bool {
        self.contactor_closed.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl Connector for FakeConnector {
    type Error = core::convert::Infallible;

    async fn lock(&self) -> Result<(), Self::Error> {
        self.locked.store(true, Ordering::Relaxed);
        tracing::info!(evse = self.evse_id, connector = self.connector_id, "connector locked");
        Ok(())
    }

    async fn unlock(&self) -> Result<(), Self::Error> {
        self.locked.store(false, Ordering::Relaxed);
        tracing::info!(evse = self.evse_id, connector = self.connector_id, "connector unlocked");
        Ok(())
    }

    async fn close_contactor(&self) -> Result<(), Self::Error> {
        self.contactor_closed.store(true, Ordering::Relaxed);
        tracing::info!(evse = self.evse_id, connector = self.connector_id, "contactor closed");
        Ok(())
    }

    async fn open_contactor(&self) -> Result<(), Self::Error> {
        self.contactor_closed.store(false, Ordering::Relaxed);
        tracing::info!(evse = self.evse_id, connector = self.connector_id, "contactor opened");
        Ok(())
    }
}

pub struct FakeEvse {
    pub connectors: Vec<FakeConnector>,
}

#[async_trait::async_trait]
impl Evse<FakeConnector> for FakeEvse {
    async fn connectors(&self) -> &[FakeConnector] {
        &self.connectors
    }
}

/// Fake hardware for a charger: an EVSE/connector layout with no physical backing,
/// built to match a [`ChargerConfig`] so the shape the OCPP stack sees lines up with
/// what the picker/dashboard shows.
pub struct FakeChargePoint {
    vendor_name: String,
    model_name: String,
    evses: Vec<FakeEvse>,
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
        }
    }
}

#[async_trait::async_trait]
impl ChargePoint<FakeEvse, FakeConnector> for FakeChargePoint {
    type StartError = core::convert::Infallible;

    async fn vendor_name(&self) -> &str {
        &self.vendor_name
    }

    async fn model_name(&self) -> &str {
        &self.model_name
    }

    async fn evses(&self) -> &[FakeEvse] {
        &self.evses
    }

    async fn start(
        &self,
        events: HardwareEventSender,
        mut commands: HardwareCommandReceiver,
    ) -> Result<(), Self::StartError> {
        while let Ok(command) = commands.recv().await {
            execute_hardware_command(&self.evses, command, &events).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charger::config::{EvseConfig, OcppVersion};

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
                EvseConfig { id: 1, connectors: 2 },
                EvseConfig { id: 2, connectors: 1 },
            ],
        };

        let charge_point = FakeChargePoint::from_config(&config);

        assert_eq!(charge_point.evses().await.len(), 2);
        assert_eq!(charge_point.evses().await[0].connectors().await.len(), 2);
        assert_eq!(charge_point.evses().await[1].connectors().await.len(), 1);
        assert_eq!(charge_point.model_name().await, "CP001");
        assert_eq!(charge_point.vendor_name().await, "Flowion");
    }

    #[tokio::test]
    async fn from_config_with_no_evses_builds_no_fake_evses() {
        let config = ChargerConfig {
            id: "CP002".into(),
            ocpp_version: OcppVersion::V21,
            evses: vec![],
        };

        let charge_point = FakeChargePoint::from_config(&config);
        assert_eq!(charge_point.evses().await.len(), 0);
    }
}
