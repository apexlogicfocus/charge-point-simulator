mod screen;
mod app;

use std::time::Duration;

use color_eyre::Result;
use color_eyre::eyre::Context;
use crossterm::event::{self, KeyCode};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};
use crate::app::App;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?; // augment errors / panics with easy to read messages
    ratatui::run(|terminal| App::default().run(terminal))
}