//! Terminal rendering for SuperSearch: tabs, filter form, results
//! table with a sticky header, preview pane, status bar, and popups.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
    },
    Frame,
};

use crate::app::{human_size, mtime_short, App, Field, Focus, Popup, SortKey, PAGE_SIZE};

const AMBER: Color = Color::Rgb(232, 170, 66);
const DIM: Color = Color::Rgb(140, 140, 140);

fn focused_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(AMBER).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(DIM)
    }
}

/// One filter field as `label value` with a block cursor when focused.
fn field_line(app: &App, field: Field) -> Line<'static> {
    let focused = app.focus == Focus::Field(field);
    let raw = match field {
        Field::Roots => app.tab().roots.clone(),
        Field::Name => app.tab().name.clone(),
        Field::Regex => app.tab().regex.clone(),
        Field::Fuzzy => app.tab().fuzzy.clone(),
        Field::Content => app.tab().content.clone(),
        Field::Glob => app.tab().glob.clone(),
        Field::Or => app.tab().or_terms.clone(),
        Field::Not => app.tab().not_terms.clone(),
        Field::MinSize => app.tab().min_size.clone(),
        Field::MaxSize => app.tab().max_size.clone(),
        Field::After => app.tab().modified_after.clone(),
        Field::Before => app.tab().modified_before.clone(),
        Field::Exclude => app.tab().exclusions.clone(),
        Field::Ext => app.tab().extensions.clone(),
        Field::DbPath => app.tab().db_path.clone(),
    };
    let value = if focused {
        let at = app.cursor.min(raw.len());
        if raw.is_char_boundary(at) {
            format!("{}▌{}", &raw[..at], &raw[at..])
        } else {
            raw.clone()
        }
    } else {
        raw
    };
    Line::from(vec![
        Span::styled(
            format!("{} ", app.tr.t(field.label_key())),
            focused_style(focused),
        ),
        Span::styled(value, Style::default().fg(Color::White)),
    ])
}

fn sort_mark(app: &App, key: SortKey) -> &'static str {
    if app.tab().sort_key != key {
        ""
    } else if app.tab().sort_asc {
        " ▲"
    } else {
        " ▼"
    }
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // tabs
            Constraint::Length(1),  // roots
            Constraint::Length(10), // filter grid + top border (7 + exclude + toggles)
            Constraint::Min(5),     // results + preview
            Constraint::Length(2),  // hint + status
        ])
        .split(area);

    draw_tabs(frame, app, chunks[0]);
    draw_roots(frame, app, chunks[1]);
    draw_filters(frame, app, chunks[2]);
    draw_main(frame, app, chunks[3]);
    draw_status(frame, app, chunks[4]);

    match &app.popup {
        Some(Popup::Help) => draw_help(frame, app, area),
        Some(Popup::ConfirmDelete) => draw_confirm_delete(frame, app, area),
        Some(Popup::Export) => draw_export(frame, app, area),
        Some(Popup::MoveDest) => draw_line_input(
            frame,
            app,
            &app.tr.t("tui.move_title").to_string(),
            &app.tr.t("tui.move_dest").to_string(),
            &app.move_dest,
        ),
        Some(Popup::SaveName) => draw_line_input(
            frame,
            app,
            &app.tr.t("tui.save_title").to_string(),
            &app.tr.t("tui.save_name").to_string(),
            &app.save_name,
        ),
        Some(Popup::LoadSaved) => draw_load_saved(frame, app, area),
        Some(Popup::Language) => draw_language(frame, app, area),
        Some(Popup::PropsDetails) => draw_props(frame, app, area),
        None => {}
    }
}

fn draw_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled(
        format!("{} ", app.tr.t("tui.title")),
        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
    )];
    for (i, tab) in app.tabs.iter().enumerate() {
        let caption = format!(" [{}: {}] ", i + 1, tab.caption());
        let style = if i == app.active {
            Style::default().fg(Color::Black).bg(AMBER)
        } else {
            Style::default().fg(DIM)
        };
        spans.push(Span::styled(caption, style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_roots(frame: &mut Frame, app: &App, area: Rect) {
    let drives = app
        .drives
        .iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let line = Line::from(vec![
        Span::styled(
            format!("{}: ", app.tr.t("tui.drives")),
            Style::default().fg(DIM),
        ),
        Span::styled(drives, Style::default().fg(Color::White)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_filters(frame: &mut Frame, app: &App, area: Rect) {
    // Two fields per row; the Roots/DbPath source rows live in the
    // field list too. Order matches `Field::ALL`.
    let pairs = [
        (Field::Roots, Some(Field::DbPath)),
        (Field::Name, Some(Field::Regex)),
        (Field::Fuzzy, Some(Field::Content)),
        (Field::Glob, Some(Field::Or)),
        (Field::Not, Some(Field::Ext)),
        (Field::MinSize, Some(Field::MaxSize)),
        (Field::After, Some(Field::Before)),
    ];
    let mut rows: Vec<Row> = pairs
        .iter()
        .map(|(a, b)| {
            let mut cells = vec![field_line(app, *a)];
            if let Some(f) = b {
                cells.push(field_line(app, *f));
            } else {
                cells.push(Line::from(""));
            }
            // ratatui `Row` takes `Cell`s; a `Line` renders as one.
            Row::new(cells)
        })
        .collect();
    // Exclude gets its own row (long skip-lists) plus the toggles.
    rows.push(Row::new(vec![
        field_line(app, Field::Exclude),
        Line::from(""),
    ]));
    rows.push(Row::new(vec![
        Line::from(Span::styled(
            format!(
                "{} [{}]  {} [{}]  {} [{}]  {} [{}]",
                app.tr.t("tui.toggle_case"),
                if app.tab().case_insensitive { "x" } else { " " },
                app.tr.t("tui.toggle_hidden"),
                if app.tab().show_hidden { "x" } else { " " },
                app.tr.t("tui.toggle_indexed"),
                if app.tab().use_indexed { "x" } else { " " },
                app.tr.t("tui.toggle_preview"),
                if app.preview_visible { "x" } else { " " },
            ),
            Style::default().fg(DIM),
        )),
        Line::from(""),
    ]));
    let table = Table::new(
        rows,
        [Constraint::Percentage(50), Constraint::Percentage(50)],
    )
    .block(
        Block::default()
            .borders(Borders::TOP)
            .title(format!(" {} (Tab) ", app.tr.t("tui.fields"))),
    );
    frame.render_widget(table, area);
}

fn draw_main(frame: &mut Frame, app: &App, area: Rect) {
    if app.preview_visible {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
            .split(area);
        draw_results(frame, app, cols[0]);
        draw_preview(frame, app, cols[1]);
    } else {
        draw_results(frame, app, area);
    }
}

fn draw_results(frame: &mut Frame, app: &App, area: Rect) {
    let tab = app.tab();
    let start = tab.page * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(tab.results.len());
    let header = Row::new(vec![
        " ✓".to_string(),
        app.tr.f(
            "results.header_path",
            &[("arrow", sort_mark(app, SortKey::Path))],
        ),
        app.tr.f(
            "results.header_size",
            &[("arrow", sort_mark(app, SortKey::Size))],
        ),
        app.tr.f(
            "results.header_mod",
            &[("arrow", sort_mark(app, SortKey::Modified))],
        ),
    ])
    .style(Style::default().fg(AMBER).add_modifier(Modifier::BOLD))
    .bottom_margin(0);
    let rows: Vec<Row> = tab.results[start..end]
        .iter()
        .enumerate()
        .map(|(k, e)| {
            let gi = start + k;
            let mark = if tab.checked.contains(&e.path) {
                "[x]"
            } else {
                "[ ]"
            };
            let mut row = Row::new(vec![
                mark.to_string(),
                e.path.display().to_string(),
                human_size(e.size),
                mtime_short(e.mtime),
            ]);
            if Some(gi) == tab.selected {
                row = row.style(Style::default().bg(Color::Rgb(60, 45, 20)).fg(Color::White));
            }
            row
        })
        .collect();
    let title = format!(
        " {} · {} ",
        app.tr
            .f("tui.results", &[("n", &tab.results.len().to_string())]),
        app.tr.f(
            "tui.page",
            &[
                ("p", &(tab.page + 1).to_string()),
                ("pages", &tab.page_count().to_string()),
            ]
        ),
    );
    let widths = [
        Constraint::Length(5),
        Constraint::Min(20),
        Constraint::Length(10),
        Constraint::Length(12),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_symbol("▶ ");
    let mut state =
        TableState::default().with_selected(tab.selected.and_then(|i| i.checked_sub(start)));
    frame.render_stateful_widget(table, area, &mut state);
    if tab.results.is_empty() && !tab.running {
        let msg = if tab.searched {
            app.tr.t("results.empty_nomatch").to_string()
        } else {
            app.tr.t("tui.empty").to_string()
        };
        let inner = Rect {
            y: area.y + 2,
            height: 1.min(area.height.saturating_sub(2)),
            ..area
        };
        frame.render_widget(Paragraph::new(msg).style(Style::default().fg(DIM)), inner);
    }
}

fn draw_preview(frame: &mut Frame, app: &App, area: Rect) {
    let text = app.preview_text();
    let title = format!(" {} ", app.tr.t("tui.preview"));
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let tab = app.tab();
    let status = if tab.running {
        app.tr.f("tui.scanning", &[("n", &tab.scanned.to_string())])
    } else {
        tab.status.clone()
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);
    frame.render_widget(
        Paragraph::new(app.tr.t("tui.hint").to_string()).style(Style::default().fg(DIM)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(Color::White)),
        rows[1],
    );
}

/// Centered modal area (60% x 60% max, at least 20x8).
fn modal_area(area: Rect) -> Rect {
    let w = (area.width * 70 / 100).clamp(24, 100);
    let h = (area.height * 70 / 100).clamp(10, 30);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w.min(area.width),
        height: h.min(area.height),
    }
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    let body = app.tr.t("tui.help_body").to_string();
    let lines: Vec<Line> = body.lines().map(|l| Line::from(l.to_string())).collect();
    let rect = modal_area(area);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", app.tr.t("tui.help_title"))),
            )
            .wrap(Wrap { trim: false }),
        rect,
    );
}

fn draw_confirm_delete(frame: &mut Frame, app: &App, area: Rect) {
    let n = app.tab().checked.len();
    let text = vec![
        Line::from(app.tr.f("modal.delete_body", &[("n", &n.to_string())])),
        Line::from(""),
        Line::from(app.tr.t("tui.confirm_hint").to_string()),
    ];
    let rect = modal_area(area);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", app.tr.t("modal.delete_title"))),
        ),
        rect,
    );
}

fn draw_export(frame: &mut Frame, app: &App, area: Rect) {
    let names = ["CSV", "TXT", "HTML", "Excel XML", "RTF"];
    let mut lines = vec![Line::from(app.tr.t("tui.export_format").to_string())];
    for (i, name) in names.iter().enumerate() {
        let mark = if i == app.export_format { "●" } else { "○" };
        let style = if i == app.export_format {
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(format!(" {mark} {name}"), style)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            format!("{} ", app.tr.t("tui.export_path")),
            Style::default().fg(DIM),
        ),
        Span::styled(
            format!("{}▌", app.export_path),
            Style::default().fg(Color::White),
        ),
    ]));
    let rect = modal_area(area);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", app.tr.t("tui.export_title"))),
        ),
        rect,
    );
}

fn draw_line_input(frame: &mut Frame, _app: &App, title: &str, label: &str, value: &str) {
    let rect = modal_area(frame.area());
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(label.to_string()),
            Line::from(Span::styled(
                format!("{value}▌"),
                Style::default().fg(Color::White),
            )),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {title} ")),
        ),
        rect,
    );
}

fn draw_load_saved(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = if app.config.saved_searches.is_empty() {
        vec![ListItem::new(app.tr.t("tui.load_empty").to_string())]
    } else {
        app.config
            .saved_searches
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let style = if i == app.popup_index {
                    Style::default().fg(AMBER).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Span::styled(format!(" {}", s.name), style))
            })
            .collect()
    };
    let rect = modal_area(area);
    frame.render_widget(Clear, rect);
    let mut state = ListState::default().with_selected(Some(app.popup_index));
    frame.render_stateful_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", app.tr.t("tui.load_title"))),
        ),
        rect,
        &mut state,
    );
}

fn draw_language(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = supersearch_i18n::Lang::all()
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let style = if i == app.popup_index {
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Span::styled(
                format!(" {} · {}", l.label(), l.code()),
                style,
            ))
        })
        .collect();
    let rect = modal_area(area);
    frame.render_widget(Clear, rect);
    let mut state = ListState::default().with_selected(Some(app.popup_index));
    frame.render_stateful_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", app.tr.t("tui.lang_title"))),
        ),
        rect,
        &mut state,
    );
}

fn draw_props(frame: &mut Frame, app: &App, area: Rect) {
    let text = match app.tab().selected.and_then(|i| app.tab().results.get(i)) {
        Some(e) => vec![
            Line::from(vec![
                Span::styled(
                    format!("{}: ", app.tr.t("tui.props_path")),
                    Style::default().fg(DIM),
                ),
                Span::raw(e.path.display().to_string()),
            ]),
            Line::from(format!(
                "{}: {}",
                app.tr.t("tui.props_type"),
                if e.is_dir {
                    app.tr.t("tui.props_folder")
                } else {
                    app.tr.t("tui.props_file")
                }
            )),
            Line::from(format!(
                "{}: {}",
                app.tr.t("tui.props_size"),
                human_size(e.size)
            )),
            Line::from(format!(
                "{}: {}",
                app.tr.t("tui.props_modified"),
                crate::app::mtime_display(e.mtime)
            )),
        ],
        None => vec![Line::from(app.tr.t("status.select_row").to_string())],
    };
    let rect = modal_area(area);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", app.tr.t("tui.props_title"))),
        ),
        rect,
    );
}
