use crate::screen::Screen;
use color_eyre::Result;
use ratatui::{DefaultTerminal, Frame};
use crossterm::event::{self, KeyCode, KeyEvent, KeyEventKind, Event};

#[derive(Debug, Clone, Default)]
pub struct App {
    pub screen: Screen,
    pub exit: bool
}

impl App {
    pub fn run(&self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            terminal.draw(|frame| self.draw(frame))?;
            if self.exit {
                break;
            }
        }
        Ok(())
    }

    fn draw(&self, frame: &mut Frame) {
        todo!()
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
        match key_event.code {
            KeyCode::Char('q') => {self.exit = true;},
            _ => {}
        }
    }
}