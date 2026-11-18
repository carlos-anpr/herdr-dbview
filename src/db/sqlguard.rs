//! Guardia léxica: una consulta de lectura, sin interpretar literales como SQL.
use crate::model::Dialect;

#[derive(Debug)]
struct Token {
    word: String,
    quoted: bool,
    end: usize,
}

fn tokens(dialect: Dialect, sql: &str, first_only: bool) -> Result<Vec<Token>, String> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if first_only && b[i] == b';' {
            out.push(Token {
                word: ";".into(),
                quoted: false,
                end: i + 1,
            });
            break;
        }
        let start = i;
        if b[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b[i..].starts_with(b"--") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b[i..].starts_with(b"/*") {
            i += 2;
            let mut depth = 1;
            while i < b.len() && depth > 0 {
                if dialect == Dialect::Postgres && b[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if b[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if depth != 0 {
                return Err("Comentario sin cerrar".into());
            }
            continue;
        }
        // Dollar quoting solo en un límite de token, nunca dentro de un identificador.
        if dialect == Dialect::Postgres && b[i] == b'$' {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 128) {
                j += 1;
            }
            if j < b.len() && b[j] == b'$' && (j == i + 1 || !b[i + 1].is_ascii_digit()) {
                let delimiter = &sql[i..=j];
                let rest = &sql[j + 1..];
                let Some(end) = rest.find(delimiter) else {
                    return Err("Cadena sin cerrar".into());
                };
                i = j + 1 + end + delimiter.len();
                out.push(Token {
                    word: String::new(),
                    quoted: true,
                    end: i,
                });
                continue;
            }
        }
        if (dialect == Dialect::Oracle || first_only)
            && matches!(b[i], b'q' | b'Q')
            && b.get(i + 1) == Some(&b'\'')
            && i + 2 < b.len()
        {
            let close = match b[i + 2] {
                b'[' => b']',
                b'(' => b')',
                b'{' => b'}',
                b'<' => b'>',
                c => c,
            };
            i += 3;
            while i + 1 < b.len() && !(b[i] == close && b[i + 1] == b'\'') {
                i += 1;
            }
            if i + 1 >= b.len() {
                return Err("Cadena sin cerrar".into());
            }
            i += 2;
            out.push(Token {
                word: String::new(),
                quoted: true,
                end: i,
            });
            continue;
        }
        let escape = dialect == Dialect::Postgres
            && matches!(b[i], b'e' | b'E')
            && b.get(i + 1) == Some(&b'\'');
        if escape {
            i += 1;
        }
        if matches!(b[i], b'\'' | b'"') {
            let quote = b[i];
            i += 1;
            let content = i;
            let mut closed = false;
            while i < b.len() {
                if escape && b[i] == b'\\' {
                    i = (i + 2).min(b.len());
                } else if b[i] == quote {
                    if b.get(i + 1) == Some(&quote) {
                        i += 2;
                    } else {
                        i += 1;
                        closed = true;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            if !closed {
                return Err("Cadena o identificador sin cerrar".into());
            }
            let word = if quote == b'"' {
                sql[content..i - 1].replace("\"\"", "\"").to_uppercase()
            } else {
                String::new()
            };
            out.push(Token {
                word,
                quoted: true,
                end: i,
            });
        } else if b[i].is_ascii_alphabetic() || b[i] == b'_' || b[i] >= 128 {
            i += 1;
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'_' | b'$') || b[i] >= 128)
            {
                i += 1;
            }
            out.push(Token {
                word: sql[start..i].to_uppercase(),
                quoted: false,
                end: i,
            });
        } else {
            i += 1;
            out.push(Token {
                word: (b[start] as char).to_string(),
                quoted: false,
                end: i,
            });
        }
    }
    Ok(out)
}

/// Fin de la primera sentencia para extraer respuestas con prosa; admite ambos dialectos.
pub(crate) fn statement_end(sql: &str) -> Option<usize> {
    tokens(Dialect::Postgres, sql, true)
        .ok()?
        .last()
        .filter(|t| !t.quoted && t.word == ";")
        .map(|t| t.end - 1)
}

pub fn check(dialect: Dialect, sql: &str) -> Result<String, String> {
    let sql = sql.trim();
    let mut ts = tokens(dialect, sql, false)?;
    if ts.is_empty() {
        return Err("Escribe una consulta".into());
    }
    let mut end = sql.len();
    if ts.last().is_some_and(|t| t.word == ";" && !t.quoted) {
        let last = ts.pop().ok_or("Escribe una consulta")?;
        end = last.end - 1;
    }
    if ts.iter().any(|t| t.word == ";" && !t.quoted) {
        return Err("Solo se permite una sentencia".into());
    }
    let first = ts.iter().find(|t| t.word != "(");
    if !first.is_some_and(|t| !t.quoted && matches!(t.word.as_str(), "SELECT" | "WITH")) {
        return Err("Solo se permiten consultas SELECT o WITH".into());
    }
    const FORBIDDEN: &[&str] = &[
        "INSERT", "UPDATE", "DELETE", "MERGE", "DROP", "TRUNCATE", "ALTER", "CREATE", "GRANT",
        "REVOKE", "COPY", "CALL", "DO", "EXECUTE", "LOCK", "VACUUM", "ANALYZE", "SET", "RESET",
        "LISTEN", "NOTIFY", "REFRESH", "COMMENT", "INTO",
    ];
    for (i, t) in ts.iter().enumerate() {
        if !t.quoted
            && t.word == "SECURITY"
            && ts
                .get(i + 1)
                .is_some_and(|n| !n.quoted && n.word == "LABEL")
        {
            return Err("Operación no permitida: SECURITY LABEL".into());
        }
        // U&"..." puede codificar nombres de funciones bloqueadas. No aceptamos
        // esta variante de identificador ni su modificador UESCAPE.
        if !t.quoted
            && t.word == "U"
            && ts.get(i + 1).is_some_and(|n| n.word == "&")
            && ts
                .get(i + 2)
                .is_some_and(|n| n.quoted && !n.word.is_empty())
        {
            return Err("Identificador con escapes Unicode no permitido".into());
        }
        if !t.quoted && FORBIDDEN.contains(&t.word.as_str()) {
            return Err(format!("Operación no permitida: {}", t.word));
        }
        if !t.quoted
            && t.word == "FOR"
            && ts
                .get(i + 1)
                .is_some_and(|n| matches!(n.word.as_str(), "SHARE" | "NO" | "KEY"))
        {
            return Err("No se permiten bloqueos".into());
        }
        let function = ts.get(i + 1).is_some_and(|n| n.word == "(");
        let dangerous = match dialect {
            Dialect::Postgres => {
                function
                    && (matches!(
                        t.word.as_str(),
                        "PG_TERMINATE_BACKEND"
                            | "PG_CANCEL_BACKEND"
                            | "PG_READ_FILE"
                            | "PG_READ_BINARY_FILE"
                            | "LO_IMPORT"
                            | "LO_EXPORT"
                            | "SET_CONFIG"
                            | "NEXTVAL"
                            | "SETVAL"
                            | "PG_RELOAD_CONF"
                    ) || t.word.starts_with("DBLINK"))
            }
            Dialect::Oracle => {
                (function || ts.get(i + 1).is_some_and(|n| n.word == "."))
                    && (t.word.starts_with("DBMS_") || t.word.starts_with("UTL_"))
            }
        };
        if dangerous {
            return Err(format!("Función no permitida: {}", t.word));
        }
    }
    Ok(sql[..end].trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_read_queries_and_opaque_literals() {
        for sql in [
            "SELECT 1;",
            "-- inicio\n SELECT updated_at, delete_flag FROM t LIMIT 2; -- fin",
            "/* a /* b */ c */ WITH x AS (SELECT 1) SELECT * FROM x",
            "(SELECT * FROM (SELECT 1) x)",
            "SELECT 'it''s; DELETE', \"DROP\" FROM t",
            "SELECT $$; DELETE$$, $tag$' UPDATE;$tag$, E'a\\\'; DROP'",
            "SELECT pg_sleep(0)",
            "SELECT 1 FETCH FIRST 2 ROWS ONLY",
        ] {
            assert!(check(Dialect::Postgres, sql).is_ok(), "{sql}");
        }
        assert!(
            check(
                Dialect::Oracle,
                "SELECT q'[it's; DELETE]', q'{DROP}' FROM dual"
            )
            .is_ok()
        );
        assert_eq!(
            check(Dialect::Postgres, " SELECT 1; -- fin").unwrap(),
            "SELECT 1"
        );
    }
    #[test]
    fn covers_forbidden_statements_and_function_calls() {
        for word in [
            "INSERT",
            "UPDATE",
            "DELETE",
            "MERGE",
            "DROP",
            "TRUNCATE",
            "ALTER",
            "CREATE",
            "GRANT",
            "REVOKE",
            "COPY",
            "CALL",
            "DO",
            "EXECUTE",
            "LOCK",
            "VACUUM",
            "ANALYZE",
            "SET",
            "RESET",
            "LISTEN",
            "NOTIFY",
            "REFRESH",
            "COMMENT",
            "SECURITY LABEL",
        ] {
            assert!(check(Dialect::Postgres, &format!("SELECT 1; {word} x")).is_err());
            assert!(check(Dialect::Postgres, &format!("WITH x AS ({word} t) SELECT 1")).is_err());
            assert!(
                check(
                    Dialect::Postgres,
                    &format!("SELECT '{word}', \"{word}\" -- {word}\n")
                )
                .is_ok()
            );
        }
        for function in [
            "pg_terminate_backend",
            "pg_cancel_backend",
            "pg_read_file",
            "pg_read_binary_file",
            "lo_import",
            "lo_export",
            "set_config",
            "nextval",
            "setval",
            "dblink",
            "dblink_exec",
            "pg_reload_conf",
        ] {
            assert!(
                check(
                    Dialect::Postgres,
                    &format!("SELECT pg_catalog.{function} /* call */ (1)")
                )
                .is_err()
            );
            assert!(check(Dialect::Postgres, &format!("SELECT \"{function}\"(1)")).is_err());
            assert!(
                check(
                    Dialect::Postgres,
                    &format!("SELECT {function}_column FROM t")
                )
                .is_ok()
            );
        }
        assert!(
            check(
                Dialect::Postgres,
                "SELECT security, updated_at, delete_flag FROM t"
            )
            .is_ok()
        );
        assert!(check(Dialect::Postgres, r#"SELECT U&"nextv\0061l"('x')"#).is_err());
        assert!(check(Dialect::Postgres, "SELECT $á$; DROP$á$").is_ok());
        assert!(check(Dialect::Oracle, "SELECT q'!it's; DROP!' FROM dual").is_ok());
        assert!(check(Dialect::Postgres, "SELECT $tag$sin cerrar").is_err());
        assert!(check(Dialect::Oracle, "SELECT q'[sin cerrar'").is_err());
    }

    #[test]
    fn rejects_writes_locks_and_side_effects() {
        for sql in [
            "INSERT INTO x VALUES (1)",
            "SELECT 1; SELECT 2",
            "SELECT 1;;",
            "WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x",
            "SELECT * INTO x FROM t",
            "SELECT * FROM t FOR UPDATE",
            "SELECT * FROM t FOR SHARE",
            "SELECT * FROM t FOR NO KEY UPDATE",
            "SELECT * FROM t FOR KEY SHARE",
            "SELECT pg_read_file('x')",
            "SELECT pg_catalog.\"nextval\"('s')",
            "SELECT dblink_exec('x','x')",
            "SELECT set_config('x','y',false)",
            "SELECT pg_cancel_backend(1)",
            "SELECT 1 /*",
            "SELECT 'x",
        ] {
            assert!(check(Dialect::Postgres, sql).is_err(), "{sql}");
        }
        for sql in [
            "SELECT DBMS_LOCK.SLEEP(1) FROM dual",
            "SELECT UTL_HTTP.REQUEST('x') FROM dual",
        ] {
            assert!(check(Dialect::Oracle, sql).is_err());
        }
        for sql in ["", " -- nada", "/* nada */"] {
            assert_eq!(
                check(Dialect::Postgres, sql).unwrap_err(),
                "Escribe una consulta"
            );
        }
    }
}

#[cfg(test)]
mod lider_tests {
    use super::*;
    use crate::model::Dialect::{Oracle, Postgres};

    #[test]
    fn rechaza_ataques_tipicos() {
        for sql in [
            "select 1; drop table x",
            "select * from t for update",
            "with x as (delete from t returning *) select * from x",
            "select pg_terminate_backend(1)",
            "select * into nueva from t",
            "SELECT/**/1;--\ndelete from t",
            "select 1 /* ; */ ; update t set a=1",
            "copy t to '/tmp/x'",
            "select set_config('a','b',false)",
            "select nextval('s')",
            "do $$ begin end $$",
            "  \n -- hola\n insert into t values (1)",
            "explain analyze delete from t",
            "select lo_import('/etc/passwd')",
            "select dblink('x','y')",
        ] {
            assert!(check(Postgres, sql).is_err(), "debía rechazar: {sql}");
        }
        for sql in [
            "select dbms_lock.sleep(5) from dual",
            "select * from t for update",
            "select utl_http.request('x') from dual",
            "delete from t",
        ] {
            assert!(
                check(Oracle, sql).is_err(),
                "debía rechazar (oracle): {sql}"
            );
        }
    }

    #[test]
    fn acepta_lecturas_legitimas() {
        for sql in [
            "select updated_at, delete_flag from t where a = 'drop table x; delete'",
            "-- c\nWITH a AS (SELECT 1) SELECT * FROM a;",
            "select \"update\" from t limit 5",
            "(select 1) union (select 2)",
            "select $$a;b$$",
            "select * from t order by 1 fetch first 5 rows only",
        ] {
            assert!(
                check(Postgres, sql).is_ok(),
                "debía aceptar: {sql} -> {:?}",
                check(Postgres, sql)
            );
        }
    }
}
