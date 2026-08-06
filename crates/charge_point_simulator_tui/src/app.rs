use crate::logs::{LogBuffer, LogEntry};
use crate::screen::Screen;
use crate::text_field::TextField;
use charge_point_simulator_core::charger::{
    ChargePointEvent, ChargePointState, ChargerEntry, ChargerState, Command, CommandParameter,
    ConnectionProfile, ConnectionStore, OcppVersion, SecurityProfile, SimulationMode, apply_ocpp_state,
    build_ocpp_event_for_connector, connect_charger, meter_sample_events,
};
use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{DefaultTerminal, Frame};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

/// How long to wait for a keyboard event before redrawing anyway, so
/// externally-sourced log lines (tracing output from the simulator, and
/// eventually `ocpp-charge-point`) show up promptly even with no input.
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How often to forward simulated meter readings to a connected CSMS as real
/// `MeterValueSampled` events. Real deployments typically use ~60s+ (configurable via
/// `SetVariables`); shorter here so a demo session actually sees TransactionEvent traffic
/// without waiting a minute for it.
const METER_VALUE_INTERVAL: Duration = Duration::from_secs(5);

/// How a status message in the command bar should read: [`theme::ok`] or [`theme::error`].
/// Carried alongside the message text itself rather than left for the renderer to infer from
/// the message's content (the previous approach parsed a `✗` prefix out of the string, which
/// broke silently for any message that didn't happen to start with it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusSeverity {
    Ok,
    Error,
}

/// How long a status message stays in the command bar before expiring. Until Phase 5 these
/// persisted forever, which permanently hid the keybinding hint line that shares the bar -
/// the very line telling a new user how to do anything else.
const STATUS_MESSAGE_TTL: Duration = Duration::from_secs(4);

/// A transient message in the command bar, carrying when it was shown so it can expire (see
/// [`STATUS_MESSAGE_TTL`]). Expiry is evaluated in `App::tick_metrics_with`, which is handed
/// `now` by its caller rather than reading the clock itself - the same injectable-clock
/// pattern the metrics tick already uses, so tests drive expiry with a chosen instant instead
/// of sleeping.
#[derive(Debug, Clone)]
pub struct Toast {
    pub severity: StatusSeverity,
    pub message: String,
    pub shown_at: Instant,
}

/// Identifies one connector within the currently loaded charger: which EVSE, and which
/// connector within it (both 0-indexed into [`ChargerState::evses`]/`EvseState::connectors`).
///
/// Replaces the old `focused_evse: usize` - commands (see [`App::available_commands`] and
/// [`App::apply_command`]) now dispatch against exactly the connector this points at, not just
/// "the first eligible connector on the focused EVSE." `Default` points at the first connector
/// of the first EVSE, matching a freshly selected charger.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FocusedConnector {
    pub evse: usize,
    pub connector: usize,
}

#[derive(Debug, Default)]
pub struct App {
    pub screen: Screen,
    pub chargers: Vec<ChargerEntry>,
    /// Indexes into [`Self::filtered_chargers`], not `chargers` directly.
    pub selected_charger: usize,
    pub picker_filter: TextField,
    pub charger_state: Option<ChargerState>,
    pub focused: FocusedConnector,
    pub logs: LogBuffer,
    pub log_receiver: Option<UnboundedReceiver<LogEntry>>,
    /// Whether `/` has opened the log filter prompt. While open, typed characters narrow the
    /// log pane live rather than reaching the dashboard's other bindings.
    pub log_filter_open: bool,
    pub log_filter_field: TextField,
    pub command_palette_open: bool,
    pub command_palette_selected: usize,
    pub command_palette_filter: TextField,
    pub parameter_prompt: Option<Command>,
    pub parameter_field: TextField,
    /// The last value accepted for each parameter, prefilled the next time that same parameter
    /// is prompted for. Keyed by [`CommandParameter`] rather than by [`Command`] so commands
    /// sharing a parameter share its history.
    pub parameter_history: HashMap<CommandParameter, String>,
    /// Why the current parameter value was rejected, shown inline under the prompt. Cleared as
    /// soon as the value changes, so a stale complaint never outlives what it was about.
    pub parameter_error: Option<&'static str>,
    pub help_open: bool,
    /// How many lines the help overlay is scrolled down by, for terminals too short to show
    /// the whole keybinding table at once.
    pub help_scroll: usize,
    pub quit_confirm_open: bool,
    pub status_message: Option<Toast>,
    pub connection_store: ConnectionStore,
    pub connection_store_path: Option<PathBuf>,
    pub connection_csms_url: TextField,
    pub connection_ocpp_identity: TextField,
    pub connection_password: TextField,
    pub connection_focused_field: usize,
    pub connect_result_receiver: Option<oneshot::Receiver<Result<(), String>>>,
    /// Live protocol state snapshots forwarded from a connected OCPP 2.1 charger's
    /// background connection thread, drained each frame by [`Self::drain_ocpp_state_receiver`].
    pub ocpp_state_receiver: Option<UnboundedReceiver<ChargePointState>>,
    /// Where dispatched commands go instead of the local simulation, once connected to a
    /// real CSMS (see [`Self::apply_command`]).
    pub ocpp_event_sender: Option<UnboundedSender<ChargePointEvent>>,
    /// The most recent snapshot from `ocpp_state_receiver`, used to decide what event a
    /// dispatched command maps to (see [`charge_point_simulator_core::charger::build_ocpp_event`]).
    pub live_ocpp_state: Option<ChargePointState>,
    /// When [`Self::tick_metrics`] last ran, so it can compute real elapsed time between
    /// frames rather than assuming a fixed interval (the main loop's actual cadence varies
    /// with input activity).
    pub last_metrics_tick: Option<Instant>,
    /// When a `MeterValueSampled` event was last sent to a connected CSMS, throttling
    /// against [`METER_VALUE_INTERVAL`].
    pub last_meter_value_sent: Option<Instant>,
    pub exit: bool,
}

/// Decides what `ChargerState::mode` should become from the (possibly blank, possibly
/// whitespace-padded) CSMS URL field on the connection setup screen. A blank URL means "no
/// CSMS, run locally"; anything else means a real connection attempt is about to be made.
///
/// Kept as a free function, separate from `App::confirm_connection_setup`, specifically so it
/// can be unit tested as a plain, synchronous decision - without going anywhere near the
/// background thread `confirm_connection_setup` spawns to actually perform the connection.
/// Validates a parameter value, returning the reason it's unacceptable or `None` if it's fine.
///
/// Kept a free function, like [`resolve_simulation_mode`], so the rule can be tested as a plain
/// decision without standing up an `App` and a prompt around it. Every parameter the app has
/// today (vehicle id, RFID tag, fault code, display message) is free text whose only real
/// requirement is being present, so this is deliberately one rule rather than a per-parameter
/// table - add that when a parameter actually needs a different rule.
fn validate_parameter(value: &str) -> Option<&'static str> {
    if value.trim().is_empty() {
        return Some("cannot be blank");
    }
    None
}

fn resolve_simulation_mode(csms_url: &str) -> SimulationMode {
    if csms_url.trim().is_empty() {
        SimulationMode::Local
    } else {
        SimulationMode::LiveCsms {
            url: csms_url.to_string(),
        }
    }
}

impl App {
    pub fn new(chargers: Vec<ChargerEntry>) -> Self {
        Self {
            chargers,
            ..Default::default()
        }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            self.drain_log_receiver();
            self.drain_ocpp_state_receiver();
            self.poll_connect_result();
            self.tick_metrics();
            terminal.draw(|frame| self.draw(frame))?;
            if self.exit {
                break;
            }
            if event::poll(INPUT_POLL_INTERVAL)? {
                self.handle_events()?;
            }
        }
        Ok(())
    }

    pub(crate) fn draw(&self, frame: &mut Frame) {
        crate::ui::draw(frame, self);
    }

    /// Commands eligible to run against the currently focused connector (see
    /// [`FocusedConnector`]) - not merely the focused EVSE's first eligible connector, so the
    /// palette never offers something that would silently act on a different connector than
    /// the one on screen.
    pub(crate) fn available_commands(&self) -> Vec<Command> {
        let Some(state) = &self.charger_state else {
            return Vec::new();
        };
        let connector = state
            .evses
            .get(self.focused.evse)
            .and_then(|evse| evse.connectors.get(self.focused.connector));
        Command::ALL
            .into_iter()
            .filter(|command| {
                if command.is_display_command() {
                    command.is_available_for_charger(state)
                } else {
                    connector.is_some_and(|connector| command.is_available_for_connector(connector))
                }
            })
            .collect()
    }

    /// [`available_commands`](Self::available_commands) further narrowed by the command
    /// palette's filter text, matched as a fuzzy subsequence (see [`crate::fuzzy::score`])
    /// rather than a plain substring: `"pv"` finds "Plug in vehicle".
    ///
    /// Results are ordered best-match first, with ties broken by the order in `Command::ALL`
    /// so a given filter always produces the same list - `sort_by_key` is stable, which is
    /// what makes that guarantee hold.
    pub(crate) fn palette_commands(&self) -> Vec<Command> {
        let filter = self.command_palette_filter.value();
        let mut scored: Vec<(Command, u32)> = self
            .available_commands()
            .into_iter()
            .filter_map(|command| {
                crate::fuzzy::score(command.label(), filter).map(|score| (command, score))
            })
            .collect();
        scored.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
        scored.into_iter().map(|(command, _)| command).collect()
    }

    fn handle_events(&mut self) -> Result<()> {
        match event::read()? {
            // it's important to check that the event is a key press event as
            // crossterm also emits key release and repeat events on Windows.
            Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                self.handle_key_event(key_event)
            }
            _ => {}
        };
        Ok(())
    }

    /// `pub(crate)` so snapshot scenarios can drive the app through real key presses rather
    /// than hand-setting the state those presses produce.
    pub(crate) fn handle_key_event(&mut self, key_event: KeyEvent) {
        if self.quit_confirm_open {
            self.handle_quit_confirm_key(key_event);
            return;
        }
        if self.parameter_prompt.is_some() {
            self.handle_parameter_prompt_key(key_event);
            return;
        }
        if self.command_palette_open {
            self.handle_command_palette_key(key_event);
            return;
        }
        if self.help_open {
            self.handle_help_key(key_event);
            return;
        }
        // Before the global 'q'/'?' shortcuts: both are typeable into a log filter.
        if self.log_filter_open {
            self.handle_log_filter_key(key_event);
            return;
        }

        // On the connection setup screen 'q'/'?' need to be typeable (URLs and passwords can
        // contain either), so the global shortcuts don't apply there. On the picker, 'q' also
        // needs to be typeable into the charger filter (Esc still quits when the filter's
        // empty - see `handle_pick_charger_key`); '?' stays global there since charger ids
        // never contain it and losing Help on the very first screen would be worse.
        if self.screen != Screen::ConnectionSetup {
            match key_event.code {
                KeyCode::Char('q') if self.screen != Screen::PickCharger => {
                    self.quit_confirm_open = true;
                    return;
                }
                KeyCode::Char('?') => {
                    self.help_open = true;
                    return;
                }
                _ => {}
            }
        }

        match self.screen {
            Screen::PickCharger => self.handle_pick_charger_key(key_event),
            Screen::ConnectionSetup => self.handle_connection_setup_key(key_event),
            Screen::Dashboard => self.handle_dashboard_key(key_event),
        }
    }

    fn handle_quit_confirm_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Char('y') | KeyCode::Enter => self.exit = true,
            KeyCode::Char('n') | KeyCode::Esc => self.quit_confirm_open = false,
            _ => {}
        }
    }

    /// Opens the log filter prompt, seeded with whatever filter is currently applied so
    /// refining one doesn't mean retyping it.
    fn open_log_filter(&mut self) {
        self.log_filter_field = TextField::default();
        for c in self.logs.filter().unwrap_or_default().chars() {
            self.log_filter_field.insert_char(c);
        }
        self.log_filter_open = true;
    }

    /// The filter applies as it is typed - the log pane narrows live rather than only on
    /// Enter, so a filter that matches nothing is visibly wrong before it's committed.
    fn handle_log_filter_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            // Esc abandons the prompt *and* the filter, matching the roadmap's "`/` to open
            // the filter and `Esc` to clear it".
            KeyCode::Esc => {
                self.log_filter_open = false;
                self.log_filter_field = TextField::default();
                self.logs.clear_filter();
            }
            KeyCode::Enter => self.log_filter_open = false,
            KeyCode::Backspace => {
                self.log_filter_field.backspace();
                self.apply_log_filter();
            }
            KeyCode::Delete => {
                self.log_filter_field.delete();
                self.apply_log_filter();
            }
            KeyCode::Left => self.log_filter_field.move_left(),
            KeyCode::Right => self.log_filter_field.move_right(),
            KeyCode::Home => self.log_filter_field.move_home(),
            KeyCode::End => self.log_filter_field.move_end(),
            KeyCode::Char(c) => {
                self.log_filter_field.insert_char(c);
                self.apply_log_filter();
            }
            _ => {}
        }
    }

    fn apply_log_filter(&mut self) {
        let value = self.log_filter_field.value().to_string();
        if value.is_empty() {
            self.logs.clear_filter();
        } else {
            self.logs.set_filter(value);
        }
    }

    fn handle_help_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Esc | KeyCode::Char('?') => {
                self.help_open = false;
                self.help_scroll = 0;
            }
            KeyCode::Down | KeyCode::PageDown => self.help_scroll += 1,
            KeyCode::Up | KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(1),
            _ => {}
        }
    }

    fn handle_pick_charger_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Down => self.select_next_charger(),
            KeyCode::Up => self.select_previous_charger(),
            KeyCode::Enter => self.confirm_charger_selection(),
            // Esc clears an active filter first (so it doesn't double as "quit" while
            // narrowing the list); with no filter typed, it opens the quit confirmation.
            KeyCode::Esc => {
                if self.picker_filter.value().is_empty() {
                    self.quit_confirm_open = true;
                } else {
                    self.picker_filter = TextField::default();
                    self.selected_charger = 0;
                }
            }
            KeyCode::Backspace => {
                self.picker_filter.backspace();
                self.selected_charger = 0;
            }
            KeyCode::Char(c) => {
                self.picker_filter.insert_char(c);
                self.selected_charger = 0;
            }
            _ => {}
        }
    }

    fn handle_connection_setup_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Esc => self.return_to_picker(),
            KeyCode::Tab | KeyCode::Down => self.connection_focus_next(),
            KeyCode::BackTab | KeyCode::Up => self.connection_focus_previous(),
            KeyCode::Enter => self.confirm_connection_setup(),
            KeyCode::Backspace => self.focused_connection_field_mut().backspace(),
            KeyCode::Delete => self.focused_connection_field_mut().delete(),
            KeyCode::Left => self.focused_connection_field_mut().move_left(),
            KeyCode::Right => self.focused_connection_field_mut().move_right(),
            KeyCode::Home => self.focused_connection_field_mut().move_home(),
            KeyCode::End => self.focused_connection_field_mut().move_end(),
            KeyCode::Char(c) => self.focused_connection_field_mut().insert_char(c),
            _ => {}
        }
    }

    fn handle_dashboard_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            // Esc clears an active log filter before it means "leave the dashboard", the same
            // way it clears the picker's filter before it means "quit".
            KeyCode::Esc if self.logs.filter().is_some() => self.logs.clear_filter(),
            KeyCode::Esc => self.return_to_picker(),
            KeyCode::PageUp => self.logs.scroll_up(),
            KeyCode::PageDown => self.logs.scroll_down(),
            KeyCode::Char('/') => self.open_log_filter(),
            KeyCode::Char('g') => self.logs.scroll_to_top(),
            KeyCode::Char('G') => self.logs.scroll_to_bottom(),
            KeyCode::Char('l') => self.logs.cycle_level_threshold(),
            KeyCode::Down => self.select_next_connector(),
            KeyCode::Up => self.select_previous_connector(),
            KeyCode::Right | KeyCode::Tab => self.select_next_evse(),
            KeyCode::Left | KeyCode::BackTab => self.select_previous_evse(),
            // Ctrl+K is the modern convention for a command palette; 'c' stays as an alias
            // rather than being removed, since it's what this app has always used.
            KeyCode::Char('k') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_command_palette()
            }
            KeyCode::Char('c') => self.open_command_palette(),
            _ => {}
        }
    }

    fn handle_command_palette_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Esc => self.close_command_palette(),
            KeyCode::Down => self.select_next_command(),
            KeyCode::Up => self.select_previous_command(),
            // Retarget without leaving the palette: the target is shown on every row, so
            // noticing it's wrong shouldn't cost an Esc and a re-open. Tab moves the target,
            // not the selection, because the palette's list already moves with ↑/↓.
            KeyCode::Tab => self.retarget_palette(true),
            KeyCode::BackTab => self.retarget_palette(false),
            KeyCode::Enter => self.dispatch_selected_command(),
            KeyCode::Backspace => {
                self.command_palette_filter.backspace();
                self.command_palette_selected = 0;
            }
            KeyCode::Char(c) => {
                self.command_palette_filter.insert_char(c);
                self.command_palette_selected = 0;
            }
            _ => {}
        }
    }

    fn handle_parameter_prompt_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Esc => self.cancel_parameter_prompt(),
            KeyCode::Enter => self.submit_parameter_prompt(),
            // Editing the value clears any complaint about the previous one.
            KeyCode::Backspace => {
                self.parameter_field.backspace();
                self.parameter_error = None;
            }
            KeyCode::Delete => {
                self.parameter_field.delete();
                self.parameter_error = None;
            }
            KeyCode::Left => self.parameter_field.move_left(),
            KeyCode::Right => self.parameter_field.move_right(),
            KeyCode::Home => self.parameter_field.move_home(),
            KeyCode::End => self.parameter_field.move_end(),
            KeyCode::Char(c) => {
                self.parameter_field.insert_char(c);
                self.parameter_error = None;
            }
            _ => {}
        }
    }

    /// [`Self::chargers`] narrowed by the picker's filter text (case-insensitive substring
    /// match on the charger id).
    pub(crate) fn filtered_chargers(&self) -> Vec<&ChargerEntry> {
        let filter = self.picker_filter.value().to_lowercase();
        self.chargers
            .iter()
            .filter(|entry| entry.config.id.to_lowercase().contains(&filter))
            .collect()
    }

    fn select_next_charger(&mut self) {
        let count = self.filtered_chargers().len();
        if count == 0 {
            return;
        }
        if self.selected_charger + 1 < count {
            self.selected_charger += 1;
        }
    }

    fn select_previous_charger(&mut self) {
        self.selected_charger = self.selected_charger.saturating_sub(1);
    }

    /// Every valid `(evse_index, connector_index)` pair in `state`, in display order - EVSEs
    /// top to bottom, each one's connectors beneath it. This is the flattened list
    /// [`Self::select_next_connector`]/[`Self::select_previous_connector`] walk, so `↑`/`↓`
    /// treat the whole tree as one list and flow across EVSE boundaries instead of stopping at
    /// them the way `Tab`/`←`/`→` do.
    fn connector_positions(state: &ChargerState) -> Vec<(usize, usize)> {
        state
            .evses
            .iter()
            .enumerate()
            .flat_map(|(evse_index, evse)| {
                (0..evse.connectors.len()).map(move |connector_index| (evse_index, connector_index))
            })
            .collect()
    }

    /// `↓`: moves focus to the next connector in display order, flowing from the last
    /// connector of one EVSE into the first connector of the next rather than stopping at the
    /// EVSE boundary - see [`Self::connector_positions`]. Clamps at the last connector in the
    /// whole tree; does nothing (rather than panicking) for a charger with no EVSEs, or whose
    /// EVSEs have no connectors.
    fn select_next_connector(&mut self) {
        let Some(state) = &self.charger_state else {
            return;
        };
        let positions = Self::connector_positions(state);
        let Some(current) = positions
            .iter()
            .position(|&pos| pos == (self.focused.evse, self.focused.connector))
        else {
            // Focus doesn't point at a real connector (e.g. an EVSE with none) - land on the
            // first one that exists rather than doing nothing.
            if let Some(&(evse, connector)) = positions.first() {
                self.focused = FocusedConnector { evse, connector };
            }
            return;
        };
        if let Some(&(evse, connector)) = positions.get(current + 1) {
            self.focused = FocusedConnector { evse, connector };
        }
    }

    /// `↑`: the mirror image of [`Self::select_next_connector`].
    fn select_previous_connector(&mut self) {
        let Some(state) = &self.charger_state else {
            return;
        };
        let positions = Self::connector_positions(state);
        let Some(current) = positions
            .iter()
            .position(|&pos| pos == (self.focused.evse, self.focused.connector))
        else {
            if let Some(&(evse, connector)) = positions.first() {
                self.focused = FocusedConnector { evse, connector };
            }
            return;
        };
        if current > 0
            && let Some(&(evse, connector)) = positions.get(current - 1)
        {
            self.focused = FocusedConnector { evse, connector };
        }
    }

    /// `→`/`Tab`: jumps focus to the next EVSE, always landing on its first connector - unlike
    /// `↑`/`↓` this treats the tree as EVSE-sized steps, matching the pre-Phase-3b behavior
    /// this key retains ("keep working as they do today").
    fn select_next_evse(&mut self) {
        let Some(state) = &self.charger_state else {
            return;
        };
        if state.evses.is_empty() {
            return;
        }
        if self.focused.evse + 1 < state.evses.len() {
            self.focused.evse += 1;
        }
        self.focused.connector = 0;
    }

    /// `←`/`BackTab`: the mirror image of [`Self::select_next_evse`].
    fn select_previous_evse(&mut self) {
        let Some(state) = &self.charger_state else {
            return;
        };
        if state.evses.is_empty() {
            return;
        }
        self.focused.evse = self.focused.evse.saturating_sub(1);
        self.focused.connector = 0;
    }

    fn open_command_palette(&mut self) {
        if self.charger_state.is_none() {
            return;
        }
        self.command_palette_selected = 0;
        self.command_palette_filter = TextField::default();
        self.command_palette_open = true;
    }

    fn close_command_palette(&mut self) {
        self.command_palette_open = false;
    }

    /// Moves the focused connector while the palette is open, so a command about to be
    /// dispatched can be pointed at a different connector in place.
    ///
    /// The selection is reset because the newly targeted connector may not be eligible for the
    /// same commands - keeping the index would silently land on a different command than the
    /// one the user was looking at.
    fn retarget_palette(&mut self, forward: bool) {
        if forward {
            self.select_next_connector();
        } else {
            self.select_previous_connector();
        }
        self.command_palette_selected = 0;
    }

    /// How the focused connector reads in the palette, e.g. `"EVSE 1 / C1"`. `None` when no
    /// charger is loaded or the focus doesn't point at a real connector.
    pub(crate) fn focused_connector_label(&self) -> Option<String> {
        let state = self.charger_state.as_ref()?;
        let evse = state.evses.get(self.focused.evse)?;
        let connector = evse.connectors.get(self.focused.connector)?;
        Some(format!("EVSE {} / C{}", evse.id, connector.id))
    }

    fn select_next_command(&mut self) {
        let count = self.palette_commands().len();
        if count == 0 {
            return;
        }
        if self.command_palette_selected + 1 < count {
            self.command_palette_selected += 1;
        }
    }

    fn select_previous_command(&mut self) {
        self.command_palette_selected = self.command_palette_selected.saturating_sub(1);
    }

    /// Dispatches the highlighted palette command, or, if it needs a
    /// parameter first (e.g. an RFID tag), closes the palette and opens a
    /// parameter prompt for it instead; the command is actually applied once
    /// that prompt is submitted (see [`submit_parameter_prompt`](Self::submit_parameter_prompt)).
    fn dispatch_selected_command(&mut self) {
        let commands = self.palette_commands();
        let command = commands.get(self.command_palette_selected).copied();
        self.close_command_palette();

        let Some(command) = command else {
            return;
        };

        if let Some(parameter) = command.parameter() {
            self.open_parameter_prompt(command, parameter);
            return;
        }

        self.apply_command(command, "");
    }

    /// Opens the prompt for `command`, prefilled with the last value accepted for the same
    /// parameter. Prefilling rather than merely suggesting means the common case - plugging the
    /// same test vehicle in again - is Enter, not retyping an id.
    fn open_parameter_prompt(&mut self, command: Command, parameter: CommandParameter) {
        let remembered = self.parameter_history.get(&parameter).cloned().unwrap_or_default();
        self.parameter_field = TextField::new(remembered);
        self.parameter_error = None;
        self.parameter_prompt = Some(command);
    }

    fn cancel_parameter_prompt(&mut self) {
        self.parameter_prompt = None;
        self.parameter_error = None;
    }

    /// Validates the typed value and, if it passes, applies the command and remembers the
    /// value for next time. A rejected value leaves the prompt open with an inline reason
    /// rather than closing and silently doing nothing.
    fn submit_parameter_prompt(&mut self) {
        let Some(command) = self.parameter_prompt else {
            return;
        };
        let input = self.parameter_field.value().trim().to_string();

        if let Some(error) = validate_parameter(input.as_str()) {
            self.parameter_error = Some(error);
            return;
        }

        if let Some(parameter) = command.parameter() {
            self.parameter_history.insert(parameter, input.clone());
        }
        self.parameter_prompt = None;
        self.parameter_error = None;
        self.apply_command(command, &input);
    }

    /// Dispatches `command` against the focused connector (see [`FocusedConnector`]) - never
    /// merely "the focused EVSE's first eligible connector," so a command run from the palette
    /// always acts on the connector actually shown as focused on screen. Once connected to a
    /// real CSMS (OCPP 2.1), this sends the matching `ChargePointEvent` to the live connection
    /// instead of mutating local state directly - the dashboard picks up the effect once the
    /// runtime reports it back via [`Self::drain_ocpp_state_receiver`].
    ///
    /// Display commands are the exception: `ocpp-charge-point` doesn't implement the
    /// DisplayMessage functional block yet, so `SetDisplayMessage`/`ClearDisplayMessage`
    /// always apply locally, live CSMS connection or not.
    fn apply_command(&mut self, command: Command, input: &str) {
        if command.is_display_command() {
            if let Some(state) = &mut self.charger_state
                && let Some(log_line) = command.apply_to_charger(state, input)
            {
                self.logs.push(log_line);
                self.set_status(StatusSeverity::Ok, format!("✓ {}", command.label()));
            }
            return;
        }

        if let (Some(ocpp_state), Some(sender)) = (&self.live_ocpp_state, &self.ocpp_event_sender) {
            match build_ocpp_event_for_connector(
                ocpp_state,
                self.focused.evse,
                self.focused.connector,
                command,
                input,
            ) {
                Some(event) => {
                    let _ = sender.send(event);
                    self.logs.push(format!("{} sent to CSMS", command.label()));
                    self.set_status(StatusSeverity::Ok, format!("→ {}", command.label()));
                }
                None => {
                    self.set_status(
                        StatusSeverity::Error,
                        format!("✗ {} not ready yet", command.label()),
                    );
                }
            }
            return;
        }

        let Some(state) = &mut self.charger_state else {
            return;
        };
        let Some(evse) = state.evses.get_mut(self.focused.evse) else {
            return;
        };
        if let Some(log_line) = command.apply_to(evse, self.focused.connector, input) {
            self.logs.push(log_line);
            self.set_status(StatusSeverity::Ok, format!("✓ {}", command.label()));
        }
    }

    fn confirm_charger_selection(&mut self) {
        if let Some(charger) = self
            .filtered_chargers()
            .get(self.selected_charger)
            .map(|entry| (*entry).clone())
        {
            self.logs = LogBuffer::default();
            self.logs.push(format!("{} booting", charger.config.id));
            self.charger_state = Some(ChargerState::from_config(charger.config.clone()));
            self.focused = FocusedConnector::default();
            self.status_message = None;
            // Reset so the first tick on the new charger sees zero elapsed time instead of
            // however long was spent idling on the picker.
            self.last_metrics_tick = None;
            self.last_meter_value_sent = None;

            if charger.config.ocpp_version == OcppVersion::V21 {
                self.enter_connection_setup(&charger.config.id);
                self.screen = Screen::ConnectionSetup;
            } else {
                self.screen = Screen::Dashboard;
            }
        }
    }

    /// Prefills the connection setup fields from the last-remembered profile for
    /// `charger_id`, or sensible blanks (identity defaulting to the charger id) if
    /// there isn't one yet.
    fn enter_connection_setup(&mut self, charger_id: &str) {
        match self.connection_store.get(charger_id).cloned() {
            Some(profile) => {
                let SecurityProfile::Basic { password } = profile.security;
                self.connection_csms_url = TextField::new(profile.csms_url);
                self.connection_ocpp_identity = TextField::new(profile.ocpp_identity);
                self.connection_password = TextField::new(password)
                    .with_max_bytes(SecurityProfile::MAX_BASIC_PASSWORD_BYTES);
            }
            None => {
                self.connection_csms_url = TextField::default();
                self.connection_ocpp_identity = TextField::new(charger_id);
                self.connection_password =
                    TextField::default().with_max_bytes(SecurityProfile::MAX_BASIC_PASSWORD_BYTES);
            }
        }
        self.connection_focused_field = 0;
    }

    fn connection_focus_next(&mut self) {
        if self.connection_focused_field + 1 < 3 {
            self.connection_focused_field += 1;
        }
    }

    fn connection_focus_previous(&mut self) {
        self.connection_focused_field = self.connection_focused_field.saturating_sub(1);
    }

    fn focused_connection_field_mut(&mut self) -> &mut TextField {
        match self.connection_focused_field {
            0 => &mut self.connection_csms_url,
            1 => &mut self.connection_ocpp_identity,
            _ => &mut self.connection_password,
        }
    }

    fn confirm_connection_setup(&mut self) {
        let Some(state) = &self.charger_state else {
            return;
        };
        let charger_id = state.config.id.clone();
        let config = state.config.clone();

        let profile = ConnectionProfile {
            csms_url: self.connection_csms_url.value().to_string(),
            ocpp_identity: self.connection_ocpp_identity.value().to_string(),
            security: SecurityProfile::Basic {
                password: self.connection_password.value().to_string(),
            },
        };

        self.connection_store.remember(charger_id, profile.clone());
        if let Some(path) = &self.connection_store_path
            && let Err(error) = self.connection_store.save(path)
        {
            tracing::warn!(%error, "failed to save connection store");
        }

        // Decide the simulation mode right here, before anything below spawns the connection
        // thread that would start feeding it live state snapshots - see
        // `resolve_simulation_mode`'s doc comment for why the ordering matters.
        if let Some(state) = &mut self.charger_state {
            state.mode = resolve_simulation_mode(&profile.csms_url);
        }

        self.screen = Screen::Dashboard;

        if !profile.csms_url.trim().is_empty() {
            let (result_sender, result_receiver) = oneshot::channel();
            self.connect_result_receiver = Some(result_receiver);

            let (state_sender, state_receiver) = mpsc::unbounded_channel();
            self.ocpp_state_receiver = Some(state_receiver);

            let (event_sender, mut event_receiver) = mpsc::unbounded_channel();
            self.ocpp_event_sender = Some(event_sender);

            // `connect_and_setup`'s future isn't `Send` (upstream uses non-Send sync
            // primitives internally), so it can't go through `tokio::spawn`. A dedicated
            // thread with its own single-threaded runtime sidesteps that: `block_on`
            // doesn't require `Send`. Unlike a one-shot connect attempt, this thread
            // outlives the initial handshake: once connected, it forwards every live state
            // snapshot and every dispatched command for as long as the App's receiver/sender
            // ends of these channels stay alive (dropped in `return_to_picker`, which ends
            // both loops and lets the thread exit).
            std::thread::spawn(move || {
                let tokio_runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build a runtime for the CSMS connection attempt");
                tokio_runtime.block_on(async move {
                    match connect_charger(&config, &profile).await {
                        Ok(charge_point_runtime) => {
                            let _ = result_sender.send(Ok(()));

                            let mut ocpp_states = charge_point_runtime.subscribe();
                            let forward_states = async {
                                loop {
                                    ocpp_states.changed().await;
                                    let state = ocpp_states.borrow();
                                    if state_sender.send(state).is_err() {
                                        break;
                                    }
                                }
                            };
                            let forward_commands = async {
                                while let Some(event) = event_receiver.recv().await {
                                    let _ = charge_point_runtime.send(event).await;
                                }
                            };
                            tokio::join!(forward_states, forward_commands);
                        }
                        Err(error) => {
                            let _ = result_sender.send(Err(error.to_string()));
                        }
                    }
                });
            });
        }
    }

    /// Checks whether a background CSMS connection attempt has resolved, and if so
    /// surfaces the outcome as a status message.
    fn poll_connect_result(&mut self) {
        let Some(receiver) = &mut self.connect_result_receiver else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(())) => {
                self.set_status(StatusSeverity::Ok, "✓ connected to CSMS".to_string());
                self.connect_result_receiver = None;
            }
            Ok(Err(error)) => {
                self.set_status(
                    StatusSeverity::Error,
                    format!("✗ CSMS connection failed: {error}"),
                );
                self.connect_result_receiver = None;
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.connect_result_receiver = None;
            }
        }
    }

    /// Drops the live-connection channels (if any), which ends the background connection
    /// thread's forwarding loops and lets it exit, rather than leaving it running against a
    /// charger that's no longer shown.
    ///
    /// This also drops `charger_state` itself, which incidentally takes any `SimulationMode`
    /// with it: the next charger picked always starts from a fresh
    /// `ChargerState::from_config`, which defaults to `SimulationMode::Local`. There's nothing
    /// further to reset here, but it's worth spelling out - a future refactor that made
    /// `charger_state` persist across selections would need to explicitly reset `mode` too.
    fn return_to_picker(&mut self) {
        self.charger_state = None;
        self.status_message = None;
        self.ocpp_state_receiver = None;
        self.ocpp_event_sender = None;
        self.live_ocpp_state = None;
        self.screen = Screen::PickCharger;
    }

    /// Pulls every entry currently buffered in the tracing bridge's channel
    /// (if one is installed) into the log panel.
    fn drain_log_receiver(&mut self) {
        let Some(receiver) = &mut self.log_receiver else {
            return;
        };
        while let Ok(entry) = receiver.try_recv() {
            self.logs.push(entry);
        }
    }

    /// Pulls every live protocol state snapshot forwarded from a connected OCPP 2.1 charger's
    /// background connection thread, applying each to the dashboard's display state and
    /// remembering the latest one for [`Self::apply_command`] to dispatch against.
    fn drain_ocpp_state_receiver(&mut self) {
        let Some(receiver) = &mut self.ocpp_state_receiver else {
            return;
        };
        while let Ok(state) = receiver.try_recv() {
            if let Some(charger_state) = &mut self.charger_state {
                apply_ocpp_state(charger_state, &state);
            }
            self.live_ocpp_state = Some(state);
        }
    }

    /// Advances the focused charger's simulated meter reading by the real time elapsed since
    /// the last call (not a fixed per-frame amount, since the main loop's cadence varies with
    /// input activity), then forwards a `MeterValueSampled` event per charging connector to a
    /// connected CSMS if [`METER_VALUE_INTERVAL`] has elapsed since the last one was sent.
    fn tick_metrics(&mut self) {
        let now = Instant::now();
        let elapsed = self
            .last_metrics_tick
            .map(|last| now.duration_since(last))
            .unwrap_or_default();
        self.last_metrics_tick = Some(now);
        self.tick_metrics_with(elapsed, now);
    }

    fn tick_metrics_with(&mut self, elapsed: Duration, now: Instant) {
        if let Some(state) = &mut self.charger_state {
            state.tick(elapsed);
        }
        self.expire_status_message(now);
        self.maybe_send_meter_values(now);
    }

    /// Shows `message` in the command bar, stamped so it expires. Always go through this
    /// rather than assigning `status_message` directly - an un-stamped toast would never
    /// expire, which is the bug this replaced.
    pub(crate) fn set_status(&mut self, severity: StatusSeverity, message: String) {
        self.status_message = Some(Toast { severity, message, shown_at: Instant::now() });
    }

    fn expire_status_message(&mut self, now: Instant) {
        if let Some(toast) = &self.status_message
            && now.duration_since(toast.shown_at) >= STATUS_MESSAGE_TTL
        {
            self.status_message = None;
        }
    }

    fn maybe_send_meter_values(&mut self, now: Instant) {
        let Some(sender) = &self.ocpp_event_sender else {
            return;
        };
        let due = match self.last_meter_value_sent {
            Some(last) => now.duration_since(last) >= METER_VALUE_INTERVAL,
            None => true,
        };
        if !due {
            return;
        }
        self.last_meter_value_sent = Some(now);

        let Some(state) = &self.charger_state else {
            return;
        };
        for event in meter_sample_events(state) {
            let _ = sender.send(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::LogLevel;
    use charge_point_simulator_core::charger::{
        ChargerConfig, ChargerSource, ConnectionStatus, ConnectorStatus, EvseConfig, OcppVersion,
    };
    use ocpp_charge_point::state::{
        ConnectorEvent, ConnectorState as OcppConnectorState, EvseEvent, RegistrationStatus,
    };

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    /// The command bar's current message, without the `Instant` a `Toast` also carries -
    /// assertions care about severity and text, never when it was shown.
    fn status(app: &App) -> Option<(StatusSeverity, String)> {
        app.status_message.as_ref().map(|toast| (toast.severity, toast.message.clone()))
    }

    /// The visible log pane as plain strings. Most assertions here care about *which* lines
    /// are shown, not the structure Phase 4 gave each entry.
    fn log_messages(logs: &LogBuffer) -> Vec<String> {
        logs.visible_lines(10)
            .into_iter()
            .map(|entry| entry.message.clone())
            .collect()
    }

    fn charger(id: &str) -> ChargerEntry {
        charger_with_evses(
            id,
            vec![EvseConfig {
                id: 1,
                connectors: 1,
            }],
        )
    }

    fn charger_with_evses(id: &str, evses: Vec<EvseConfig>) -> ChargerEntry {
        ChargerEntry {
            config: ChargerConfig {
                id: id.into(),
                ocpp_version: OcppVersion::V16J,
                evses,
                has_display: false,
            },
            source: ChargerSource::BuiltIn,
        }
    }

    fn charger_v21(id: &str) -> ChargerEntry {
        ChargerEntry {
            config: ChargerConfig {
                id: id.into(),
                ocpp_version: OcppVersion::V21,
                evses: vec![EvseConfig {
                    id: 1,
                    connectors: 1,
                }],
                has_display: false,
            },
            source: ChargerSource::BuiltIn,
        }
    }

    fn charger_with_display(id: &str) -> ChargerEntry {
        ChargerEntry {
            config: ChargerConfig {
                id: id.into(),
                ocpp_version: OcppVersion::V16J,
                evses: vec![EvseConfig {
                    id: 1,
                    connectors: 1,
                }],
                has_display: true,
            },
            source: ChargerSource::BuiltIn,
        }
    }

    #[test]
    fn starts_on_the_charger_picker() {
        let app = App::new(vec![charger("CP001")]);
        assert_eq!(app.screen, Screen::PickCharger);
        assert_eq!(app.selected_charger, 0);
        assert!(app.charger_state.is_none());
    }

    #[test]
    fn q_opens_a_quit_confirmation_instead_of_exiting_immediately() {
        // On the picker, 'q' is typeable into the charger filter instead - Esc (with the
        // filter empty) is how the picker opens the quit confirmation.
        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Esc));
        assert!(app.quit_confirm_open);
        assert!(!app.exit);

        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('q')));
        assert!(app.quit_confirm_open);
        assert!(!app.exit);
    }

    #[test]
    fn y_or_enter_confirms_the_quit() {
        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Esc));
        app.handle_key_event(key(KeyCode::Char('y')));
        assert!(app.exit);

        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Esc));
        app.handle_key_event(key(KeyCode::Enter));
        assert!(app.exit);
    }

    #[test]
    fn n_or_esc_cancels_the_quit() {
        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Esc));
        app.handle_key_event(key(KeyCode::Char('n')));
        assert!(!app.quit_confirm_open);
        assert!(!app.exit);

        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Esc));
        app.handle_key_event(key(KeyCode::Esc));
        assert!(!app.quit_confirm_open);
        assert!(!app.exit);
    }

    #[test]
    fn question_mark_toggles_the_help_overlay() {
        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Char('?')));
        assert!(app.help_open);

        app.handle_key_event(key(KeyCode::Char('?')));
        assert!(!app.help_open);
    }

    #[test]
    fn esc_closes_the_help_overlay() {
        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Char('?')));
        app.handle_key_event(key(KeyCode::Esc));
        assert!(!app.help_open);
    }

    #[test]
    fn keys_are_swallowed_by_the_help_overlay_while_it_is_open() {
        let mut app = App::new(vec![charger("CP001"), charger("CP002")]);
        app.handle_key_event(key(KeyCode::Char('?')));
        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.selected_charger, 0);
    }

    #[test]
    fn down_moves_selection_and_clamps_at_the_end() {
        let mut app = App::new(vec![charger("CP001"), charger("CP002")]);
        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.selected_charger, 1);
        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.selected_charger, 1);
    }

    #[test]
    fn up_moves_selection_and_clamps_at_the_start() {
        let mut app = App::new(vec![charger("CP001"), charger("CP002")]);
        app.selected_charger = 1;
        app.handle_key_event(key(KeyCode::Up));
        assert_eq!(app.selected_charger, 0);
        app.handle_key_event(key(KeyCode::Up));
        assert_eq!(app.selected_charger, 0);
    }

    #[test]
    fn navigation_on_an_empty_charger_list_does_not_panic() {
        let mut app = App::new(vec![]);
        app.handle_key_event(key(KeyCode::Down));
        app.handle_key_event(key(KeyCode::Up));
        assert_eq!(app.selected_charger, 0);
    }

    #[test]
    fn enter_selects_the_highlighted_charger_and_opens_the_dashboard() {
        let mut app = App::new(vec![charger("CP001"), charger("CP002")]);
        app.handle_key_event(key(KeyCode::Down));
        app.handle_key_event(key(KeyCode::Enter));
        assert_eq!(app.screen, Screen::Dashboard);
        assert_eq!(app.charger_state.unwrap().config.id, "CP002");
    }

    #[test]
    fn typing_in_the_picker_filters_by_charger_id_and_resets_the_selection() {
        let mut app = App::new(vec![charger("CP001"), charger("CP002")]);
        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.selected_charger, 1);

        for c in "cp002".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }

        let ids: Vec<&str> = app
            .filtered_chargers()
            .iter()
            .map(|e| e.config.id.as_str())
            .collect();
        assert_eq!(ids, vec!["CP002"]);
        assert_eq!(app.selected_charger, 0);
    }

    #[test]
    fn enter_selects_the_highlighted_charger_from_the_filtered_list() {
        let mut app = App::new(vec![charger("CP001"), charger("CP002")]);
        for c in "cp002".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }

        app.handle_key_event(key(KeyCode::Enter));

        assert_eq!(app.charger_state.unwrap().config.id, "CP002");
    }

    #[test]
    fn esc_clears_the_filter_before_opening_the_quit_confirmation() {
        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Char('x')));
        assert_eq!(app.picker_filter.value(), "x");

        app.handle_key_event(key(KeyCode::Esc));
        assert_eq!(app.picker_filter.value(), "");
        assert!(!app.quit_confirm_open);

        app.handle_key_event(key(KeyCode::Esc));
        assert!(app.quit_confirm_open);
    }

    #[test]
    fn q_is_typed_into_the_picker_filter_instead_of_opening_quit_confirm() {
        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Char('q')));

        assert_eq!(app.picker_filter.value(), "q");
        assert!(!app.quit_confirm_open);
    }

    #[test]
    fn question_mark_still_opens_help_from_the_picker() {
        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Char('?')));

        assert!(app.help_open);
        assert_eq!(app.picker_filter.value(), "");
    }

    #[test]
    fn enter_with_no_chargers_available_does_nothing() {
        let mut app = App::new(vec![]);
        app.handle_key_event(key(KeyCode::Enter));
        assert_eq!(app.screen, Screen::PickCharger);
        assert!(app.charger_state.is_none());
    }

    #[test]
    fn confirming_a_selection_seeds_a_fresh_booting_charger_state() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();

        let state = app.charger_state.unwrap();
        assert_eq!(state.connection_status, ConnectionStatus::Booting);
        assert_eq!(state.evses.len(), 1);
        assert_eq!(state.evses[0].connectors.len(), 1);
    }

    #[test]
    fn confirming_a_selection_logs_a_boot_message() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        assert_eq!(log_messages(&app.logs), vec!["CP001 booting"]);
    }

    #[test]
    fn confirming_a_selection_focuses_the_first_connector_of_the_first_evse() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![
                EvseConfig {
                    id: 1,
                    connectors: 1,
                },
                EvseConfig {
                    id: 2,
                    connectors: 1,
                },
            ],
        )]);
        app.confirm_charger_selection();
        assert_eq!(
            app.focused,
            FocusedConnector {
                evse: 0,
                connector: 0
            }
        );
    }

    #[test]
    fn right_and_tab_move_focus_to_the_next_evse_and_clamp_at_the_end() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![
                EvseConfig {
                    id: 1,
                    connectors: 1,
                },
                EvseConfig {
                    id: 2,
                    connectors: 1,
                },
            ],
        )]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Right));
        assert_eq!(app.focused.evse, 1);
        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(app.focused.evse, 1);
    }

    #[test]
    fn left_and_backtab_move_focus_to_the_previous_evse_and_clamp_at_the_start() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![
                EvseConfig {
                    id: 1,
                    connectors: 1,
                },
                EvseConfig {
                    id: 2,
                    connectors: 1,
                },
            ],
        )]);
        app.confirm_charger_selection();
        app.focused.evse = 1;

        app.handle_key_event(key(KeyCode::Left));
        assert_eq!(app.focused.evse, 0);
        app.handle_key_event(key(KeyCode::BackTab));
        assert_eq!(app.focused.evse, 0);
    }

    #[test]
    fn jumping_evse_with_tab_resets_focus_to_that_evses_first_connector() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![
                EvseConfig {
                    id: 1,
                    connectors: 2,
                },
                EvseConfig {
                    id: 2,
                    connectors: 2,
                },
            ],
        )]);
        app.confirm_charger_selection();
        app.focused = FocusedConnector {
            evse: 0,
            connector: 1,
        };

        app.handle_key_event(key(KeyCode::Tab));

        assert_eq!(
            app.focused,
            FocusedConnector {
                evse: 1,
                connector: 0
            }
        );
    }

    #[test]
    fn evse_focus_navigation_on_a_charger_with_no_evses_does_not_panic() {
        let mut app = App::new(vec![charger_with_evses("CP001", vec![])]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Right));
        app.handle_key_event(key(KeyCode::Left));
        assert_eq!(app.focused, FocusedConnector::default());
    }

    #[test]
    fn down_moves_focus_across_connectors_within_one_evse() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![EvseConfig {
                id: 1,
                connectors: 2,
            }],
        )]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(
            app.focused,
            FocusedConnector {
                evse: 0,
                connector: 1
            }
        );
    }

    #[test]
    fn down_flows_from_the_last_connector_of_one_evse_into_the_first_of_the_next() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![
                EvseConfig {
                    id: 1,
                    connectors: 2,
                },
                EvseConfig {
                    id: 2,
                    connectors: 1,
                },
            ],
        )]);
        app.confirm_charger_selection();
        app.focused = FocusedConnector {
            evse: 0,
            connector: 1,
        };

        app.handle_key_event(key(KeyCode::Down));

        assert_eq!(
            app.focused,
            FocusedConnector {
                evse: 1,
                connector: 0
            }
        );
    }

    #[test]
    fn down_clamps_at_the_very_last_connector_of_the_whole_tree() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![
                EvseConfig {
                    id: 1,
                    connectors: 1,
                },
                EvseConfig {
                    id: 2,
                    connectors: 1,
                },
            ],
        )]);
        app.confirm_charger_selection();
        app.focused = FocusedConnector {
            evse: 1,
            connector: 0,
        };

        app.handle_key_event(key(KeyCode::Down));

        assert_eq!(
            app.focused,
            FocusedConnector {
                evse: 1,
                connector: 0
            }
        );
    }

    #[test]
    fn up_flows_from_the_first_connector_of_one_evse_into_the_last_of_the_previous() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![
                EvseConfig {
                    id: 1,
                    connectors: 2,
                },
                EvseConfig {
                    id: 2,
                    connectors: 1,
                },
            ],
        )]);
        app.confirm_charger_selection();
        app.focused = FocusedConnector {
            evse: 1,
            connector: 0,
        };

        app.handle_key_event(key(KeyCode::Up));

        assert_eq!(
            app.focused,
            FocusedConnector {
                evse: 0,
                connector: 1
            }
        );
    }

    #[test]
    fn up_clamps_at_the_very_first_connector_of_the_whole_tree() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![EvseConfig {
                id: 1,
                connectors: 1,
            }],
        )]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Up));

        assert_eq!(app.focused, FocusedConnector::default());
    }

    #[test]
    fn up_and_down_navigation_on_a_charger_with_no_evses_does_not_panic() {
        let mut app = App::new(vec![charger_with_evses("CP001", vec![])]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Down));
        app.handle_key_event(key(KeyCode::Up));

        assert_eq!(app.focused, FocusedConnector::default());
    }

    #[test]
    fn up_and_down_navigation_on_an_evse_with_no_connectors_does_not_panic() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![EvseConfig {
                id: 1,
                connectors: 0,
            }],
        )]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Down));
        app.handle_key_event(key(KeyCode::Up));

        assert_eq!(app.focused, FocusedConnector::default());
    }

    #[test]
    fn apply_command_acts_on_the_specifically_focused_connector_not_just_the_first_eligible_one() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![EvseConfig {
                id: 1,
                connectors: 2,
            }],
        )]);
        app.confirm_charger_selection();
        app.focused = FocusedConnector {
            evse: 0,
            connector: 1,
        };

        app.apply_command(Command::PlugInVehicle, "MY-EV-2");

        let state = app.charger_state.unwrap();
        assert_eq!(state.evses[0].connectors[0].vehicle, None);
        assert_eq!(
            state.evses[0].connectors[1].vehicle.as_ref().unwrap().id,
            "MY-EV-2"
        );
    }

    #[test]
    fn available_commands_reflect_the_specifically_focused_connector() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![EvseConfig {
                id: 1,
                connectors: 2,
            }],
        )]);
        app.confirm_charger_selection();
        // Occupy connector 1 (index 0) so it no longer offers "Plug in vehicle"; connector 2
        // (index 1) stays Available and offers it.
        app.charger_state.as_mut().unwrap().evses[0].connectors[0].status =
            ConnectorStatus::Occupied;

        app.focused = FocusedConnector {
            evse: 0,
            connector: 0,
        };
        let labels_for_connector_1: Vec<&str> =
            app.available_commands().iter().map(|c| c.label()).collect();
        assert!(!labels_for_connector_1.contains(&"Plug in vehicle"));
        assert!(labels_for_connector_1.contains(&"Present RFID card"));

        app.focused = FocusedConnector {
            evse: 0,
            connector: 1,
        };
        let labels_for_connector_2: Vec<&str> =
            app.available_commands().iter().map(|c| c.label()).collect();
        assert!(labels_for_connector_2.contains(&"Plug in vehicle"));
        assert!(!labels_for_connector_2.contains(&"Present RFID card"));
    }

    #[test]
    fn c_opens_the_command_palette_on_the_first_command() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Char('c')));
        assert!(app.command_palette_open);
        assert_eq!(app.command_palette_selected, 0);
    }

    #[test]
    fn c_does_nothing_without_a_selected_charger() {
        let mut app = App::new(vec![]);
        app.handle_key_event(key(KeyCode::Char('c')));
        assert!(!app.command_palette_open);
    }

    #[test]
    fn a_charger_without_a_display_never_offers_display_commands() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();

        let labels: Vec<&str> = app.available_commands().iter().map(|c| c.label()).collect();
        assert!(!labels.contains(&"Set display message"));
        assert!(!labels.contains(&"Clear display message"));
    }

    #[test]
    fn a_charger_with_a_display_offers_set_but_not_clear_until_a_message_is_showing() {
        let mut app = App::new(vec![charger_with_display("CP-display")]);
        app.confirm_charger_selection();

        let labels: Vec<&str> = app.available_commands().iter().map(|c| c.label()).collect();
        assert!(labels.contains(&"Set display message"));
        assert!(!labels.contains(&"Clear display message"));
    }

    #[test]
    fn setting_a_display_message_shows_it_and_then_offers_clear() {
        let mut app = App::new(vec![charger_with_display("CP-display")]);
        app.confirm_charger_selection();

        app.apply_command(Command::SetDisplayMessage, "Welcome to Flowion");

        assert_eq!(
            app.charger_state.as_ref().unwrap().display_message,
            Some("Welcome to Flowion".to_string())
        );
        assert_eq!(status(&app), Some((StatusSeverity::Ok, "✓ Set display message".to_string())));
        let labels: Vec<&str> = app.available_commands().iter().map(|c| c.label()).collect();
        assert!(labels.contains(&"Clear display message"));
    }

    #[test]
    fn clearing_a_display_message_blanks_it() {
        let mut app = App::new(vec![charger_with_display("CP-display")]);
        app.confirm_charger_selection();
        app.apply_command(Command::SetDisplayMessage, "hello");

        app.apply_command(Command::ClearDisplayMessage, "");

        assert_eq!(app.charger_state.as_ref().unwrap().display_message, None);
    }

    #[test]
    fn display_commands_apply_locally_even_when_a_live_csms_sender_is_present() {
        let mut app = App::new(vec![charger_with_display("CP-display")]);
        app.confirm_charger_selection();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.ocpp_event_sender = Some(sender);

        app.apply_command(Command::SetDisplayMessage, "hello");

        assert_eq!(
            app.charger_state.as_ref().unwrap().display_message,
            Some("hello".to_string())
        );
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn esc_closes_the_command_palette_without_dispatching() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));

        app.handle_key_event(key(KeyCode::Esc));
        assert!(!app.command_palette_open);
        // still booting, no vehicle plugged in - nothing was dispatched
        assert_eq!(
            app.charger_state.unwrap().evses[0].connectors[0].vehicle,
            None
        );
    }

    #[test]
    fn typing_in_the_command_palette_filters_by_label_and_resets_the_selection() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));

        // a fresh connector offers "Plug in vehicle" and "Report fault"
        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.command_palette_selected, 1);

        for c in "fault".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }

        let labels: Vec<&str> = app.palette_commands().iter().map(|c| c.label()).collect();
        assert_eq!(labels, vec!["Report fault"]);
        assert_eq!(app.command_palette_selected, 0);
    }

    #[test]
    fn backspacing_the_command_palette_filter_restores_hidden_commands() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));

        for c in "fault".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
        for _ in 0..5 {
            app.handle_key_event(key(KeyCode::Backspace));
        }

        assert_eq!(app.palette_commands().len(), app.available_commands().len());
    }

    #[test]
    fn selecting_a_command_that_needs_a_parameter_opens_a_prompt_instead_of_dispatching() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter)); // highlighted command is "Plug in vehicle"

        assert!(!app.command_palette_open);
        assert_eq!(app.parameter_prompt, Some(Command::PlugInVehicle));
        // nothing applied yet
        assert_eq!(
            app.charger_state.unwrap().evses[0].connectors[0].vehicle,
            None
        );
    }

    #[test]
    fn submitting_the_parameter_prompt_applies_the_command_with_the_given_input() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter)); // opens the "Vehicle ID" prompt

        for c in "MY-EV-1".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
        app.handle_key_event(key(KeyCode::Enter));

        assert!(app.parameter_prompt.is_none());
        let state = app.charger_state.unwrap();
        assert_eq!(
            state.evses[0].connectors[0].vehicle.as_ref().unwrap().id,
            "MY-EV-1"
        );
    }

    #[test]
    fn esc_cancels_the_parameter_prompt_without_applying_the_command() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter)); // opens the prompt

        app.handle_key_event(key(KeyCode::Esc));

        assert!(app.parameter_prompt.is_none());
        assert_eq!(
            app.charger_state.unwrap().evses[0].connectors[0].vehicle,
            None
        );
    }

    #[test]
    fn down_and_up_move_the_command_palette_selection_and_clamp() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));

        // a fresh connector only has "Plug in vehicle" and "Report fault" available
        assert_eq!(app.available_commands().len(), 2);

        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.command_palette_selected, 1);
        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.command_palette_selected, 1);

        app.handle_key_event(key(KeyCode::Up));
        assert_eq!(app.command_palette_selected, 0);
        app.handle_key_event(key(KeyCode::Up));
        assert_eq!(app.command_palette_selected, 0);
    }

    #[test]
    fn enter_dispatches_the_selected_command_and_closes_the_palette() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter)); // opens the "Vehicle ID" parameter prompt
        app.handle_key_event(key(KeyCode::Char('E')));
        app.handle_key_event(key(KeyCode::Enter)); // submits it, applying the command

        assert!(!app.command_palette_open);
        assert!(app.parameter_prompt.is_none());
        let state = app.charger_state.unwrap();
        assert_eq!(
            state.evses[0].connectors[0].status,
            charge_point_simulator_core::charger::ConnectorStatus::Occupied
        );
        assert!(state.evses[0].connectors[0].vehicle.is_some());
        assert!(
            log_messages(&app.logs)
                .iter()
                .any(|l| l.contains("plugged in"))
        );
    }

    #[test]
    fn dispatching_a_command_updates_the_available_commands_for_the_next_open() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter)); // opens the parameter prompt
        app.handle_key_event(key(KeyCode::Char('E')));
        app.handle_key_event(key(KeyCode::Enter)); // submits it: plug in vehicle

        app.handle_key_event(key(KeyCode::Char('c')));
        let labels: Vec<&str> = app.available_commands().iter().map(|c| c.label()).collect();
        assert!(labels.contains(&"Present RFID card"));
        assert!(labels.contains(&"Unplug vehicle"));
        assert!(!labels.contains(&"Plug in vehicle"));
    }

    #[test]
    fn dispatching_a_command_shows_a_confirmation_status_message() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        assert_eq!(status(&app), None);

        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter)); // opens the parameter prompt
        app.handle_key_event(key(KeyCode::Char('E')));
        app.handle_key_event(key(KeyCode::Enter)); // submits it

        assert_eq!(status(&app), Some((StatusSeverity::Ok, "✓ Plug in vehicle".to_string())));
    }

    #[test]
    fn returning_to_the_picker_clears_any_status_message() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.set_status(StatusSeverity::Ok, "✓ Plug in vehicle".to_string());

        app.handle_key_event(key(KeyCode::Esc));
        assert_eq!(status(&app), None);
    }

    #[test]
    fn escape_on_the_dashboard_returns_to_the_picker() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::PickCharger);
        assert!(app.charger_state.is_none());
    }

    #[test]
    fn draining_the_log_receiver_appends_every_pending_line() {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(vec![]);
        app.log_receiver = Some(receiver);

        sender.send(LogEntry::from("first")).unwrap();
        sender.send(LogEntry::from("second")).unwrap();

        app.drain_log_receiver();
        assert_eq!(log_messages(&app.logs), vec!["first", "second"]);
    }

    #[test]
    fn draining_without_a_receiver_installed_does_nothing() {
        let mut app = App::new(vec![]);
        app.drain_log_receiver();
        assert_eq!(log_messages(&app.logs), Vec::<String>::new());
    }

    /// An app sitting on the dashboard with `count` log entries, for the log-binding tests.
    fn app_with_logs(count: usize) -> App {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.logs = LogBuffer::default();
        for i in 0..count {
            app.logs.push(format!("entry {i}"));
        }
        app
    }

    #[test]
    fn slash_opens_the_log_filter_and_typing_narrows_the_pane_live() {
        let mut app = app_with_logs(0);
        app.logs.push("heartbeat sent");
        app.logs.push("connector faulted");

        app.handle_key_event(key(KeyCode::Char('/')));
        assert!(app.log_filter_open);

        for c in "fault".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
        // Narrowed before Enter is ever pressed - a filter that matches nothing is visible as
        // soon as it's typed.
        assert_eq!(log_messages(&app.logs), vec!["connector faulted"]);

        app.handle_key_event(key(KeyCode::Enter));
        assert!(!app.log_filter_open);
        assert_eq!(app.logs.filter(), Some("fault"));
    }

    #[test]
    fn esc_in_the_filter_prompt_clears_the_filter_rather_than_committing_it() {
        let mut app = app_with_logs(0);
        app.logs.push("heartbeat sent");
        app.logs.push("connector faulted");

        app.handle_key_event(key(KeyCode::Char('/')));
        app.handle_key_event(key(KeyCode::Char('f')));
        app.handle_key_event(key(KeyCode::Esc));

        assert!(!app.log_filter_open);
        assert_eq!(app.logs.filter(), None);
        assert_eq!(
            log_messages(&app.logs),
            vec!["heartbeat sent", "connector faulted"]
        );
    }

    #[test]
    fn reopening_the_filter_prompt_seeds_it_with_the_active_filter() {
        let mut app = app_with_logs(0);
        app.logs.push("connector faulted");

        app.handle_key_event(key(KeyCode::Char('/')));
        app.handle_key_event(key(KeyCode::Char('f')));
        app.handle_key_event(key(KeyCode::Enter));
        app.handle_key_event(key(KeyCode::Char('/')));

        assert_eq!(app.log_filter_field.value(), "f");
    }

    #[test]
    fn q_and_question_mark_are_typeable_into_the_log_filter() {
        let mut app = app_with_logs(0);
        app.handle_key_event(key(KeyCode::Char('/')));
        app.handle_key_event(key(KeyCode::Char('q')));
        app.handle_key_event(key(KeyCode::Char('?')));

        assert_eq!(app.log_filter_field.value(), "q?");
        assert!(!app.quit_confirm_open);
        assert!(!app.help_open);
    }

    #[test]
    fn esc_on_the_dashboard_clears_an_active_log_filter_before_it_means_go_back() {
        let mut app = app_with_logs(0);
        app.logs.push("connector faulted");
        app.logs.set_filter("fault");

        app.handle_key_event(key(KeyCode::Esc));
        assert_eq!(app.logs.filter(), None);
        assert_eq!(app.screen, Screen::Dashboard);

        app.handle_key_event(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::PickCharger);
    }

    #[test]
    fn g_and_shift_g_jump_to_the_oldest_and_newest_log_entries() {
        let mut app = app_with_logs(20);

        app.handle_key_event(key(KeyCode::Char('g')));
        assert!(app.logs.is_paused());
        assert_eq!(app.logs.visible_lines(1)[0].message, "entry 0");

        app.handle_key_event(key(KeyCode::Char('G')));
        assert!(!app.logs.is_paused());
        assert_eq!(app.logs.visible_lines(1)[0].message, "entry 19");
    }

    #[test]
    fn l_cycles_the_log_level_threshold() {
        let mut app = app_with_logs(0);
        assert_eq!(app.logs.level_threshold(), LogLevel::Info);

        app.handle_key_event(key(KeyCode::Char('l')));
        assert_eq!(app.logs.level_threshold(), LogLevel::Debug);
    }

    #[test]
    fn ctrl_k_opens_the_command_palette_and_c_still_works_as_an_alias() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();

        app.handle_key_event(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert!(app.command_palette_open);

        app.handle_key_event(key(KeyCode::Esc));
        app.handle_key_event(key(KeyCode::Char('c')));
        assert!(app.command_palette_open);
    }

    #[test]
    fn a_bare_k_does_not_open_the_palette() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Char('k')));
        assert!(!app.command_palette_open);
    }

    #[test]
    fn the_palette_matches_commands_as_a_fuzzy_subsequence() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));
        for c in "pv".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }

        // "pv" is not a substring of any label - the old matcher found nothing here.
        let labels: Vec<&str> = app.palette_commands().iter().map(|c| c.label()).collect();
        assert_eq!(labels.first(), Some(&"Plug in vehicle"));
    }

    #[test]
    fn tab_retargets_the_palette_to_the_next_connector() {
        let mut app = App::new(vec![charger_with_evses("CP-2C", vec![EvseConfig { id: 1, connectors: 2 }])]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));
        assert_eq!(app.focused_connector_label().as_deref(), Some("EVSE 1 / C1"));

        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(app.focused_connector_label().as_deref(), Some("EVSE 1 / C2"));
        // Still open: retargeting happens in place, without a round trip through the dashboard.
        assert!(app.command_palette_open);
        assert_eq!(app.command_palette_selected, 0);

        app.handle_key_event(key(KeyCode::BackTab));
        assert_eq!(app.focused_connector_label().as_deref(), Some("EVSE 1 / C1"));
    }

    #[test]
    fn a_blank_parameter_is_rejected_inline_and_leaves_the_prompt_open() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter));

        app.handle_key_event(key(KeyCode::Enter)); // submit blank

        assert!(app.parameter_prompt.is_some());
        assert_eq!(app.parameter_error, Some("cannot be blank"));
        assert_eq!(status(&app), None);

        // Typing clears the complaint about the value that's no longer there.
        app.handle_key_event(key(KeyCode::Char('E')));
        assert_eq!(app.parameter_error, None);
    }

    #[test]
    fn whitespace_only_counts_as_blank() {
        assert_eq!(validate_parameter("   "), Some("cannot be blank"));
        assert_eq!(validate_parameter("EV-1"), None);
    }

    #[test]
    fn a_parameter_prompt_is_prefilled_with_the_last_accepted_value() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter));
        for c in "MY-EV".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
        app.handle_key_event(key(KeyCode::Enter));

        // Unplug, then plug in again: the vehicle id is remembered.
        app.apply_command(Command::UnplugVehicle, "");
        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter));

        assert_eq!(app.parameter_field.value(), "MY-EV");
        // Prefilled, not merely suggested: Enter alone submits it.
        assert_eq!(app.parameter_field.cursor(), "MY-EV".chars().count());
    }

    #[test]
    fn a_rejected_parameter_is_not_remembered() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter));
        app.handle_key_event(key(KeyCode::Enter)); // blank, rejected

        assert!(app.parameter_history.is_empty());
    }

    #[test]
    fn a_status_message_expires_after_its_ttl() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.set_status(StatusSeverity::Ok, "✓ Plug in vehicle".to_string());
        let shown_at = app.status_message.as_ref().unwrap().shown_at;

        // Just short of the TTL: still there, so the message is actually readable.
        app.tick_metrics_with(Duration::ZERO, shown_at + STATUS_MESSAGE_TTL - Duration::from_millis(1));
        assert!(app.status_message.is_some());

        app.tick_metrics_with(Duration::ZERO, shown_at + STATUS_MESSAGE_TTL);
        assert_eq!(status(&app), None);
    }

    #[test]
    fn selecting_a_2_1_charger_goes_to_connection_setup_not_the_dashboard() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        assert_eq!(app.screen, Screen::ConnectionSetup);
    }

    #[test]
    fn selecting_a_1_6j_or_2_0_1_charger_still_goes_straight_to_the_dashboard() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        assert_eq!(app.screen, Screen::Dashboard);
    }

    #[test]
    fn connection_setup_defaults_to_an_empty_url_and_the_charger_id_as_identity() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();

        assert_eq!(app.connection_csms_url.value(), "");
        assert_eq!(app.connection_ocpp_identity.value(), "CP-2.1");
        assert_eq!(app.connection_password.value(), "");
        assert_eq!(app.connection_focused_field, 0);
    }

    #[test]
    fn connection_setup_prefills_from_a_remembered_profile() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.connection_store.remember(
            "CP-2.1",
            ConnectionProfile {
                csms_url: "wss://csms.example.com".into(),
                ocpp_identity: "remembered-id".into(),
                security: SecurityProfile::Basic {
                    password: "secret".into(),
                },
            },
        );

        app.confirm_charger_selection();

        assert_eq!(app.connection_csms_url.value(), "wss://csms.example.com");
        assert_eq!(app.connection_ocpp_identity.value(), "remembered-id");
        assert_eq!(app.connection_password.value(), "secret");
    }

    #[test]
    fn tab_and_shifttab_move_focus_between_fields_and_clamp() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(app.connection_focused_field, 1);
        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(app.connection_focused_field, 2);
        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(app.connection_focused_field, 2);

        app.handle_key_event(key(KeyCode::BackTab));
        assert_eq!(app.connection_focused_field, 1);
    }

    #[test]
    fn typing_edits_the_focused_field() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();

        for c in "ws://host".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
        assert_eq!(app.connection_csms_url.value(), "ws://host");

        app.handle_key_event(key(KeyCode::Backspace));
        assert_eq!(app.connection_csms_url.value(), "ws://hos");
    }

    #[test]
    fn q_and_question_mark_are_typed_into_the_field_instead_of_opening_global_modals() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Char('q')));
        app.handle_key_event(key(KeyCode::Char('?')));

        assert_eq!(app.connection_csms_url.value(), "q?");
        assert!(!app.quit_confirm_open);
        assert!(!app.help_open);
    }

    #[test]
    fn esc_on_connection_setup_returns_to_the_picker() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::PickCharger);
    }

    #[test]
    fn confirming_with_a_url_remembers_the_profile_and_starts_a_connect_attempt() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();

        for c in "ws://localhost:9999/dev".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
        // identity already defaults to the charger id (see
        // `connection_setup_defaults_to_an_empty_url_and_the_charger_id_as_identity`)

        app.handle_key_event(key(KeyCode::Enter));

        assert_eq!(app.screen, Screen::Dashboard);
        assert!(app.connect_result_receiver.is_some());
        assert!(app.ocpp_state_receiver.is_some());
        assert!(app.ocpp_event_sender.is_some());
        let remembered = app.connection_store.get("CP-2.1").unwrap();
        assert_eq!(remembered.csms_url, "ws://localhost:9999/dev");
        assert_eq!(remembered.ocpp_identity, "CP-2.1");
    }

    #[test]
    fn confirming_with_a_url_puts_the_charger_in_live_csms_mode_with_that_url() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();

        for c in "ws://localhost:9999/dev".chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
        app.handle_key_event(key(KeyCode::Enter));

        assert_eq!(
            app.charger_state.unwrap().mode,
            SimulationMode::LiveCsms {
                url: "ws://localhost:9999/dev".to_string()
            }
        );
    }

    #[test]
    fn confirming_with_a_blank_url_does_not_start_a_connect_attempt() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Enter));

        assert_eq!(app.screen, Screen::Dashboard);
        assert!(app.connect_result_receiver.is_none());
    }

    #[test]
    fn confirming_with_a_blank_url_leaves_the_charger_in_local_mode() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Enter));

        assert_eq!(app.charger_state.unwrap().mode, SimulationMode::Local);
    }

    #[test]
    fn resolve_simulation_mode_is_local_for_a_blank_or_whitespace_only_url() {
        assert_eq!(resolve_simulation_mode(""), SimulationMode::Local);
        assert_eq!(resolve_simulation_mode("   "), SimulationMode::Local);
    }

    #[test]
    fn resolve_simulation_mode_is_live_csms_with_the_url_for_a_non_blank_url() {
        assert_eq!(
            resolve_simulation_mode("wss://csms.example.com"),
            SimulationMode::LiveCsms {
                url: "wss://csms.example.com".to_string()
            }
        );
    }

    #[test]
    fn a_live_csms_charger_does_not_self_promote_to_connected_on_ticks() {
        // With a real CSMS connection in the picture, `connection_status` must come from the
        // OCPP bridge alone (see `SimulationMode`'s doc comment) - `tick`'s simulated boot
        // timer must never race it and flip the dashboard to "connected" on its own.
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.charger_state.as_mut().unwrap().mode = SimulationMode::LiveCsms {
            url: "wss://csms.example.com".to_string(),
        };

        app.tick_metrics_with(Duration::from_secs(60), Instant::now());

        assert_eq!(
            app.charger_state.unwrap().connection_status,
            ConnectionStatus::Booting
        );
    }

    #[test]
    fn returning_to_the_picker_leaves_no_stale_live_csms_mode_for_the_next_charger() {
        let mut app = App::new(vec![charger_v21("CP-2.1"), charger("CP001")]);
        app.confirm_charger_selection();
        app.charger_state.as_mut().unwrap().mode = SimulationMode::LiveCsms {
            url: "wss://csms.example.com".to_string(),
        };

        app.handle_key_event(key(KeyCode::Esc)); // back to the picker
        app.handle_key_event(key(KeyCode::Down)); // select CP001 (1.6J, straight to dashboard)
        app.handle_key_event(key(KeyCode::Enter));

        assert_eq!(app.charger_state.unwrap().mode, SimulationMode::Local);
    }

    #[test]
    fn poll_connect_result_reports_success() {
        let (sender, receiver) = oneshot::channel();
        let mut app = App::new(vec![]);
        app.connect_result_receiver = Some(receiver);
        sender.send(Ok(())).unwrap();

        app.poll_connect_result();

        assert_eq!(status(&app), Some((StatusSeverity::Ok, "✓ connected to CSMS".to_string())));
        assert!(app.connect_result_receiver.is_none());
    }

    #[test]
    fn poll_connect_result_reports_failure() {
        let (sender, receiver) = oneshot::channel();
        let mut app = App::new(vec![]);
        app.connect_result_receiver = Some(receiver);
        sender.send(Err("boom".to_string())).unwrap();

        app.poll_connect_result();

        assert_eq!(
            status(&app),
            Some((StatusSeverity::Error, "✗ CSMS connection failed: boom".to_string()))
        );
        assert!(app.connect_result_receiver.is_none());
    }

    #[test]
    fn poll_connect_result_leaves_status_untouched_while_still_pending() {
        let (_sender, receiver) = oneshot::channel();
        let mut app = App::new(vec![]);
        app.connect_result_receiver = Some(receiver);

        app.poll_connect_result();

        assert_eq!(status(&app), None);
        assert!(app.connect_result_receiver.is_some());
    }

    fn ocpp_state_with(connector: OcppConnectorState) -> ChargePointState {
        let mut state = ChargePointState::new([1]);
        state.registration = Some(RegistrationStatus::Accepted);
        state.evses[0].connectors[0] = connector;
        state
    }

    #[test]
    fn tick_metrics_advances_the_focused_chargers_simulated_meter_by_the_given_elapsed_time() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.charger_state.as_mut().unwrap().evses[0].connectors[0].status =
            ConnectorStatus::Charging;

        app.tick_metrics_with(Duration::from_secs(3600), Instant::now());

        let metrics = app.charger_state.as_ref().unwrap().evses[0].metrics;
        assert!(metrics.power_kw > 0.0);
        assert!(metrics.energy_kwh > 0.0);
    }

    #[test]
    fn tick_metrics_does_nothing_without_a_selected_charger() {
        let mut app = App::new(vec![]);
        // Just proving this doesn't panic with no charger selected.
        app.tick_metrics_with(Duration::from_secs(1), Instant::now());
        assert!(app.charger_state.is_none());
    }

    #[test]
    fn maybe_send_meter_values_sends_immediately_the_first_time_a_csms_is_connected() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.charger_state.as_mut().unwrap().evses[0].connectors[0].status =
            ConnectorStatus::Charging;
        app.charger_state.as_mut().unwrap().evses[0]
            .metrics
            .energy_kwh = 1.0;
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.ocpp_event_sender = Some(sender);

        app.tick_metrics_with(Duration::ZERO, Instant::now());

        assert!(receiver.try_recv().is_ok());
    }

    #[test]
    fn maybe_send_meter_values_is_throttled_until_the_interval_elapses() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.charger_state.as_mut().unwrap().evses[0].connectors[0].status =
            ConnectorStatus::Charging;
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.ocpp_event_sender = Some(sender);

        let start = Instant::now();
        app.tick_metrics_with(Duration::ZERO, start);
        receiver.try_recv().unwrap(); // drain the first, immediate send

        app.tick_metrics_with(Duration::ZERO, start + Duration::from_secs(1));
        assert!(
            receiver.try_recv().is_err(),
            "resent before the interval elapsed"
        );

        app.tick_metrics_with(Duration::ZERO, start + METER_VALUE_INTERVAL);
        assert!(
            receiver.try_recv().is_ok(),
            "did not resend once the interval elapsed"
        );
    }

    #[test]
    fn maybe_send_meter_values_does_nothing_without_a_live_csms_sender() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.charger_state.as_mut().unwrap().evses[0].connectors[0].status =
            ConnectorStatus::Charging;

        // No panic and nothing queued, since there's no `ocpp_event_sender` to send through.
        app.tick_metrics_with(Duration::from_secs(3600), Instant::now());
        assert!(app.ocpp_event_sender.is_none());
    }

    #[test]
    fn drain_ocpp_state_receiver_applies_incoming_snapshots_and_remembers_the_latest() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        let (sender, receiver) = mpsc::unbounded_channel();
        app.ocpp_state_receiver = Some(receiver);

        sender
            .send(ocpp_state_with(OcppConnectorState::Locked))
            .unwrap();
        app.drain_ocpp_state_receiver();

        assert_eq!(
            app.charger_state.as_ref().unwrap().connection_status,
            ConnectionStatus::Connected
        );
        assert_eq!(
            app.live_ocpp_state.as_ref().unwrap().evses[0].connectors[0],
            OcppConnectorState::Locked
        );
    }

    #[test]
    fn apply_command_sends_the_matching_event_when_connected_to_a_real_csms() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.ocpp_event_sender = Some(sender);
        app.live_ocpp_state = Some(ocpp_state_with(OcppConnectorState::Available));

        app.apply_command(Command::PlugInVehicle, "");

        let sent = receiver.try_recv().unwrap();
        assert_eq!(
            sent,
            ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::CableConnected,
                },
            }
        );
    }

    #[test]
    fn apply_command_reports_not_ready_when_no_connector_is_eligible_yet() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.ocpp_event_sender = Some(sender);
        app.live_ocpp_state = Some(ocpp_state_with(OcppConnectorState::Charging));

        app.apply_command(Command::PlugInVehicle, "");

        assert!(receiver.try_recv().is_err());
        assert_eq!(
            status(&app),
            Some((StatusSeverity::Error, "✗ Plug in vehicle not ready yet".to_string()))
        );
    }

    #[test]
    fn returning_to_the_picker_tears_down_the_live_connection_channels() {
        let mut app = App::new(vec![charger_v21("CP-2.1")]);
        app.confirm_charger_selection();
        let (_sender, receiver) = mpsc::unbounded_channel::<ChargePointState>();
        let (event_sender, _event_receiver) = mpsc::unbounded_channel();
        app.ocpp_state_receiver = Some(receiver);
        app.ocpp_event_sender = Some(event_sender);
        app.live_ocpp_state = Some(ocpp_state_with(OcppConnectorState::Available));

        app.handle_key_event(key(KeyCode::Esc));

        assert!(app.ocpp_state_receiver.is_none());
        assert!(app.ocpp_event_sender.is_none());
        assert!(app.live_ocpp_state.is_none());
    }
}
