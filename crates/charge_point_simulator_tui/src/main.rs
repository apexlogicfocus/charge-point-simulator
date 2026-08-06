mod screen;
mod app;
mod fuzzy;
mod keybindings;
mod logs;
#[cfg(test)]
mod snapshot;
mod text_field;
mod theme;
mod tracing_bridge;
mod ui;

use std::path::PathBuf;

use charge_point_simulator_core::charger::{ConnectionStore, all_chargers};
use color_eyre::Result;
use crate::app::App;

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
    ratatui::run(|terminal| app.run(terminal))
}
