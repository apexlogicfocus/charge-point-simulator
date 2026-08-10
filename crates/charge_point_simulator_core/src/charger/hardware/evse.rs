use ocpp_charge_point::hardware::Evse;

use super::connector::FakeConnector;

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
