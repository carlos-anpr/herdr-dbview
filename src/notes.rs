//! Notas del usuario por conexión: reglas de negocio que la IA debe respetar
//! (`~/.config/herdr-dbview/notes/<id>.md`).

use std::path::PathBuf;

fn dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("herdr-dbview/notes"))
}

fn file(id: &str) -> Option<PathBuf> {
    let safe: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(dir()?.join(format!("{safe}.md")))
}

pub fn load(id: &str) -> String {
    file(id)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default()
}

pub fn save(id: &str, text: &str) -> std::io::Result<()> {
    let Some(p) = file(id) else {
        return Err(std::io::Error::other("sin directorio de configuración"));
    };
    if text.trim().is_empty() {
        let _ = std::fs::remove_file(&p);
        return Ok(());
    }
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension("md.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, p)
}

// ───────────── conocimiento: documentos de referencia por conexión ─────────────

fn knowledge_dirs(id: &str) -> Vec<PathBuf> {
    let Some(d) = dir().and_then(|d| d.parent().map(|p| p.join("knowledge"))) else {
        return vec![];
    };
    let safe: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    vec![d.join(safe), d.join("_all")]
}

fn fold(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'ä' => 'a',
            'é' | 'è' | 'ë' => 'e',
            'í' | 'ï' => 'i',
            'ó' | 'ö' => 'o',
            'ú' | 'ü' => 'u',
            c => c,
        })
        .collect()
}

/// Trozos (secciones o bloques) de un texto, de unos 1 500 caracteres como máximo.
fn chunks(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let mut cur = String::new();
    for line in text.lines() {
        let heading = line.starts_with('#');
        if (heading && !cur.is_empty()) || cur.len() + line.len() > 1_500 {
            out.push(std::mem::take(&mut cur));
        }
        cur.push_str(line);
        cur.push('\n');
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// Fragmentos de los documentos de `~/.config/herdr-dbview/knowledge/<conexión>/` (y `_all/`; admite enlaces simbólicos)
/// más relevantes para la pregunta. Devuelve texto con el nombre del documento de origen, ≤ `max` caracteres.
pub fn knowledge(id: &str, question: &str, max: usize) -> String {
    let terms: Vec<String> = fold(question)
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.chars().count() >= 4)
        .map(str::to_string)
        .collect();
    if terms.is_empty() {
        return String::new();
    }
    let mut scored: Vec<(usize, String, String)> = vec![];
    for d in knowledge_dirs(id) {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let path = e.path();
            if !matches!(
                path.extension().and_then(|x| x.to_str()),
                Some("md" | "txt")
            ) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if text.len() > 2_000_000 {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            for c in chunks(&text) {
                let f = fold(&c);
                let score: usize = terms
                    .iter()
                    .map(|t| f.matches(t.as_str()).count().min(4))
                    .sum();
                // Las secciones que nombran varios términos distintos valen más que una palabra repetida.
                let distinct = terms.iter().filter(|t| f.contains(t.as_str())).count();
                if distinct >= 1 {
                    scored.push((score + distinct * 3, name.clone(), c));
                }
            }
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out = String::new();
    for (_, name, c) in scored.into_iter().take(8) {
        if out.len() + c.len() > max {
            break;
        }
        out.push_str(&format!("[{name}]\n{c}\n"));
    }
    out
}
