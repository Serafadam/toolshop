use std::borrow::Cow;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::Span,
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{
    app::{App, Focus},
    container_builder::ContainerBuilder,
};

fn is_focused(focus_app: &Focus, focus_widget: &Focus) -> Color {
    if focus_app == focus_widget {
        Color::Green
    } else {
        Color::Gray
    }
}

fn render_list_widget<'a, T: Clone + IntoIterator>(
    list: &'a T,
    selected: Vec<usize>,
    state: &mut ListState,
    focus_id: Focus,
    focus_current: &Focus,
    area: Rect,
    frame: &mut Frame,
) where
    Cow<'a, str>: From<<T as IntoIterator>::Item>,
{
    let apps: Vec<ListItem> = list
        .clone()
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            let style = if selected.contains(&i) {
                Style::default().bg(Color::default()).fg(Color::Green)
            } else {
                Style::default()
            };
            ListItem::new(Span::styled(item, style))
        })
        .collect();
    let list = List::new(apps)
        .block(
            Block::default()
                .title("List")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(is_focused(focus_current, &focus_id)),
        )
        .style(Style::default())
        .highlight_symbol(">")
        .highlight_style(Style::new().bg(Color::Black));
    frame.render_stateful_widget(list, area, state);
}

fn render_containerfile_widget(
    builder: &mut ContainerBuilder,
    focus_app: &Focus,
    area: Rect,
    frame: &mut Frame,
) {
    let mut cont_file = builder.get_container_file();
    if cont_file.is_empty() {
        cont_file = "Select distro first".to_string();
    }
    let cont = Paragraph::new(cont_file.as_str().to_owned())
        .block(
            Block::default()
                .title("Containterfile")
                .borders(Borders::ALL)
                .border_style(is_focused(focus_app, &Focus::Container)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(Clear, area);
    frame.render_widget(cont, area);
}
fn render_logs_widget(logs: String, focus_app: &Focus, area: Rect, frame: &mut Frame) {
    let log = Paragraph::new(logs.as_str().to_owned())
        .block(
            Block::default()
                .title("Podman Build Logs (Press Ctrl+C to cancel, ↑/↓ to scroll, q to quit)")
                .borders(Borders::ALL)
                .border_style(is_focused(focus_app, &Focus::Logs)),
        )
        .wrap(Wrap { trim: true });
    frame.render_widget(log, area);
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
    render_list_widget(
        &app.distro_list,
        app.distros_selected.clone(),
        &mut app.distro_list_state,
        Focus::Distro,
        &app.focus,
        first,
        frame,
    );
    render_list_widget(
        &app.apps_list.packages,
        app.apps_selected.clone(),
        &mut app.apps_list_state,
        Focus::Apps,
        &app.focus,
        second,
        frame,
    );
    render_containerfile_widget(&mut app.builder, &app.focus, third, frame);
    render_logs_widget(
        app.logs.lock().unwrap().to_string(),
        &app.focus,
        fourth,
        frame,
    );
}
