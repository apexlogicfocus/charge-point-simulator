use crate::dashboard::dashboard_layout;
use crate::screen::Screen;
use charge_point_simulator_core::charger::ChargerEntry;
use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

#[derive(Debug, Clone, Default)]
pub struct App {
    pub screen: Screen,
    pub chargers: Vec<ChargerEntry>,
    pub selected_charger: usize,
    pub active_charger: Option<ChargerEntry>,
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
            terminal.draw(|frame| self.draw(frame))?;
            if self.exit {
                break;
            }
            self.handle_events()?;
        }
        Ok(())
    }

    fn draw(&self, frame: &mut Frame) {
        match self.screen {
            Screen::PickCharger => self.render_picker(frame),
            Screen::Dashboard => self.render_dashboard(frame),
        }
    }

    fn render_picker(&self, frame: &mut Frame) {
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
            .block(Block::bordered().title("Select a charger"))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");

        let mut state = ListState::default();
        if !self.chargers.is_empty() {
            state.select(Some(self.selected_charger));
        }

        frame.render_stateful_widget(list, frame.area(), &mut state);
    }

    fn render_dashboard(&self, frame: &mut Frame) {
        let layout = dashboard_layout(frame.area());

        let overview = match &self.active_charger {
            Some(charger) => format!(
                "{}  |  {}  |  connection: unknown",
                charger.config.id, charger.config.ocpp_version
            ),
            None => "no charger selected".to_string(),
        };
        frame.render_widget(
            Paragraph::new(overview).block(Block::bordered().title("Overview")),
            layout.overview,
        );

        let evse_strip = match &self.active_charger {
            Some(charger) if !charger.config.evses.is_empty() => charger
                .config
                .evses
                .iter()
                .map(|evse| format!("EVSE {} ({} connectors)", evse.id, evse.connectors))
                .collect::<Vec<_>>()
                .join("   "),
            _ => "no EVSEs".to_string(),
        };
        frame.render_widget(
            Paragraph::new(evse_strip).block(Block::bordered().title("EVSEs")),
            layout.evse_strip,
        );

        frame.render_widget(Block::bordered().title("EVSE detail"), layout.evse_detail);
        frame.render_widget(Block::bordered().title("Logs"), layout.log);
        frame.render_widget(
            Paragraph::new("q: quit  Esc: back  c: command"),
            layout.command_bar,
        );
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
        if let KeyCode::Char('q') = key_event.code {
            self.exit = true;
            return;
        }

        match self.screen {
            Screen::PickCharger => self.handle_pick_charger_key(key_event),
            Screen::Dashboard => self.handle_dashboard_key(key_event),
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

    fn handle_dashboard_key(&mut self, key_event: KeyEvent) {
        if key_event.code == KeyCode::Esc {
            self.return_to_picker();
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

    fn confirm_charger_selection(&mut self) {
        if let Some(charger) = self.chargers.get(self.selected_charger) {
            self.active_charger = Some(charger.clone());
            self.screen = Screen::Dashboard;
        }
    }

    fn return_to_picker(&mut self) {
        self.active_charger = None;
        self.screen = Screen::PickCharger;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use charge_point_simulator_core::charger::{ChargerConfig, ChargerSource, OcppVersion};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    fn charger(id: &str) -> ChargerEntry {
        ChargerEntry {
            config: ChargerConfig {
                id: id.into(),
                ocpp_version: OcppVersion::V16J,
                evses: Vec::new(),
            },
            source: ChargerSource::BuiltIn,
        }
    }

    #[test]
    fn starts_on_the_charger_picker() {
        let app = App::new(vec![charger("CP001")]);
        assert_eq!(app.screen, Screen::PickCharger);
        assert_eq!(app.selected_charger, 0);
        assert_eq!(app.active_charger, None);
    }

    #[test]
    fn q_exits_from_either_screen() {
        let mut app = App::new(vec![charger("CP001")]);
        app.handle_key_event(key(KeyCode::Char('q')));
        assert!(app.exit);

        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Char('q')));
        assert!(app.exit);
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
        assert_eq!(app.active_charger, Some(charger("CP002")));
    }

    #[test]
    fn enter_with_no_chargers_available_does_nothing() {
        let mut app = App::new(vec![]);
        app.handle_key_event(key(KeyCode::Enter));
        assert_eq!(app.screen, Screen::PickCharger);
        assert_eq!(app.active_charger, None);
    }

    #[test]
    fn escape_on_the_dashboard_returns_to_the_picker() {
        let mut app = App::new(vec![charger("CP001")]);
        app.confirm_charger_selection();
        app.handle_key_event(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::PickCharger);
        assert_eq!(app.active_charger, None);
    }
}
