mod app;
mod clipboard;
mod fuzzy;
mod keybindings;
mod logs;
mod screen;
#[cfg(test)]
mod snapshot;
mod text_field;
mod theme;
mod tracing_bridge;
mod ui;

use std::io::stdout;
use std::path::PathBuf;

use crate::app::App;
use charge_point_simulator_core::charger::{ConnectionStore, all_chargers};
use color_eyre::Result;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;

fn config_dir() -> PathBuf {
    std::env::var("FLOWION_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./chargers"))
}

fn connection_store_path() -> PathBuf {
    if let Ok(path) = std::env::var("FLOWION_STATE_DIR") {
        return PathBuf::from(path).join("connections.yaml");
    }
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("flowion-charge-point-simulator")
        .join("connections.yaml")
}

/// Initializes the terminal the same way [`ratatui::run`] would (raw mode, alternate screen, a
/// panic hook that restores both), plus mouse capture - which `ratatui::init`'s panic hook
/// doesn't know about, so it's enabled here and the panic hook is re-chained to disable it
/// before running whatever hook was already installed (see [`restore_terminal`] for the mirror
/// image on the normal exit path).
fn init_terminal() -> Result<ratatui::DefaultTerminal> {
    let terminal = ratatui::try_init()?;
    execute!(stdout(), EnableMouseCapture)?;

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        previous_hook(info);
    }));

    Ok(terminal)
}

/// Disables mouse capture before handing off to [`ratatui::restore`] for raw mode/alternate
/// screen - mouse capture must go first, the same "more side effects last" ordering
/// `ratatui::try_restore` uses internally for raw mode vs. the alternate screen.
fn restore_terminal() {
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?; // augment errors / panics with easy to read messages
    let log_receiver = tracing_bridge::install();
    let chargers = all_chargers(&config_dir());
    let connection_store_path = connection_store_path();

    let mut app = App::new(chargers);
    app.log_receiver = Some(log_receiver);
    app.connection_store = ConnectionStore::load(&connection_store_path);
    app.connection_store_path = Some(connection_store_path);

    let mut terminal = init_terminal()?;
    let result = app.run(&mut terminal);
    restore_terminal();
    result
}
