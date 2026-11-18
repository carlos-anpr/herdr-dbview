//! Dibujo con ratatui. Cada fotograma reconstruye `app.hits` para el ratón.

use crate::app::*;
use crate::editor::Editor;
use crate::form::{self, Form, TestState};
use crate::model::*;
use crate::sqlhl::{self, Tok};
use crate::theme::*;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

fn spin(app: &App) -> &'static str {
    SPIN[(app.tick as usize / 2) % SPIN.len()]
}

fn env_color(e: EnvKind) -> Color {
    match e {
        EnvKind::Dev => OK,
        EnvKind::Prod => ERR,
        EnvKind::Other => WARN,
    }
}

fn env_tag(e: EnvKind) -> &'static str {
    match e {
        EnvKind::Dev => "DEV",
        EnvKind::Prod => "PROD",
        EnvKind::Other => "OTRA",
    }
}

fn panel(title: Vec<Span<'static>>, color: Color) -> Block<'static> {
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(Line::from(title))
}

fn focus_color(focused: bool) -> Color {
    if focused { ACCENT } else { BORDER }
}

fn button(label: &str, key: &str, color: Color) -> Span<'static> {
    let txt = if key.is_empty() {
        format!(" {label} ")
    } else {
        format!(" {label} {key} ")
    };
    Span::styled(
        txt,
        Style::new()
            .bg(color)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
    )
}

fn ghost(label: &str, key: &str) -> Span<'static> {
    let txt = if key.is_empty() {
        format!(" {label} ")
    } else {
        format!(" {label} {key} ")
    };
    Span::styled(txt, Style::new().bg(SEL_BG_DIM).fg(TEXT))
}

fn clip_spans(spans: Vec<Span<'static>>, w: usize) -> Vec<Span<'static>> {
    if spans_width(&spans) <= w {
        return spans;
    }
    let mut out = Vec::new();
    let mut acc = 0;
    for sp in spans {
        let sw = width(&sp.content);
        if acc + sw < w {
            acc += sw;
            out.push(sp);
        } else {
            let room = w - acc;
            if room > 0 {
                out.push(Span::styled(trunc(&sp.content, room), sp.style));
            }
            break;
        }
    }
    out
}

fn fit(left: Vec<Span<'static>>, right: Vec<Span<'static>>, w: usize) -> Vec<Span<'static>> {
    let rw = spans_width(&right).min(w);
    let gap = if rw > 0 { 1 } else { 0 };
    let mut out = clip_spans(left, w.saturating_sub(rw + gap));
    let used = spans_width(&out);
    out.push(Span::raw(" ".repeat(w.saturating_sub(used + rw))));
    out.extend(clip_spans(right, rw));
    out
}

fn with_bg(spans: Vec<Span<'static>>, bg: Option<Color>) -> Line<'static> {
    let l = Line::from(spans);
    match bg {
        Some(c) => l.style(Style::new().bg(c)),
        None => l,
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

/// Pinta una fila de botones alineada a la derecha del borde superior de `block_area` y registra los clics.
fn title_buttons(
    f: &mut Frame,
    app: &mut App,
    block_area: Rect,
    buttons: Vec<(Span<'static>, Act)>,
) {
    let total: usize = buttons.iter().map(|(s, _)| width(&s.content) + 1).sum();
    if block_area.width < total as u16 + 4 {
        return;
    }
    let mut x = block_area.x + block_area.width - 2 - total as u16 + 1;
    for (sp, act) in buttons {
        let w = width(&sp.content) as u16;
        let r = Rect::new(x, block_area.y, w, 1);
        f.render_widget(Paragraph::new(Line::from(sp)), r);
        app.hits.push((r, act));
        x += w + 1;
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    app.hits.clear();
    let area = f.area();
    if area.width < 60 || area.height < 18 {
        f.render_widget(
            Paragraph::new("Ventana demasiado pequeña (mínimo 60×18)"),
            area,
        );
        return;
    }
    let banner_h = if matches!(app.ws().map(|w| &w.analysis), Some(Analysis::Running(_))) {
        4
    } else {
        0
    };
    let [head, banner, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(banner_h),
        Constraint::Min(8),
        Constraint::Length(1),
    ])
    .areas(area);
    draw_header(f, app, head);
    if banner_h > 0 {
        draw_banner(f, app, banner);
    }
    let side_w = app
        .side_w
        .unwrap_or(((area.width as u32 * 26 / 100) as u16).clamp(38, 48))
        .clamp(24, area.width.saturating_sub(40).max(24));
    let [side, main] =
        Layout::horizontal([Constraint::Length(side_w), Constraint::Min(30)]).areas(body);
    app.geo.area = area;
    app.geo.side = side;
    app.geo.main = main;
    draw_sidebar(f, app, side);
    if app.wss.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(dim(
                "No hay conexiones. Crea la primera con Ctrl+N",
            )))
            .centered(),
            Rect::new(main.x, main.y + main.height / 2, main.width, 1),
        );
    } else {
        draw_main(f, app, main);
    }
    draw_footer(f, app, foot);
    match &app.dialog {
        Dialog::Form(_) => draw_form(f, app, area),
        Dialog::ConfirmDelete(i) => draw_confirm(f, app, *i, area),
        Dialog::Help => draw_help(f, area),
        Dialog::Cell(t) => draw_cell(f, t.clone(), area),
        Dialog::Info(t) => draw_info(f, t.clone(), area),
        Dialog::Notes(_) => draw_notes(f, app, area),
        Dialog::None => {}
    }
}

// ─────────────────────────────── cabecera ───────────────────────────────

fn draw_header(f: &mut Frame, app: &mut App, area: Rect) {
    let mut left = vec![
        Span::styled(
            " ◆ dbview ",
            Style::new()
                .bg(ACCENT)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];
    if let Some(ws) = app.ws() {
        let i = &ws.spec.info;
        left.push(Span::styled(
            format!(" {} ", env_tag(i.env)),
            Style::new()
                .bg(env_color(i.env))
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ));
        left.push(Span::raw(" "));
        left.push(bold(i.label.clone()));
        left.push(dim(format!("  {} · {}", i.dialect.name(), i.target)));
        match &ws.status {
            ConnStatus::Ok(v) => {
                left.push(fg(format!("  ● {}", v.split(" (").next().unwrap_or(v)), OK))
            }
            ConnStatus::Connecting => left.push(fg(format!("  {} conectando…", spin(app)), ACCENT)),
            ConnStatus::Failed(_) => left.push(fg("  ✗ sin conexión", ERR)),
            ConnStatus::Idle => {}
        }
    }
    let right = vec![
        faint(format!("build {} · ", app.build)),
        dim(format!("IA {} ", app.cfg.ai.model)),
    ];
    f.render_widget(
        Paragraph::new(Line::from(fit(left, right, area.width as usize))),
        area,
    );
}

// ─────────────────────────────── pie ───────────────────────────────

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    if let Some(t) = &app.toast {
        let c = if t.good { OK } else { ERR };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(" {} ", if t.good { "✓" } else { "✗" }),
                    Style::new().bg(c).fg(Color::Black),
                ),
                fg(format!(" {}", t.text), c),
            ])),
            area,
        );
        return;
    }
    let hints: &[(&str, &str)] = match (&app.dialog, app.focus) {
        (Dialog::Form(_), _) => &[
            ("Tab", "siguiente"),
            ("←→ espacio", "elegir opción"),
            ("^T", "probar"),
            ("^S", "guardar"),
            ("Esc", "cancelar"),
            ("^P", "ver clave"),
        ],
        (Dialog::Notes(_), _) => &[("^S", "guardar"), ("Esc", "cancelar")],
        (Dialog::None, Focus::Conns) => &[
            ("↑↓", "cambiar"),
            ("⏎", "usar"),
            ("^N", "nueva"),
            ("F2", "editar"),
            ("Supr", "borrar"),
            ("^L", "reconectar"),
        ],
        (Dialog::None, Focus::Tables) => &[
            ("escribe", "filtrar"),
            ("↑↓", "mover"),
            ("⏎", "insertar en SQL"),
            ("espacio/clic dcho", "★ fijar para la IA"),
        ],
        (Dialog::None, Focus::Question) => &[
            ("⏎", "generar SQL"),
            ("Alt+⏎", "nueva línea"),
            ("^T", "nueva/ajustar"),
            ("Alt+T", "pestaña nueva"),
            ("Tab", "siguiente"),
        ],
        (Dialog::None, Focus::Sql) => &[
            ("F5", "ejecutar"),
            ("^E", "corregir con IA"),
            ("^Z", "deshacer"),
            ("Tab", "siguiente"),
        ],
        (Dialog::None, Focus::Result) => &[
            ("↑↓←→", "celda"),
            ("⏎", "ver"),
            ("y", "copiar"),
            ("s", "ordenar"),
            ("e", "CSV"),
        ],
        _ => &[("Esc", "cerrar")],
    };
    let mut spans = vec![Span::raw(" ")];
    for (k, v) in hints {
        spans.push(Span::styled(
            format!(" {k} "),
            Style::new().bg(SEL_BG_DIM).fg(TEXT),
        ));
        spans.push(dim(format!(" {v}   ")));
    }
    let right = vec![
        fg("F1", ACCENT),
        dim(" ayuda  "),
        fg("Alt+↑↓", ACCENT),
        dim(" conexión  "),
        fg("^Q", ACCENT),
        dim(" salir "),
    ];
    f.render_widget(
        Paragraph::new(Line::from(fit(spans, right, area.width as usize))),
        area,
    );
}

// ─────────────────────────────── barra lateral ───────────────────────────────

fn draw_sidebar(f: &mut Frame, app: &mut App, area: Rect) {
    let n = app.wss.len() as u16;
    // Las conexiones mandan: se ven todas (o la mayoría) antes que las tablas y las columnas.
    let conn_h = app
        .conns_h
        .unwrap_or((n + 5).clamp(7, (area.height * 11 / 20).max(7)))
        .clamp(5, area.height.saturating_sub(6).max(5));
    let rest = area.height.saturating_sub(conn_h);
    let cols_h = app
        .cols_h
        .unwrap_or(if rest > 26 {
            10
        } else if rest > 18 {
            7
        } else {
            0
        })
        .min(rest.saturating_sub(5));
    let [conns, tables, cols] = Layout::vertical([
        Constraint::Length(conn_h),
        Constraint::Min(4),
        Constraint::Length(cols_h),
    ])
    .areas(area);
    app.geo.conns = conns;
    app.geo.tables = tables;
    app.geo.cols = cols;
    draw_conns(f, app, conns);
    draw_tables(f, app, tables);
    if cols_h > 0 {
        draw_columns(f, app, cols);
    }
}

fn draw_conns(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Conns && matches!(app.dialog, Dialog::None);
    let block = panel(
        vec![fg(" Conexiones ", if focused { ACCENT } else { DIM })],
        focus_color(focused),
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    // Cualquier clic dentro del panel (también en los huecos) le da el foco. Va PRIMERO: lo que se registre
    // después (filas, botones) tiene prioridad.
    app.hits.push((area, Act::Focus(Focus::Conns)));
    let w = inner.width as usize;
    let visible = inner.height.saturating_sub(3) as usize;
    let top = if app.cur >= visible {
        app.cur + 1 - visible
    } else {
        0
    };
    let mut lines = vec![];
    for (i, ws) in app.wss.iter().enumerate().skip(top).take(visible) {
        let sel = i == app.cur;
        let dot = match &ws.status {
            ConnStatus::Ok(_) => fg("●", OK),
            ConnStatus::Connecting => fg(spin(app), ACCENT),
            ConnStatus::Failed(_) => fg("✗", ERR),
            ConnStatus::Idle => fg("○", FAINT),
        };
        let i0 = ws.spec.info.clone();
        let busy = match ws.busy {
            Busy::No => Span::raw(""),
            _ => fg(format!("{} ", spin(app)), ACCENT),
        };
        let left = vec![
            // La selección se ve igual con o sin foco (antes, sin foco, el fondo casi no se distinguía): cambia la marca.
            if sel {
                fg(if focused { "▸" } else { "▏" }, ACCENT)
            } else {
                Span::raw(" ")
            },
            dot,
            Span::raw(" "),
            if sel {
                bold(i0.label.clone())
            } else {
                raw(i0.label.clone())
            },
        ];
        let engine = match i0.dialect {
            Dialect::Postgres => "PG",
            Dialect::Oracle => "ORA",
        };
        let right = vec![
            busy,
            faint(format!("{engine} ")),
            fg(env_tag(i0.env), env_color(i0.env)),
            Span::raw(" "),
        ];
        let line = fit(left, right, w);
        lines.push(with_bg(line, sel.then_some(SEL_BG)));
        let r = Rect::new(inner.x, inner.y + (i - top) as u16, inner.width, 1);
        app.hits.push((r, Act::Conn(i)));
    }
    f.render_widget(
        Paragraph::new(lines),
        Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(3),
        ),
    );
    // Botones de gestión (dos filas, siempre visibles)
    if inner.height >= 4 {
        let rows: [Vec<(&str, Act)>; 2] = [
            vec![("+ Nueva conexión", Act::NewConn)],
            vec![
                ("✎ Editar", Act::EditConn(app.cur)),
                ("✕ Borrar", Act::DelConn(app.cur)),
            ],
        ];
        for (ri, row) in rows.into_iter().enumerate() {
            let y = inner.y + inner.height - 2 + ri as u16;
            let mut x = inner.x + 1;
            for (label, act) in row {
                let sp = ghost(label, "");
                let wd = width(&sp.content) as u16;
                if x + wd > inner.x + inner.width {
                    break;
                }
                let r = Rect::new(x, y, wd, 1);
                f.render_widget(Paragraph::new(Line::from(sp)), r);
                app.hits.push((r, act));
                x += wd + 1;
            }
        }
    }
}

fn draw_tables(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Tables && matches!(app.dialog, Dialog::None);
    let Some(ws) = app.wss.get_mut(app.cur) else {
        f.render_widget(panel(vec![dim(" Tablas ")], BORDER), area);
        return;
    };
    let list = ws.filtered_tables();
    let total = ws.schema.as_ref().map(|s| s.tables.len()).unwrap_or(0);
    let pinned = ws.pinned.len();
    let title = vec![
        fg(
            format!(
                " Tablas {}",
                if total > 0 {
                    format!("({}) ", total)
                } else {
                    String::new()
                }
            ),
            if focused { ACCENT } else { DIM },
        ),
        if pinned > 0 {
            fg(format!("★{pinned} "), WARN)
        } else {
            Span::raw("")
        },
        match &ws.analysis {
            Analysis::Running(p) => fg(
                format!(
                    "{} {:.0} % ",
                    SPIN[(app.tick as usize / 2) % SPIN.len()],
                    p.fraction() * 100.0
                ),
                ACCENT,
            ),
            Analysis::Done if ws.catalog.is_some() => fg("✓ analizada ", OK),
            Analysis::Failed(_) => fg("✗ sin analizar ", ERR),
            _ => Span::raw(""),
        },
    ];
    let block = panel(title, focus_color(focused));
    let inner = block.inner(area);
    f.render_widget(block, area);
    // Todo el panel (filtro, hueco bajo la lista…) da el foco a las tablas.
    app.hits.push((area, Act::Focus(Focus::Tables)));
    if inner.height < 3 {
        return;
    }
    let w = inner.width as usize;
    // Filtro
    let filter = if ws.tables_filter.is_empty() {
        vec![fg(" ⌕ ", FAINT), faint("filtrar tablas…")]
    } else {
        vec![
            fg(" ⌕ ", ACCENT),
            raw(ws.tables_filter.clone()),
            if focused {
                fg("▏", ACCENT)
            } else {
                Span::raw("")
            },
        ]
    };
    f.render_widget(
        Paragraph::new(Line::from(filter)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    if !ws.tables_filter.is_empty() {
        let r = Rect::new(inner.x + inner.width.saturating_sub(4), inner.y, 3, 1);
        f.render_widget(Paragraph::new(fg("✕", DIM)), r);
        app.hits.push((r, Act::ClearFilter));
    }
    let body = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1);
    let mut lines = vec![];
    match (&ws.schema, &ws.schema_state) {
        (None, SchemaState::Loading) => lines.push(Line::from(vec![
            fg(
                format!(" {} ", SPIN[(app.tick as usize / 2) % SPIN.len()]),
                ACCENT,
            ),
            dim("leyendo el esquema…"),
        ])),
        (None, SchemaState::Failed(e)) => {
            lines.push(Line::from(fg(" ✗ no se pudo leer el esquema", ERR)));
            lines.extend(
                wrap(e, w.saturating_sub(2))
                    .into_iter()
                    .take(4)
                    .map(|l| Line::from(faint(format!(" {l}")))),
            );
        }
        (None, _) => lines.push(Line::from(faint(" sin esquema"))),
        (Some(s), _) => {
            if list.is_empty() {
                lines.push(Line::from(faint(" ninguna tabla coincide")));
            }
            let vis = body.height as usize;
            ws.tables_sel = ws.tables_sel.min(list.len().saturating_sub(1));
            if ws.tables_sel < ws.tables_top {
                ws.tables_top = ws.tables_sel;
            } else if ws.tables_sel >= ws.tables_top + vis {
                ws.tables_top = ws.tables_sel + 1 - vis;
            }
            for (vi, &ti) in list.iter().enumerate().skip(ws.tables_top).take(vis) {
                let t = &s.tables[ti];
                let sel = vi == ws.tables_sel;
                let pin = ws.pinned.contains(&t.qualified());
                let glyph = if t.kind == TableKind::View {
                    fg("◫", VIOLET)
                } else {
                    fg("▦", CODE)
                };
                let left = vec![
                    if sel {
                        fg(if focused { "▸" } else { "▏" }, ACCENT)
                    } else {
                        Span::raw(" ")
                    },
                    glyph,
                    Span::raw(" "),
                    if sel {
                        bold(t.qualified())
                    } else {
                        raw(t.qualified())
                    },
                ];
                let right = vec![if pin { fg("★ ", WARN) } else { Span::raw("") }];
                lines.push(with_bg(fit(left, right, w), sel.then_some(SEL_BG)));
                let r = Rect::new(body.x, body.y + (vi - ws.tables_top) as u16, body.width, 1);
                app.hits.push((r, Act::Table(vi)));
                // Solo la ★ ya puesta responde al clic izquierdo (la quita): una zona de «fijar» invisible al final
                // del nombre se activaba sin querer al pulsar sobre el texto. Fijar: espacio, o clic derecho.
                if pin {
                    let rp = Rect::new(
                        body.x + body.width.saturating_sub(2),
                        body.y + (vi - ws.tables_top) as u16,
                        2,
                        1,
                    );
                    app.hits.push((rp, Act::Pin(vi)));
                }
            }
        }
    }
    f.render_widget(Paragraph::new(lines), body);
}

fn draw_columns(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(ws) = app.wss.get_mut(app.cur) else {
        return;
    };
    let list = ws.filtered_tables();
    let schema = ws.schema.clone();
    let table = schema
        .as_ref()
        .and_then(|s| list.get(ws.tables_sel).map(|&i| &s.tables[i]));
    // Al cambiar de tabla, las columnas vuelven al principio.
    let qualified = table.map(|t| t.qualified()).unwrap_or_default();
    if ws.cols_table != qualified {
        ws.cols_table = qualified;
        ws.cols_top = 0;
    }
    let block_title = |extra: String| match table {
        Some(t) => vec![
            dim(" Columnas · "),
            fg(format!("{} ", trunc(&t.name, 20)), CODE),
            faint(extra),
        ],
        None => vec![dim(" Columnas ")],
    };
    let block = panel(block_title(String::new()), BORDER);
    let inner = block.inner(area);
    app.hits.push((area, Act::Focus(Focus::Tables)));
    let Some(t) = table else {
        f.render_widget(block, area);
        return;
    };
    let w = inner.width as usize;
    let mut lines: Vec<Line> = vec![];
    if let Some(d) = ws
        .catalog
        .as_ref()
        .and_then(|c| c.tables.get(&t.qualified()))
    {
        for l in wrap(d, w.saturating_sub(2)).into_iter().take(2) {
            lines.push(Line::from(vec![Span::raw(" "), fg(l, VIOLET)]));
        }
    }
    let head = lines.len();
    let room = (inner.height as usize).saturating_sub(head);
    let n = t.columns.len();
    // La rueda solo suma: aquí se recorta al último desplazamiento que aún llena el panel.
    ws.cols_top = ws.cols_top.min(n.saturating_sub(room));
    let top = ws.cols_top;
    let extra = if n > room {
        format!("· {}-{} de {} ", top + 1, (top + room).min(n), n)
    } else {
        String::new()
    };
    f.render_widget(panel(block_title(extra), BORDER), area);
    let mut hits = vec![];
    for (k, (ci, c)) in t
        .columns
        .iter()
        .enumerate()
        .skip(top)
        .take(room)
        .enumerate()
    {
        let left = vec![
            Span::raw(" "),
            if c.primary_key {
                fg("⚿ ", WARN)
            } else {
                Span::raw("  ")
            },
            raw(c.name.clone()),
        ];
        lines.push(Line::from(fit(
            left,
            vec![faint(c.type_name.clone()), Span::raw(" ")],
            w,
        )));
        hits.push((
            Rect::new(inner.x, inner.y + (head + k) as u16, inner.width, 1),
            Act::Column(ci),
        ));
    }
    f.render_widget(Paragraph::new(lines), inner);
    app.hits.extend(hits);
}

// ─────────────────────────────── zona principal ───────────────────────────────

fn draw_main(f: &mut Frame, app: &mut App, area: Rect) {
    let err_rows: u16 = app
        .ws()
        .map(|w| {
            if w.ai_error.is_some() {
                2 + w.suggest.is_some() as u16
            } else {
                0
            }
        })
        .unwrap_or(0);
    if app.zoom {
        // Zoom (Alt+Z): el panel con el foco ocupa toda la zona principal.
        app.geo.q = Rect::default();
        app.geo.s = Rect::default();
        app.geo.r = area;
        match app.focus {
            Focus::Sql => {
                app.geo.s = area;
                draw_sql(f, app, area)
            }
            Focus::Question => {
                app.geo.q = area;
                draw_question(f, app, area)
            }
            _ => draw_result(f, app, area),
        }
        return;
    }
    let q_h = app
        .q_h
        .unwrap_or(if err_rows > 0 {
            (5 + err_rows).min(area.height / 2)
        } else {
            6u16.min(area.height / 4).max(5)
        })
        .clamp(5, area.height.saturating_sub(10).max(5));
    let [q, rest] = Layout::vertical([Constraint::Length(q_h), Constraint::Min(8)]).areas(area);
    let sql_h = app
        .sql_h
        .unwrap_or((rest.height * 2 / 5).max(7))
        .clamp(6, rest.height.saturating_sub(5).max(6));
    let [s, r] = Layout::vertical([Constraint::Length(sql_h), Constraint::Min(4)]).areas(rest);
    app.geo.q = q;
    app.geo.s = s;
    app.geo.r = r;
    draw_question(f, app, q);
    draw_sql(f, app, s);
    draw_result(f, app, r);
}

fn draw_question(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Question && matches!(app.dialog, Dialog::None);
    let spinner = spin(app);
    let Some(ws) = app.ws() else { return };
    let busy = ws.busy == Busy::Ai;
    let mut title = vec![
        fg(" ① ", if focused { ACCENT } else { DIM }),
        bold("Pregunta "),
        faint("· en lenguaje natural "),
    ];
    if busy {
        title.push(fg(format!("{spinner} generando SQL… "), ACCENT));
    }
    let color = focus_color(focused);
    let block = panel(title, color);
    let inner = block.inner(area);
    f.render_widget(block, area);
    // El clic de todo el panel va PRIMERO: lo que se registre después (botones, pestañas) tiene prioridad.
    app.hits.push((area, Act::Focus(Focus::Question)));
    let Some(ws) = app.ws() else { return };
    let mode = ws.mode;
    let has_sql = !ws.sql.is_empty();
    let ai_err = ws.ai_error.clone();
    let pending = ws.pending_generate;
    let mut buttons = vec![];
    let mode_sp = if mode == AiMode::New {
        Span::styled(" ✦ Nueva consulta ", Style::new().bg(SEL_BG_DIM).fg(TEXT))
    } else {
        Span::styled(
            " ✎ Ajustar la actual ",
            Style::new().bg(VIOLET).fg(Color::Black),
        )
    };
    if has_sql || mode == AiMode::Adjust {
        buttons.push((mode_sp, Act::ToggleMode));
    }
    if busy {
        buttons.push((button("■ Parar", "Esc", ERR), Act::Cancel));
    } else {
        buttons.push((button("✨ Generar SQL", "⏎", ACCENT), Act::Generate));
    }
    title_buttons(f, app, area, buttons);
    let Some(ws) = app.wss.get_mut(app.cur) else {
        return;
    };
    let suggest = ws
        .suggest
        .and_then(|i| app.wss.get(i).map(|w| (i, w.spec.info.label.clone())));
    let Some(ws) = app.wss.get_mut(app.cur) else {
        return;
    };
    let mut msg_lines: Vec<String> = ai_err
        .as_ref()
        .map(|e| {
            wrap(&format!("✗ {e}"), inner.width.saturating_sub(4) as usize)
                .into_iter()
                .take(2)
                .collect()
        })
        .unwrap_or_default();
    // El editor conserva siempre una fila: pestañas (1) + editor (1) mandan sobre el aviso y la sugerencia.
    let max_reserve = inner.height.saturating_sub(2);
    let sug_rows = (suggest.is_some() as u16).min(max_reserve);
    let suggest = suggest.filter(|_| sug_rows > 0);
    msg_lines.truncate((max_reserve - sug_rows) as usize);
    let reserve = if ai_err.is_some() {
        (msg_lines.len() as u16 + sug_rows).min(max_reserve)
    } else {
        (pending as u16).min(max_reserve)
    };
    let text_area = Rect::new(
        inner.x + 1,
        inner.y + 1,
        inner.width.saturating_sub(2),
        inner.height.saturating_sub(reserve + 1),
    );
    draw_editor(
        f,
        text_area,
        &mut ws.question,
        focused,
        false,
        None,
        "Escribe lo que quieres consultar. Ej.: los últimos clientes con código de cliente 806, de hace una semana hasta ahora",
        false,
    );
    let base_y = inner.y + inner.height - reserve;
    for (i, l) in msg_lines.iter().enumerate() {
        let r = Rect::new(
            inner.x + 1,
            base_y + i as u16,
            inner.width.saturating_sub(2),
            1,
        );
        f.render_widget(Paragraph::new(Line::from(fg(l.clone(), ERR))), r);
    }
    if let Some((idx, label)) = suggest {
        let txt = format!(" ↪ Probar esta pregunta en «{label}»  F7 ");
        let w = width(&txt) as u16;
        let r = Rect::new(
            inner.x + 1,
            base_y + msg_lines.len() as u16,
            w.min(inner.width.saturating_sub(2)),
            1,
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                txt,
                Style::new()
                    .bg(VIOLET)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ))),
            r,
        );
        app.hits.push((r, Act::SwitchAsk(idx)));
    }
    if ai_err.is_none() && pending && reserve > 0 {
        let r = Rect::new(
            inner.x + 1,
            inner.y + inner.height - 1,
            inner.width.saturating_sub(2),
            1,
        );
        f.render_widget(
            Paragraph::new(Line::from(fg(
                "esperando a que termine de leerse el esquema…",
                WARN,
            ))),
            r,
        );
    }
    draw_tabs(
        f,
        app,
        Rect::new(inner.x, inner.y, inner.width, 1),
        TabKind::Question,
    );
}

fn draw_sql(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Sql && matches!(app.dialog, Dialog::None);
    let spinner = spin(app);
    let Some(ws) = app.ws() else { return };
    let running = ws.busy == Busy::Run;
    let ai = ws.busy == Busy::Ai;
    let has_err = ws.error.is_some();
    let mut title = vec![
        fg(" ② ", if focused { ACCENT } else { DIM }),
        bold("SQL "),
        faint("· generado por la IA, editable "),
    ];
    if ai {
        let step = if ws.phase.is_empty() {
            "escribiendo".to_string()
        } else {
            ws.phase.clone()
        };
        let txt = if ws.repair > 0 {
            format!(
                "{spinner} corrigiendo la consulta (intento {}/{}) ",
                ws.repair, MAX_REPAIRS
            )
        } else {
            format!("{spinner} {step}… ")
        };
        title.push(fg(txt, ACCENT));
    } else if running && ws.chain {
        title.push(fg(format!("{spinner} comprobando en la base… "), ACCENT));
    }
    let block = panel(title, focus_color(focused));
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.hits.push((area, Act::Focus(Focus::Sql)));
    let mut buttons = vec![];
    if has_err {
        buttons.push((button("✨ Corregir con IA", "^E", VIOLET), Act::Fix));
    }
    buttons.push((ghost("⌫ Limpiar", ""), Act::Clear));
    if running {
        buttons.push((button("■ Cancelar", "Esc", ERR), Act::Cancel));
    } else {
        buttons.push((button("▶ Ejecutar", "F5", OK), Act::Run));
    }
    title_buttons(f, app, area, buttons);
    let Some(ws) = app.wss.get_mut(app.cur) else {
        return;
    };
    let err_line = ws.error.as_ref().map(|e| {
        let loc = e.position.map(|p| {
            let lead = ws
                .error_sql
                .chars()
                .take_while(|c| c.is_whitespace())
                .count();
            let (r, c) = ws.sql.pos_of_offset(p + lead);
            (r, c)
        });
        (e.clone(), loc)
    });
    // Zona inferior del panel: pasos del agente, tablas elegidas, aprendizaje y error (de abajo arriba).
    let plan_line = ws
        .plan
        .as_ref()
        .filter(|p| !p.tables.is_empty())
        .map(|p| (p.tables.clone(), p.reason.clone()));
    let steps: Vec<String> = ws.steps.clone();
    let show_steps = !steps.is_empty() && ai;
    let summary = !steps.is_empty() && !ai;
    let lessons = ws.lessons.clone();
    let mut extra: u16 = err_line.is_some() as u16
        + plan_line.is_some() as u16
        + (!lessons.is_empty()) as u16
        + summary as u16;
    let max_extra = inner.height.saturating_sub(4);
    let step_rows = if show_steps {
        (steps.len() as u16)
            .min(4)
            .min(max_extra.saturating_sub(extra))
    } else {
        0
    };
    extra += step_rows;
    let reserve = extra.min(max_extra);
    let text_area = Rect::new(
        inner.x,
        inner.y + 1,
        inner.width,
        inner.height.saturating_sub(reserve + 1),
    );
    let mut y = inner.y + inner.height;
    let take_row = |y: &mut u16| -> Rect {
        *y = y.saturating_sub(1);
        Rect::new(inner.x + 1, *y, inner.width.saturating_sub(2), 1)
    };
    if err_line.is_some() {
        let _ = take_row(&mut y);
    }
    if !lessons.is_empty() {
        let r = take_row(&mut y);
        let label = "[ Guardar en las notas  F6 ]";
        let lw = width(label) as u16;
        f.render_widget(
            Paragraph::new(Line::from(vec![
                fg("💡 La IA ha descubierto: ", WARN),
                raw(trunc(
                    &lessons.join("; "),
                    (r.width as usize).saturating_sub(lw as usize + 28),
                )),
            ])),
            r,
        );
        if r.width > lw {
            let b = Rect::new(r.x + r.width - lw, r.y, lw, 1);
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    label,
                    Style::new()
                        .bg(WARN)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD),
                ))),
                b,
            );
            app.hits.push((b, Act::SaveLessons));
        }
    }
    if summary {
        let r = take_row(&mut y);
        app.hits.push((r, Act::Info));
        let list = steps
            .iter()
            .map(|x| {
                x.replace("mirando las columnas de ", "columnas ")
                    .replace("probando la consulta (paso", "prueba (paso")
            })
            .collect::<Vec<_>>()
            .join(" › ");
        f.render_widget(
            Paragraph::new(Line::from(vec![
                fg("🔎 Investigó en ", DIM),
                fg(format!("{} pasos: ", steps.len()), TEXT),
                faint(trunc(&list, (r.width as usize).saturating_sub(24))),
            ])),
            r,
        );
    }
    if let Some((tables, reason)) = &plan_line {
        let r = take_row(&mut y);
        app.hits.push((r, Act::Info));
        let names = tables.join(", ");
        let nm = trunc(&names, (r.width as usize) / 2);
        let why = if reason.is_empty() {
            String::new()
        } else {
            format!(" — {}", reason.replace('\n', " "))
        };
        let room = (r.width as usize).saturating_sub(20 + width(&nm));
        f.render_widget(
            Paragraph::new(Line::from(vec![
                fg("▦ Tablas elegidas: ", VIOLET),
                raw(nm),
                if reason.starts_with('⚠') {
                    fg(trunc(&why, room), WARN)
                } else {
                    dim(trunc(&why, room))
                },
            ])),
            r,
        );
    }
    if step_rows > 0 {
        let first = steps.len().saturating_sub(step_rows as usize);
        for (i, st) in steps.iter().enumerate().skip(first).rev() {
            let r = take_row(&mut y);
            let last = i + 1 == steps.len();
            let mark = if ai && last {
                fg(format!("{spinner} "), ACCENT)
            } else {
                fg("✓ ", OK)
            };
            f.render_widget(
                Paragraph::new(Line::from(vec![mark, dim(trunc(st, r.width as usize - 2))])),
                r,
            );
        }
    }
    let mark = err_line.as_ref().and_then(|(_, l)| *l);
    draw_editor(
        f,
        text_area,
        &mut ws.sql,
        focused,
        true,
        mark,
        "El SQL aparecerá aquí. También puedes escribirlo tú y pulsar F5.",
        true,
    );
    if let Some((e, loc)) = err_line {
        let r = Rect::new(
            inner.x + 1,
            inner.y + inner.height - 1,
            inner.width.saturating_sub(2),
            1,
        );
        let place = loc
            .map(|(r, c)| format!("  · línea {}, col {}", r + 1, c + 1))
            .unwrap_or_default();
        f.render_widget(
            Paragraph::new(Line::from(vec![
                fg("✗ ", ERR),
                fg(
                    trunc(
                        &e.message.replace('\n', " "),
                        (r.width as usize).saturating_sub(place.len() + 3),
                    ),
                    ERR,
                ),
                dim(place),
            ])),
            r,
        );
    }
    draw_tabs(
        f,
        app,
        Rect::new(inner.x, inner.y, inner.width, 1),
        TabKind::Sql,
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TabKind {
    /// Tira del panel ①: nombre = principio de la pregunta.
    Question,
    /// Tira del panel ②: `SQL n`, enlazada con la misma pestaña de ①.
    Sql,
}

/// Tira de pestañas al estilo DBeaver: ` nombre ✕ ` por pestaña y `+` para abrir otra.
/// Las dos tiras (① y ②) enseñan las mismas pestañas: elegir una cambia también el SQL y el resultado.
fn draw_tabs(f: &mut Frame, app: &mut App, row: Rect, kind: TabKind) {
    if row.width < 12 || row.height == 0 {
        return;
    }
    let spinner = spin(app);
    let Some(ws) = app.ws() else { return };
    let active = ws.active;
    struct Item {
        text: String,
        close: String,
        err: bool,
    }
    let items: Vec<Item> = ws
        .tabs
        .iter()
        .map(|t| {
            let mark = if t.busy != Busy::No {
                format!("{spinner} ")
            } else if t.error.is_some() || t.ai_error.is_some() {
                "✗ ".to_string()
            } else {
                String::new()
            };
            let name = match kind {
                TabKind::Question => t.question_label(),
                TabKind::Sql => t.sql_label(),
            };
            Item {
                text: format!(" {mark}{name} "),
                close: "✕ ".to_string(),
                err: t.error.is_some() || t.ai_error.is_some(),
            }
        })
        .collect();
    let plus_w: usize = 3;
    let w_of = |it: &Item| width(&it.text) + width(&it.close) + 1;
    let total: usize = items.iter().map(w_of).sum();
    let room = (row.width as usize).saturating_sub(plus_w + 1);
    // Ventana de pestañas visibles: siempre incluye la activa.
    let (first, last, marks) = if total <= room {
        (0, items.len() - 1, false)
    } else {
        let room = room.saturating_sub(4);
        let (mut a, mut b) = (active, active);
        let mut used = w_of(&items[active]).min(room);
        loop {
            let mut grew = false;
            if a > 0 && used + w_of(&items[a - 1]) <= room {
                a -= 1;
                used += w_of(&items[a]);
                grew = true;
            }
            if b + 1 < items.len() && used + w_of(&items[b + 1]) <= room {
                b += 1;
                used += w_of(&items[b]);
                grew = true;
            }
            if !grew {
                break;
            }
        }
        (a, b, true)
    };
    let mut x = row.x;
    let end = row.x + row.width;
    let put = |f: &mut Frame, x: &mut u16, txt: String, style: Style| -> Rect {
        let w = (width(&txt) as u16).min(end.saturating_sub(*x));
        let r = Rect::new(*x, row.y, w, 1);
        f.render_widget(Paragraph::new(Line::from(Span::styled(txt, style))), r);
        *x += w;
        r
    };
    if marks {
        let txt = if first > 0 { "‹ " } else { "  " };
        put(f, &mut x, txt.to_string(), Style::new().fg(DIM));
    }
    let mut hits = vec![];
    for (i, it) in items.iter().enumerate().take(last + 1).skip(first) {
        let on = i == active;
        let base = if on {
            Style::new()
                .bg(ACCENT)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new()
                .bg(SEL_BG_DIM)
                .fg(if it.err { ERR } else { TEXT })
        };
        let avail = (end.saturating_sub(x) as usize).saturating_sub(width(&it.close) + 1);
        let r1 = put(f, &mut x, trunc(&it.text, avail.max(1)), base);
        let close_style = if on {
            base
        } else {
            Style::new().bg(SEL_BG_DIM).fg(DIM)
        };
        let r2 = put(f, &mut x, it.close.clone(), close_style);
        put(f, &mut x, " ".into(), Style::new());
        hits.push((r1, Act::Tab(i)));
        hits.push((r2, Act::CloseTab(i)));
    }
    if marks {
        let txt = if last + 1 < items.len() { "› " } else { "  " };
        put(f, &mut x, txt.to_string(), Style::new().fg(DIM));
    }
    let r = put(
        f,
        &mut x,
        " + ".to_string(),
        Style::new()
            .bg(OK)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
    );
    hits.push((r, Act::NewTab));
    app.hits.extend(hits);
}

/// Editor con ajuste de línea, resaltado SQL opcional, selección y marca de error.
fn draw_editor(
    f: &mut Frame,
    area: Rect,
    ed: &mut Editor,
    focused: bool,
    sql: bool,
    mark: Option<(usize, usize)>,
    placeholder: &str,
    gutter: bool,
) {
    if area.width < 4 || area.height == 0 {
        return;
    }
    let gw = if gutter {
        (ed.lines().len().to_string().len() + 2) as u16
    } else {
        1
    };
    let text = Rect::new(
        area.x + gw,
        area.y,
        area.width.saturating_sub(gw + 1),
        area.height,
    );
    ed.area = text;
    ed.width = text.width as usize;
    ed.height = text.height as usize;
    if ed.is_empty() {
        let lines: Vec<Line> = wrap(placeholder, text.width as usize)
            .into_iter()
            .map(|l| Line::from(faint(l)))
            .collect();
        f.render_widget(Paragraph::new(lines), text);
        if focused {
            f.set_cursor_position(Position::new(text.x, text.y));
        }
        return;
    }
    let rows = ed.layout(text.width as usize);
    ed.follow(&rows, text.height as usize);
    let toks = if sql {
        sqlhl::classify(ed.lines())
    } else {
        vec![]
    };
    let sel = ed.selection();
    let in_sel = |r: usize, c: usize| sel.is_some_and(|(s, e)| (r, c) >= s && (r, c) < e);
    let mut out: Vec<Line> = vec![];
    let mut nums: Vec<Line> = vec![];
    for (vi, vr) in rows
        .iter()
        .enumerate()
        .skip(ed.scroll)
        .take(text.height as usize)
    {
        let _ = vi;
        let line = &ed.lines()[vr.line];
        let mut spans: Vec<Span> = vec![];
        let (mut buf, mut cur_style) = (String::new(), Style::new());
        for c in vr.start..vr.end {
            let ch = line[c];
            let tok = toks
                .get(vr.line)
                .and_then(|l| l.get(c))
                .copied()
                .unwrap_or(Tok::Plain);
            let mut st = Style::new().fg(if sql { tok.color() } else { TEXT });
            if in_sel(vr.line, c) {
                st = st.bg(SEL_BG).fg(Color::White);
            }
            if mark == Some((vr.line, c)) {
                st = st.bg(ERR).fg(Color::White).add_modifier(Modifier::BOLD);
            }
            if st != cur_style && !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buf), cur_style));
            }
            cur_style = st;
            if ch == '\t' {
                buf.push_str("  ")
            } else {
                buf.push(ch)
            }
        }
        if mark == Some((vr.line, vr.end)) && vr.end == line.len() {
            if !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buf), cur_style));
            }
            spans.push(Span::styled(" ", Style::new().bg(ERR)));
        }
        if !buf.is_empty() {
            spans.push(Span::styled(buf, cur_style));
        }
        out.push(Line::from(spans));
        if gutter {
            let first = vr.start == 0;
            nums.push(Line::from(fg(
                if first {
                    format!("{:>w$} ", vr.line + 1, w = gw as usize - 1)
                } else {
                    " ".repeat(gw as usize)
                },
                FAINT,
            )));
        }
    }
    f.render_widget(Paragraph::new(out), text);
    if gutter {
        f.render_widget(
            Paragraph::new(nums),
            Rect::new(area.x, area.y, gw, area.height),
        );
    }
    if focused {
        let (vr, vc) = ed.cursor_visual(&rows);
        if vr >= ed.scroll && vr < ed.scroll + text.height as usize {
            f.set_cursor_position(Position::new(
                text.x + vc as u16,
                text.y + (vr - ed.scroll) as u16,
            ));
        }
    }
}

// ─────────────────────────────── resultado ───────────────────────────────

fn draw_result(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Result && matches!(app.dialog, Dialog::None);
    let spinner = spin(app);
    let tick = app.tick;
    let Some(ws) = app.wss.get_mut(app.cur) else {
        return;
    };
    let mut title = vec![
        fg(" ③ ", if focused { ACCENT } else { DIM }),
        bold("Resultado "),
    ];
    let color = if ws.error.is_some() {
        ERR
    } else {
        focus_color(focused)
    };
    match (&ws.result, &ws.error) {
        (_, Some(_)) => title.push(fg("· error de la base de datos ", ERR)),
        (Some(r), None) => {
            let n = r.rows.len();
            title.push(faint(format!(
                "· {}{} filas · {} ",
                n,
                if r.truncated { "+" } else { "" },
                fmt_ms(r.elapsed_ms)
            )));
            if r.truncated {
                title.push(fg("(limitado) ", WARN));
            }
        }
        _ => {}
    }
    if ws.busy == Busy::Run {
        let secs = ws
            .busy_since
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(0.0);
        title.push(fg(format!("{spinner} ejecutando {secs:.1}s "), ACCENT));
    }
    let block = panel(title, color);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if ws.result.is_some() && ws.error.is_none() {
        let mut buttons = vec![(ghost("⇩ CSV", "e"), Act::Export)];
        buttons.insert(0, (ghost("⎘ Copiar celda", "y"), Act::Focus(Focus::Result)));
        title_buttons(f, app, area, buttons.split_off(1));
    }
    let Some(ws) = app.wss.get_mut(app.cur) else {
        return;
    };
    if let Some(e) = ws.error.clone() {
        let w = inner.width.saturating_sub(4) as usize;
        let mut lines = vec![
            Line::default(),
            Line::from(vec![
                Span::raw("  "),
                fg("✗ ", ERR),
                Span::styled(
                    e.message.clone(),
                    Style::new().fg(ERR).add_modifier(Modifier::BOLD),
                ),
            ]),
        ];
        for l in wrap(&e.message, w).into_iter().skip(1) {
            lines.push(Line::from(vec![Span::raw("    "), fg(l, ERR)]));
        }
        let mut meta = vec![];
        if let Some(c) = &e.code {
            meta.push(format!("código {c}"));
        }
        if let Some(p) = e.position {
            let lead = ws
                .error_sql
                .chars()
                .take_while(|c| c.is_whitespace())
                .count();
            let (r, c) = ws.sql.pos_of_offset(p + lead);
            meta.push(format!("línea {}, columna {}", r + 1, c + 1));
        }
        if !meta.is_empty() {
            lines.push(Line::from(vec![Span::raw("  "), dim(meta.join(" · "))]));
        }
        for (label, v) in [("Detalle", &e.detail), ("Sugerencia", &e.hint)] {
            if let Some(v) = v {
                lines.push(Line::default());
                for (i, l) in wrap(v, w.saturating_sub(12)).into_iter().enumerate() {
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        if i == 0 {
                            bold(format!("{label:<11}"))
                        } else {
                            Span::raw(" ".repeat(11))
                        },
                        raw(l),
                    ]));
                }
            }
        }
        lines.push(Line::default());
        if e.connection {
            lines.push(Line::from(vec![
                Span::raw("  "),
                dim("Problema de conexión: revísala con "),
                fg("F2", ACCENT),
                dim(" (editar) o reintenta con "),
                fg("Ctrl+L", ACCENT),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw("  "),
                dim("Edita el SQL de arriba y pulsa "),
                fg("F5", OK),
                dim(", o pide a la IA que lo arregle con "),
                fg("Ctrl+E", VIOLET),
            ]));
        }
        f.render_widget(Paragraph::new(lines), inner);
        return;
    }
    if ws.result.is_none() {
        let msg = if ws.busy == Busy::Run {
            format!("{spinner} Ejecutando la consulta…  (Esc para cancelar)")
        } else {
            "Aquí verás las filas reales de la base de datos.".into()
        };
        let hint = "Pregunta arriba y pulsa ⏎, o escribe SQL y pulsa F5";
        let y = inner.y + inner.height / 2;
        if inner.height >= 2 {
            f.render_widget(
                Paragraph::new(Line::from(dim(msg))).centered(),
                Rect::new(inner.x, y.saturating_sub(1), inner.width, 1),
            );
            f.render_widget(
                Paragraph::new(Line::from(faint(hint))).centered(),
                Rect::new(inner.x, y, inner.width, 1),
            );
        }
        return;
    }
    draw_table(f, app, inner, focused, tick);
}

fn draw_table(f: &mut Frame, app: &mut App, area: Rect, focused: bool, _tick: u64) {
    let Some(ws) = app.wss.get_mut(app.cur) else {
        return;
    };
    let Some(r) = ws.result.clone() else { return };
    if r.columns.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(dim(" La consulta no devolvió columnas"))),
            area,
        );
        return;
    }
    if r.rows.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(" "),
                fg("∅ ", WARN),
                dim("0 filas: la consulta es válida pero no devolvió resultados"),
            ])),
            area,
        );
    }
    let tv = &mut ws.tv;
    let nrows = tv.order.len();
    let numw = nrows.to_string().len().max(2) + 1;
    let body_h = area.height.saturating_sub(1) as usize;
    tv.page_rows = body_h.max(1);
    tv.row = tv.row.min(nrows.saturating_sub(1));
    tv.col = tv.col.min(r.columns.len() - 1);
    if tv.row < tv.top {
        tv.top = tv.row;
    } else if tv.row >= tv.top + body_h && body_h > 0 {
        tv.top = tv.row + 1 - body_h;
    }
    tv.top = tv.top.min(nrows.saturating_sub(body_h.max(1)));
    let avail = (area.width as usize).saturating_sub(numw + 1);
    // Columnas visibles desde `left`.
    if tv.col < tv.left {
        tv.left = tv.col;
    }
    let span_of = |from: usize, widths: &[u16]| -> usize {
        let mut used = 0;
        let mut n = 0;
        for w in &widths[from..] {
            let add = *w as usize + 3;
            if used + add > avail && n > 0 {
                break;
            }
            used += add;
            n += 1;
        }
        n
    };
    while tv.col >= tv.left + span_of(tv.left, &tv.widths).max(1) && tv.left < tv.col {
        tv.left += 1;
    }
    let ncols = span_of(tv.left, &tv.widths).max(1);
    tv.page_cols = ncols;
    let (left, top, sel_row, sel_col) = (tv.left, tv.top, tv.row, tv.col);
    let widths = tv.widths.clone();
    let numeric = tv.numeric.clone();
    let sort = tv.sort;
    let order = tv.order.clone();
    let last_col_w = |c: usize, w: u16| -> usize {
        if c + 1 == left + ncols {
            (avail.saturating_sub(
                widths[left..c]
                    .iter()
                    .map(|x| *x as usize + 3)
                    .sum::<usize>(),
            ))
            .min(w as usize)
            .max(1)
        } else {
            w as usize
        }
    };
    // Cabecera
    let mut head = vec![fg(pad_left("#", numw), FAINT), Span::raw(" ")];
    let mut x = area.x + numw as u16 + 1;
    for c in left..left + ncols {
        let w = last_col_w(c, widths[c]);
        let arrow = match sort {
            Some((sc, asc)) if sc == c => {
                if asc {
                    " ▲"
                } else {
                    " ▼"
                }
            }
            _ => "",
        };
        let name = format!("{}{}", r.columns[c].name, arrow);
        let st = if c == sel_col {
            Style::new()
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD)
        };
        head.push(Span::styled(
            if numeric[c] {
                pad_left(&trunc(&name, w), w)
            } else {
                pad(&trunc(&name, w), w)
            },
            st,
        ));
        head.push(fg(" │ ", FAINT));
        let rect = Rect::new(x, area.y, (w + 3) as u16, 1);
        app.hits.push((rect, Act::Sort(c)));
        x += (w + 3) as u16;
    }
    f.render_widget(
        Paragraph::new(with_bg(head, Some(BG_BAR))),
        Rect::new(area.x, area.y, area.width, 1),
    );
    // Filas
    let mut lines = vec![];
    for (vi, &ri) in order.iter().enumerate().skip(top).take(body_h) {
        let row = &r.rows[ri];
        let is_sel_row = vi == sel_row;
        let mut spans = vec![
            fg(pad_left(&(vi + 1).to_string(), numw), FAINT),
            Span::raw(" "),
        ];
        let mut x = area.x + numw as u16 + 1;
        for c in left..left + ncols {
            let w = last_col_w(c, widths[c]);
            let (txt, isnull) = match row.get(c).and_then(|v| v.as_ref()) {
                Some(v) => (v.replace(['\n', '\r', '\t'], " "), false),
                None => ("NULL".to_string(), true),
            };
            let cell = if numeric[c] && !isnull {
                pad_left(&trunc(&txt, w), w)
            } else {
                pad(&trunc(&txt, w), w)
            };
            let mut st = Style::new().fg(if isnull { FAINT } else { TEXT });
            if isnull {
                st = st.add_modifier(Modifier::ITALIC);
            }
            if is_sel_row {
                st = st.bg(SEL_BG_DIM);
            }
            if is_sel_row && c == sel_col {
                st = Style::new()
                    .bg(if focused { ACCENT } else { SEL_BG })
                    .fg(if focused { Color::Black } else { Color::White })
                    .add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled(cell, st));
            let sep_bg = if is_sel_row {
                Style::new().fg(FAINT).bg(SEL_BG_DIM)
            } else {
                Style::new().fg(FAINT)
            };
            spans.push(Span::styled(" │ ", sep_bg));
            let rect = Rect::new(x, area.y + 1 + (vi - top) as u16, (w + 3) as u16, 1);
            app.hits.push((rect, Act::Cell(vi, c)));
            x += (w + 3) as u16;
        }
        let bg = if vi % 2 == 1 && !is_sel_row {
            Some(Color::Rgb(0x16, 0x1a, 0x20))
        } else {
            None
        };
        lines.push(with_bg(spans, bg));
    }
    f.render_widget(
        Paragraph::new(lines),
        Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(1),
        ),
    );
    // Indicador de posición
    let info = format!(
        " fila {}/{} · col {}/{} ",
        sel_row + 1,
        nrows.max(1),
        sel_col + 1,
        r.columns.len()
    );
    let w = width(&info) as u16;
    if area.width > w + 2 && area.height > 0 {
        let rect = Rect::new(area.x + area.width - w, area.y + area.height - 1, w, 1);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                info,
                Style::new().bg(BG_BAR).fg(DIM),
            ))),
            rect,
        );
    }
    app.hits.push((area, Act::Focus(Focus::Result)));
    // las celdas están encima del foco general; el orden de `hits` (el último gana) lo permite
    let hits = std::mem::take(&mut app.hits);
    let (general, specific): (Vec<_>, Vec<_>) = hits
        .into_iter()
        .partition(|(rc, a)| *rc == area && matches!(a, Act::Focus(Focus::Result)));
    app.hits = general.into_iter().chain(specific).collect();
}

// ─────────────────────────────── diálogos ───────────────────────────────

fn draw_cell(f: &mut Frame, text: String, area: Rect) {
    let r = centered(area, 100, (area.height * 3 / 4).max(8));
    let block = panel(
        vec![
            bold(" Valor de la celda "),
            faint("· y copiar · esc cerrar "),
        ],
        ACCENT,
    );
    let inner = block.inner(r);
    f.render_widget(Clear, r);
    f.render_widget(block, r);
    let lines: Vec<Line> = text
        .lines()
        .flat_map(|l| wrap(l, inner.width.saturating_sub(2) as usize))
        .take(inner.height as usize)
        .map(|l| Line::from(vec![Span::raw(" "), raw(l)]))
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_confirm(f: &mut Frame, app: &mut App, i: usize, area: Rect) {
    let Some(ws) = app.wss.get(i) else { return };
    let name = ws.spec.info.label.clone();
    let r = centered(area, 60, 9);
    let block = panel(vec![bold(" Borrar conexión ")], ERR);
    let inner = block.inner(r);
    f.render_widget(Clear, r);
    f.render_widget(block, r);
    let lines = vec![
        Line::default(),
        Line::from(vec![
            Span::raw("  ¿Borrar la conexión "),
            bold(format!("«{name}»")),
            Span::raw("?"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            dim("Se quita de la configuración; la base de datos no se toca."),
        ]),
        Line::default(),
    ];
    f.render_widget(Paragraph::new(lines), inner);
    let y = inner.y + inner.height - 2;
    let yes = Rect::new(inner.x + 2, y, 12, 1);
    let no = Rect::new(inner.x + 16, y, 14, 1);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ^D Borrar ",
            Style::new()
                .bg(ERR)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ))),
        yes,
    );
    f.render_widget(Paragraph::new(Line::from(ghost("⏎ Cancelar", ""))), no);
    app.hits.push((yes, Act::ConfirmYes));
    app.hits.push((no, Act::ConfirmNo));
}

fn draw_help(f: &mut Frame, area: Rect) {
    const ROWS: &[(&str, &str)] = &[
        ("", "Flujo"),
        ("Enter (pregunta)", "la IA escribe el SQL y lo ejecuta"),
        ("F5 · Ctrl+R", "ejecutar el SQL del editor"),
        ("Ctrl+E", "pedir a la IA que corrija el error"),
        ("Ctrl+T", "alternar «Nueva consulta» / «Ajustar la actual»"),
        (
            "",
            "Pestañas de consulta (① pregunta + ② SQL + ③ resultado)",
        ),
        (
            "Alt+T · botón +",
            "nueva pestaña; cada una tiene su pregunta, SQL y resultado",
        ),
        (
            "Ctrl+RePág/AvPág",
            "pestaña anterior / siguiente (también clic, o Alt+RePág/AvPág)",
        ),
        (
            "Alt+W · ✕",
            "cerrar la pestaña (se borra también su SQL y su resultado)",
        ),
        ("Alt+U", "reabrir la última pestaña cerrada"),
        ("", "Moverse"),
        ("Tab · Shift+Tab", "siguiente / anterior panel (o clic)"),
        ("Alt+↑↓ · Alt+1…9", "cambiar de conexión"),
        ("Ctrl+N · F2", "nueva conexión · editar la actual"),
        ("Ctrl+L", "reconectar y releer el esquema"),
        (
            "F3",
            "notas de la BD: reglas de negocio que la IA debe respetar",
        ),
        (
            "F4",
            "análisis profundo con IA (lento, opcional): describe las tablas aún sin descripción",
        ),
        ("Esc", "parar lo que esté en marcha; si no hay nada, salir"),
        ("", "Editor"),
        ("Alt+Enter", "nueva línea en la pregunta"),
        ("Ctrl+Z · Ctrl+Y", "deshacer · rehacer"),
        (
            "Ctrl+A · C · X · V",
            "seleccionar todo · copiar · cortar · pegar",
        ),
        ("", "Resultado"),
        (
            "↑↓←→ · PgUp/PgDn",
            "mover la celda · g/G primera/última fila",
        ),
        ("Enter · y · Y", "ver valor · copiar celda · copiar fila"),
        ("s · clic en cabecera", "ordenar por la columna"),
        ("e", "exportar a CSV"),
        ("", "Tablas"),
        ("escribir · Enter", "filtrar · insertar el nombre en el SQL"),
        (
            "espacio · clic derecho",
            "★ fijar tabla para que la IA la tenga siempre en cuenta",
        ),
        (
            "rueda · clic en columna",
            "desplazar tablas/columnas · insertar la columna en el SQL",
        ),
        (
            "",
            "Solo lectura: nada de lo que hagas aquí modifica datos.",
        ),
    ];
    let r = centered(area, 84, ROWS.len() as u16 + 4);
    let block = panel(vec![bold(" Ayuda ")], ACCENT);
    let inner = block.inner(r);
    f.render_widget(Clear, r);
    f.render_widget(block, r);
    let lines: Vec<Line> = ROWS
        .iter()
        .map(|(k, v)| {
            if k.is_empty() {
                Line::from(vec![Span::raw(" "), fg(v.to_string(), VIOLET)])
            } else {
                Line::from(vec![
                    Span::raw("   "),
                    fg(pad(k, 22), ACCENT),
                    raw(v.to_string()),
                ])
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn field_line(
    ed: &Editor,
    w: usize,
    focused: bool,
    mask: bool,
    f: &mut Frame,
    rect: Rect,
    placeholder: &str,
) {
    let text: Vec<char> = if mask {
        ed.lines()[0].iter().map(|_| '•').collect()
    } else {
        ed.lines()[0].clone()
    };
    let col = ed.col.min(text.len());
    let start = (col + 1).saturating_sub(w);
    let shown: String = text.iter().skip(start).take(w).collect();
    let bg = if focused {
        Style::new().bg(SEL_BG).fg(Color::White)
    } else {
        Style::new().bg(SEL_BG_DIM).fg(TEXT)
    };
    let body = if shown.is_empty() {
        Span::styled(
            pad(placeholder, w),
            Style::new()
                .bg(if focused { SEL_BG } else { SEL_BG_DIM })
                .fg(FAINT),
        )
    } else {
        Span::styled(pad(&shown, w), bg)
    };
    f.render_widget(Paragraph::new(Line::from(body)), rect);
    if focused {
        let cx = rect.x + (col - start).min(w.saturating_sub(1)) as u16;
        f.set_cursor_position(Position::new(cx, rect.y));
    }
}

fn radio(opts: &[(&str, bool, Color)]) -> Vec<Span<'static>> {
    let mut v = vec![];
    for (name, on, c) in opts {
        v.push(if *on {
            fg("◉ ", *c)
        } else {
            fg("○ ", FAINT)
        });
        v.push(if *on {
            bold(name.to_string())
        } else {
            dim(name.to_string())
        });
        v.push(Span::raw("   "));
    }
    v
}

/// Un clic por opción del selector: cada una ocupa `◉ ` + nombre + 3 espacios, como las pinta `radio`.
fn radio_hits(opts: &[&str], rc: Rect, item: usize) -> Vec<(Rect, Act)> {
    let mut x = rc.x;
    let mut out = vec![];
    for (i, name) in opts.iter().enumerate() {
        let w = (width(name) + 2 + 2) as u16;
        out.push((
            Rect::new(x, rc.y, w.min(rc.width), 1),
            Act::FormPick(item, i),
        ));
        x += w + 1;
    }
    out
}

fn draw_form(f: &mut Frame, app: &mut App, area: Rect) {
    let spinner = spin(app);
    let Dialog::Form(form) = &app.dialog else {
        return;
    };
    let form: &Form = form;
    let editing = form.editing.is_some();
    let r = centered(area, 78, 26);
    let title = if editing {
        " ✎ Editar conexión "
    } else {
        " + Nueva conexión "
    };
    let block = panel(vec![bold(title)], ACCENT);
    let inner = block.inner(r);
    f.render_widget(Clear, r);
    f.render_widget(block, r);
    let lw = 16u16;
    let fw = inner.width.saturating_sub(lw + 4) as usize;
    let mut hits: Vec<(Rect, Act)> = vec![];
    // Clics por opción: se registran después de los de fila, para ganarles.
    let mut pick_hits: Vec<(Rect, Act)> = vec![];
    let mut y = inner.y + 1;
    let mut row = |f: &mut Frame, label: &str, item: usize, y: u16| -> Rect {
        let focused = form.focus == item;
        let lab = Span::styled(
            format!(" {label:<w$}", w = lw as usize - 1),
            Style::new().fg(if focused { ACCENT } else { DIM }),
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                if focused {
                    fg("▸", ACCENT)
                } else {
                    Span::raw(" ")
                },
                lab,
            ])),
            Rect::new(inner.x, y, lw + 1, 1),
        );
        let rect = Rect::new(inner.x + lw + 2, y, fw as u16, 1);
        hits.push((Rect::new(inner.x, y, inner.width, 1), Act::FormItem(item)));
        rect
    };
    let rc = row(f, "Nombre", form::F_NAME, y);
    field_line(
        &form.name,
        fw,
        form.focus == form::F_NAME,
        false,
        f,
        rc,
        "p. ej. Ventas · DESARROLLO",
    );
    y += 2;
    let rc = row(f, "Tipo de BD", form::F_DIALECT, y);
    f.render_widget(
        Paragraph::new(Line::from(radio(&[
            ("PostgreSQL", form.dialect == Dialect::Postgres, ACCENT),
            ("Oracle", form.dialect == Dialect::Oracle, ACCENT),
        ]))),
        rc,
    );
    pick_hits.extend(radio_hits(&["PostgreSQL", "Oracle"], rc, form::F_DIALECT));
    y += 1;
    let rc = row(f, "Entorno", form::F_ENV, y);
    f.render_widget(
        Paragraph::new(Line::from(radio(&[
            ("Desarrollo", form.env == EnvKind::Dev, OK),
            ("Producción", form.env == EnvKind::Prod, ERR),
            ("Otro", form.env == EnvKind::Other, WARN),
        ]))),
        rc,
    );
    pick_hits.extend(radio_hits(
        &["Desarrollo", "Producción", "Otro"],
        rc,
        form::F_ENV,
    ));
    y += 2;
    let rc = row(f, "Servidor", form::F_HOST, y);
    field_line(
        &form.host,
        fw,
        form.focus == form::F_HOST,
        false,
        f,
        rc,
        "host o IP",
    );
    y += 1;
    let rc = row(f, "Puerto", form::F_PORT, y);
    field_line(
        &form.port,
        8.min(fw),
        form.focus == form::F_PORT,
        false,
        f,
        Rect::new(rc.x, rc.y, 8.min(rc.width), 1),
        "",
    );
    y += 1;
    let rc = row(
        f,
        if form.is_oracle() {
            "SID / servicio"
        } else {
            "Base de datos"
        },
        form::F_DB,
        y,
    );
    field_line(
        &form.database,
        fw,
        form.focus == form::F_DB,
        false,
        f,
        rc,
        if form.is_oracle() {
            "p. ej. ORCL"
        } else {
            "nombre de la base"
        },
    );
    y += 1;
    if form.is_oracle() {
        let rc = row(f, "Se conecta por", form::F_MODE, y);
        f.render_widget(
            Paragraph::new(Line::from(radio(&[
                ("SID", !form.oracle_service, ACCENT),
                ("Nombre de servicio", form.oracle_service, ACCENT),
            ]))),
            rc,
        );
        pick_hits.extend(radio_hits(&["SID", "Nombre de servicio"], rc, form::F_MODE));
        y += 1;
    }
    let rc = row(f, "Esquemas", form::F_SCHEMAS, y);
    field_line(
        &form.schemas,
        fw,
        form.focus == form::F_SCHEMAS,
        false,
        f,
        rc,
        "vacío = todos · o p. ej. VENTAS_APP, VENTAS_AUX",
    );
    y += 2;
    let rc = row(f, "Usuario", form::F_USER, y);
    field_line(&form.user, fw, form.focus == form::F_USER, false, f, rc, "");
    y += 1;
    let rc = row(f, "Contraseña", form::F_PASS, y);
    let note = form.password_note().unwrap_or_default();
    field_line(
        &form.password,
        fw,
        form.focus == form::F_PASS,
        !form.show_pw,
        f,
        rc,
        &note,
    );
    y += 2;
    // Resultado de la prueba / error de validación
    let rect = Rect::new(inner.x + 2, y, inner.width.saturating_sub(4), 3);
    let lines: Vec<Line> = if let Some(e) = &form.error {
        vec![Line::from(fg(format!("⚠ {e}"), WARN))]
    } else {
        match &form.test {
            TestState::Idle => vec![Line::from(faint(
                "Pulsa «Probar conexión» para comprobar los datos antes de guardar.",
            ))],
            TestState::Running => vec![Line::from(fg(
                format!("{spinner} Probando la conexión…"),
                ACCENT,
            ))],
            TestState::Ok { info, ms } => vec![Line::from(vec![
                fg("✓ Conectado correctamente", OK),
                dim(format!(" · {info} · {}", fmt_ms(*ms))),
            ])],
            TestState::Err(e) => {
                let mut v = vec![Line::from(fg(
                    format!("✗ {}", trunc(&e.message, rect.width as usize - 3)),
                    ERR,
                ))];
                if let Some(d) = &e.detail {
                    v.push(Line::from(faint(trunc(d, rect.width as usize))));
                }
                v
            }
        }
    };
    f.render_widget(Paragraph::new(lines), rect);
    // Botones
    let by = inner.y + inner.height - 2;
    let mut x = inner.x + 2;
    for (label, key, act, item, color) in [
        (
            "⚡ Probar conexión",
            "^T",
            Act::FormTest,
            form::F_TEST,
            ACCENT,
        ),
        ("✓ Guardar", "^S", Act::FormSave, form::F_SAVE, OK),
        ("Cancelar", "Esc", Act::FormCancel, form::F_CANCEL, DIM),
    ] {
        let focused = form.focus == item;
        let sp = if focused || item != form::F_CANCEL {
            Span::styled(
                format!(" {label} {key} "),
                Style::new()
                    .bg(color)
                    .fg(Color::Black)
                    .add_modifier(if focused {
                        Modifier::BOLD | Modifier::UNDERLINED
                    } else {
                        Modifier::BOLD
                    }),
            )
        } else {
            ghost(label, key)
        };
        let wd = width(&sp.content) as u16;
        let rect = Rect::new(x, by, wd, 1);
        f.render_widget(Paragraph::new(Line::from(sp)), rect);
        hits.push((rect, act));
        x += wd + 2;
    }
    app.hits.extend(hits);
    app.hits.extend(pick_hits);
}

fn draw_notes(f: &mut Frame, app: &mut App, area: Rect) {
    let Dialog::Notes(n) = &mut app.dialog else {
        return;
    };
    let label = app
        .wss
        .get(n.conn)
        .map(|w| w.spec.info.label.clone())
        .unwrap_or_default();
    let r = centered(area, 100, (area.height * 3 / 4).max(14));
    let block = panel(
        vec![
            bold(format!(" Notas de la BD · {label} ")),
            faint("· la IA las respeta por encima de sus suposiciones "),
        ],
        VIOLET,
    );
    let inner = block.inner(r);
    f.render_widget(Clear, r);
    f.render_widget(block, r);
    let hint = "Escribe reglas de negocio, una por línea. Ej.: «Un pedido es una fila de PEDIDOS; la fecha es FCHPEDIDO». «Código de cliente = CODCLI».";
    let lines: Vec<Line> = wrap(hint, inner.width.saturating_sub(4) as usize)
        .into_iter()
        .map(|l| Line::from(vec![Span::raw("  "), dim(l)]))
        .collect();
    let hh = lines.len() as u16;
    f.render_widget(
        Paragraph::new(lines),
        Rect::new(inner.x, inner.y + 1, inner.width, hh),
    );
    let text = Rect::new(
        inner.x + 2,
        inner.y + hh + 2,
        inner.width.saturating_sub(4),
        inner.height.saturating_sub(hh + 5),
    );
    draw_editor(
        f,
        text,
        &mut n.ed,
        true,
        false,
        None,
        "(sin notas todavía)",
        false,
    );
    let by = inner.y + inner.height - 1;
    let save = Rect::new(inner.x + 2, by, 16, 1);
    let cancel = Rect::new(inner.x + 20, by, 14, 1);
    f.render_widget(
        Paragraph::new(Line::from(button("✓ Guardar", "^S", OK))),
        save,
    );
    f.render_widget(Paragraph::new(Line::from(ghost("Cancelar", "Esc"))), cancel);
    app.hits.push((save, Act::NotesSave));
    app.hits.push((cancel, Act::CloseDialog));
}

/// Aviso bien visible mientras la IA estudia la base de datos: porcentaje, fase y qué esperar.
fn draw_banner(f: &mut Frame, app: &App, area: Rect) {
    let Some(ws) = app.ws() else { return };
    let Analysis::Running(p) = &ws.analysis else {
        return;
    };
    let spinner = SPIN[(app.tick as usize / 2) % SPIN.len()];
    let frac = p.fraction();
    let title = if p.first {
        " Comprendiendo la base de datos "
    } else {
        " Actualizando lo que la IA sabe de la base "
    };
    let block = panel(vec![fg(format!(" {spinner}"), ACCENT), bold(title)], ACCENT);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let phase = match p.phase.as_str() {
        "estructura" => "leyendo comentarios, claves y relaciones".to_string(),
        "resumen" => "redactando el resumen general".to_string(),
        _ => format!(
            "entendiendo para qué sirve cada tabla ({}/{})",
            p.done, p.total
        ),
    };
    let secs = p.since.elapsed().as_secs_f32();
    let eta = if frac > 0.06 {
        format!(" · quedan ≈ {}", human_secs(secs / frac * (1.0 - frac)))
    } else {
        String::new()
    };
    let bar_w = (inner.width as usize).saturating_sub(36).clamp(10, 60);
    let filled = ((bar_w as f32) * frac).round() as usize;
    let line1 = Line::from(vec![
        Span::raw(" "),
        fg("█".repeat(filled), ACCENT),
        fg("░".repeat(bar_w - filled.min(bar_w)), FAINT),
        bold(format!("  {:>3.0} %", frac * 100.0)),
        dim(format!("  {phase}{eta}")),
    ]);
    let why = if p.first {
        format!(
            "Solo ocurre la primera vez con esta base ({} tablas). Ya puedes preguntar, pero las respuestas serán más precisas al terminar.",
            p.tables
        )
    } else {
        format!(
            "Se han detectado {} tablas nuevas o modificadas. Se revisa la estructura de vez en cuando por si cambia.",
            p.pending
        )
    };
    let line2 = Line::from(vec![
        Span::raw(" "),
        fg(trunc(&why, (inner.width as usize).saturating_sub(2)), WARN),
    ]);
    f.render_widget(Paragraph::new(vec![line1, line2]), inner);
}

fn human_secs(s: f32) -> String {
    if s < 90.0 {
        format!("{:.0} s", s.max(1.0))
    } else {
        format!("{:.0} min", (s / 60.0).ceil())
    }
}

fn draw_info(f: &mut Frame, text: String, area: Rect) {
    let r = centered(area, 110, (area.height * 4 / 5).max(10));
    let block = panel(
        vec![
            bold(" Detalle de la investigación "),
            faint("· nada recortado · Esc cierra "),
        ],
        VIOLET,
    );
    let inner = block.inner(r);
    f.render_widget(Clear, r);
    f.render_widget(block, r);
    let w = inner.width.saturating_sub(3) as usize;
    let lines: Vec<Line> = text
        .lines()
        .flat_map(|l| {
            if l.is_empty() {
                vec![String::new()]
            } else {
                wrap(l, w)
            }
        })
        .take(inner.height as usize)
        .map(|l| Line::from(vec![Span::raw(" "), raw(l)]))
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}
