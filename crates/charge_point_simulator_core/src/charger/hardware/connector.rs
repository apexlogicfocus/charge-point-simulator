use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use ocpp_charge_point::hardware::Connector;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
