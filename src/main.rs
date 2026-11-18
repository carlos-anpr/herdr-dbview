#![allow(dead_code, clippy::too_many_arguments, clippy::needless_range_loop)]
mod agent;
mod ai;
mod app;
mod bench;
mod catalog;
mod config;
mod db;
mod editor;
mod explorer;
#[cfg(test)]
mod explorer_tests;
mod form;
mod history;
mod model;
mod nl2sql;
mod notes;
mod sqlhl;
mod theme;
mod ui;

use app::App;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use ratatui::crossterm::execute;
use std::time::Duration;

const USAGE: &str = "herdr-dbview [OPCIONES]

Consola de base de datos para herdr: pregunta en lenguaje natural, SQL editable y
resultados reales, con cambio rápido de conexión. Solo lectura.

  --demo            conexiones y datos de ejemplo, sin red
  --render AxB      pintar un fotograma de A columnas por B filas como HTML y salir
  --keys TECLAS     (con --render) teclas a pulsar antes de pintar: letras y {enter} {esc} {tab} {up}…
  DBVIEW_RENDER_WAIT_MS  (con --render) ms que se espera a la IA tras {enter}/{f5}
  -V, --version     versión

Configuración: ~/.config/herdr-dbview/config.toml  ·  IA: DBVIEW_AI_MODEL, DBVIEW_AI_API_URL,
DBVIEW_AI_API_KEY_VALUE, DBVIEW_AI_EFFORT";

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cfg = config::load();
    let (mut demo, mut render, mut keys) = (false, None, String::new());
    let (mut ask, mut conn, mut mode, mut with_notes) =
        (None::<String>, String::new(), "full".to_string(), true);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--demo" => demo = true,
            "--ask" => ask = it.next().cloned(),
            "--conn" => conn = it.next().cloned().unwrap_or_default(),
            "--mode" => mode = it.next().cloned().unwrap_or_default(),
            "--notes" => with_notes = it.next().is_some_and(|v| v == "on"),
            "--render" => {
                render = it
                    .next()
                    .and_then(|s| s.split_once('x'))
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
            }
            "--keys" => keys = it.next().cloned().unwrap_or_default(),
            "-V" | "--version" => {
                println!("herdr-dbview {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => {
                println!("{USAGE}");
                return Ok(());
            }
        }
    }
    if let Some(q) = ask {
        return bench::run(&cfg, &q, &conn, &mode, with_notes);
    }
    if demo {
        cfg.connections = db::demo::connections();
        cfg.default_conn = None;
    }
    let mut app = App::new(cfg);
    if let Some((w, h)) = render {
        return render_html(&mut app, w, h, &keys, demo);
    }
    let mut term = ratatui::init();
    execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    let res = run(&mut term, &mut app);
    let _ = execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste
    );
    ratatui::restore();
    res
}

fn run(term: &mut ratatui::DefaultTerminal, app: &mut App) -> anyhow::Result<()> {
    while !app.quit {
        term.draw(|f| ui::draw(f, app))?;
        if event::poll(Duration::from_millis(60))? {
            app.on_event(event::read()?);
            while event::poll(Duration::ZERO)? {
                app.on_event(event::read()?);
                if app.quit {
                    break;
                }
            }
        }
        while let Ok(m) = app.rx.try_recv() {
            app.on_msg(m);
        }
        app.tick();
    }
    Ok(())
}

fn render_html(app: &mut App, w: u16, h: u16, keys: &str, demo: bool) -> anyhow::Result<()> {
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let pump = |app: &mut App, ms: u64| {
        let start = std::time::Instant::now();
        let end = start + Duration::from_millis(ms);
        loop {
            while let Ok(m) = app.rx.try_recv() {
                app.on_msg(m);
            }
            if std::time::Instant::now() >= end
                || (ms >= 5000
                    && start.elapsed() > Duration::from_millis(1500)
                    && app.wss.iter().all(|w| !w.any_work()))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    pump(app, if demo { 300 } else { 2500 });
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h))?;
    term.draw(|f| ui::draw(f, app))?;
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let mut slow = false;
        let (code, mods) = if c == '{' {
            let name: String = chars.by_ref().take_while(|c| *c != '}').collect();
            match name.as_str() {
                "enter" => {
                    slow = true;
                    (KeyCode::Enter, KeyModifiers::NONE)
                }
                "esc" => (KeyCode::Esc, KeyModifiers::NONE),
                "tab" => (KeyCode::Tab, KeyModifiers::NONE),
                "up" => (KeyCode::Up, KeyModifiers::NONE),
                "down" => (KeyCode::Down, KeyModifiers::NONE),
                "left" => (KeyCode::Left, KeyModifiers::NONE),
                "right" => (KeyCode::Right, KeyModifiers::NONE),
                "f5" => {
                    slow = true;
                    (KeyCode::F(5), KeyModifiers::NONE)
                }
                "f1" => (KeyCode::F(1), KeyModifiers::NONE),
                "f2" => (KeyCode::F(2), KeyModifiers::NONE),
                "f3" => (KeyCode::F(3), KeyModifiers::NONE),
                "f4" => (KeyCode::F(4), KeyModifiers::NONE),
                "altdown" => (KeyCode::Down, KeyModifiers::ALT),
                "altenter" => (KeyCode::Enter, KeyModifiers::ALT),
                "ctrln" => (KeyCode::Char('n'), KeyModifiers::CONTROL),
                "ctrlt" => (KeyCode::Char('t'), KeyModifiers::CONTROL),
                "ctrle" => (KeyCode::Char('e'), KeyModifiers::CONTROL),
                _ => continue,
            }
        } else {
            (KeyCode::Char(c), KeyModifiers::NONE)
        };
        app.on_event(Event::Key(KeyEvent::new(code, mods)));
        pump(
            app,
            if let Some(ms) = std::env::var("DBVIEW_RENDER_WAIT_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|_| slow)
            {
                ms
            } else if demo {
                400
            } else if slow {
                9000
            } else {
                30
            },
        );
        term.draw(|f| ui::draw(f, app))?;
    }
    app.tick = 4;
    term.draw(|f| ui::draw(f, app))?;
    print!("{}", theme::buffer_html(term.backend().buffer()));
    Ok(())
}
