use std::fmt;
use std::sync::Arc;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;

use crate::logs::{Direction, LogEntry, LogLevel};

/// Produces the timestamp string stored on each [`LogEntry`]. Injectable so
/// tests can assert on deterministic output instead of wall-clock time.
type Clock = Arc<dyn Fn() -> Option<String> + Send + Sync>;

fn real_clock() -> Option<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    let millis = now.as_millis();
    let secs_total = millis / 1000;
    let ms = millis % 1000;
    let secs_of_day = secs_total % 86_400;
    let hours = secs_of_day / 3600;
    let minutes = (secs_of_day % 3600) / 60;
    let seconds = secs_of_day % 60;
    Some(format!("{hours:02}:{minutes:02}:{seconds:02}.{ms:03}"))
}

/// A `tracing` layer that converts every event into a structured
/// [`LogEntry`] and forwards it through a channel, so log output from any
/// crate (including, later, `ocpp-charge-point`) can be displayed in the
/// TUI's log panel instead of being written to the terminal, where it would
/// corrupt the ratatui UI.
pub struct TuiLogLayer {
    sender: UnboundedSender<LogEntry>,
    clock: Clock,
}

impl TuiLogLayer {
    pub fn new(sender: UnboundedSender<LogEntry>) -> Self {
        Self {
            sender,
            clock: Arc::new(real_clock),
        }
    }

    /// A layer reading time from `clock` instead of the wall clock. Only tests construct one
    /// today - the app always wants the real clock - but the injection point is what keeps
    /// this module's tests from asserting against `SystemTime::now()`.
    #[cfg(test)]
    pub fn with_clock(sender: UnboundedSender<LogEntry>, clock: Clock) -> Self {
        Self { sender, clock }
    }
}

fn map_level(level: &Level) -> LogLevel {
    match *level {
        Level::ERROR => LogLevel::Error,
        Level::WARN => LogLevel::Warn,
        Level::INFO => LogLevel::Info,
        Level::DEBUG => LogLevel::Debug,
        Level::TRACE => LogLevel::Trace,
    }
}

fn parse_direction(value: &str) -> Option<Direction> {
    match value.to_ascii_lowercase().as_str() {
        "in" | "inbound" | "rx" | "received" => Some(Direction::Inbound),
        "out" | "outbound" | "tx" | "sent" => Some(Direction::Outbound),
        _ => None,
    }
}

impl<S: Subscriber> Layer<S> for TuiLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);

        let mut direction = None;
        let mut action = None;
        let mut fields = Vec::with_capacity(visitor.fields.len());
        for (name, value) in visitor.fields {
            match name.as_str() {
                "direction" | "dir" if direction.is_none() => {
                    if let Some(d) = parse_direction(&value) {
                        direction = Some(d);
                        continue;
                    }
                    fields.push((name, value));
                }
                "action" | "ocpp_action" if action.is_none() => {
                    action = Some(value);
                }
                _ => fields.push((name, value)),
            }
        }

        let entry = LogEntry {
            timestamp: (self.clock)(),
            level: map_level(event.metadata().level()),
            target: event.metadata().target().to_string(),
            message: visitor.message.unwrap_or_default(),
            fields,
            direction,
            action,
        };

        // The UI may not be draining yet (or may have shut down); dropping
        // the entry in that case is preferable to blocking or panicking.
        let _ = self.sender.send(entry);
    }
}

#[derive(Default)]
struct EventVisitor {
    message: Option<String>,
    fields: Vec<(String, String)>,
}

impl EventVisitor {
    fn push(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = Some(value);
        } else {
            self.fields.push((field.name().to_string(), value));
        }
    }
}

impl Visit for EventVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.push(field, value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.push(field, value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.push(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        // The fallback for anything the typed `record_*` methods above didn't claim - notably
        // `message` itself, which tracing always records through this path.
        self.push(field, format!("{value:?}"));
    }
}

/// Installs a [`TuiLogLayer`] as the process-wide tracing subscriber and
/// returns the paired receiver. Must be called at most once per process,
/// before the UI starts.
pub fn install() -> UnboundedReceiver<LogEntry> {
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
    fn captures_the_level_target_and_message() {
        let (sender, mut receiver) = unbounded_channel();
        let subscriber = tracing_subscriber::registry().with(TuiLogLayer::new(sender));

        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!("something happened");
        });

        let entry = receiver.try_recv().unwrap();
        assert!(matches!(entry.level, LogLevel::Warn));
        assert_eq!(entry.target, module_path!());
        assert_eq!(entry.message, "something happened");
    }

    #[test]
    fn structured_fields_do_not_drop_the_message() {
        let (sender, mut receiver) = unbounded_channel();
        let subscriber = tracing_subscriber::registry().with(TuiLogLayer::new(sender));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(charger = "CP001", evses = 2, "charger event");
        });

        let entry = receiver.try_recv().unwrap();
        assert!(matches!(entry.level, LogLevel::Info));
        assert_eq!(entry.message, "charger event");
        assert!(
            entry
                .fields
                .contains(&("charger".to_string(), "CP001".to_string()))
        );
        assert!(
            entry
                .fields
                .contains(&("evses".to_string(), "2".to_string()))
        );
    }

    #[test]
    fn each_event_produces_exactly_one_entry() {
        let (sender, mut receiver) = unbounded_channel();
        let subscriber = tracing_subscriber::registry().with(TuiLogLayer::new(sender));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("first");
            tracing::info!("second");
        });

        assert_eq!(receiver.try_recv().unwrap().message, "first");
        assert_eq!(receiver.try_recv().unwrap().message, "second");
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn string_fields_are_not_debug_quoted() {
        let (sender, mut receiver) = unbounded_channel();
        let subscriber = tracing_subscriber::registry().with(TuiLogLayer::new(sender));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(charger = "CP001", "event");
        });

        let entry = receiver.try_recv().unwrap();
        let (_, value) = entry
            .fields
            .iter()
            .find(|(name, _)| name == "charger")
            .unwrap();
        assert_eq!(value, "CP001");
    }

    #[test]
    fn direction_field_is_parsed_inbound() {
        let (sender, mut receiver) = unbounded_channel();
        let subscriber = tracing_subscriber::registry().with(TuiLogLayer::new(sender));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(direction = "inbound", "msg received");
        });

        let entry = receiver.try_recv().unwrap();
        assert!(matches!(entry.direction, Some(Direction::Inbound)));
        assert!(entry.fields.iter().all(|(name, _)| name != "direction"));
    }

    #[test]
    fn direction_field_is_parsed_outbound() {
        let (sender, mut receiver) = unbounded_channel();
        let subscriber = tracing_subscriber::registry().with(TuiLogLayer::new(sender));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(dir = "tx", "msg sent");
        });

        let entry = receiver.try_recv().unwrap();
        assert!(matches!(entry.direction, Some(Direction::Outbound)));
        assert!(entry.fields.iter().all(|(name, _)| name != "dir"));
    }

    #[test]
    fn action_field_is_extracted() {
        let (sender, mut receiver) = unbounded_channel();
        let subscriber = tracing_subscriber::registry().with(TuiLogLayer::new(sender));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(ocpp_action = "BootNotification", "msg");
        });

        let entry = receiver.try_recv().unwrap();
        assert_eq!(entry.action.as_deref(), Some("BootNotification"));
        assert!(entry.fields.iter().all(|(name, _)| name != "ocpp_action"));
    }

    #[test]
    fn uses_the_injected_clock() {
        let (sender, mut receiver) = unbounded_channel();
        let clock: Clock = Arc::new(|| Some("12:34:56.789".to_string()));
        let subscriber =
            tracing_subscriber::registry().with(TuiLogLayer::with_clock(sender, clock));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("time test");
        });

        let entry = receiver.try_recv().unwrap();
        assert_eq!(entry.timestamp.as_deref(), Some("12:34:56.789"));
    }
}
