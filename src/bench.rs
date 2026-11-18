//! Herramienta de desarrollo: `herdr-dbview --ask "pregunta" --conn ID [--mode full|structure|none] [--notes on|off]`
//! ejecuta el agente explorador sin interfaz y escribe el resultado (para comparar configuraciones).

use crate::catalog::{self, Catalog};
use crate::config::Config;
use crate::{agent, db, explorer, notes};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Instant;

pub fn run(
    cfg: &Config,
    question: &str,
    conn: &str,
    mode: &str,
    with_notes: bool,
) -> anyhow::Result<()> {
    let spec = cfg
        .connections
        .iter()
        .find(|c| c.info.id == conn)
        .ok_or_else(|| anyhow::anyhow!("conexión «{conn}» desconocida"))?;
    let mut d = db::open(spec);
    let engine = d.check().map_err(|e| anyhow::anyhow!(e.message))?;
    let schema = d
        .schema()
        .map_err(|e| anyhow::anyhow!(e.message))?
        .restricted(&spec.schemas);
    let cached = catalog::load(conn).unwrap_or_default();
    let cat: Catalog = match mode {
        "full" => cached,
        "structure" => Catalog {
            relations: catalog::relations(&schema),
            ..Catalog::default()
        },
        _ => Catalog::default(),
    };
    let mut notes_text = if with_notes {
        notes::load(conn)
    } else {
        String::new()
    };
    if std::env::var("DBVIEW_KNOW")
        .map(|v| v != "off")
        .unwrap_or(true)
    {
        let k = notes::knowledge(conn, question, 5_000);
        if !k.is_empty() {
            notes_text.push_str("\n\nDocumentación de referencia sobre esta base (fragmentos relevantes; fiable salvo que contradiga a los datos):\n");
            notes_text.push_str(&k);
        }
    }
    let candidates: Vec<String> = agent::rank_hybrid(&schema, question, &[], &cat, 25)
        .into_iter()
        .map(|i| schema.tables[i].qualified())
        .collect();
    if std::env::var_os("DBVIEW_TRACE").is_some() {
        eprintln!(
            "[conocimiento+notas {} caracteres]\n{}",
            notes_text.len(),
            notes_text.chars().take(2500).collect::<String>()
        );
    }
    let relations = explorer::relation_map(&cat);
    let cancel = Arc::new(AtomicBool::new(false));
    let today = std::process::Command::new("date")
        .arg("+%F")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let ctx = explorer::Ctx {
        cfg: &cfg.ai,
        dialect: spec.info.dialect,
        engine: &engine,
        today: today.trim(),
        question,
        schema: &schema,
        summaries: &cat.tables,
        overview: &cat.overview,
        relations: &relations,
        candidates: &candidates,
        notes: &notes_text,
        pinned: &[],
        current_sql: None,
        cancel: &cancel,
    };
    let (mut steps, t0) = (0usize, Instant::now());
    let mut on_event = |e: explorer::Event| {
        if let explorer::Event::Step(s) = e {
            steps += 1;
            eprintln!("  · {s}");
        }
    };
    let strong = if std::env::var("DBVIEW_SMART")
        .map(|v| v == "off")
        .unwrap_or(false)
    {
        None
    } else {
        cfg.ai.strong()
    };
    let r = explorer::explore_smart(&ctx, strong.as_ref(), &mut *d, &mut on_event);
    let secs = t0.elapsed().as_secs();
    if let Ok(o) = &r {
        if !o.sql.trim().is_empty() {
            let t = Instant::now();
            match d.run(o.sql.trim().trim_end_matches(';'), 6, &cancel) {
                Ok(rs) => {
                    println!(
                        "EXEC ok {} filas{} en {} ms",
                        rs.rows.len(),
                        if rs.truncated { "+" } else { "" },
                        t.elapsed().as_millis()
                    );
                    for row in rs.rows.iter().take(6) {
                        println!(
                            "ROW {}",
                            row.iter()
                                .map(|v| v.clone().unwrap_or("NULL".into()))
                                .map(|v| v.chars().take(40).collect::<String>())
                                .collect::<Vec<_>>()
                                .join(" | ")
                        );
                    }
                }
                Err(e) => println!("EXEC ERROR {}", e.message),
            }
        }
    }
    match r {
        Ok(o) => println!(
            "RESULT mode={mode} notes={with_notes} secs={secs} steps={steps} tables=[{}] why={} sql={}",
            o.tables.join(","),
            o.explanation.replace('\n', " "),
            o.sql
                .lines()
                .filter(|l| !l.trim_start().starts_with("--"))
                .collect::<Vec<_>>()
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Err(e) => {
            println!("RESULT mode={mode} notes={with_notes} secs={secs} steps={steps} ERROR={e}")
        }
    }
    Ok(())
}
