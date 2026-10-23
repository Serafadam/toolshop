pub mod app;
pub mod catalog;
pub mod cli;
pub mod container_builder;
pub mod distro;
pub mod engine;
pub mod event;
pub mod manifest;
pub mod theme;
pub mod toolbox;
pub mod tui;
pub mod ui;
pub mod update;

use std::time::Duration;

use color_eyre::Result;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::{sync::mpsc, time::MissedTickBehavior};

use app::{App, DistroRefresh};
use catalog::Catalog;
use cli::Command;
use event::{Event, EventHandler};
use toolbox::ToolboxScan;
use tui::Tui;
use update::update;

/// How long the terminal event thread waits between `Tick` events.
const TICK_RATE_MS: u64 = 250;
/// How often the UI is redrawn even when nothing happens.
const FRAME_TIME: Duration = Duration::from_millis(50);
/// Buffered log lines between the tasks and the UI.
const LOG_BUFFER: usize = 1_024;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = match cli::parse(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(err) => {
            eprintln!("error: {err}\n");
            eprint!("{}", cli::USAGE);
            std::process::exit(2);
        }
    };

    // Theme: an explicit flag wins, otherwise the saved preference (unless suppressed).
    if let Some(flavour) = cli.theme {
        theme::set_flavour(flavour);
    } else if cli.use_theme_config {
        for warning in theme::load() {
            eprintln!("warning: {warning}");
        }
    }

    // Headless modes: no terminal, no event loop.
    match &cli.command {
        Command::Help => {
            print!("{}", cli::USAGE);
            return Ok(());
        }
        Command::Version => {
            println!("toolshop {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Command::ListPackages => {
            print!("{}", cli::list_packages(&cli::load_catalog()?));
            return Ok(());
        }
        Command::ListDistros => {
            let (distros, warnings) = distro::available().await;
            for warning in warnings {
                eprintln!("warning: {warning}");
            }
            print!("{}", cli::list_distros(&distros));
            return Ok(());
        }
        Command::Print {
            distro: wanted,
            packages,
            all,
            user,
        } => {
            let catalog = cli::load_catalog()?;
            let (distros, warnings) = distro::available().await;
            for warning in warnings {
                eprintln!("warning: {warning}");
            }
            let distro = cli::pick_distro(&distros, wanted.as_deref())?;
            let (file, warnings) =
                cli::render_container_file(&catalog, &distro, packages, *all, user.as_deref())?;
            for warning in warnings {
                eprintln!("warning: {warning}");
            }
            print!("{file}");
            return Ok(());
        }
        Command::Interactive => {}
    }

    // The catalog must load: without packages there is nothing to build.
    let catalog = Catalog::load()?;
    let (distros, discovery_warnings) = distro::available().await;

    let (log_tx, mut log_rx) = mpsc::channel(LOG_BUFFER);
    let (distro_tx, mut distro_rx) = mpsc::channel(4);
    let (toolbox_tx, mut toolbox_rx) = mpsc::channel(4);
    let mut app = App::new(catalog, distros, log_tx, distro_tx, toolbox_tx);

    let catalog_warnings = app.catalog.warnings.clone();
    for warning in catalog_warnings.iter().chain(discovery_warnings.iter()) {
        app.append_log(format!("warning: {warning}"));
    }

    let backend = CrosstermBackend::new(std::io::stderr());
    let terminal = Terminal::new(backend)?;
    let events = EventHandler::new(TICK_RATE_MS);
    let mut tui = Tui::new(terminal, events);
    tui.enter()?;

    // Load previously built toolboxes right away, so the tab is populated.
    app.refresh_toolboxes();

    let result = run(
        &mut app,
        &mut tui,
        &mut log_rx,
        &mut distro_rx,
        &mut toolbox_rx,
    )
    .await;

    // The terminal must be restored even when the run loop fails.
    if let Err(err) = tui.exit() {
        eprintln!("failed to restore terminal: {err}");
    }
    result
}

async fn run(
    app: &mut App,
    tui: &mut Tui,
    log_rx: &mut mpsc::Receiver<String>,
    distro_rx: &mut mpsc::Receiver<DistroRefresh>,
    toolbox_rx: &mut mpsc::Receiver<ToolboxScan>,
) -> Result<()> {
    let mut frame_interval = tokio::time::interval(FRAME_TIME);
    frame_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_frame = std::time::Instant::now();

    while !app.should_quit {
        // Drain everything the background tasks produced since the last frame,
        // otherwise a chatty build fills its channel and stalls podman on a full pipe.
        while let Ok(line) = log_rx.try_recv() {
            app.append_log(line);
        }
        while let Ok(refresh) = distro_rx.try_recv() {
            app.apply_distro_refresh(refresh);
        }
        while let Ok(scan) = toolbox_rx.try_recv() {
            app.apply_toolbox_scan(scan);
        }
        app.on_tick();
        app.frame_dt = last_frame.elapsed();
        last_frame = std::time::Instant::now();
        tui.draw(app)?;

        tokio::select! {
            Some(line) = log_rx.recv() => app.append_log(line),
            Some(refresh) = distro_rx.recv() => app.apply_distro_refresh(refresh),
            Some(scan) = toolbox_rx.recv() => app.apply_toolbox_scan(scan),
            event = tui.events.next() => match event? {
                Event::Key(key) => update(app, key),
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                Event::Tick | Event::Resize(_, _) => {}
            },
            _ = frame_interval.tick() => {}
        }
    }
    Ok(())
}
