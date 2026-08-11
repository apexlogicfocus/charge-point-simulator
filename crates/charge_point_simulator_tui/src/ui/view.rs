//! View models: plain data describing *what* to draw, built from [`App`] in one place so the
//! `render` functions under `src/ui/` consume only that instead of reaching into `App` (and, for
//! the dashboard, five levels beneath `App::charger_state`) directly.
//!
//! This is deliberately not a parallel copy of the whole state model - each view borrows the
//! [`ChargerState`] it needs wholesale rather than re-encoding its fields, and only pulls in the
//! handful of other `App` fields the screen it serves actually renders (focus, logs, the status
//! bar message, whether a connection attempt is in flight). That keeps each `from_app` a visible,
//! one-to-one mapping - "obvious where a value comes from" - without inventing a second copy of
//! `ChargerState`/`EvseState`/`ConnectorState` to keep in sync with the first.
//!
//! A secondary benefit: every view here can be constructed by hand in a unit test, so
//! `dashboard::render`/`palette::render_*` can be exercised without building a whole `App`
//! (picker state, connection-setup fields, etc.) that has nothing to do with what they draw.

use crate::actions::PaletteEntry;
use crate::app::{App, CampaignProgress, FocusedConnector, StatusSeverity};
use crate::logs::LogBuffer;
use charge_point_simulator_core::charger::{ChargerState, Command};

/// Everything [`crate::ui::dashboard::render`] needs, gathered from `App` in one place.
pub struct DashboardView<'a> {
    /// `None` before a charger has finished booting into state - see `App::charger_state`.
    pub charger: Option<&'a ChargerState>,
    /// Which connector is focused - see [`FocusedConnector`]. Kept meaningful even when
    /// `charger` is `None` or the indices don't (yet) point at a real connector; renderers are
    /// expected to clamp/`.get()` defensively, the same way `App`'s own navigation does.
    pub focused: FocusedConnector,
    /// Whether a background CSMS connection attempt is in flight (`App::connect_result_receiver`
    /// is `Some`), which swaps the header's status glyph for an animated spinner.
    pub connecting: bool,
    pub logs: &'a LogBuffer,
    /// The in-progress log filter text when `/` has opened the filter prompt, which takes over
    /// the command bar while it's open. `None` when the prompt is closed - an *applied* filter
    /// with the prompt closed is read from `logs` instead.
    pub log_filter_input: Option<&'a str>,
    pub status_message: Option<(StatusSeverity, &'a str)>,
    /// Charger-wide firmware/file-transfer activity - see `App::campaigns`. Copied rather than
    /// borrowed: it is a handful of `Copy` fields, unlike the `ChargerState` above.
    pub campaigns: CampaignProgress,
}

impl<'a> DashboardView<'a> {
    pub fn from_app(app: &'a App) -> Self {
        Self {
            charger: app.charger_state.as_ref(),
            focused: app.focused,
            connecting: app.connect_result_receiver.is_some(),
            logs: &app.logs,
            log_filter_input: app.log_filter_open.then(|| app.log_filter_field.value()),
            status_message: app
                .status_message
                .as_ref()
                .map(|toast| (toast.severity, toast.message.as_str())),
            campaigns: app.campaigns,
        }
    }
}

/// Everything [`crate::ui::palette::render_command_palette`] needs. Notably `commands` is
/// already resolved (via `App::palette_commands`, which itself narrows
/// `App::available_commands` - the focused connector's eligible commands - by the filter text):
/// computing that list is state logic that belongs in `App`, not in a render function, so it's
/// done once here rather than the render function calling back into `app` mid-draw.
pub struct PaletteView<'a> {
    pub filter: &'a str,
    pub cursor: usize,
    pub commands: Vec<PaletteEntry>,
    pub selected: usize,
    /// What the *selected* entry would act on, e.g. `"EVSE 1 / C1"` for a connector-scoped command
    /// or the charger's own id for a charger-wide one (a display message, a firmware update), so the
    /// consequence of pressing Enter is visible before it's pressed. `None` when there is nothing
    /// selected, or when a connector-scoped entry's focus doesn't resolve to a real connector.
    pub target: Option<String>,
    /// Whether the selected entry is connector-scoped, which is what decides whether `Tab`
    /// retargeting means anything for it - see [`crate::actions::PaletteEntry::is_connector_scoped`].
    pub target_is_connector: bool,
}

impl<'a> PaletteView<'a> {
    pub fn from_app(app: &'a App) -> Self {
        let commands = app.palette_commands();
        // The target follows the selection rather than being fixed, because the two kinds of entry
        // act on different things: a charger-wide entry showing "EVSE 1 / C1" would name a target it
        // is not going to touch.
        let selected = commands.get(app.command_palette_selected);
        let target_is_connector = selected.is_none_or(|entry| entry.is_connector_scoped());
        let target = if target_is_connector {
            app.focused_connector_label()
        } else {
            app.charger_state
                .as_ref()
                .map(|state| state.config.id.clone())
        };
        Self {
            target,
            target_is_connector,
            filter: app.command_palette_filter.value(),
            cursor: app.command_palette_filter.cursor(),
            commands,
            selected: app.command_palette_selected,
        }
    }
}

/// Everything [`crate::ui::palette::render_parameter_prompt`] needs.
pub struct ParameterPromptView<'a> {
    pub command: Command,
    pub value: &'a str,
    pub cursor: usize,
    /// Why the current value was rejected, rendered under the field - see
    /// `App::parameter_error`.
    pub error: Option<&'static str>,
    /// The connector the command will act on, mirrored from the palette so the prompt doesn't
    /// lose the context the palette had.
    pub target: Option<String>,
}

impl<'a> ParameterPromptView<'a> {
    pub fn from_app(app: &'a App, command: Command) -> Self {
        Self {
            command,
            value: app.parameter_field.value(),
            cursor: app.parameter_field.cursor(),
            error: app.parameter_error,
            target: app.focused_connector_label(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::Screen;
    use crate::text_field::TextField;
    use charge_point_simulator_core::charger::{ChargerConfig, EvseConfig, OcppVersion};

    fn app_with_charger() -> App {
        let mut app = App::new(vec![]);
        app.screen = Screen::Dashboard;
        app.charger_state = Some(ChargerState::from_config(ChargerConfig {
            id: "CP001".into(),
            ocpp_version: OcppVersion::V16J,
            evses: vec![EvseConfig {
                id: 1,
                connectors: 2,
            }],
            has_display: false,
            capabilities: Default::default(),
        }));
        app
    }

    #[test]
    fn dashboard_view_borrows_the_chargers_state_as_is() {
        let app = app_with_charger();
        let view = DashboardView::from_app(&app);

        assert_eq!(view.charger.unwrap().config.id, "CP001");
        assert_eq!(
            view.charger.unwrap() as *const ChargerState,
            app.charger_state.as_ref().unwrap() as *const _
        );
    }

    #[test]
    fn dashboard_view_is_none_without_a_selected_charger() {
        let app = App::new(vec![]);
        let view = DashboardView::from_app(&app);
        assert!(view.charger.is_none());
    }

    #[test]
    fn dashboard_view_carries_the_focused_connector() {
        let mut app = app_with_charger();
        app.focused = FocusedConnector {
            evse: 0,
            connector: 1,
        };

        let view = DashboardView::from_app(&app);

        assert_eq!(
            view.focused,
            FocusedConnector {
                evse: 0,
                connector: 1
            }
        );
    }

    #[test]
    fn dashboard_view_reflects_a_pending_connection_attempt() {
        let app = app_with_charger();
        assert!(!DashboardView::from_app(&app).connecting);

        let mut app = app;
        let (_sender, receiver) = tokio::sync::oneshot::channel();
        app.connect_result_receiver = Some(receiver);
        assert!(DashboardView::from_app(&app).connecting);
    }

    #[test]
    fn dashboard_view_carries_the_status_message() {
        let mut app = app_with_charger();
        app.set_status(StatusSeverity::Ok, "✓ Plug in vehicle".to_string());

        let view = DashboardView::from_app(&app);

        assert_eq!(
            view.status_message,
            Some((StatusSeverity::Ok, "✓ Plug in vehicle"))
        );
    }

    #[test]
    fn dashboard_view_carries_the_log_buffer() {
        let mut app = app_with_charger();
        app.logs.push("hello");

        let view = DashboardView::from_app(&app);

        assert_eq!(
            view.logs
                .visible_lines(10)
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>(),
            vec!["hello"]
        );
    }

    #[test]
    fn palette_view_resolves_the_filtered_and_focus_narrowed_command_list() {
        let mut app = app_with_charger();
        app.command_palette_filter = TextField::new("fault");
        app.command_palette_selected = 0;

        let view = PaletteView::from_app(&app);

        let labels: Vec<&str> = view.commands.iter().map(|c| c.label()).collect();
        assert_eq!(labels, vec!["Report fault"]);
        assert_eq!(view.filter, "fault");
        assert_eq!(view.cursor, "fault".chars().count());
    }

    #[test]
    fn parameter_prompt_view_carries_the_command_and_field() {
        let mut app = app_with_charger();
        app.parameter_field = TextField::new("MY-EV-1");

        let view = ParameterPromptView::from_app(&app, Command::PlugInVehicle);

        assert_eq!(view.command, Command::PlugInVehicle);
        assert_eq!(view.value, "MY-EV-1");
        assert_eq!(view.cursor, "MY-EV-1".chars().count());
    }
}
