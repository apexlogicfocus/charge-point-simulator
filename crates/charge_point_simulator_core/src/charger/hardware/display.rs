// `FakeDisplay` is complete and fully exercised by the tests below, but nothing outside this
// file constructs one yet: registering it via `ChargePointBuilder::display_messages` is H6b
// (`docs/hardware-roadmap.md`), a separate task owned by wiring in `charger/connect.rs` and
// `charger/mod.rs`'s re-export - files this task (H6a) deliberately does not touch. Until that
// lands, `rustc` sees everything here as unreachable from the crate's public surface even though
// the test module below constructs and calls every bit of it. Drop this once H6b registers
// `FakeDisplay` somewhere reachable.
#![allow(dead_code)]

use std::sync::Mutex;

use ocpp_charge_point::hardware::Display;
use ocpp_charge_point::state::{DisplayedMessage, MessageFormat};

/// The message formats this simulator claims to render (OCPP `MessageFormatEnum`).
///
/// Deliberately restrictive: `Ascii` and `Utf8` are plain text, so a recording fake can honestly
/// claim them, but `Html`, `Uri`, and `QrCode` are left out on purpose rather than added for
/// completeness. A simulator that claims to render every format never exercises the handler's
/// `NotSupportedMessageFormat` rejection path (see [`Display::supported_formats`]) - and that
/// path is exactly what someone developing a CSMS needs to be able to test against. This is a
/// choice, not a limitation of the fake.
const SUPPORTED_FORMATS: [MessageFormat; 2] = [MessageFormat::Ascii, MessageFormat::Utf8];

/// What [`FakeDisplay`] currently has shown, distinguishing "nothing has ever been shown" (the
/// initial state, before any `show` call) from "explicitly cleared" (`show(None)` was called).
/// Both render as a blank screen to a frontend - see [`FakeDisplay::current_message`] - but only
/// the latter means a handler actually touched the display, which is worth being able to assert
/// on separately; see [`FakeDisplay::has_shown_anything`].
#[derive(Debug, Clone, Default)]
enum Shown {
    #[default]
    Never,
    Cleared,
    Message(DisplayedMessage),
}

/// A simulated charger display: renders nothing, but remembers the message it was last told to
/// show so a frontend can render the charger's screen and tests can assert on it. Mirrors
/// [`super::connector::FakeConnector`]'s shape - no real hardware behind it, state tracked in
/// memory, every action reported via `tracing` so it flows into whatever is bridging tracing
/// output (e.g. the TUI's log panel) the same way a real driver's output would.
#[derive(Debug, Default)]
pub struct FakeDisplay {
    shown: Mutex<Shown>,
}

impl FakeDisplay {
    pub fn new() -> Self {
        Self::default()
    }

    /// The message currently shown on the simulated screen, or `None` if the screen is blank -
    /// either because nothing has ever been shown, or because it was explicitly cleared via
    /// `show(None)`. Use [`Self::has_shown_anything`] to tell those two apart.
    pub fn current_message(&self) -> Option<DisplayedMessage> {
        match &*self.shown.lock().expect("lock poisoned") {
            Shown::Never | Shown::Cleared => None,
            Shown::Message(message) => Some(message.clone()),
        }
    }

    /// Whether `show` has been called at all yet. `false` only in the initial state before any
    /// call; becomes `true` on the very first call, whether it showed a message or cleared the
    /// screen with `show(None)`.
    pub fn has_shown_anything(&self) -> bool {
        !matches!(&*self.shown.lock().expect("lock poisoned"), Shown::Never)
    }
}

#[async_trait::async_trait]
impl Display for FakeDisplay {
    type Error = core::convert::Infallible;

    async fn show(&self, message: Option<&DisplayedMessage>) -> Result<(), Self::Error> {
        match message {
            Some(message) => {
                tracing::info!(
                    id = message.id.0,
                    format = message.message.format.name(),
                    content = %message.message.content,
                    "display message shown"
                );
                *self.shown.lock().expect("lock poisoned") = Shown::Message(message.clone());
            }
            None => {
                tracing::info!("display cleared");
                *self.shown.lock().expect("lock poisoned") = Shown::Cleared;
            }
        }
        Ok(())
    }

    fn supported_formats(&self) -> &[MessageFormat] {
        &SUPPORTED_FORMATS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocpp_charge_point::state::{DisplayMessageId, MessageContent, MessagePriority};

    fn message() -> DisplayedMessage {
        DisplayedMessage {
            id: DisplayMessageId(1),
            priority: MessagePriority::NormalCycle,
            state: None,
            message: MessageContent {
                content: "hello".into(),
                format: MessageFormat::Ascii,
                language: None,
            },
            transaction_id: None,
        }
    }

    #[test]
    fn a_fresh_display_has_shown_nothing() {
        let display = FakeDisplay::new();

        assert_eq!(display.current_message(), None);
        assert!(!display.has_shown_anything());
    }

    #[tokio::test]
    async fn showing_a_message_records_it_and_it_reads_back() {
        let display = FakeDisplay::new();

        display.show(Some(&message())).await.unwrap();

        assert_eq!(display.current_message(), Some(message()));
        assert!(display.has_shown_anything());
    }

    #[tokio::test]
    async fn showing_none_clears_a_previously_shown_message() {
        let display = FakeDisplay::new();
        display.show(Some(&message())).await.unwrap();

        display.show(None).await.unwrap();

        assert_eq!(display.current_message(), None);
    }

    #[tokio::test]
    async fn showing_none_is_distinguishable_from_never_having_shown_anything() {
        let display = FakeDisplay::new();
        assert!(!display.has_shown_anything());

        display.show(None).await.unwrap();

        // Both read back as "nothing to render"...
        assert_eq!(display.current_message(), None);
        // ...but only one of them means a handler actually called `show`.
        assert!(display.has_shown_anything());
    }

    #[tokio::test]
    async fn showing_a_second_message_replaces_the_first() {
        let display = FakeDisplay::new();
        display.show(Some(&message())).await.unwrap();

        let mut second = message();
        second.id = DisplayMessageId(2);
        second.message.content = "goodbye".into();
        display.show(Some(&second)).await.unwrap();

        assert_eq!(display.current_message(), Some(second));
    }

    #[test]
    fn supported_formats_includes_plain_text_but_excludes_rich_formats() {
        let display = FakeDisplay::new();
        let formats = display.supported_formats();

        assert!(formats.contains(&MessageFormat::Ascii));
        assert!(formats.contains(&MessageFormat::Utf8));
        assert!(!formats.contains(&MessageFormat::Html));
        assert!(!formats.contains(&MessageFormat::Uri));
        assert!(!formats.contains(&MessageFormat::QrCode));
    }

    /// The builder registration this drives (H6b) will require `Display + Send + Sync +
    /// 'static` - a compile-time check that `FakeDisplay` satisfies those bounds now, so a
    /// regression here is a build failure rather than a surprise when H6b lands.
    #[allow(dead_code)]
    fn assert_satisfies_the_builder_bounds<T: Display + Send + Sync + 'static>() {}

    #[allow(dead_code)]
    fn fake_display_satisfies_the_builder_bounds() {
        assert_satisfies_the_builder_bounds::<FakeDisplay>();
    }
}
