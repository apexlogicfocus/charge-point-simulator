mod connection_setup;
mod dashboard;
mod overlays;
mod palette;
mod picker;

use crate::app::App;
use crate::screen::Screen;
use ratatui::Frame;
use ratatui::layout::Rect;

pub(crate) fn draw(frame: &mut Frame, app: &App) {
    if dashboard::is_terminal_too_small(frame.area()) {
        overlays::render_too_small(frame);
        return;
    }

    match app.screen {
        Screen::PickCharger => picker::render(frame, app),
        Screen::ConnectionSetup => connection_setup::render(frame, app),
        Screen::Dashboard => {
            dashboard::render(frame, app);
            if app.command_palette_open {
                palette::render_command_palette(frame, app);
            }
            if let Some(command) = app.parameter_prompt {
                palette::render_parameter_prompt(frame, app, command);
            }
        }
    }

    if app.help_open {
        overlays::render_help(frame);
    }
    if app.quit_confirm_open {
        overlays::render_quit_confirm(frame);
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 2;
    Rect::new(x, y, width, height)
}
