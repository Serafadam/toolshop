use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Style},
    text::Span,
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

use crate::app::{App, Focus};

fn is_focused(focus_app: &Focus, focus_widget: &Focus) -> Color {
    if focus_app == focus_widget {
        Color::Green
    } else {
        Color::Gray
    }
}

pub fn render(app: &mut App, frame: &mut Frame) {
    let constraints = [
        Constraint::Percentage(10),
        Constraint::Percentage(10),
        Constraint::Percentage(40),
        Constraint::Percentage(40),
    ];
    let layout = Layout::horizontal(constraints).spacing(1);
    let [first, second, third, fourth] = frame.area().layout(&layout);
    let apps: Vec<ListItem> = app
        .apps_list
        .packages
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let style = if app.apps_selected.contains(&i) {
                Style::default().bg(Color::default()).fg(Color::Green)
            } else {
                Style::default()
            };
            ListItem::new(Span::styled(item.clone().name, style))
        })
        .collect();
    let distros: Vec<ListItem> = app
        .distro_list
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let style = if app.distros_selected.contains(&i) {
                Style::default().bg(Color::default()).fg(Color::Green)
            } else {
                Style::default()
            };
            ListItem::new(Span::styled(item.clone(), style))
        })
        .collect();
    let list_apps = List::new(apps)
        .block(
            Block::default()
                .title("List")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(is_focused(&app.focus, &Focus::Apps)),
        )
        .style(Style::default())
        .highlight_symbol(">")
        .highlight_style(Style::new().bg(Color::Black));
    let list_distros = List::new(distros)
        .block(
            Block::default()
                .title("List")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(is_focused(&app.focus, &Focus::Distro)),
        )
        .style(Style::default())
        .highlight_symbol(">")
        .highlight_style(Style::new().bg(Color::Black));
    let mut cont_file = app.builder.get_container_file();
    if cont_file.is_empty(){
        cont_file = "Select distro first".to_string();
    }
    let command = Paragraph::new(cont_file.as_str())
        .block(
            Block::default()
                .title("Containterfile")
                .borders(Borders::ALL)
                .border_style(is_focused(&app.focus, &Focus::Container)),
        )
        .wrap(Wrap { trim: false }); // Vertical scroll
    let logs = app.logs.lock().unwrap();
    let log = Paragraph::new(logs.as_str())
        .block(
            Block::default()
                .title("Podman Build Logs (Press Ctrl+C to cancel, ↑/↓ to scroll, q to quit)")
                .borders(Borders::ALL)
                .border_style(is_focused(&app.focus, &Focus::Logs)),
        )
        .wrap(Wrap { trim: true }); // Vertical scroll
    frame.render_widget(Clear, third);
    frame.render_widget(command, third);
    frame.render_widget(log, fourth);
    frame.render_stateful_widget(&list_distros, first, &mut app.distro_list_state);
    frame.render_stateful_widget(&list_apps, second, &mut app.apps_list_state);
}
