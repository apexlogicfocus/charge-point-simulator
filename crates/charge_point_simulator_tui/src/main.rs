mod screen;
mod app;
mod dashboard;
mod logs;
mod picker;
mod theme;

use std::path::PathBuf;

use charge_point_simulator_core::charger::all_chargers;
use color_eyre::Result;
use crate::app::App;

fn config_dir() -> PathBuf {
    std::env::var("FLOWION_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./chargers"))
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?; // augment errors / panics with easy to read messages
    let chargers = all_chargers(&config_dir());
    ratatui::run(|terminal| App::new(chargers).run(terminal))
}
