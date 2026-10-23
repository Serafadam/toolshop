use std::{
    thread,
    time::{Duration, Instant},
};

use color_eyre::{Result, eyre::eyre};
use ratatui::crossterm::event::{self, Event as CrosstermEvent, KeyEvent, MouseEvent};
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug)]
pub enum Event {
    Tick,
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize(u16, u16),
}

/// Reads terminal events on a dedicated thread and forwards them over an async channel.
///
/// Crossterm's `poll`/`read` are blocking, so they cannot live in the async runtime.
/// Events are pushed into an unbounded channel; the UI loop drains it once per frame.
#[derive(Debug)]
pub struct EventHandler {
    receiver: mpsc::UnboundedReceiver<Event>,
    /// Kept around so the reader thread is owned by this handler.
    #[allow(dead_code)]
    handler: thread::JoinHandle<()>,
}

impl EventHandler {
    pub fn new(tick_rate_ms: u64) -> Self {
        let tick_rate = Duration::from_millis(tick_rate_ms);
        let (sender, receiver) = mpsc::unbounded_channel();

        let handler = thread::spawn(move || {
            let mut last_tick = Instant::now();
            loop {
                let timeout = tick_rate
                    .checked_sub(last_tick.elapsed())
                    .unwrap_or(tick_rate);

                match event::poll(timeout) {
                    Ok(true) => match event::read() {
                        // Ignore release/repeat on Windows, but keep real presses.
                        Ok(CrosstermEvent::Key(e)) if e.kind == event::KeyEventKind::Press => {
                            if sender.send(Event::Key(e)).is_err() {
                                break;
                            }
                        }
                        Ok(CrosstermEvent::Mouse(e)) => {
                            if sender.send(Event::Mouse(e)).is_err() {
                                break;
                            }
                        }
                        Ok(CrosstermEvent::Resize(w, h)) => {
                            if sender.send(Event::Resize(w, h)).is_err() {
                                break;
                            }
                        }
                        // Paste / focus / key-release events are not interesting yet.
                        Ok(_) => {}
                        Err(_) => break,
                    },
                    Ok(false) => {}
                    Err(_) => break,
                }

                if last_tick.elapsed() >= tick_rate {
                    if sender.send(Event::Tick).is_err() {
                        break;
                    }
                    last_tick = Instant::now();
                }
            }
        });

        Self { receiver, handler }
    }

    /// Wait for the next terminal event.
    pub async fn next(&mut self) -> Result<Event> {
        self.receiver
            .recv()
            .await
            .ok_or_else(|| eyre!("terminal event handler stopped"))
    }
}
