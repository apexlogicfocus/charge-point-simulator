use crate::connection_setup::connection_setup_layout;
use crate::dashboard::{dashboard_layout, is_terminal_too_small};
use crate::logs::LogBuffer;
use crate::picker::picker_layout;
use crate::screen::Screen;
use crate::text_field::TextField;
use crate::theme::{BRAND_TEAL, bordered_block, connection_status_color, connector_status_color};
use charge_point_simulator_core::charger::{
    ChargerEntry, ChargerState, Command, ConnectionProfile, ConnectionStore, OcppVersion,
    SecurityProfile, connect_charger,
};
use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::oneshot;
use tui_big_text::{BigText, PixelSize};

/// Rows occupied by the stacked "CHARGE" / "POINT" / "SIMULATOR" banner at
/// `PixelSize::Quadrant` (4 terminal rows per glyph line).
const BANNER_HEIGHT: u16 = 3 * 4;

/// How long to wait for a keyboard event before redrawing anyway, so
/// externally-sourced log lines (tracing output from the simulator, and
/// eventually `ocpp-charge-point`) show up promptly even with no input.
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Default)]
pub struct App {
    pub screen: Screen,
    pub chargers: Vec<ChargerEntry>,
    pub selected_charger: usize,
    pub charger_state: Option<ChargerState>,
    pub focused_evse: usize,
    pub logs: LogBuffer,
    pub log_receiver: Option<UnboundedReceiver<String>>,
    pub command_palette_open: bool,
    pub command_palette_selected: usize,
    pub help_open: bool,
    pub quit_confirm_open: bool,
    pub status_message: Option<String>,
    pub connection_store: ConnectionStore,
    pub connection_store_path: Option<PathBuf>,
    pub connection_csms_url: TextField,
    pub connection_ocpp_identity: TextField,
    pub connection_password: TextField,
    pub connection_focused_field: usize,
    pub connect_result_receiver: Option<oneshot::Receiver<Result<(), String>>>,
    pub exit: bool,
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
            self.poll_connect_result();
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

    fn draw(&self, frame: &mut Frame) {
        if is_terminal_too_small(frame.area()) {
            self.render_too_small(frame);
            return;
        }

        match self.screen {
            Screen::PickCharger => self.render_picker(frame),
            Screen::ConnectionSetup => self.render_connection_setup(frame),
            Screen::Dashboard => {
                self.render_dashboard(frame);
                if self.command_palette_open {
                    self.render_command_palette(frame);
                }
            }
        }

        if self.help_open {
            self.render_help(frame);
        }
        if self.quit_confirm_open {
            self.render_quit_confirm(frame);
        }
    }

    fn render_too_small(&self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(
            Paragraph::new(format!(
                "Terminal too small.\nResize to at least {}x{}.",
                crate::dashboard::MIN_WIDTH,
                crate::dashboard::MIN_HEIGHT
            ))
            .alignment(Alignment::Center),
            area,
        );
    }

    fn render_picker(&self, frame: &mut Frame) {
        let layout = picker_layout(frame.area());
        self.render_banner(frame, layout.banner);
        self.render_charger_list(frame, layout.list);
    }

    fn render_banner(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let [_, banner, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(BANNER_HEIGHT),
            Constraint::Fill(1),
        ])
        .areas(area);

        let big_text = BigText::builder()
            .pixel_size(PixelSize::Quadrant)
            .style(Style::new().fg(BRAND_TEAL).add_modifier(Modifier::BOLD))
            .centered()
            .lines(vec!["CHARGE".into(), "POINT".into(), "SIMULATOR".into()])
            .build();

        frame.render_widget(big_text, banner);
    }

    fn render_charger_list(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let items: Vec<ListItem> = self
            .chargers
            .iter()
            .map(|entry| {
                let evse_count = entry.config.evses.len();
                ListItem::new(format!(
                    "{}  [{}]  {} EVSE{}",
                    entry.config.id,
                    entry.config.ocpp_version,
                    evse_count,
                    if evse_count == 1 { "" } else { "s" }
                ))
            })
            .collect();

        let list = List::new(items)
            .block(bordered_block("Select a charger"))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");

        let mut state = ListState::default();
        if !self.chargers.is_empty() {
            state.select(Some(self.selected_charger));
        }

        frame.render_stateful_widget(list, area, &mut state);
    }

    fn render_connection_setup(&self, frame: &mut Frame) {
        let layout = connection_setup_layout(frame.area());

        let field_block = |title: &'static str, focused: bool| {
            let style = if focused {
                Style::new().fg(BRAND_TEAL).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(BRAND_TEAL)
            };
            Block::bordered().title(title).border_style(style)
        };

        let fields: [(&str, &TextField, bool, Rect); 3] = [
            (
                "CSMS URL",
                &self.connection_csms_url,
                self.connection_focused_field == 0,
                layout.csms_url,
            ),
            (
                "OCPP Identity",
                &self.connection_ocpp_identity,
                self.connection_focused_field == 1,
                layout.ocpp_identity,
            ),
            (
                "Password",
                &self.connection_password,
                self.connection_focused_field == 2,
                layout.password,
            ),
        ];

        for (title, field, focused, area) in fields {
            let display_value = if title == "Password" {
                "*".repeat(field.value().chars().count())
            } else {
                field.value().to_string()
            };
            frame.render_widget(
                Paragraph::new(display_value).block(field_block(title, focused)),
                area,
            );
            if focused {
                frame.set_cursor_position((area.x + 1 + field.cursor() as u16, area.y + 1));
            }
        }

        frame.render_widget(
            Paragraph::new("Tab: next field  Enter: connect  Esc: cancel"),
            layout.hint,
        );
    }

    fn render_dashboard(&self, frame: &mut Frame) {
        let layout = dashboard_layout(frame.area());

        let overview_line = match &self.charger_state {
            Some(state) => Line::from(vec![
                Span::raw(format!(
                    "{}  |  {}  |  status: ",
                    state.config.id, state.config.ocpp_version
                )),
                Span::styled(
                    state.connection_status.to_string(),
                    Style::new().fg(connection_status_color(state.connection_status)),
                ),
            ]),
            None => Line::from("no charger selected"),
        };
        frame.render_widget(
            Paragraph::new(overview_line).block(bordered_block("Overview")),
            layout.overview,
        );

        let evse_strip_line = match &self.charger_state {
            Some(state) if !state.evses.is_empty() => {
                let spans: Vec<Span> = state
                    .evses
                    .iter()
                    .enumerate()
                    .flat_map(|(index, evse)| {
                        let label = format!(" EVSE {} ({}) ", evse.id, evse.connectors.len());
                        let style = if index == self.focused_evse {
                            Style::new().add_modifier(Modifier::REVERSED)
                        } else {
                            Style::new()
                        };
                        [Span::styled(label, style), Span::raw("  ")]
                    })
                    .collect();
                Line::from(spans)
            }
            _ => Line::from("no EVSEs"),
        };
        frame.render_widget(
            Paragraph::new(evse_strip_line)
                .block(bordered_block("EVSEs"))
                .wrap(Wrap { trim: true }),
            layout.evse_strip,
        );

        let evse_detail_title = match &self.charger_state {
            Some(state) if !state.evses.is_empty() => {
                format!("EVSE detail ({}/{})", self.focused_evse + 1, state.evses.len())
            }
            _ => "EVSE detail".to_string(),
        };
        let evse_detail_lines: Vec<Line> = match &self.charger_state {
            Some(state) if !state.evses.is_empty() => {
                let evse = &state.evses[self.focused_evse.min(state.evses.len() - 1)];
                let mut lines = vec![Line::from(format!(
                    "EVSE {}  |  {:.2} kW  {:.1} A  {:.3} kWh",
                    evse.id, evse.metrics.power_kw, evse.metrics.current_a, evse.metrics.energy_kwh
                ))];
                lines.extend(evse.connectors.iter().map(|connector| {
                    let vehicle = connector
                        .vehicle
                        .as_ref()
                        .map(|vehicle| format!(" - {}", vehicle.id))
                        .unwrap_or_default();
                    Line::from(vec![
                        Span::raw(format!("  connector {}: ", connector.id)),
                        Span::styled(
                            connector.status.to_string(),
                            Style::new().fg(connector_status_color(connector.status)),
                        ),
                        Span::raw(vehicle),
                    ])
                }));
                lines
            }
            Some(_) => vec![Line::from("no EVSEs configured")],
            None => vec![Line::from("no charger selected")],
        };
        frame.render_widget(
            Paragraph::new(evse_detail_lines).block(bordered_block(evse_detail_title)),
            layout.evse_detail,
        );

        let log_title = if self.logs.is_paused() {
            "Logs (scrolled up, paused)"
        } else {
            "Logs"
        };
        let log_height = layout.log.height.saturating_sub(2) as usize;
        let log_lines: Vec<Line> = self
            .logs
            .visible_lines(log_height)
            .into_iter()
            .map(Line::from)
            .collect();
        frame.render_widget(
            Paragraph::new(log_lines).block(bordered_block(log_title)),
            layout.log,
        );

        let command_bar_line = match &self.status_message {
            Some(message) => {
                let color = if message.starts_with('✗') { Color::Red } else { Color::Green };
                Line::styled(message.clone(), Style::new().fg(color))
            }
            None => Line::from(
                "q: quit  Esc: back  ←/→/Tab: focus EVSE  PgUp/PgDn: scroll logs  c: command  ?: help",
            ),
        };
        frame.render_widget(Paragraph::new(command_bar_line), layout.command_bar);
    }

    fn render_command_palette(&self, frame: &mut Frame) {
        let commands = self.available_commands();
        let area = frame.area();
        let width = area.width.min(50);
        let height = (commands.len() as u16 + 2).max(3).min(area.height);
        let popup = centered_rect(width, height, area);

        frame.render_widget(Clear, popup);

        let items: Vec<ListItem> = if commands.is_empty() {
            vec![ListItem::new("no commands available for this EVSE")]
        } else {
            commands.iter().map(|command| ListItem::new(command.label())).collect()
        };

        let list = List::new(items)
            .block(bordered_block("Command"))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");

        let mut state = ListState::default();
        if !commands.is_empty() {
            state.select(Some(self.command_palette_selected));
        }

        frame.render_stateful_widget(list, popup, &mut state);
    }

    fn render_help(&self, frame: &mut Frame) {
        let area = frame.area();
        let popup = centered_rect(area.width.min(56), area.height.min(14), area);
        frame.render_widget(Clear, popup);

        let text = "Global\n\
             \u{20}q            quit (confirm)\n\
             \u{20}?            toggle this help\n\n\
             Charger picker\n\
             \u{20}\u{2191}/\u{2193} or k/j    move selection\n\
             \u{20}Enter        select charger\n\n\
             Dashboard\n\
             \u{20}Esc          back to picker\n\
             \u{20}\u{2190}/\u{2192} or Tab   focus EVSE\n\
             \u{20}PgUp/PgDn    scroll logs\n\
             \u{20}c            open command palette";

        frame.render_widget(
            Paragraph::new(text).block(bordered_block("Help (Esc to close)")),
            popup,
        );
    }

    fn render_quit_confirm(&self, frame: &mut Frame) {
        let area = frame.area();
        let popup = centered_rect(area.width.min(34), 3, area);
        frame.render_widget(Clear, popup);

        frame.render_widget(
            Paragraph::new("Quit the simulator? (y/n)")
                .alignment(Alignment::Center)
                .block(bordered_block("Quit?")),
            popup,
        );
    }

    /// Commands eligible to run against the currently focused EVSE.
    fn available_commands(&self) -> Vec<Command> {
        let Some(state) = &self.charger_state else {
            return Vec::new();
        };
        let Some(evse) = state.evses.get(self.focused_evse) else {
            return Vec::new();
        };
        Command::ALL
            .into_iter()
            .filter(|command| command.is_available(evse))
            .collect()
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

    fn handle_key_event(&mut self, key_event: KeyEvent) {
        if self.quit_confirm_open {
            self.handle_quit_confirm_key(key_event);
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

        // On the connection setup screen 'q'/'?' need to be typeable (URLs and passwords can
        // contain either), so the global shortcuts don't apply there.
        if self.screen != Screen::ConnectionSetup {
            match key_event.code {
                KeyCode::Char('q') => {
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

    fn handle_help_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Esc | KeyCode::Char('?') => self.help_open = false,
            _ => {}
        }
    }

    fn handle_pick_charger_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Down | KeyCode::Char('j') => self.select_next_charger(),
            KeyCode::Up | KeyCode::Char('k') => self.select_previous_charger(),
            KeyCode::Enter => self.confirm_charger_selection(),
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
            KeyCode::Esc => self.return_to_picker(),
            KeyCode::PageUp => self.logs.scroll_up(),
            KeyCode::PageDown => self.logs.scroll_down(),
            KeyCode::Right | KeyCode::Tab => self.select_next_evse(),
            KeyCode::Left | KeyCode::BackTab => self.select_previous_evse(),
            KeyCode::Char('c') => self.open_command_palette(),
            _ => {}
        }
    }

    fn handle_command_palette_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Esc => self.close_command_palette(),
            KeyCode::Down => self.select_next_command(),
            KeyCode::Up => self.select_previous_command(),
            KeyCode::Enter => self.dispatch_selected_command(),
            _ => {}
        }
    }

    fn select_next_charger(&mut self) {
        if self.chargers.is_empty() {
            return;
        }
        if self.selected_charger + 1 < self.chargers.len() {
            self.selected_charger += 1;
        }
    }

    fn select_previous_charger(&mut self) {
        self.selected_charger = self.selected_charger.saturating_sub(1);
    }

    fn select_next_evse(&mut self) {
        let Some(state) = &self.charger_state else {
            return;
        };
        if self.focused_evse + 1 < state.evses.len() {
            self.focused_evse += 1;
        }
    }

    fn select_previous_evse(&mut self) {
        self.focused_evse = self.focused_evse.saturating_sub(1);
    }

    fn open_command_palette(&mut self) {
        if self.charger_state.is_none() {
            return;
        }
        self.command_palette_selected = 0;
        self.command_palette_open = true;
    }

    fn close_command_palette(&mut self) {
        self.command_palette_open = false;
    }

    fn select_next_command(&mut self) {
        let count = self.available_commands().len();
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

    fn dispatch_selected_command(&mut self) {
        let commands = self.available_commands();
        let command = commands.get(self.command_palette_selected).copied();
        self.close_command_palette();

        let Some(command) = command else {
            return;
        };
        let Some(state) = &mut self.charger_state else {
            return;
        };
        let Some(evse) = state.evses.get_mut(self.focused_evse) else {
            return;
        };
        if let Some(log_line) = command.apply(evse) {
            self.logs.push(log_line);
            self.status_message = Some(format!("✓ {}", command.label()));
        }
    }

    fn confirm_charger_selection(&mut self) {
        if let Some(charger) = self.chargers.get(self.selected_charger).cloned() {
            self.logs = LogBuffer::default();
            self.logs.push(format!("{} booting", charger.config.id));
            self.charger_state = Some(ChargerState::from_config(charger.config.clone()));
            self.focused_evse = 0;
            self.status_message = None;

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
                self.connection_password =
                    TextField::new(password).with_max_bytes(SecurityProfile::MAX_BASIC_PASSWORD_BYTES);
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

        self.screen = Screen::Dashboard;

        if !profile.csms_url.trim().is_empty() {
            let (sender, receiver) = oneshot::channel();
            self.connect_result_receiver = Some(receiver);
            // `connect_and_setup`'s future isn't `Send` (upstream uses non-Send sync
            // primitives internally), so it can't go through `tokio::spawn`. A dedicated
            // thread with its own single-threaded runtime sidesteps that: `block_on`
            // doesn't require `Send`.
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build a runtime for the CSMS connection attempt");
                let outcome = runtime.block_on(connect_charger(&config, &profile));
                let result = match &outcome {
                    Ok(_) => Ok(()),
                    Err(error) => Err(error.to_string()),
                };
                let _ = sender.send(result);
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
                self.status_message = Some("✓ connected to CSMS".to_string());
                self.connect_result_receiver = None;
            }
            Ok(Err(error)) => {
                self.status_message = Some(format!("✗ CSMS connection failed: {error}"));
                self.connect_result_receiver = None;
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.connect_result_receiver = None;
            }
        }
    }

    fn return_to_picker(&mut self) {
        self.charger_state = None;
        self.status_message = None;
        self.screen = Screen::PickCharger;
    }

    /// Pulls every line currently buffered in the tracing bridge's channel
    /// (if one is installed) into the log panel.
    fn drain_log_receiver(&mut self) {
        let Some(receiver) = &mut self.log_receiver else {
            return;
        };
        while let Ok(line) = receiver.try_recv() {
            self.logs.push(line);
        }
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 2;
    Rect::new(x, y, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use charge_point_simulator_core::charger::{
        ChargerConfig, ChargerSource, ConnectionStatus, EvseConfig, OcppVersion,
    };

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    fn charger(id: &str) -> ChargerEntry {
        charger_with_evses(id, vec![EvseConfig { id: 1, connectors: 1 }])
    }

    fn charger_with_evses(id: &str, evses: Vec<EvseConfig>) -> ChargerEntry {
        ChargerEntry {
            config: ChargerConfig {
                id: id.into(),
                ocpp_version: OcppVersion::V16J,
                evses,
            },
            source: ChargerSource::BuiltIn,
        }
    }

    fn charger_v21(id: &str) -> ChargerEntry {
        ChargerEntry {
            config: ChargerConfig {
                id: id.into(),
                ocpp_version: OcppVersion::V21,
                evses: vec![EvseConfig { id: 1, connectors: 1 }],
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
        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Char('q')));
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
        app.handle_key_event(key(KeyCode::Char('q')));
        app.handle_key_event(key(KeyCode::Char('y')));
        assert!(app.exit);

        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Char('q')));
        app.handle_key_event(key(KeyCode::Enter));
        assert!(app.exit);
    }

    #[test]
    fn n_or_esc_cancels_the_quit() {
        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Char('q')));
        app.handle_key_event(key(KeyCode::Char('n')));
        assert!(!app.quit_confirm_open);
        assert!(!app.exit);

        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Char('q')));
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
        assert_eq!(app.logs.visible_lines(10), vec!["CP001 booting"]);
    }

    #[test]
    fn confirming_a_selection_focuses_the_first_evse() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![
                EvseConfig { id: 1, connectors: 1 },
                EvseConfig { id: 2, connectors: 1 },
            ],
        )]);
        app.confirm_charger_selection();
        assert_eq!(app.focused_evse, 0);
    }

    #[test]
    fn right_and_tab_move_focus_to_the_next_evse_and_clamp_at_the_end() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![
                EvseConfig { id: 1, connectors: 1 },
                EvseConfig { id: 2, connectors: 1 },
            ],
        )]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Right));
        assert_eq!(app.focused_evse, 1);
        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(app.focused_evse, 1);
    }

    #[test]
    fn left_and_backtab_move_focus_to_the_previous_evse_and_clamp_at_the_start() {
        let mut app = App::new(vec![charger_with_evses(
            "CP001",
            vec![
                EvseConfig { id: 1, connectors: 1 },
                EvseConfig { id: 2, connectors: 1 },
            ],
        )]);
        app.confirm_charger_selection();
        app.focused_evse = 1;

        app.handle_key_event(key(KeyCode::Left));
        assert_eq!(app.focused_evse, 0);
        app.handle_key_event(key(KeyCode::BackTab));
        assert_eq!(app.focused_evse, 0);
    }

    #[test]
    fn evse_focus_navigation_on_a_charger_with_no_evses_does_not_panic() {
        let mut app = App::new(vec![charger_with_evses("CP001", vec![])]);
        app.confirm_charger_selection();

        app.handle_key_event(key(KeyCode::Right));
        app.handle_key_event(key(KeyCode::Left));
        assert_eq!(app.focused_evse, 0);
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
        app.handle_key_event(key(KeyCode::Enter));

        assert!(!app.command_palette_open);
        let state = app.charger_state.unwrap();
        assert_eq!(
            state.evses[0].connectors[0].status,
            charge_point_simulator_core::charger::ConnectorStatus::Occupied
        );
        assert!(state.evses[0].connectors[0].vehicle.is_some());
        assert!(app.logs.visible_lines(10).iter().any(|l| l.contains("plugged in")));
    }

    #[test]
    fn dispatching_a_command_updates_the_available_commands_for_the_next_open() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter)); // plug in vehicle

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
        assert_eq!(app.status_message, None);

        app.handle_key_event(key(KeyCode::Char('c')));
        app.handle_key_event(key(KeyCode::Enter));

        assert_eq!(app.status_message, Some("✓ Plug in vehicle".to_string()));
    }

    #[test]
    fn returning_to_the_picker_clears_any_status_message() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.status_message = Some("✓ Plug in vehicle".to_string());

        app.handle_key_event(key(KeyCode::Esc));
        assert_eq!(app.status_message, None);
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

        sender.send("first".to_string()).unwrap();
        sender.send("second".to_string()).unwrap();

        app.drain_log_receiver();
        assert_eq!(app.logs.visible_lines(10), vec!["first", "second"]);
    }

    #[test]
    fn draining_without_a_receiver_installed_does_nothing() {
        let mut app = App::new(vec![]);
        app.drain_log_receiver();
        assert_eq!(app.logs.visible_lines(10), Vec::<&str>::new());
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
        let remembered = app.connection_store.get("CP-2.1").unwrap();
        assert_eq!(remembered.csms_url, "ws://localhost:9999/dev");
        assert_eq!(remembered.ocpp_identity, "CP-2.1");
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
    fn poll_connect_result_reports_success() {
        let (sender, receiver) = oneshot::channel();
        let mut app = App::new(vec![]);
        app.connect_result_receiver = Some(receiver);
        sender.send(Ok(())).unwrap();

        app.poll_connect_result();

        assert_eq!(app.status_message, Some("✓ connected to CSMS".to_string()));
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
            app.status_message,
            Some("✗ CSMS connection failed: boom".to_string())
        );
        assert!(app.connect_result_receiver.is_none());
    }

    #[test]
    fn poll_connect_result_leaves_status_untouched_while_still_pending() {
        let (_sender, receiver) = oneshot::channel();
        let mut app = App::new(vec![]);
        app.connect_result_receiver = Some(receiver);

        app.poll_connect_result();

        assert_eq!(app.status_message, None);
        assert!(app.connect_result_receiver.is_some());
    }
}
