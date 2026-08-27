pub mod app;

pub mod event;

pub mod ui;

pub mod tui;

pub mod update;
pub mod container_builder;
use app::App;
use color_eyre::Result;
use event::{Event, EventHandler};
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc;
use tui::Tui;
use update::update;

#[tokio::main]
async fn main() -> Result<()> {
    let (tx, mut rx) = mpsc::channel(100);
    let mut app = App::new(tx);

    let backend = CrosstermBackend::new(std::io::stderr());
    let terminal = Terminal::new(backend)?;
    let events = EventHandler::new(250);
    let mut tui = Tui::new(terminal, events);
    tui.enter()?;

    while !app.should_quit {
        tui.draw(&mut app)?;
        tokio::select! {
               Some(log) = rx.recv() => {
            app.append_log(&log);
        },
        _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
            // Small delay to prevent busy-waiting
        }}
        match tui.events.next()? {
            Event::Tick => {}
            Event::Key(key_event) => update(&mut app, key_event).await,
            Event::Mouse(_) => {}
            Event::Resize(_, _) => {}
        };
    }

    tui.exit()?;
    Ok(())
}
