//! Memoria de consultas confirmadas por el usuario (sin configurar nada): cuando el usuario corrige el SQL que dio la IA
//! y funciona, o copia/exporta su resultado, la pregunta y el SQL quedan guardados y guían las preguntas parecidas.
//! `~/.local/share/herdr-dbview/history/<conexión>.jsonl`.

use serde_json::json;
use std::io::Write;
use std::path::PathBuf;

fn file(conn: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    let safe: String = conn
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(
        base.join("herdr-dbview/history")
            .join(format!("{safe}.jsonl")),
    )
}

fn fold(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' => 'a',
            'é' | 'è' => 'e',
            'í' => 'i',
            'ó' => 'o',
            'ú' | 'ü' => 'u',
            c => c,
        })
        .collect()
}

fn terms(q: &str) -> Vec<String> {
    let mut t: Vec<String> = fold(q)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4)
        .map(|w| w.trim_end_matches('s').to_string())
        .collect();
    t.sort();
    t.dedup();
    t
}

/// Guarda una consulta confirmada (`kind`: «corregida por el usuario» o «aprobada»). Evita duplicados exactos.
pub fn add(conn: &str, question: &str, sql: &str, kind: &str) {
    let (Some(path), true) = (
        file(conn),
        !question.trim().is_empty() && !sql.trim().is_empty(),
    ) else {
        return;
    };
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let line = json!({"q": question.trim(), "sql": sql.trim(), "kind": kind}).to_string();
    if std::fs::read_to_string(&path).is_ok_and(|t| t.lines().any(|l| l == line)) {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Hasta `n` consultas confirmadas parecidas a la pregunta (≥ 2 términos en común), listas para el prompt.
pub fn similar(conn: &str, question: &str, n: usize) -> String {
    let Some(path) = file(conn) else {
        return String::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let q = terms(question);
    let mut scored: Vec<(usize, usize, String)> = vec![];
    for (i, l) in text.lines().enumerate() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(l) else {
            continue;
        };
        let (Some(oq), Some(sql)) = (v["q"].as_str(), v["sql"].as_str()) else {
            continue;
        };
        let t = terms(oq);
        let common = q.iter().filter(|w| t.contains(w)).count();
        if common >= 2 {
            let kind = v["kind"].as_str().unwrap_or("confirmada");
            scored.push((
                common,
                i,
                format!(
                    "- Pregunta: «{}»  ({kind})\n  SQL: {}\n",
                    oq,
                    sql.chars().take(700).collect::<String>().replace('\n', " ")
                ),
            ));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    scored.into_iter().take(n).map(|x| x.2).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminos_normalizados() {
        let t = terms("Cuántas tasaciones, tasaciones ayer");
        assert!(t.contains(&"tasacione".to_string()) && t.contains(&"ayer".to_string()));
        assert_eq!(t.iter().filter(|w| w.as_str() == "tasacione").count(), 1);
    }
}
