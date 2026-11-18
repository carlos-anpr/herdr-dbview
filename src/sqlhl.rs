//! Resaltado de SQL por líneas (palabras clave, cadenas, números, comentarios, identificadores).

use crate::theme::{CODE, DIM, OK, TEXT, VIOLET, WARN};
use ratatui::style::Color;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tok {
    Plain,
    Keyword,
    Func,
    Str,
    Num,
    Comment,
    Ident,
    Punct,
}

impl Tok {
    pub fn color(self) -> Color {
        match self {
            Tok::Plain => TEXT,
            Tok::Keyword => VIOLET,
            Tok::Func => CODE,
            Tok::Str => OK,
            Tok::Num => WARN,
            Tok::Comment => DIM,
            Tok::Ident => Color::Rgb(0xe8, 0xc4, 0x7a),
            Tok::Punct => DIM,
        }
    }
}

const KEYWORDS: &[&str] = &[
    "select",
    "from",
    "where",
    "and",
    "or",
    "not",
    "in",
    "is",
    "null",
    "as",
    "on",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "outer",
    "cross",
    "group",
    "by",
    "order",
    "having",
    "limit",
    "offset",
    "fetch",
    "first",
    "next",
    "rows",
    "row",
    "only",
    "union",
    "all",
    "intersect",
    "except",
    "minus",
    "distinct",
    "with",
    "recursive",
    "case",
    "when",
    "then",
    "else",
    "end",
    "between",
    "like",
    "ilike",
    "exists",
    "asc",
    "desc",
    "nulls",
    "last",
    "over",
    "partition",
    "interval",
    "using",
    "true",
    "false",
    "any",
    "some",
    "lateral",
    "filter",
    "within",
    "cast",
    "extract",
    "values",
    "returning",
    "top",
    "connect",
    "start",
    "prior",
    "level",
    "rownum",
    "similar",
    "to",
    "escape",
];

const FUNCS: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "coalesce",
    "nullif",
    "now",
    "current_date",
    "current_timestamp",
    "sysdate",
    "systimestamp",
    "date_trunc",
    "to_char",
    "to_date",
    "to_timestamp",
    "to_number",
    "trunc",
    "round",
    "upper",
    "lower",
    "length",
    "substr",
    "substring",
    "trim",
    "ltrim",
    "rtrim",
    "replace",
    "concat",
    "nvl",
    "nvl2",
    "decode",
    "greatest",
    "least",
    "row_number",
    "rank",
    "dense_rank",
    "lag",
    "lead",
    "array_agg",
    "string_agg",
    "listagg",
    "regexp_replace",
    "regexp_like",
    "age",
    "add_months",
    "months_between",
    "ceil",
    "floor",
    "abs",
    "mod",
    "position",
];

/// Clasifica cada carácter del texto; devuelve un `Tok` por carácter y línea.
pub fn classify(lines: &[Vec<char>]) -> Vec<Vec<Tok>> {
    let mut out: Vec<Vec<Tok>> = lines.iter().map(|l| vec![Tok::Plain; l.len()]).collect();
    let flat: Vec<(usize, usize, char)> = lines
        .iter()
        .enumerate()
        .flat_map(|(r, l)| l.iter().enumerate().map(move |(c, ch)| (r, c, *ch)))
        .chain(std::iter::empty())
        .collect();
    // Posición plana ↔ (línea, col); los saltos de línea cuentan como espacio en blanco implícito.
    let n = flat.len();
    let at = |i: usize| flat.get(i).map(|x| x.2);
    let same_line = |a: usize, b: usize| flat[a].0 == flat[b].0;
    let set = |out: &mut Vec<Vec<Tok>>, a: usize, b: usize, t: Tok| {
        for k in a..b {
            let (r, c, _) = flat[k];
            out[r][c] = t;
        }
    };
    let mut i = 0;
    while i < n {
        let ch = flat[i].2;
        let nxt = at(i + 1);
        if ch == '-' && nxt == Some('-') {
            let mut j = i;
            while j < n && same_line(i, j) {
                j += 1;
            }
            set(&mut out, i, j, Tok::Comment);
            i = j;
        } else if ch == '/' && nxt == Some('*') {
            let mut j = i + 2;
            while j < n && !(flat[j].2 == '*' && at(j + 1) == Some('/')) {
                j += 1;
            }
            let j = (j + 2).min(n);
            set(&mut out, i, j, Tok::Comment);
            i = j;
        } else if ch == '\'' {
            let mut j = i + 1;
            while j < n {
                if flat[j].2 == '\'' {
                    if at(j + 1) == Some('\'') {
                        j += 2;
                        continue;
                    }
                    j += 1;
                    break;
                }
                j += 1;
            }
            set(&mut out, i, j.min(n), Tok::Str);
            i = j.min(n);
        } else if ch == '"' {
            let mut j = i + 1;
            while j < n && flat[j].2 != '"' {
                j += 1;
            }
            let j = (j + 1).min(n);
            set(&mut out, i, j, Tok::Ident);
            i = j;
        } else if ch.is_ascii_digit() {
            let mut j = i;
            while j < n && (flat[j].2.is_ascii_digit() || flat[j].2 == '.') {
                j += 1;
            }
            set(&mut out, i, j, Tok::Num);
            i = j;
        } else if ch.is_alphabetic() || ch == '_' {
            let mut j = i;
            while j < n && (flat[j].2.is_alphanumeric() || matches!(flat[j].2, '_' | '$' | '#')) {
                j += 1;
            }
            let word: String = flat[i..j]
                .iter()
                .map(|x| x.2.to_ascii_lowercase())
                .collect();
            let mut k = j;
            while k < n && flat[k].2 == ' ' {
                k += 1;
            }
            let t = if KEYWORDS.contains(&word.as_str()) {
                Tok::Keyword
            } else if FUNCS.contains(&word.as_str())
                || (at(k) == Some('(') && k < n && same_line(i, k))
            {
                Tok::Func
            } else {
                Tok::Plain
            };
            set(&mut out, i, j, t);
            i = j;
        } else if "(),;.=<>+-*/|:%".contains(ch) {
            set(&mut out, i, i + 1, Tok::Punct);
            i += 1;
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<Vec<char>> {
        s.split('\n').map(|l| l.chars().collect()).collect()
    }

    #[test]
    fn clasifica() {
        let c = classify(&lines("SELECT a, 'x''y' -- hola\nFROM \"T\" WHERE n = 12"));
        assert_eq!(c[0][0], Tok::Keyword);
        assert_eq!(c[0][7], Tok::Plain);
        assert_eq!(c[0][10], Tok::Str);
        assert_eq!(c[0][15], Tok::Str);
        assert_eq!(c[0][19], Tok::Comment);
        assert_eq!(c[1][5], Tok::Ident);
        assert_eq!(c[1][20], Tok::Num);
    }

    #[test]
    fn comentario_de_bloque_multilinea() {
        let c = classify(&lines("a /* x\ny */ b"));
        assert_eq!(c[0][3], Tok::Comment);
        assert_eq!(c[1][0], Tok::Comment);
        assert_eq!(c[1][5], Tok::Plain);
    }
}
