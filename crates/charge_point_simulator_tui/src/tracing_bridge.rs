use std::fmt;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;

/// A `tracing` layer that formats every event into a single line and forwards
/// it through a channel, so log output from any crate (including, later,
/// `ocpp-charge-point`) can be displayed in the TUI's log panel instead of
/// being written to the terminal, where it would corrupt the ratatui UI.
pub struct TuiLogLayer {
    sender: UnboundedSender<String>,
}

impl TuiLogLayer {
    pub fn new(sender: UnboundedSender<String>) -> Self {
        Self { sender }
    }
}

impl<S: Subscriber> Layer<S> for TuiLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let line = format!(
            "[{}] {}: {}",
            event.metadata().level(),
            event.metadata().target(),
            visitor.message.unwrap_or_default()
        );
        // The UI may not be draining yet (or may have shut down); dropping
        // the line in that case is preferable to blocking or panicking.
        let _ = self.sender.send(line);
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        }
    }
}

/// Installs a [`TuiLogLayer`] as the process-wide tracing subscriber and
/// returns the paired receiver. Must be called at most once per process,
/// before the UI starts.
pub fn install() -> UnboundedReceiver<String> {
    let (sender, receiver) = unbounded_channel();
    let subscriber = tracing_subscriber::registry().with(TuiLogLayer::new(sender));
    tracing::subscriber::set_global_default(subscriber)
        .expect("tracing subscriber already installed");
    receiver
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_the_level_target_and_message() {
        let (sender, mut receiver) = unbounded_channel();
        let subscriber = tracing_subscriber::registry().with(TuiLogLayer::new(sender));

        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!("something happened");
        });

        let line = receiver.try_recv().unwrap();
        assert!(line.contains("WARN"), "{line}");
        assert!(line.contains(module_path!()), "{line}");
        assert!(line.contains("something happened"), "{line}");
    }

    #[test]
    fn formats_events_with_structured_fields_without_dropping_the_message() {
        let (sender, mut receiver) = unbounded_channel();
        let subscriber = tracing_subscriber::registry().with(TuiLogLayer::new(sender));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(charger = "CP001", evses = 2, "charger event");
        });

        let line = receiver.try_recv().unwrap();
        assert!(line.contains("INFO"), "{line}");
        assert!(line.contains("charger event"), "{line}");
    }

    #[test]
    fn each_event_produces_exactly_one_line() {
        let (sender, mut receiver) = unbounded_channel();
        let subscriber = tracing_subscriber::registry().with(TuiLogLayer::new(sender));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("first");
            tracing::info!("second");
        });

        assert!(receiver.try_recv().unwrap().contains("first"));
        assert!(receiver.try_recv().unwrap().contains("second"));
        assert!(receiver.try_recv().is_err());
    }
}
