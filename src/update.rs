use crate::app::App;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub async fn update(app: &mut App, key_event: KeyEvent) {
    match key_event.code {
        KeyCode::Esc | KeyCode::Char('q') => app.quit(),
        KeyCode::Char('c') | KeyCode::Char('C') if key_event.modifiers == KeyModifiers::CONTROL => {
            app.quit();
        }
        KeyCode::Up | KeyCode::Char('j') => app.move_up(),
        KeyCode::Down | KeyCode::Char('k') => app.move_down(),
        KeyCode::Right | KeyCode::Char('l') => app.focus_right(),
        KeyCode::Left | KeyCode::Char('h') => app.focus_left(),
        KeyCode::Enter => {
            app.toggle_option();
        }
        KeyCode::Tab => app.create_container_file(),
        KeyCode::Backspace => {
            app.run_build();
        }
        _ => {}
    }
}
