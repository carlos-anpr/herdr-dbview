//! Editor de texto multilínea con ajuste de línea, selección, deshacer y ratón.
//! Sin dependencias de dibujo: `ui.rs` lo pinta a partir de `layout()`.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Debug, PartialEq)]
struct Snap {
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
}

/// Fila visual: un tramo de una línea lógica que cabe en una fila de pantalla.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VRow {
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug)]
pub struct Editor {
    lines: Vec<Vec<char>>,
    pub row: usize,
    pub col: usize,
    anchor: Option<(usize, usize)>,
    undo: Vec<Snap>,
    redo: Vec<Snap>,
    /// Primera fila visual visible.
    pub scroll: usize,
    /// Zona donde se pintó por última vez (para ratón) y su ancho útil.
    pub area: Rect,
    /// Columna visual deseada al subir/bajar.
    want: Option<usize>,
    pub width: usize,
    pub height: usize,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new("")
    }
}

fn chars_w(cs: &[char]) -> usize {
    cs.iter().map(|c| c.width().unwrap_or(0)).sum()
}

impl Editor {
    pub fn new(text: &str) -> Self {
        let mut e = Editor {
            lines: vec![vec![]],
            row: 0,
            col: 0,
            anchor: None,
            undo: vec![],
            redo: vec![],
            scroll: 0,
            area: Rect::default(),
            want: None,
            width: 40,
            height: 5,
        };
        e.set_text(text);
        e
    }

    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(|l| l.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub fn lines(&self) -> &[Vec<char>] {
        &self.lines
    }

    /// Sustituye todo el texto (sin entrar en el historial de deshacer) y pone el cursor al final.
    pub fn set_text(&mut self, text: &str) {
        self.lines = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .split('\n')
            .map(|l| l.chars().collect())
            .collect();
        if self.lines.is_empty() {
            self.lines.push(vec![]);
        }
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].len();
        self.anchor = None;
        self.undo.clear();
        self.redo.clear();
        self.scroll = 0;
        self.want = None;
    }

    /// Como `set_text` pero conservando el historial: el usuario puede deshacer lo que escribió la IA.
    pub fn replace_all(&mut self, text: &str) {
        self.push_undo();
        let keep = std::mem::take(&mut self.undo);
        self.set_text(text);
        self.undo = keep;
        self.scroll = 0;
    }

    /// Guarda el estado actual para poder deshacer (antes de que la IA escriba encima).
    pub fn checkpoint(&mut self) {
        self.push_undo();
    }

    /// Sustituye el texto sin tocar el historial (escritura en directo de la IA).
    pub fn stream_set(&mut self, text: &str) {
        let (u, r) = (
            std::mem::take(&mut self.undo),
            std::mem::take(&mut self.redo),
        );
        self.set_text(text);
        self.undo = u;
        self.redo = r;
    }

    fn snap(&self) -> Snap {
        Snap {
            lines: self.lines.clone(),
            row: self.row,
            col: self.col,
        }
    }

    fn push_undo(&mut self) {
        let s = self.snap();
        if self.undo.last() != Some(&s) {
            self.undo.push(s);
            if self.undo.len() > 200 {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
    }

    pub fn undo(&mut self) {
        if let Some(s) = self.undo.pop() {
            self.redo.push(self.snap());
            self.restore(s);
        }
    }

    pub fn redo(&mut self) {
        if let Some(s) = self.redo.pop() {
            self.undo.push(self.snap());
            self.restore(s);
        }
    }

    fn restore(&mut self, s: Snap) {
        self.lines = s.lines;
        self.row = s.row;
        self.col = s.col;
        self.anchor = None;
        self.want = None;
    }

    // ───────────── disposición visual ─────────────

    pub fn layout(&self, width: usize) -> Vec<VRow> {
        let width = width.max(2);
        let mut out = vec![];
        for (i, l) in self.lines.iter().enumerate() {
            let (mut start, mut w) = (0, 0);
            for (j, c) in l.iter().enumerate() {
                let cw = c.width().unwrap_or(0);
                if w + cw > width && j > start {
                    out.push(VRow {
                        line: i,
                        start,
                        end: j,
                    });
                    start = j;
                    w = 0;
                }
                w += cw;
            }
            out.push(VRow {
                line: i,
                start,
                end: l.len(),
            });
        }
        out
    }

    /// Fila visual y columna de pantalla del cursor.
    pub fn cursor_visual(&self, rows: &[VRow]) -> (usize, usize) {
        let mut found = 0;
        for (i, r) in rows.iter().enumerate() {
            if r.line == self.row
                && self.col >= r.start
                && (self.col < r.end
                    || (self.col == r.end && (i + 1 >= rows.len() || rows[i + 1].line != r.line)))
            {
                found = i;
                break;
            }
        }
        let r = rows[found];
        (
            found,
            chars_w(&self.lines[r.line][r.start..self.col.min(r.end)]),
        )
    }

    /// Ajusta el desplazamiento para que el cursor quede a la vista.
    pub fn follow(&mut self, rows: &[VRow], height: usize) {
        let (vr, _) = self.cursor_visual(rows);
        let h = height.max(1);
        if vr < self.scroll {
            self.scroll = vr;
        } else if vr >= self.scroll + h {
            self.scroll = vr + 1 - h;
        }
        self.scroll = self.scroll.min(rows.len().saturating_sub(h));
    }

    // ───────────── selección ─────────────

    pub fn selection(&self) -> Option<((usize, usize), (usize, usize))> {
        let a = self.anchor?;
        let b = (self.row, self.col);
        if a == b {
            return None;
        }
        Some(if a < b { (a, b) } else { (b, a) })
    }

    pub fn selected_text(&self) -> Option<String> {
        let (s, e) = self.selection()?;
        let mut out = String::new();
        for r in s.0..=e.0 {
            let l = &self.lines[r];
            let from = if r == s.0 { s.1 } else { 0 };
            let to = if r == e.0 { e.1 } else { l.len() };
            out.extend(&l[from..to]);
            if r != e.0 {
                out.push('\n');
            }
        }
        Some(out)
    }

    pub fn select_all(&mut self) {
        self.anchor = Some((0, 0));
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].len();
    }

    fn delete_selection(&mut self) -> bool {
        let Some((s, e)) = self.selection() else {
            return false;
        };
        let tail: Vec<char> = self.lines[e.0][e.1..].to_vec();
        self.lines[s.0].truncate(s.1);
        self.lines[s.0].extend(tail);
        self.lines.drain(s.0 + 1..=e.0);
        self.row = s.0;
        self.col = s.1;
        self.anchor = None;
        true
    }

    // ───────────── edición ─────────────

    pub fn insert_str(&mut self, text: &str) {
        self.push_undo();
        self.delete_selection();
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let mut parts = text.split('\n');
        if let Some(first) = parts.next() {
            let ins: Vec<char> = first
                .chars()
                .filter(|c| !c.is_control() || *c == '\t')
                .collect();
            let n = ins.len();
            let at = self.col;
            self.lines[self.row].splice(at..at, ins);
            self.col += n;
        }
        for p in parts {
            self.split_line(false);
            let ins: Vec<char> = p
                .chars()
                .filter(|c| !c.is_control() || *c == '\t')
                .collect();
            let n = ins.len();
            let at = self.col;
            self.lines[self.row].splice(at..at, ins);
            self.col += n;
        }
        self.want = None;
    }

    fn split_line(&mut self, indent: bool) {
        let rest: Vec<char> = self.lines[self.row].split_off(self.col);
        let mut new: Vec<char> = vec![];
        if indent {
            new.extend(
                self.lines[self.row]
                    .iter()
                    .take_while(|c| **c == ' ' || **c == '\t'),
            );
        }
        let n = new.len();
        new.extend(rest);
        self.lines.insert(self.row + 1, new);
        self.row += 1;
        self.col = n;
    }

    pub fn newline(&mut self) {
        self.push_undo();
        self.delete_selection();
        self.split_line(true);
        self.want = None;
    }

    /// Inserta en el cursor (nombre de tabla desde el explorador, por ejemplo).
    pub fn insert_at_cursor(&mut self, text: &str) {
        self.insert_str(text);
    }

    fn word_left(&self) -> (usize, usize) {
        let (mut r, mut c) = (self.row, self.col);
        if c == 0 {
            return if r > 0 {
                (r - 1, self.lines[r - 1].len())
            } else {
                (0, 0)
            };
        }
        let l = &self.lines[r];
        while c > 0 && !l[c - 1].is_alphanumeric() && l[c - 1] != '_' {
            c -= 1;
        }
        while c > 0 && (l[c - 1].is_alphanumeric() || l[c - 1] == '_') {
            c -= 1;
        }
        r = r.min(self.lines.len() - 1);
        (r, c)
    }

    fn word_right(&self) -> (usize, usize) {
        let (r, mut c) = (self.row, self.col);
        let l = &self.lines[r];
        if c >= l.len() {
            return if r + 1 < self.lines.len() {
                (r + 1, 0)
            } else {
                (r, l.len())
            };
        }
        while c < l.len() && !(l[c].is_alphanumeric() || l[c] == '_') {
            c += 1;
        }
        while c < l.len() && (l[c].is_alphanumeric() || l[c] == '_') {
            c += 1;
        }
        (r, c)
    }

    fn move_to(&mut self, pos: (usize, usize), select: bool) {
        if select {
            if self.anchor.is_none() {
                self.anchor = Some((self.row, self.col));
            }
        } else {
            self.anchor = None;
        }
        self.row = pos.0;
        self.col = pos.1.min(self.lines[pos.0].len());
    }

    fn vertical(&mut self, delta: isize, select: bool) {
        let rows = self.layout(self.width);
        let (vr, vc) = self.cursor_visual(&rows);
        let want = self.want.unwrap_or(vc);
        let target = vr as isize + delta;
        if target < 0 {
            self.move_to((0, 0), select);
            return;
        }
        if target as usize >= rows.len() {
            let r = self.lines.len() - 1;
            let c = self.lines[r].len();
            self.move_to((r, c), select);
            return;
        }
        let r = rows[target as usize];
        let mut w = 0;
        let mut col = r.start;
        while col < r.end {
            let cw = self.lines[r.line][col].width().unwrap_or(0);
            if w + cw > want {
                break;
            }
            w += cw;
            col += 1;
        }
        self.move_to((r.line, col), select);
        self.want = Some(want);
    }

    /// Devuelve `true` si el texto cambió.
    pub fn handle_key(&mut self, k: KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let before = self.lines.clone();
        let keep_want = matches!(
            k.code,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
        );
        match k.code {
            KeyCode::Char('a') if ctrl => self.select_all(),
            KeyCode::Char('z') if ctrl => self.undo(),
            KeyCode::Char('y') if ctrl => self.redo(),
            KeyCode::Char('w') if ctrl => self.delete_word_back(),
            KeyCode::Char('u') if ctrl => {
                self.push_undo();
                if !self.delete_selection() {
                    self.lines[self.row].drain(..self.col);
                    self.col = 0;
                }
            }
            KeyCode::Char('k') if ctrl => {
                self.push_undo();
                let c = self.col;
                self.lines[self.row].truncate(c);
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                let mut b = [0; 4];
                self.insert_str(c.encode_utf8(&mut b));
            }
            KeyCode::Enter => self.newline(),
            KeyCode::Tab => self.insert_str("  "),
            KeyCode::Backspace if ctrl || alt => self.delete_word_back(),
            KeyCode::Backspace => {
                self.push_undo();
                if !self.delete_selection() {
                    if self.col > 0 {
                        self.col -= 1;
                        let c = self.col;
                        self.lines[self.row].remove(c);
                    } else if self.row > 0 {
                        let cur = self.lines.remove(self.row);
                        self.row -= 1;
                        self.col = self.lines[self.row].len();
                        self.lines[self.row].extend(cur);
                    }
                }
            }
            KeyCode::Delete => {
                self.push_undo();
                if !self.delete_selection() {
                    if self.col < self.lines[self.row].len() {
                        let c = self.col;
                        self.lines[self.row].remove(c);
                    } else if self.row + 1 < self.lines.len() {
                        let next = self.lines.remove(self.row + 1);
                        self.lines[self.row].extend(next);
                    }
                }
            }
            KeyCode::Left => {
                if let (Some((s, _)), false) = (self.selection(), shift) {
                    self.move_to(s, false);
                } else if ctrl || alt {
                    let p = self.word_left();
                    self.move_to(p, shift);
                } else if self.col > 0 {
                    self.move_to((self.row, self.col - 1), shift);
                } else if self.row > 0 {
                    self.move_to((self.row - 1, self.lines[self.row - 1].len()), shift);
                }
            }
            KeyCode::Right => {
                if let (Some((_, e)), false) = (self.selection(), shift) {
                    self.move_to(e, false);
                } else if ctrl || alt {
                    let p = self.word_right();
                    self.move_to(p, shift);
                } else if self.col < self.lines[self.row].len() {
                    self.move_to((self.row, self.col + 1), shift);
                } else if self.row + 1 < self.lines.len() {
                    self.move_to((self.row + 1, 0), shift);
                }
            }
            KeyCode::Up => self.vertical(-1, shift),
            KeyCode::Down => self.vertical(1, shift),
            KeyCode::PageUp => self.vertical(-(self.height.max(2) as isize - 1), shift),
            KeyCode::PageDown => self.vertical(self.height.max(2) as isize - 1, shift),
            KeyCode::Home if ctrl => self.move_to((0, 0), shift),
            KeyCode::End if ctrl => {
                let r = self.lines.len() - 1;
                self.move_to((r, self.lines[r].len()), shift)
            }
            KeyCode::Home => self.move_to((self.row, 0), shift),
            KeyCode::End => self.move_to((self.row, self.lines[self.row].len()), shift),
            _ => {}
        }
        if !keep_want {
            self.want = None;
        }
        self.lines != before
    }

    fn delete_word_back(&mut self) {
        self.push_undo();
        if self.delete_selection() {
            return;
        }
        if self.col == 0 {
            if self.row > 0 {
                let cur = self.lines.remove(self.row);
                self.row -= 1;
                self.col = self.lines[self.row].len();
                self.lines[self.row].extend(cur);
            }
            return;
        }
        let (_, c) = self.word_left();
        let at = self.col;
        self.lines[self.row].drain(c..at);
        self.col = c;
    }

    // ───────────── ratón ─────────────

    /// Coloca el cursor donde se hizo clic (`x`,`y` absolutos de pantalla). `true` si cayó dentro.
    pub fn click(&mut self, x: u16, y: u16, select: bool) -> bool {
        let a = self.area;
        if x < a.x || y < a.y || x >= a.x + a.width || y >= a.y + a.height {
            return false;
        }
        let rows = self.layout(self.width);
        let vr = (self.scroll + (y - a.y) as usize).min(rows.len() - 1);
        let r = rows[vr];
        let target = (x - a.x) as usize;
        let (mut w, mut col) = (0, r.start);
        while col < r.end {
            let cw = self.lines[r.line][col].width().unwrap_or(0);
            if w + cw / 2 >= target.max(w) && w + cw > target {
                break;
            }
            if w + cw > target {
                break;
            }
            w += cw;
            col += 1;
        }
        self.move_to((r.line, col), select);
        self.want = None;
        true
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let rows = self.layout(self.width).len();
        let max = rows.saturating_sub(self.height.max(1));
        self.scroll = (self.scroll as isize + delta).clamp(0, max as isize) as usize;
    }

    /// Posición 1-based en caracteres sobre el texto completo → (línea, columna).
    pub fn pos_of_offset(&self, offset1: usize) -> (usize, usize) {
        let mut left = offset1.saturating_sub(1);
        for (i, l) in self.lines.iter().enumerate() {
            if left <= l.len() {
                return (i, left);
            }
            left -= l.len() + 1;
        }
        let r = self.lines.len() - 1;
        (r, self.lines[r].len())
    }

    pub fn move_cursor_to(&mut self, pos: (usize, usize)) {
        self.move_to(pos, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEventKind;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new_with_kind(c, KeyModifiers::NONE, KeyEventKind::Press)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn escribe_y_borra() {
        let mut e = Editor::new("");
        for c in "hola".chars() {
            e.handle_key(key(KeyCode::Char(c)));
        }
        e.handle_key(key(KeyCode::Enter));
        e.handle_key(key(KeyCode::Char('x')));
        assert_eq!(e.text(), "hola\nx");
        e.handle_key(key(KeyCode::Backspace));
        e.handle_key(key(KeyCode::Backspace));
        assert_eq!(e.text(), "hola");
    }

    #[test]
    fn deshacer_y_rehacer() {
        let mut e = Editor::new("abc");
        e.handle_key(key(KeyCode::Char('d')));
        assert_eq!(e.text(), "abcd");
        e.handle_key(ctrl('z'));
        assert_eq!(e.text(), "abc");
        e.handle_key(ctrl('y'));
        assert_eq!(e.text(), "abcd");
    }

    #[test]
    fn seleccion_y_pegado_multilinea() {
        let mut e = Editor::new("uno\ndos");
        e.select_all();
        assert_eq!(e.selected_text().as_deref(), Some("uno\ndos"));
        e.insert_str("a\nb\nc");
        assert_eq!(e.text(), "a\nb\nc");
        assert_eq!((e.row, e.col), (2, 1));
    }

    #[test]
    fn ajuste_de_linea_y_cursor() {
        let mut e = Editor::new("abcdefghij");
        e.width = 4;
        let rows = e.layout(4);
        assert_eq!(rows.len(), 3);
        assert_eq!(e.cursor_visual(&rows), (2, 2));
        e.handle_key(key(KeyCode::Up));
        assert_eq!((e.row, e.col), (0, 6));
    }

    #[test]
    fn posicion_de_error() {
        let e = Editor::new("select\nfoo, bar");
        assert_eq!(e.pos_of_offset(1), (0, 0));
        assert_eq!(e.pos_of_offset(8), (1, 0));
        assert_eq!(e.pos_of_offset(13), (1, 5));
    }

    #[test]
    fn autoindentado() {
        let mut e = Editor::new("  x");
        e.handle_key(key(KeyCode::Enter));
        assert_eq!((e.row, e.col), (1, 2));
    }
}
