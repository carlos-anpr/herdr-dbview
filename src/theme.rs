//! Paleta, glifos y utilidades de texto de la interfaz.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const BG_BAR: Color = Color::Rgb(0x1b, 0x1f, 0x26);
pub const ACCENT: Color = Color::Rgb(0x4f, 0xa3, 0xff);
pub const DIM: Color = Color::Rgb(0x80, 0x86, 0x90);
pub const FAINT: Color = Color::Rgb(0x4a, 0x50, 0x5a);
pub const BORDER: Color = Color::Rgb(0x3b, 0x41, 0x4b);
pub const TEXT: Color = Color::Rgb(0xd7, 0xdb, 0xe0);
pub const SEL_BG: Color = Color::Rgb(0x1f, 0x3a, 0x5a);
pub const SEL_BG_DIM: Color = Color::Rgb(0x26, 0x2b, 0x33);
pub const OK: Color = Color::Rgb(0x3f, 0xb9, 0x50);
pub const ERR: Color = Color::Rgb(0xf8, 0x51, 0x49);
pub const WARN: Color = Color::Rgb(0xe3, 0xb3, 0x41);
pub const CODE: Color = Color::Rgb(0x56, 0xb6, 0xc2);
pub const VIOLET: Color = Color::Rgb(0xb3, 0x8d, 0xf7);

pub const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// ───────────────────────────── spans ─────────────────────────────

pub fn dim(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::new().fg(DIM))
}

pub fn faint(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::new().fg(FAINT))
}

pub fn bold(s: impl Into<String>) -> Span<'static> {
    Span::styled(
        s.into(),
        Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
    )
}

pub fn fg(s: impl Into<String>, c: Color) -> Span<'static> {
    Span::styled(s.into(), Style::new().fg(c))
}

pub fn raw(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::new().fg(TEXT))
}

pub fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| width(&s.content)).sum()
}

// ───────────────────────────── texto ─────────────────────────────

pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Recorta a `w` columnas con "…".
pub fn trunc(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut acc = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if acc + cw > w - 1 {
            break;
        }
        acc += cw;
        out.push(c);
    }
    out.push('…');
    out
}

/// Recorta por la izquierda (deja el final): útil para rutas.
pub fn trunc_left(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let mut rev: Vec<char> = Vec::new();
    let mut acc = 0;
    for c in s.chars().rev() {
        let cw = c.width().unwrap_or(0);
        if acc + cw > w - 1 {
            break;
        }
        acc += cw;
        rev.push(c);
    }
    std::iter::once('…').chain(rev.into_iter().rev()).collect()
}

pub fn pad(s: &str, w: usize) -> String {
    let t = trunc(s, w);
    let n = w.saturating_sub(width(&t));
    format!("{t}{}", " ".repeat(n))
}

pub fn pad_left(s: &str, w: usize) -> String {
    let t = trunc(s, w);
    let n = w.saturating_sub(width(&t));
    format!("{}{t}", " ".repeat(n))
}

/// Parte un texto en líneas de ancho `w` (por palabras; las palabras largas se cortan).
pub fn wrap(s: &str, w: usize) -> Vec<String> {
    let w = w.max(8);
    let mut out = Vec::new();
    for raw in s.lines() {
        if raw.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let indent: String = raw.chars().take_while(|c| *c == ' ').take(w / 2).collect();
        let mut cur = String::new();
        for word in raw.split_whitespace() {
            if !cur.is_empty() && width(&cur) + 1 + width(word) > w {
                out.push(std::mem::take(&mut cur));
                cur.push_str(&indent);
            } else if cur.is_empty() {
                cur.push_str(&indent);
            } else {
                cur.push(' ');
            }
            let mut word = word.to_string();
            while width(&cur) + width(&word) > w {
                let room = w.saturating_sub(width(&cur)).max(1);
                let mut cut = String::new();
                let mut acc = 0;
                for c in word.chars() {
                    let cw = c.width().unwrap_or(0);
                    if acc + cw > room && !cut.is_empty() {
                        break;
                    }
                    acc += cw;
                    cut.push(c);
                }
                word = word[cut.len()..].to_string();
                cur.push_str(&cut);
                out.push(std::mem::take(&mut cur));
                cur.push_str(&indent);
            }
            cur.push_str(&word);
        }
        out.push(cur);
    }
    out
}

// ───────────────────────────── tiempo y cifras ─────────────────────────────

/// Milisegundos de una herramienta: "350ms", "2.4s", "1m 05s".
pub fn fmt_ms(ms: u64) -> String {
    match ms {
        0..1000 => format!("{ms}ms"),
        1000..10_000 => format!("{:.1}s", ms as f64 / 1000.0),
        _ => format!("{}m {:02}s", ms / 60_000, (ms / 1000) % 60),
    }
}

/// Vuelca un buffer de ratatui como página HTML (revisión del diseño y capturas).
pub fn buffer_html(buf: &ratatui::buffer::Buffer) -> String {
    fn css(c: Color, default: &str) -> String {
        const BASE: [&str; 16] = [
            "#1e2227", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#abb2bf",
            "#5c6370", "#ff7b86", "#b1e18b", "#f2d08a", "#7cc0ff", "#d992ee", "#6fd0dc", "#ffffff",
        ];
        match c {
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            Color::Black => BASE[0].into(),
            Color::White => "#ffffff".into(),
            Color::Indexed(i) if i < 16 => BASE[i as usize].into(),
            Color::Indexed(i) if i >= 232 => {
                let v = 8 + (i - 232) as u32 * 10;
                format!("#{v:02x}{v:02x}{v:02x}")
            }
            Color::Indexed(i) => {
                let i = i - 16;
                let f = |v: u8| if v == 0 { 0 } else { 55 + v as u32 * 40 };
                format!("#{:02x}{:02x}{:02x}", f(i / 36), f((i / 6) % 6), f(i % 6))
            }
            _ => default.into(),
        }
    }
    let area = buf.area;
    let mut out = String::from(
        "<!doctype html><meta charset=utf-8><body style=\"margin:0;background:#0d1117\"><pre style=\"margin:0;padding:14px;\
         font:16px/18px 'DejaVu Sans Mono','JetBrains Mono',monospace;color:#d7dbe0;background:#11151b;display:inline-block\">",
    );
    for y in 0..area.height {
        let mut skip = 0;
        for x in 0..area.width {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let cell = &buf[(x, y)];
            let sym = cell.symbol();
            skip = width(sym).saturating_sub(1);
            let (mut fgc, mut bgc) = (css(cell.fg, "#d7dbe0"), css(cell.bg, "transparent"));
            if cell.modifier.contains(Modifier::REVERSED) {
                std::mem::swap(&mut fgc, &mut bgc);
                if fgc == "transparent" {
                    fgc = "#11151b".into();
                }
            }
            // Celda de ancho fijo: un glifo que la fuente dibuje más ancho no descoloca la fila.
            let mut st = format!(
                "display:inline-block;width:{}ch;height:18px;overflow:hidden;vertical-align:top;color:{fgc};background:{bgc}",
                skip + 1
            );
            if cell.modifier.contains(Modifier::BOLD) {
                st.push_str(";font-weight:bold");
            }
            if cell.modifier.contains(Modifier::DIM) {
                st.push_str(";opacity:.6");
            }
            if cell.modifier.contains(Modifier::ITALIC) {
                st.push_str(";font-style:italic");
            }
            let esc = sym
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            out.push_str(&format!("<span style=\"{st}\">{esc}</span>"));
        }
        out.push('\n');
    }
    out.push_str("</pre></body>");
    out
}
