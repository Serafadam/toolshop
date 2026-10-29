use std::{borrow::Cow, collections::VecDeque};

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Tabs, Wrap,
    },
};

use crate::{
    app::{App, BuildPhase, ContainerHighlight, DistroRow, Focus, PackageRow, View},
    container_builder::ContainerBuilder,
    engine,
    theme::THEME,
    toolbox::{Toolbox, ToolboxStatus},
};

/// Border block for a pane, themed by focus state.
fn pane_block(title: String, focused: bool) -> Block<'static> {
    Block::default()
        .title(Line::styled(
            format!(" {title} "),
            THEME.pane_title(focused),
        ))
        .borders(Borders::ALL)
        .border_type(THEME.pane_border_type(focused))
        .border_style(THEME.pane_border(focused))
}

fn render_list_widget<'a>(
    title: String,
    items: impl Iterator<Item = (Cow<'a, str>, Style)>,
    state: &mut ListState,
    focused: bool,
    area: Rect,
    frame: &mut Frame,
) {
    // Borrowed labels: no per-frame clone of the underlying catalog.
    let items: Vec<ListItem> = items
        .map(|(label, style)| ListItem::new(Line::styled(label, style)))
        .collect();

    let list = List::new(items)
        .block(pane_block(title, focused))
        .highlight_symbol("> ")
        .highlight_style(THEME.cursor());

    frame.render_stateful_widget(list, area, state);
}

fn render_containerfile_widget(
    builder: &ContainerBuilder,
    highlight: &ContainerHighlight,
    focused: bool,
    scroll: u16,
    area: Rect,
    frame: &mut Frame,
) {
    let container_file = builder.get_container_file();
    let (mut title, text) = if container_file.trim().is_empty() {
        (
            "Containerfile".to_string(),
            Text::from("Select a distro (Enter) to start"),
        )
    } else {
        let title = match builder.distro() {
            Some(distro) => format!(
                "Containerfile — {} ({})",
                engine::normalize_image(&distro.image),
                distro.family.label()
            ),
            None => "Containerfile".to_string(),
        };

        // Styling only: the file itself stays plain text.
        let lines: Vec<Line> = builder
            .container_lines()
            .iter()
            .map(|line| Line::styled(line.text.as_str(), container_line_style(line, highlight)))
            .collect();
        (title, Text::from(lines))
    };

    if scroll > 0 {
        title = format!("{title} · line {}", scroll + 1);
    }

    // No paragraph-level style: the per-line styles above must win.
    let paragraph = Paragraph::new(text)
        .block(pane_block(title, focused))
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

/// Highlight the lines that install `highlight.primary` (and its dependencies).
fn container_line_style(
    line: &crate::container_builder::ContainerLine,
    highlight: &ContainerHighlight,
) -> Style {
    let belongs_to_primary = highlight
        .primary
        .as_deref()
        .is_some_and(|primary| line.packages.iter().any(|package| package == primary));

    if belongs_to_primary {
        THEME.container_primary()
    } else if line
        .packages
        .iter()
        .any(|package| highlight.dependencies.contains(package))
    {
        THEME.container_dependency()
    } else {
        THEME.text()
    }
}

/// Colour a log line by what it says, so failures jump out of the stream.
fn log_line_style(line: &str) -> Style {
    let line = line.trim_start();
    if line.starts_with("Build failed") {
        THEME.log_banner()
    } else if line.starts_with("Build finished") {
        THEME.log_success()
    } else if line.starts_with("Build cancelled") || line.starts_with("Cancelling build") {
        THEME.log_warning()
    } else if line.contains("level=error")
        || line.starts_with("error:")
        || line.starts_with("Error:")
        || line.starts_with("cannot ")
    {
        THEME.log_error()
    } else if line.contains("level=warning") || line.starts_with("warning:") {
        THEME.log_warning()
    } else if line.starts_with("STEP ") || line.starts_with("COMMIT") || line.starts_with("--> ") {
        THEME.log_step()
    } else {
        THEME.muted()
    }
}

/// Text shown in the log pane title: the build progress when there is one.
fn progress_title(summary: &str) -> String {
    if summary.is_empty() {
        "build output".to_string()
    } else {
        summary.to_string()
    }
}

fn render_logs_widget(
    logs: &VecDeque<String>,
    summary: &str,
    scroll: usize,
    focused: bool,
    area: Rect,
    frame: &mut Frame,
) {
    let inner_height = area.height.saturating_sub(2) as usize;
    // `scroll` counts lines up from the bottom of the buffer.
    let offset = logs
        .len()
        .saturating_sub(inner_height)
        .saturating_sub(scroll);

    let lines: Vec<Line> = logs
        .iter()
        .map(|line| Line::styled(line.as_str(), log_line_style(line)))
        .collect();
    let summary = progress_title(summary);
    let title = format!("Logs — {summary} ({scroll} up)");

    // No wrapping on purpose: `Paragraph::scroll` counts *visual* rows, so wrapped lines
    // would make the offset computed from log lines land in the wrong place and the view
    // would jump around while output streams in. Long lines are clipped instead.
    let paragraph = Paragraph::new(Text::from(lines))
        .block(pane_block(title, focused))
        .scroll((offset as u16, 0));

    frame.render_widget(paragraph, area);
}

pub fn render(app: &mut App, frame: &mut Frame) {
    let [tabs_area, body_area, footer_area] = frame.area().layout(&Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ]));

    render_tabs(app, tabs_area, frame);

    match app.view {
        View::Builder => {
            render_builder(app, body_area, frame);
        }
        View::Toolboxes => {
            render_toolboxes(app, body_area, frame);
        }
        View::Logs => {
            render_logs_view(app, body_area, frame);
        }
    }

    render_footer(app, footer_area, frame);

    let dt = app.frame_dt;

    // While an image is being built a highlight sweeps along every pane border, so it is
    // obvious something is happening even when the log is not in view. Each effect carries
    // its own area (see `App::sync_build_effects`), so one pass advances them all.
    if app.view == View::Builder && app.info_open {
        let area = render_package_info(app, body_area, frame);
        app.hit.info_popup = area;
        if app.animation
            && let Some(effect) = &mut app.info_effect
        {
            effect.process(dt, frame.buffer_mut(), area);
            if effect.done() {
                app.info_effect = None;
            }
        }
    }

    // The input overlay is modal and is usually opened *from* the info popup (`m`, `e`,
    // `v`, `r`), so it has to be painted last: drawn before the popup, it was invisible
    // behind it and the keys seemed to do nothing.
    if app.prompt_is_open() {
        render_prompt(app, footer_area.union(body_area), frame);
    }
}

/// Checkboxes for the global build options.
fn render_options_widget(app: &App, area: Rect, frame: &mut Frame) {
    let focused = app.focus == Focus::Options;
    let items: Vec<ListItem> = app
        .options_rows()
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let style = if focused && index == app.options_cursor {
                THEME.cursor()
            } else if row.enabled {
                THEME.selected()
            } else {
                THEME.muted()
            };
            // Value rows are prompts, not checkboxes: the ellipsis hints at that.
            let text = if row.editable {
                format!("{} …", row.label)
            } else {
                row.label.clone()
            };
            ListItem::new(Line::styled(text, style))
        })
        .collect();

    let block = pane_block("Options".to_string(), focused);
    frame.render_widget(List::new(items).block(block), area);
}

/// Centered description window for the package under the cursor.
fn render_package_info(app: &App, area: Rect, frame: &mut Frame) -> Rect {
    let Some(info) = app.package_info() else {
        return Rect::ZERO;
    };

    let title = format!(" {} ", info.name);
    let version = if info.version.is_empty() {
        String::new()
    } else {
        format!(" {}", info.version)
    };
    let requires = if info.requires.is_empty() {
        "none".to_string()
    } else {
        info.requires.join(", ")
    };

    let mut lines = vec![
        Line::from(vec![Span::styled(
            info.summary.as_str(),
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        )]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("category  ", label_style()),
            Span::raw(info.category.as_str()),
            Span::styled("   version  ", label_style()),
            Span::raw(if version.is_empty() {
                "-".to_string()
            } else {
                version.trim().to_string()
            }),
        ]),
        Line::from(vec![
            Span::styled("installs  ", label_style()),
            Span::styled(
                info.method.label(),
                if info.overridden {
                    THEME.warning()
                } else {
                    THEME.text()
                },
            ),
            Span::styled("   on  ", label_style()),
            Span::raw(info.distro.clone().unwrap_or_else(|| "-".to_string())),
            if info.overridden {
                Span::styled("   (overridden, r resets)", label_style())
            } else {
                Span::raw("")
            },
        ]),
        Line::from(vec![
            Span::styled("command   ", label_style()),
            Span::raw(info.value.as_str()),
        ]),
        Line::from(vec![
            Span::styled("requires  ", label_style()),
            Span::raw(requires),
        ]),
        Line::from(vec![
            Span::styled("state     ", label_style()),
            if info.selected {
                Span::styled("selected", THEME.success())
            } else {
                Span::styled("not selected", THEME.muted())
            },
        ]),
    ];

    if let Some(script) = &info.script {
        lines.push(Line::raw(""));
        lines.push(Line::styled("script", label_style()));
        for line in script.lines().take(6) {
            lines.push(Line::styled(
                format!("  {line}"),
                Style::new().fg(Color::DarkGray),
            ));
        }
    }

    if !info.available {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "not available on the selected distro",
            THEME.warning(),
        ));
    }

    // Height follows the content, so the window stays small.
    let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup = centered_fixed(area, 62, height);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Line::styled(title, THEME.window_title()))
        .title_bottom(Line::styled(
            " m method · e edit · v version · r reset · i/Esc close ",
            THEME.muted(),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(THEME.window_border())
        .style(THEME.surface())
        .padding(ratatui::widgets::Padding::horizontal(1));

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        popup,
    );
    popup
}

/// Single-line input overlay: image reference plus family, or an install command.
fn render_prompt(app: &App, area: Rect, frame: &mut Frame) {
    let Some(prompt) = &app.prompt else {
        return;
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("› ", THEME.info()),
            Span::styled(prompt.input.as_str(), THEME.text()),
            Span::styled("▏", THEME.info()),
        ]),
        Line::raw(""),
    ];

    if let Some(image) = prompt.toolbox_image() {
        lines.push(Line::from(vec![
            Span::styled("image  ", label_style()),
            Span::styled(image, THEME.info()),
        ]));
    }

    if let Some(family) = prompt.family() {
        lines.push(Line::from(vec![
            Span::styled("type  ", label_style()),
            Span::styled(format!("‹ {} ›", family.label()), THEME.warning()),
            Span::styled("   (Tab/←→)", label_style()),
        ]));
    }

    let height = lines.len() as u16 + 2;
    let popup = centered_fixed(area, 60, height);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Line::styled(
            format!(" {} ", prompt.title),
            THEME.window_title(),
        ))
        .title_bottom(Line::styled(" Enter confirm · Esc cancel ", THEME.muted()))
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(THEME.window_border())
        .style(THEME.surface())
        .padding(ratatui::widgets::Padding::horizontal(1));

    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn label_style() -> Style {
    THEME.label()
}

/// Rect of the given width percentage and exact height, centered in `area`.
fn centered_fixed(area: Rect, percent_x: u16, height: u16) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Length(area.height.saturating_sub(height) / 2),
        Constraint::Length(height),
        Constraint::Min(0),
    ])
    .split(area);

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

/// Bottom bar: key hints on the left, build state on the right.
fn render_footer(app: &App, area: Rect, frame: &mut Frame) {
    let hints = match app.view {
        _ if app.prompt_is_open() => "Enter confirm · Esc cancel · Ctrl+U clear",
        View::Builder if app.info_open => {
            "m method · e edit · r reset · i/Esc close · Tab switch · q quit"
        }
        View::Builder => {
            "Tab switch · ↑↓ move · ←→ focus · Enter select/fold · / filter · z fold all · C-a select all · i info · n name · a image · L logs · Backspace build · q quit"
        }
        View::Toolboxes => {
            "Tab switch · ↑↓/jk move · c create · x remove · d forget · R rescan · q quit"
        }
        View::Logs => "Tab switch · ↑↓ scroll · L back to the builder · q quit",
    };

    // Right-hand side: a progress gauge while building, a short status otherwise.
    let building = app.build.is_active();
    let label = if building {
        match &app.pending_toolbox {
            Some(name) => format!("{} → {name}", app.build.summary()),
            None => app.build.summary(),
        }
    } else {
        match (app.build.phase, &app.pending_toolbox) {
            (BuildPhase::Idle, Some(name)) => format!("toolbox: {name}"),
            (BuildPhase::Idle, None) => "idle".to_string(),
            (_, Some(name)) => format!("{} · toolbox: {name}", app.build.summary()),
            (_, None) => app.build.summary(),
        }
    };

    let status_width = (label.chars().count() as u16 + 4).clamp(11, 52);

    // One glyph per fact: running, warnings, errors, pending toolbox name.
    let mut indicators: Vec<Span> = Vec::new();
    if app.build.is_active() {
        indicators.push(Span::styled("●", THEME.status_running()));
    }
    if app.build.warnings > 0 {
        indicators.push(Span::styled("▲", THEME.warning()));
    }
    if app.build.errors > 0 {
        indicators.push(Span::styled("✖", THEME.error()));
    }
    if app.pending_toolbox.is_some() {
        indicators.push(Span::styled("⚑", THEME.info()));
    }
    // One extra column keeps the glyphs from touching the gauge.
    let indicator_width = indicators.len() as u16 + u16::from(!indicators.is_empty());

    let [hint_area, indicator_area, status_area] = area.layout(&Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(indicator_width),
        Constraint::Length(status_width),
    ]));
    frame.render_widget(
        Paragraph::new(Line::styled(hints, THEME.muted())),
        hint_area,
    );
    if !indicators.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(indicators)).alignment(Alignment::Left),
            indicator_area,
        );
    }

    if building {
        // Unknown step counts get a sliver instead of a misleading empty bar.
        let ratio = app.build.ratio().unwrap_or(0.05);
        let gauge = Gauge::default()
            .gauge_style(THEME.gauge())
            .ratio(ratio)
            .label(Span::styled(label, THEME.gauge_label()));
        frame.render_widget(gauge, status_area);
    } else {
        let style = match app.build.phase {
            BuildPhase::Failed => THEME.error(),
            BuildPhase::Succeeded => THEME.status_idle(),
            BuildPhase::Cancelled => THEME.warning(),
            _ if app.pending_toolbox.is_some() => THEME.info(),
            _ => THEME.status_idle(),
        };
        frame.render_widget(
            Paragraph::new(Line::styled(label, style)).alignment(Alignment::Right),
            status_area,
        );
    }
}

/// Tab label, with a marker on the log tab while a build is running.
fn tab_title(app: &App, view: View) -> String {
    match view {
        View::Logs if app.build.is_active() => match app.build.ratio() {
            Some(ratio) => format!("{} {}%", view.label(), (ratio * 100.0) as u16),
            None => format!("{} ●", view.label()),
        },
        _ => view.label().to_string(),
    }
}

fn render_tabs(app: &mut App, area: Rect, frame: &mut Frame) {
    let titles: Vec<Line> = View::ALL
        .iter()
        .map(|view| Line::raw(tab_title(app, *view)))
        .collect();

    // Mirrors `Tabs` layout (left pad, title, right pad, then a one-cell divider) so the
    // recorded rects line up with what is drawn.
    let mut x = area.x;
    app.hit.tabs.clear();
    for (index, view) in View::ALL.iter().enumerate() {
        let width = tab_title(app, *view).chars().count() as u16 + 2;
        app.hit.tabs.push((
            *view,
            Rect {
                x,
                y: area.y,
                width,
                height: area.height,
            },
        ));
        x = x.saturating_add(width);
        if index + 1 < View::ALL.len() {
            x = x.saturating_add(1);
        }
    }

    let tabs = Tabs::new(titles)
        .select(app.view_index())
        .divider("│")
        .padding(" ", " ")
        .style(THEME.tab_inactive())
        .highlight_style(THEME.tab_active());
    frame.render_widget(tabs, area);

    // The flavour names itself on the right, where it cannot collide with the tabs.
    let name = crate::theme::flavour().name();
    let width = name.chars().count() as u16 + 2;
    if area.width > width + 40 {
        let chip = Rect {
            x: area.right().saturating_sub(width),
            y: area.y,
            width,
            height: area.height,
        };
        frame.render_widget(
            Paragraph::new(Line::styled(name, THEME.muted())).alignment(Alignment::Right),
            chip,
        );
    }
}

fn render_builder(app: &mut App, area: Rect, frame: &mut Frame) {
    let layout = Layout::horizontal([
        Constraint::Percentage(20),
        Constraint::Percentage(30),
        Constraint::Percentage(50),
    ])
    .spacing(1);
    let [left_area, apps_area, container_area] = area.layout(&layout);

    // The left column holds the options block and, under it, the distro list.
    let [options_area, distros_area] = left_area.layout(&Layout::vertical([
        Constraint::Length(App::OPTIONS_ROWS as u16 + 2),
        Constraint::Min(0),
    ]));
    app.hit.distros = distros_area;
    app.hit.options = options_area;
    app.hit.packages = apps_area;
    app.hit.containerfile = container_area;

    let selected_distro = app.selected_distro;
    render_list_widget(
        "Distro".to_string(),
        app.distro_rows.iter().map(|row| {
            let style = match row {
                DistroRow::Header { accent, .. } => THEME.heading(*accent),
                DistroRow::Distro { index, .. } if selected_distro == Some(*index) => {
                    THEME.selected()
                }
                DistroRow::Distro { .. } => Style::new(),
            };
            (Cow::Borrowed(row.label()), style)
        }),
        &mut app.distro_list_state,
        app.focus == Focus::Distro,
        distros_area,
        frame,
    );

    let apps_title = if app.filter_active() {
        format!(
            "Packages ({}/{} · {:?}) — Esc clears",
            app.selected_packages.len(),
            app.visible_packages.len(),
            app.filter.trim()
        )
    } else {
        format!(
            "Packages ({}/{}) — i info · / filter",
            app.selected_packages.len(),
            app.visible_packages.len()
        )
    };
    let visible = &app.visible_packages;
    let catalog = &app.catalog;
    let checked = &app.selected_packages;
    render_list_widget(
        apps_title,
        app.package_rows.iter().map(|row| {
            if let PackageRow::Header { accent, .. } = row {
                return (row.display_label(), THEME.heading(*accent));
            }
            let is_checked = match row {
                PackageRow::Package { visible: index, .. } => visible
                    .get(*index)
                    .and_then(|package_index| catalog.packages.get(*package_index))
                    .is_some_and(|package| checked.contains(&package.name)),
                PackageRow::Header { .. } => false,
            };
            (
                row.display_label(),
                if is_checked {
                    THEME.selected()
                } else {
                    Style::new()
                },
            )
        }),
        &mut app.apps_list_state,
        app.focus == Focus::Apps,
        apps_area,
        frame,
    );

    // Never let the pane scroll past the end of the file: a stale offset used to hide
    // the `FROM` line after switching distros.
    let file_lines = app.builder.get_container_file().lines().count() as u16;
    let max_scroll = file_lines.saturating_sub(container_area.height.saturating_sub(2));
    app.container_scroll = app.container_scroll.min(max_scroll);

    let highlight = app.container_highlight();
    render_options_widget(app, options_area, frame);

    render_containerfile_widget(
        &app.builder,
        &highlight,
        app.focus == Focus::Container,
        app.container_scroll,
        container_area,
        frame,
    );
}

/// The build log on its own, full window (`L`).
fn render_logs_view(app: &mut App, area: Rect, frame: &mut Frame) {
    app.hit.logs = area;
    app.log_view_height = area.height.saturating_sub(2) as usize;
    app.log_scroll = app.log_scroll.min(app.max_log_scroll());
    render_logs_widget(
        &app.logs,
        &app.build.summary(),
        app.log_scroll,
        true,
        area,
        frame,
    );
}

fn render_toolboxes(app: &mut App, area: Rect, frame: &mut Frame) {
    let layout =
        Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)]).spacing(1);
    let [list_area, details_area] = area.layout(&layout);
    app.hit.toolboxes = list_area;
    app.hit.toolbox_details = details_area;

    let title = format!("Toolboxes ({})", app.toolboxes.len());
    let labels: Vec<String> = app.toolboxes.iter().map(Toolbox::label).collect();
    let statuses: Vec<&ToolboxStatus> = app
        .toolboxes
        .iter()
        .map(|toolbox| &toolbox.status)
        .collect();

    render_list_widget(
        title,
        labels.iter().zip(statuses.iter()).map(|(label, status)| {
            let style = match status {
                ToolboxStatus::Present => THEME.success(),
                ToolboxStatus::Modified { .. } => THEME.warning(),
                ToolboxStatus::Missing => THEME.muted(),
                ToolboxStatus::Unknown => THEME.label(),
            };
            (Cow::Borrowed(label.as_str()), style)
        }),
        &mut app.toolbox_state,
        true,
        list_area,
        frame,
    );

    let details = match app.selected_toolbox() {
        Some(toolbox) => toolbox_details(toolbox),
        None if app.toolboxes.is_empty() => {
            "No toolboxes yet.\n\nBuild an image in the Builder tab (Backspace); every \
             build records a manifest in the cache so it shows up here."
                .to_string()
        }
        None => "Select a toolbox with ↑/↓.".to_string(),
    };

    let paragraph = Paragraph::new(details)
        .style(THEME.text())
        .block(pane_block(
            "Details — c create toolbox, x remove, d forget, R rescan, PgUp/PgDn".to_string(),
            false,
        ))
        .scroll((app.toolbox_scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, details_area);
}

fn toolbox_details(toolbox: &Toolbox) -> String {
    let containers = if toolbox.containers.is_empty() {
        "none — create one with `toolbox create --image <image>`".to_string()
    } else {
        toolbox.containers.join(", ")
    };

    let Some(manifest) = &toolbox.manifest else {
        return format!(
            "Image:      {}\nOrigin:     not built by toolshop — no manifest, tool list unknown\n\
             Status:     {}\nContainers: {}\n\n\
             Use the Builder tab to build a toolshop image for a tracked package list.\n",
            toolbox.image,
            status_text(&toolbox.status, false),
            containers,
        );
    };

    let mut text = format!(
        "Image:      {}\nOrigin:     toolshop build v{}\nDistro:     {} ({}) from {}\n\
         Created:    {} ({})\nStatus:     {}\nContainers: {}\nManifest:   {}\n\nPackages ({}):\n",
        manifest.image,
        manifest.tool_version,
        manifest.distro.name,
        manifest.distro.family,
        manifest.distro.image,
        crate::toolbox::humanize_age(manifest.age_seconds()),
        manifest.created_at,
        status_text(&toolbox.status, true),
        containers,
        crate::manifest::path_for(&crate::manifest::cache_dir(), &manifest.image).display(),
        manifest.packages.len(),
    );

    if manifest.packages.is_empty() {
        text.push_str("  (no packages)\n");
    }
    for package in &manifest.packages {
        let version = if package.version.is_empty() {
            String::new()
        } else {
            format!(" {}", package.version)
        };
        text.push_str(&format!(
            "  {:<20}{}{}\n",
            package.name,
            version,
            package.install_label()
        ));
    }

    text
}

/// Human readable status; `tracked` is false for images without a manifest.
fn status_text(status: &ToolboxStatus, tracked: bool) -> String {
    match status {
        ToolboxStatus::Present if tracked => "present — image matches the manifest".to_string(),
        ToolboxStatus::Present => "present — image exists".to_string(),
        ToolboxStatus::Modified { current_id } => {
            format!("modified — image is now {current_id}, rebuilt outside toolshop")
        }
        ToolboxStatus::Missing => "missing — the image is no longer in the engine".to_string(),
        ToolboxStatus::Unknown => "unknown — could not query the engine".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::{App, View},
        catalog::{Catalog, DEFAULT_INSTALL_KEY, InstallSpec, Package},
        distro::Distro,
        manifest::{Manifest, ManifestDistro, ManifestPackage},
        toolbox::{Toolbox, ToolboxScan, ToolboxStatus},
    };
    use ratatui::{Terminal, backend::TestBackend};
    use std::collections::BTreeMap;
    use tokio::sync::mpsc;

    fn pkg(name: &str, category: &str, requires: &[&str]) -> Package {
        Package {
            name: name.to_string(),
            version: String::new(),
            summary: format!("{name} summary"),
            category: category.to_string(),
            requires: requires.iter().map(|req| (*req).to_string()).collect(),
            install: BTreeMap::from([(
                DEFAULT_INSTALL_KEY.to_string(),
                InstallSpec::PackageManager {
                    pkg: name.to_string(),
                },
            )]),
            env: BTreeMap::new(),
        }
    }

    /// htop requires wget, so highlighting can distinguish primary and dependency lines.
    fn app() -> App {
        let catalog = Catalog {
            packages: vec![pkg("htop", "Shell", &["wget"]), pkg("wget", "Network", &[])],
            warnings: vec![],
        };
        let (log_tx, _log_rx) = mpsc::channel(8);
        let (distro_tx, _distro_rx) = mpsc::channel(8);
        let (toolbox_tx, _toolbox_rx) = mpsc::channel(8);
        App::new(catalog, Distro::predefined(), log_tx, distro_tx, toolbox_tx)
    }

    fn toolbox(image: &str, status: ToolboxStatus) -> Toolbox {
        Toolbox::built(
            Manifest {
                version: crate::manifest::MANIFEST_VERSION,
                image: image.to_string(),
                image_id: Some("sha256:aaaa11112222".to_string()),
                distro: ManifestDistro {
                    name: "jazzy".to_string(),
                    image: "docker.io/library/ros:jazzy-ros-base".to_string(),
                    family: "ros".to_string(),
                },
                packages: vec![
                    ManifestPackage {
                        name: "htop".to_string(),
                        version: String::new(),
                        pkg: Some("htop".to_string()),
                        script: None,
                    },
                    ManifestPackage {
                        name: "rust".to_string(),
                        version: String::new(),
                        pkg: None,
                        script: Some("curl https://sh.rustup.rs | sh -s -- -y".to_string()),
                    },
                ],
                created_at: 1,
                tool_version: "test".to_string(),
            },
            status,
            vec!["dev".to_string()],
        )
    }

    /// Render one frame and return the buffer, for assertions on styles.
    fn screen_state(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(app, frame)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// Render one frame and return the visible text.
    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(app, frame)).unwrap();
        let buffer = terminal.backend().buffer();

        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn builder_tab_shows_three_panes_and_no_log() {
        let mut app = app();
        app.append_log("STEP 1/1: something".to_string());
        let screen = screen(&mut app, 160, 30);

        assert!(screen.contains("Builder"), "{screen}");
        assert!(screen.contains("Toolboxes"), "{screen}");
        assert!(screen.contains("Logs"), "the Logs tab stays in the tab bar");
        assert!(screen.contains("Distro"), "{screen}");
        assert!(screen.contains("Packages"), "{screen}");
        assert!(screen.contains("Containerfile"), "{screen}");
        assert!(screen.contains("jazzy"), "{screen}");

        // The log itself lives in its own tab now.
        assert!(!screen.contains("Logs — build output"), "{screen}");
        assert!(!screen.contains("STEP 1/1"), "{screen}");
    }

    #[test]
    fn package_pane_shows_category_headings() {
        let mut app = app();
        let screen = screen(&mut app, 160, 30);
        assert!(screen.contains("Shell"), "{screen}");
        assert!(screen.contains("htop"), "{screen}");
        assert!(screen.contains("i info"), "{screen}");
        assert!(screen.contains("Tab switch"), "{screen}");
        assert!(screen.contains("idle"), "{screen}");
    }

    #[test]
    fn hiding_local_images_shortens_the_distro_list_and_shows_in_the_options() {
        use crate::distro::{DistroFamily, DistroSource};

        let catalog = Catalog {
            packages: vec![pkg("htop", "Shell", &[]), pkg("wget", "Network", &[])],
            warnings: vec![],
        };
        let mut distros = Distro::predefined();
        distros.push(Distro::new(
            DistroFamily::Ubuntu,
            "local-24.04",
            "localhost/my-ubuntu:24.04",
            DistroSource::Local,
        ));
        let (log_tx, _log_rx) = mpsc::channel(8);
        let (distro_tx, _distro_rx) = mpsc::channel(8);
        let (toolbox_tx, _toolbox_rx) = mpsc::channel(8);
        let mut app = App::new(catalog, distros, log_tx, distro_tx, toolbox_tx);

        let shown = screen(&mut app, 160, 40);
        assert!(shown.contains("my-ubuntu:24.04 (local)"), "{shown}");
        assert!(shown.contains("[x] local images"), "{shown}");

        app.toggle_local_images();
        let hidden = screen(&mut app, 160, 40);
        assert!(!hidden.contains("my-ubuntu"), "{hidden}");
        assert!(!hidden.contains("(local)"), "{hidden}");
        assert!(hidden.contains("[ ] local images"), "{hidden}");
        // Predefined distros are untouched.
        assert!(hidden.contains("jazzy"), "{hidden}");
    }

    #[test]
    fn the_filter_shows_in_the_packages_title() {
        let mut app = app();
        select_jazzy(&mut app);
        let plain = screen(&mut app, 150, 24);
        assert!(plain.contains("Packages (0/2)"), "{plain}");
        assert!(plain.contains("/ filter"), "{plain}");

        app.set_filter("wget");
        let filtered = screen(&mut app, 150, 24);
        assert!(filtered.contains("Packages (0/1 · \"wget\")"), "{filtered}");
        assert!(filtered.contains("Esc clears"), "{filtered}");
        assert!(filtered.contains("wget"), "{filtered}");
        assert!(
            !filtered.contains("htop"),
            "htop is filtered out: {filtered}"
        );

        app.clear_filter();
        let cleared = screen(&mut app, 150, 24);
        assert!(cleared.contains("Packages (0/2)"), "{cleared}");
    }

    #[test]
    fn the_env_row_lists_the_variables_in_effect() {
        let mut app = app();
        let empty = screen(&mut app, 150, 26);
        assert!(empty.contains("env: (none)"), "{empty}");

        app.set_env("FOO=bar");
        let filled = screen(&mut app, 150, 26);
        assert!(filled.contains("env: FOO"), "{filled}");
    }

    #[test]
    fn the_reset_row_is_listed_in_the_options_block() {
        let mut app = app();
        let screen = screen(&mut app, 150, 26);
        assert!(screen.contains("reset last configuration"), "{screen}");
    }

    #[test]
    fn categories_render_with_fold_markers_and_counts() {
        let mut app = app();
        let expanded = screen(&mut app, 160, 30);
        assert!(expanded.contains("▾ Shell (1)"), "{expanded}");
        assert!(expanded.contains("▾ Network (1)"), "{expanded}");

        // Folding a category hides its packages and flips the marker.
        app.toggle_category("Shell");
        let folded = screen(&mut app, 160, 30);
        assert!(folded.contains("▸ Shell (1)"), "{folded}");
        assert!(!folded.contains("> htop"), "htop is hidden: {folded}");
        assert!(
            folded.contains("wget"),
            "other categories stay open: {folded}"
        );

        app.toggle_category("Shell");
        let reopened = screen(&mut app, 160, 30);
        assert!(reopened.contains("▾ Shell (1)"), "{reopened}");
        assert!(reopened.contains("htop"), "{reopened}");
    }

    #[test]
    fn info_popup_renders_the_description() {
        let mut app = app();
        app.toggle_info();
        let screen = screen(&mut app, 160, 30);

        assert!(screen.contains("htop summary"), "{screen}");
        assert!(screen.contains("category"), "{screen}");
        assert!(screen.contains("installs"), "{screen}");
        assert!(screen.contains("m method"), "{screen}");
        assert!(screen.contains("e edit"), "{screen}");
    }

    #[test]
    fn image_prompt_renders_input_and_family() {
        let mut app = app();
        app.open_image_prompt();
        for character in "localhost/dev:ubuntu24".chars() {
            app.prompt_insert(character);
        }
        let first = screen(&mut app, 160, 30);
        assert!(first.contains("Add image"), "{first}");
        assert!(first.contains("localhost/dev:ubuntu24"), "{first}");
        assert!(first.contains("ROS"), "{first}");
        assert!(first.contains("Enter confirm"), "{first}");

        app.prompt_cycle_family(true); // ROS -> Ubuntu
        let second = screen(&mut app, 160, 30);
        assert!(second.contains("Ubuntu"), "{second}");
    }

    #[test]
    fn info_popup_shows_the_effective_install_method() {
        let mut app = app();
        select_jazzy(&mut app);
        app.toggle_info();

        let before = screen(&mut app, 160, 30);
        assert!(before.contains("package manager"), "{before}");
        assert!(before.contains("command"), "{before}");
        assert!(before.contains("htop"), "{before}");

        // After switching to a script the override is marked.
        app.toggle_install_method();
        let after = screen(&mut app, 160, 30);
        assert!(after.contains("script"), "{after}");
        assert!(after.contains("overridden"), "{after}");
    }

    #[test]
    fn prompts_opened_from_the_info_popup_are_visible() {
        let catalog = Catalog::load().expect("bundled catalog");
        let (log_tx, _log_rx) = mpsc::channel(8);
        let (distro_tx, _distro_rx) = mpsc::channel(8);
        let (toolbox_tx, _toolbox_rx) = mpsc::channel(8);
        let mut app = App::new(catalog, Distro::predefined(), log_tx, distro_tx, toolbox_tx);
        app.animation = false;
        select_ubuntu_2404(&mut app);
        select_package(&mut app, "neovim");
        app.toggle_info();

        // The version prompt is opened from the popup, so it has to be painted on top of
        // it -- drawn underneath, the keys looked like they did nothing.
        app.open_version_prompt();
        let version = screen(&mut app, 130, 28);
        assert!(version.contains("Version — neovim"), "{version}");
        assert!(version.contains("0.12.1"), "{version}");
        assert!(version.contains("Enter confirm"), "{version}");

        for character in "0.11.2".chars() {
            app.prompt_insert(character);
        }
        let typed = screen(&mut app, 130, 28);
        assert!(typed.contains("0.11.2"), "{typed}");

        app.prompt_submit();
        let after = screen(&mut app, 130, 28);
        assert!(
            !after.contains("Version — neovim"),
            "prompt closed: {after}"
        );
        // The popup reports the new version and the command it produced.
        assert!(after.contains("0.11.2"), "{after}");
        assert!(after.contains("download/v0.11.2"), "{after}");
        assert!(
            !after.contains("{{version}}"),
            "the recipe is shown expanded: {after}"
        );

        // Same for the command editor.
        app.open_script_prompt();
        let edit = screen(&mut app, 130, 28);
        assert!(edit.contains("Install command — neovim"), "{edit}");
        assert!(edit.contains("Enter confirm"), "{edit}");
    }

    /// Select Ubuntu 24.04, where the tarball recipes (neovim) apply.
    fn select_ubuntu_2404(app: &mut App) {
        app.focus = Focus::Distro;
        let row = app
            .distro_rows
            .iter()
            .position(|row| row.label() == "24.04")
            .expect("24.04 row");
        app.distro_list_state.select(Some(row));
        app.toggle_option();
    }

    fn select_package(app: &mut App, name: &str) {
        let row = app
            .package_rows
            .iter()
            .position(|row| match row {
                PackageRow::Package { label, .. } => label.split(' ').next() == Some(name),
                PackageRow::Header { .. } => false,
            })
            .unwrap_or_else(|| panic!("package {name} not listed"));
        app.focus = Focus::Apps;
        app.apps_list_state.select(Some(row));
    }

    fn select_jazzy(app: &mut App) {
        app.focus = Focus::Distro;
        let row = app
            .distro_rows
            .iter()
            .position(|row| row.label() == "jazzy")
            .expect("jazzy row");
        app.distro_list_state.select(Some(row));
        app.toggle_option();
    }

    #[test]
    fn toolboxes_tab_lists_manifests_with_status() {
        let mut app = app();
        app.apply_toolbox_scan(ToolboxScan {
            toolboxes: vec![
                toolbox("localhost/toolshop:jazzy-1a2b3c", ToolboxStatus::Present),
                toolbox(
                    "localhost/toolshop:jazzy-9f9f9f",
                    ToolboxStatus::Modified {
                        current_id: "123456789abc".to_string(),
                    },
                ),
            ],
            warnings: vec![],
        });
        app.switch_view(View::Toolboxes);
        let screen = screen(&mut app, 160, 30);

        assert!(screen.contains("Toolboxes (2)"), "{screen}");
        assert!(
            screen.contains("toolshop:jazzy-1a2b3c [present]"),
            "{screen}"
        );
        assert!(screen.contains("[modified]"), "{screen}");

        // The details pane describes the selected toolbox.
        assert!(screen.contains("Packages (2)"), "{screen}");
        assert!(screen.contains("pkg: htop"), "{screen}");
        assert!(screen.contains("script"), "{screen}");
        assert!(screen.contains("jazzy (ros)"), "{screen}");
        assert!(screen.contains("toolshop-jazzy-1a2b3c.toml"), "{screen}");
    }

    #[test]
    fn toolboxes_tab_lists_external_toolboxes_without_a_manifest() {
        let mut app = app();
        app.apply_toolbox_scan(ToolboxScan {
            toolboxes: vec![crate::toolbox::Toolbox::external(
                "localhost/test-dev:ubuntu24",
                ToolboxStatus::Present,
                vec!["test".to_string()],
            )],
            warnings: vec![],
        });
        app.switch_view(View::Toolboxes);
        let screen = screen(&mut app, 160, 30);

        assert!(screen.contains("test-dev:ubuntu24"), "{screen}");
        assert!(screen.contains("external"), "{screen}");
        assert!(screen.contains("not built by toolshop"), "{screen}");
        assert!(screen.contains("Containers: test"), "{screen}");
    }

    #[test]
    fn hit_areas_are_recorded_for_the_current_view() {
        let mut app = app();
        let _ = screen(&mut app, 120, 24);

        assert_eq!(app.hit.tabs.len(), 3, "one rect per tab");
        assert_eq!(app.hit.tabs[0].0, View::Builder);
        assert_eq!(app.hit.tabs[2].0, View::Logs);
        assert!(app.hit.distros.width > 0 && app.hit.distros.height > 0);
        assert!(app.hit.packages.width > 0);
        assert!(app.hit.containerfile.width > 0);
        // Distro, packages and containerfile tile the body without overlapping.
        assert!(app.hit.distros.right() <= app.hit.packages.x);
        assert!(app.hit.packages.right() <= app.hit.containerfile.x);

        app.switch_view(View::Toolboxes);
        let _ = screen(&mut app, 120, 24);
        assert!(app.hit.toolboxes.width > 0 && app.hit.toolbox_details.width > 0);

        app.switch_view(View::Logs);
        let _ = screen(&mut app, 120, 24);
        assert!(app.hit.logs.width > 0);
    }

    #[test]
    fn clicking_a_tab_rect_matches_what_is_drawn() {
        let mut app = app();
        let _ = screen(&mut app, 120, 24);

        // The recorded tab rects must point at the drawn labels.
        let (view, rect) = app.hit.tabs[1];
        assert_eq!(view, View::Toolboxes);
        let frame = screen(&mut app, 120, 24);
        let row: String = frame
            .lines()
            .next()
            .unwrap()
            .chars()
            .skip(rect.x as usize)
            .take(rect.width as usize)
            .collect();
        assert!(row.contains("Toolboxes"), "drawn tab row was {row:?}");
    }

    #[test]
    fn changing_the_distro_shows_the_new_base_image() {
        let mut app = app();
        select_jazzy(&mut app);

        // A short window forces the file to be scrollable.
        let fresh = screen(&mut app, 120, 10);
        assert!(
            fresh.contains("FROM docker.io/library/ros:jazzy-ros-base"),
            "{fresh}"
        );

        app.focus = Focus::Container;
        for _ in 0..5 {
            app.scroll_container(true);
        }
        let _ = screen(&mut app, 120, 10);
        assert!(app.container_scroll > 0, "scrolled away from the top");

        // Switching distro rewrites the file, so it must be shown from the top again.
        app.focus = Focus::Distro;
        let row = app
            .distro_rows
            .iter()
            .position(|row| row.label() == "kilted")
            .unwrap();
        app.distro_list_state.select(Some(row));
        app.toggle_option();

        assert_eq!(app.container_scroll, 0, "content change resets the scroll");
        let after = screen(&mut app, 120, 10);
        assert!(
            after.contains("FROM docker.io/library/ros:kilted-ros-base"),
            "{after}"
        );
    }

    #[test]
    fn containerfile_scroll_is_clamped_and_labelled() {
        let mut app = app();
        select_jazzy(&mut app);
        app.focus = Focus::Container;

        for _ in 0..200 {
            app.scroll_container(true);
        }
        let bottom = screen(&mut app, 120, 10);
        let max = app.container_scroll;
        assert!(max > 0, "a tall file scrolls");
        assert!(
            bottom.contains("· line "),
            "title shows the offset: {bottom}"
        );

        // One more render must not grow past the end of the content.
        let _ = screen(&mut app, 120, 10);
        assert_eq!(app.container_scroll, max, "clamped at the last page");

        // Removing the distro empties the file and resets the scroll.
        app.focus = Focus::Distro;
        app.selected_distro = None;
        app.builder.set_distro_option(None);
        app.create_container_file();
        assert_eq!(app.container_scroll, 0);
    }

    #[test]
    fn the_tab_bar_names_the_active_flavour() {
        use crate::theme::{Flavour, with_flavour};

        let mut app = app();
        let mocha = with_flavour(Flavour::Mocha, || screen(&mut app, 140, 20));
        assert!(mocha.lines().next().unwrap().contains("Mocha"), "{mocha}");

        let latte = with_flavour(Flavour::Latte, || screen(&mut app, 140, 20));
        assert!(latte.lines().next().unwrap().contains("Latte"), "{latte}");
        // Text content is flavour-independent, so the layout still matches.
        assert!(latte.contains("Distro"), "{latte}");
    }

    #[test]
    fn the_cursor_package_and_its_dependencies_are_highlighted_in_the_containerfile() {
        let _flavour = crate::theme::FlavourGuard::new(crate::theme::Flavour::Mocha);
        let mut app = app();
        select_jazzy(&mut app);
        select_package(&mut app, "htop");
        app.toggle_option(); // htop + its requirement wget
        assert!(app.selected_packages.contains("htop"));
        assert!(app.selected_packages.contains("wget"));

        let rendered = screen(&mut app, 160, 30);
        assert!(rendered.contains("htop \\"), "{rendered}");
        assert!(rendered.contains("wget \\"), "{rendered}");

        let mut terminal = Terminal::new(TestBackend::new(160, 30)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();

        // Only look inside the Containerfile pane, past its border.
        let rect = app.hit.containerfile;
        let columns = || (rect.x + 1)..rect.right().saturating_sub(1);
        let style_of = |needle: &str| {
            let row = ((rect.y + 1)..rect.bottom().saturating_sub(1))
                .find(|y| {
                    let line: String = columns().map(|x| buffer[(x, *y)].symbol()).collect();
                    line.contains(needle)
                })
                .unwrap_or_else(|| panic!("no row with {needle:?}"));
            columns()
                .map(|x| &buffer[(x, row)])
                .find(|cell| cell.symbol() != " ")
                .map(|cell| cell.style())
                .expect("styled cell")
        };

        let primary = style_of("htop \\");
        let dependency = style_of("wget \\");
        let scaffold = style_of("SHELL [");

        assert_eq!(primary.fg, THEME.container_primary().fg, "cursor package");
        assert_eq!(primary.bg, THEME.container_primary().bg, "on its own band");
        assert_eq!(
            dependency.fg,
            THEME.container_dependency().fg,
            "dependencies in their own colour"
        );
        assert_eq!(scaffold.fg, THEME.text().fg, "everything else stays plain");
        assert_ne!(primary.fg, dependency.fg);
    }

    #[test]
    fn containerfile_pane_scrolls() {
        let mut app = app();
        select_jazzy(&mut app);
        app.focus = Focus::Container;
        let _ = screen(&mut app, 120, 10); // clamp with the real pane height

        let before = app.container_scroll;
        app.scroll_container(true);
        assert_eq!(app.container_scroll, before + 1);
        app.scroll_container(false);
        assert_eq!(app.container_scroll, before);
    }

    #[test]
    fn build_progress_is_visible_in_the_footer_and_logs_tab() {
        let mut app = app();
        select_jazzy(&mut app);
        app.toggle_info();
        app.close_info();

        // Idle: plain status.
        let idle = screen(&mut app, 160, 30);
        assert!(idle.contains("idle"), "{idle}");

        // Running: gauge with the step count, marker on the Logs tab.
        app.build.start();
        app.append_log("STEP 2/5: RUN apt-get update".to_string());
        app.append_log("level=warning msg=\"mapping\"".to_string());
        app.process_running = true;

        let busy = screen(&mut app, 160, 30);
        assert!(busy.contains("STEP 2/5"), "gauge label: {busy}");
        assert!(busy.contains('%'), "tab shows a percentage: {busy}");
        assert!(busy.contains('▲'), "warning indicator: {busy}");
        assert!(busy.contains('●'), "running indicator: {busy}");

        // The full-window log repeats the progress in its title.
        app.switch_view(View::Logs);
        let logs = screen(&mut app, 160, 30);
        assert!(logs.contains("Logs — STEP 2/5"), "{logs}");

        // Finished: a summary instead of the gauge.
        app.process_running = false;
        app.append_log(
            "Build finished (exit status: 0): localhost/toolshop:jazzy-aaaaaa".to_string(),
        );
        app.switch_view(View::Builder);
        let done = screen(&mut app, 160, 30);
        assert!(done.contains("done · "), "{done}");
        assert!(!done.contains("STEP 2/5"), "{done}");
    }

    #[test]
    fn failed_builds_are_shown_in_red() {
        let mut app = app();
        select_jazzy(&mut app);
        app.build.start();
        app.append_log("Build failed: podman exited with exit status: 125".to_string());
        app.process_running = false;

        let screen = screen(&mut app, 160, 30);
        assert!(screen.contains("failed · "), "{screen}");
        assert!(screen.contains('✖'), "error indicator: {screen}");
    }

    #[test]
    fn failures_warnings_and_steps_are_styled_differently() {
        let _flavour = crate::theme::FlavourGuard::new(crate::theme::Flavour::Mocha);
        let mut app = app();
        select_jazzy(&mut app);
        app.build.start();
        app.append_log("STEP 2/3: RUN apt-get install -y htop".to_string());
        app.append_log("level=warning msg=\"mapping\"".to_string());
        app.append_log("Build failed: podman exited with exit status: 125".to_string());
        app.switch_view(View::Logs);

        let mut terminal = Terminal::new(TestBackend::new(120, 16)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();

        let row_of = |needle: &str| -> Option<u16> {
            (0..buffer.area.height).find(|y| {
                let line: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect();
                line.contains(needle)
            })
        };
        // Skip the pane border: take the first non-blank cell inside it.
        let style_at = |row: u16| {
            (1..buffer.area.width - 1)
                .map(|x| &buffer[(x, row)])
                .find(|cell| cell.symbol() != " ")
                .map(|cell| cell.style())
                .expect("styled cell")
        };

        // NB: the pane title also says "STEP 2/3", so match the log line itself.
        let step = style_at(row_of("RUN apt-get install -y htop").expect("step line"));
        let warning = style_at(row_of("level=warning").expect("warning line"));
        let failure = style_at(row_of("Build failed").expect("failure line"));

        assert_eq!(step.fg, THEME.log_step().fg, "steps are highlighted");
        assert_eq!(warning.fg, THEME.log_warning().fg, "warnings are yellow");
        assert_eq!(
            failure.fg,
            THEME.log_banner().fg,
            "the failure is drawn as a banner"
        );
        assert!(
            failure
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD),
            "and bold"
        );
        assert_eq!(failure.bg, THEME.log_banner().bg, "on its own band");
    }

    #[test]
    fn the_builder_shows_the_option_checkboxes() {
        let mut app = app();
        let default = screen(&mut app, 160, 30);
        assert!(default.contains("Options"), "{default}");
        assert!(default.contains("[x] user setup"), "{default}");
        assert!(default.contains("[x] hostname fix"), "{default}");

        assert!(default.contains("user: "), "{default}");

        app.focus = Focus::Options;
        app.move_down();
        app.toggle_option();
        let toggled = screen(&mut app, 160, 30);
        assert!(toggled.contains("[x] user setup"), "{toggled}");
        assert!(toggled.contains("[ ] hostname fix"), "{toggled}");
    }

    #[test]
    fn long_log_lines_do_not_wrap_so_scrolling_stays_on_line_boundaries() {
        let mut app = app();
        app.append_log("a".repeat(300));
        app.append_log("second".to_string());
        app.append_log("third".to_string());
        app.switch_view(View::Logs);

        let buffer = screen_state(&mut app, 60, 12);
        let rows: Vec<String> = (1..buffer.area.height - 1)
            .map(|y| {
                (1..buffer.area.width - 1)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();

        // One row per log line: the long line is clipped, never wrapped onto the next row.
        let first = rows
            .iter()
            .position(|row| row.starts_with("aaa"))
            .unwrap_or_else(|| panic!("no log rows: {rows:?}"));
        assert_eq!(
            rows.get(first + 1).map(String::as_str),
            Some("second"),
            "the next line follows immediately: {rows:?}"
        );
        assert_eq!(
            rows.get(first + 2).map(String::as_str),
            Some("third"),
            "{rows:?}"
        );
        assert!(
            !rows[first + 1].starts_with("aaa"),
            "the long line must not continue: {rows:?}"
        );
    }

    #[test]
    fn prologue_lines_of_the_cursor_package_are_highlighted_too() {
        // The assertions compare against THEME colours, so pin the flavour for the duration.
        let _flavour = crate::theme::FlavourGuard::new(crate::theme::Flavour::Mocha);
        let mut app = app_with_script_package();
        select_jazzy(&mut app);
        select_package(&mut app, "rust");
        app.toggle_option(); // rust + its requirements

        let rendered = screen(&mut app, 160, 30);
        assert!(
            rendered.contains("ENV CARGO_HOME=/opt/toolbox/cargo"),
            "{rendered}"
        );

        let mut terminal = Terminal::new(TestBackend::new(160, 30)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let rect = app.hit.containerfile;
        let columns = || (rect.x + 1)..rect.right().saturating_sub(1);
        let style_of = |needle: &str| {
            let row = ((rect.y + 1)..rect.bottom().saturating_sub(1))
                .find(|y| {
                    let line: String = columns().map(|x| buffer[(x, *y)].symbol()).collect();
                    line.contains(needle)
                })
                .unwrap_or_else(|| panic!("no row with {needle:?}"));
            columns()
                .map(|x| &buffer[(x, row)])
                .find(|cell| cell.symbol() != " ")
                .map(|cell| cell.style())
                .expect("styled cell")
        };

        // The directory/ENV lines the package needs highlight like its install step …
        let env = style_of("ENV CARGO_HOME");
        assert_eq!(
            env.fg,
            THEME.container_primary().fg,
            "toolchain env is highlighted"
        );
        let mkdir = style_of("RUN mkdir -p");
        assert_eq!(
            mkdir.fg,
            THEME.container_primary().fg,
            "its directories too"
        );
        // … while scaffolding nobody caused stays plain.
        let shell = style_of("SHELL [");
        assert_eq!(shell.fg, THEME.text().fg);
    }

    /// Catalog with a script package that needs a toolchain (`rust`-like).
    fn app_with_script_package() -> App {
        let catalog = Catalog {
            packages: vec![
                pkg("htop", "Shell", &[]),
                Package {
                    name: "rust".to_string(),
                    version: String::new(),
                    summary: "Rust toolchain".to_string(),
                    category: "Rust".to_string(),
                    requires: vec![],
                    install: BTreeMap::from([(
                        DEFAULT_INSTALL_KEY.to_string(),
                        InstallSpec::Script {
                            script: "rm -rf $CARGO_HOME && install-rust-into $CARGO_HOME"
                                .to_string(),
                        },
                    )]),
                    env: BTreeMap::new(),
                },
            ],
            warnings: vec![],
        };
        let (log_tx, _log_rx) = mpsc::channel(8);
        let (distro_tx, _distro_rx) = mpsc::channel(8);
        let (toolbox_tx, _toolbox_rx) = mpsc::channel(8);
        let mut app = App::new(catalog, Distro::predefined(), log_tx, distro_tx, toolbox_tx);
        app.animation = false;
        app.frame_dt = std::time::Duration::ZERO;
        app
    }

    #[test]
    fn logs_tab_shows_the_log_full_window() {
        let mut app = app();
        app.append_log("STEP 1/3: FROM docker.io/library/ros:jazzy-ros-base".to_string());
        app.append_log("STEP 2/3: RUN apt-get update && apt-get install -y htop".to_string());
        app.switch_view(View::Logs);

        let screen = screen(&mut app, 120, 24);
        assert!(screen.contains("Logs — build output"), "{screen}");
        assert!(screen.contains("STEP 1/3"), "{screen}");
        assert!(screen.contains("STEP 2/3"), "{screen}");
        assert!(screen.contains("Logs"), "tab bar still lists the tab");

        // The other tabs are gone from the body: no distro or package panes.
        assert!(!screen.contains("Containerfile"), "{screen}");
        assert!(!screen.contains("Distro"), "{screen}");
        assert!(screen.contains("L back to the builder"), "{screen}");
    }

    #[test]
    fn logs_tab_scrolls_when_the_log_is_long() {
        let mut app = app();
        for i in 0..80 {
            app.append_log(format!("line {i:02}"));
        }
        app.switch_view(View::Logs);
        let tail = screen(&mut app, 100, 16);
        assert!(
            tail.contains("line 79"),
            "follows the tail by default: {tail}"
        );
        assert!(!tail.contains("line 00"), "{tail}");

        for _ in 0..80 {
            app.scroll_logs_up();
        }
        let top = screen(&mut app, 100, 16);
        assert!(top.contains("line 00"), "{top}");
        assert!(!top.contains("line 79"), "{top}");
    }

    #[test]
    fn toolboxes_tab_explains_an_empty_cache() {
        let mut app = app();
        app.switch_view(View::Toolboxes);
        let screen = screen(&mut app, 160, 30);
        assert!(screen.contains("No toolboxes yet"), "{screen}");
    }

    #[test]
    fn missing_images_are_reported_in_the_details() {
        let mut app = app();
        app.apply_toolbox_scan(ToolboxScan {
            toolboxes: vec![toolbox(
                "localhost/toolshop:jazzy-1a2b3c",
                ToolboxStatus::Missing,
            )],
            warnings: vec![],
        });
        app.switch_view(View::Toolboxes);
        let screen = screen(&mut app, 160, 30);
        assert!(screen.contains("no longer in the engine"), "{screen}");
    }
}
