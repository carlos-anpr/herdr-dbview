//! PostgreSQL síncrono, conexión perezosa y transacciones de solo lectura.
use super::{Database, sqlguard};
use crate::model::*;
use postgres::{Client, Config, NoTls, SimpleQueryMessage, error::ErrorPosition};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

pub struct Pg {
    spec: ConnSpec,
    client: Option<Client>,
}
impl Pg {
    pub fn new(spec: ConnSpec) -> Self {
        Self { spec, client: None }
    }

    fn connect(&mut self) -> Result<(), DbError> {
        let mut client = Config::new()
            .host(&self.spec.host)
            .port(self.spec.port)
            .user(&self.spec.user)
            .password(&self.spec.password)
            .dbname(&self.spec.database)
            .connect_timeout(Duration::from_secs(10))
            .application_name("herdr-dbview")
            .connect(NoTls)
            .map_err(connection_error)?;
        let timeout = self.spec.timeout_secs.saturating_mul(1000);
        client.batch_execute(&format!(
            "SET default_transaction_read_only = on; SET statement_timeout = {timeout}; SET idle_in_transaction_session_timeout = 60000"
        )).map_err(connection_error)?;
        self.client = Some(client);
        Ok(())
    }

    fn operation<T>(
        &mut self,
        mut op: impl FnMut(&mut Client) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        for attempt in 0..2 {
            if self.client.as_ref().is_none_or(Client::is_closed) {
                self.connect()?;
            }
            let client = self
                .client
                .as_mut()
                .ok_or_else(|| DbError::conn("No hay conexión"))?;
            let result = op(client);
            if result.is_err() && client.is_closed() && attempt == 0 {
                self.client = None;
                continue;
            }
            return result;
        }
        Err(DbError::conn("Se perdió la conexión con PostgreSQL"))
    }
}

fn connection_error(error: postgres::Error) -> DbError {
    let message = match error.as_db_error().map(|e| e.code().code()) {
        Some("28P01" | "28000") => {
            "No se pudo autenticar con PostgreSQL; revisa el usuario y la clave"
        }
        Some("3D000") => "La base de datos indicada no existe",
        _ => "No se pudo conectar con PostgreSQL; revisa el servidor, el puerto y la red",
    };
    DbError::conn(message)
}

fn query_error(
    error: postgres::Error,
    sql: &str,
    prefix_chars: usize,
    cancelled: bool,
    timeout: u64,
) -> DbError {
    let Some(db) = error.as_db_error() else {
        return connection_error(error);
    };
    if db.code().code() == "57014" {
        return DbError::new(if cancelled {
            "Consulta cancelada".into()
        } else {
            format!("La consulta superó el límite de {timeout} s")
        });
    }
    // El protocolo PostgreSQL ya entrega P en caracteres (no bytes), 1-based.
    // No convertirlo otra vez: desplazaría los errores posteriores a un acento.
    let position = match db.position() {
        Some(ErrorPosition::Original(p)) => (*p as usize)
            .checked_sub(prefix_chars)
            .filter(|p| *p > 0 && *p <= sql.chars().count() + 1),
        _ => None,
    };
    DbError {
        message: db.message().into(),
        detail: db.detail().map(str::to_owned),
        hint: db.hint().map(str::to_owned),
        code: Some(db.code().code().into()),
        position,
        connection: false,
    }
}

const CATALOG: &str = "
SELECT n.nspname, c.relname, c.relkind::text, obj_description(c.oid, 'pg_class'),
       a.attname, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull,
       EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = c.oid AND i.indisprimary AND a.attnum = ANY(i.indkey)),
       col_description(c.oid, a.attnum),
       CASE WHEN c.relkind IN ('v','m') OR c.reltuples < 0 THEN NULL ELSE c.reltuples::bigint END,
       EXISTS (SELECT 1 FROM pg_constraint u WHERE u.conrelid = c.oid AND u.contype = 'u' AND u.conkey = ARRAY[a.attnum])
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
LEFT JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
WHERE c.relkind IN ('r','p','v','m','f')
  AND n.nspname NOT IN ('pg_catalog','information_schema')
  AND n.nspname !~ '^pg_(toast|temp_)'
ORDER BY n.nspname, c.relname, a.attnum";

// Una fila por restricción: preserva el orden de cada pareja de columnas compuestas.
const FOREIGN_KEYS: &str = "
SELECT n.nspname, c.relname, fk.oid,
       array_agg(a.attname::text ORDER BY keys.pos), rn.nspname, rc.relname,
       array_agg(ra.attname::text ORDER BY keys.pos)
FROM pg_constraint fk
JOIN pg_class c ON c.oid = fk.conrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_class rc ON rc.oid = fk.confrelid
JOIN pg_namespace rn ON rn.oid = rc.relnamespace
CROSS JOIN LATERAL unnest(fk.conkey, fk.confkey) WITH ORDINALITY keys(col, refcol, pos)
JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = keys.col
JOIN pg_attribute ra ON ra.attrelid = rc.oid AND ra.attnum = keys.refcol
WHERE fk.contype = 'f' AND c.relkind NOT IN ('v','m')
  AND n.nspname NOT IN ('pg_catalog','information_schema')
  AND n.nspname !~ '^pg_(toast|temp_)'
GROUP BY n.nspname, c.relname, fk.oid, rn.nspname, rc.relname
ORDER BY n.nspname, c.relname, fk.oid";

fn cell(value: &str) -> String {
    let mut chars = value.chars();
    let mut result: String = chars.by_ref().take(4096).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

fn execute(
    client: &mut Client,
    sql: &str,
    limit: usize,
    cancel: &Arc<AtomicBool>,
    timeout: u64,
) -> Result<ResultSet, DbError> {
    if cancel.load(Ordering::SeqCst) {
        return Err(DbError::new("Consulta cancelada"));
    }
    let token = client.cancel_token();
    let (done, watch) = mpsc::channel();
    let started = Instant::now();
    // El vigilante termina y se une antes del rollback o de reutilizar la conexión.
    let result = std::thread::scope(|scope| {
        let watcher = scope.spawn(move || {
            loop {
                match watch.recv_timeout(Duration::from_millis(50)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if cancel.load(Ordering::SeqCst) {
                            let _ = token.cancel_query(NoTls);
                        }
                    }
                }
            }
        });
        let result = (|| {
            let statement = client
                .prepare(sql)
                .map_err(|e| query_error(e, sql, 0, cancel.load(Ordering::SeqCst), timeout))?;
            let columns = statement
                .columns()
                .iter()
                .map(|c| ColumnMeta {
                    name: c.name().into(),
                    type_name: c.type_().name().into(),
                })
                .collect();
            client
                .batch_execute("BEGIN READ ONLY")
                .map_err(connection_error)?;
            let prefix = "DECLARE dbv NO SCROLL CURSOR FOR ";
            // La nueva línea impide que un -- al final se coma delimitadores.
            client
                .batch_execute(&format!("{prefix}{sql}\n"))
                .map_err(|e| {
                    query_error(
                        e,
                        sql,
                        prefix.chars().count(),
                        cancel.load(Ordering::SeqCst),
                        timeout,
                    )
                })?;
            let messages = client
                .simple_query(&format!("FETCH {} FROM dbv", limit.saturating_add(1)))
                .map_err(|e| {
                    query_error(e, sql, usize::MAX, cancel.load(Ordering::SeqCst), timeout)
                })?;
            if cancel.load(Ordering::SeqCst) {
                return Err(DbError::new("Consulta cancelada"));
            }
            let mut rows = Vec::new();
            let mut truncated = false;
            for message in messages {
                if let SimpleQueryMessage::Row(row) = message {
                    if rows.len() == limit {
                        truncated = true;
                        break;
                    }
                    rows.push((0..row.len()).map(|i| row.get(i).map(cell)).collect());
                }
            }
            Ok(ResultSet {
                columns,
                rows,
                truncated,
                elapsed_ms: started.elapsed().as_millis() as u64,
            })
        })();
        let _ = done.send(());
        let _ = watcher.join();
        result
    });
    // También se hace si falla PREPARE o DECLARE; ROLLBACK sin transacción es inocuo.
    let rollback = client.batch_execute("ROLLBACK");
    match result {
        Err(e) => Err(e),
        Ok(value) => {
            rollback.map_err(connection_error)?;
            Ok(value)
        }
    }
}

impl Database for Pg {
    fn dialect(&self) -> Dialect {
        Dialect::Postgres
    }
    fn check(&mut self) -> Result<String, DbError> {
        self.operation(|c| {
            let row = c
                .query_one("SELECT version()", &[])
                .map_err(connection_error)?;
            let version: String = row.get(0);
            Ok(version.split(" on ").next().unwrap_or(&version).to_string())
        })
    }
    fn schema(&mut self) -> Result<Schema, DbError> {
        self.operation(|c| {
            let rows = c.query(CATALOG, &[]).map_err(connection_error)?;
            let mut schema = Schema::default();
            for row in rows {
                let ns: String = row.get(0);
                let name: String = row.get(1);
                if schema
                    .tables
                    .last()
                    .is_none_or(|t| t.schema != ns || t.name != name)
                {
                    let kind: &str = row.get(2);
                    schema.tables.push(TableInfo {
                        schema: ns,
                        name,
                        kind: if matches!(kind, "v" | "m") {
                            TableKind::View
                        } else {
                            TableKind::Table
                        },
                        columns: Vec::new(),
                        comment: row.get(3),
                        rows: row.get(9),
                        fks: Vec::new(),
                    });
                }
                if let Some(name) = row.get::<_, Option<String>>(4)
                    && let Some(table) = schema.tables.last_mut()
                {
                    table.columns.push(ColInfo {
                        name,
                        type_name: row.get(5),
                        nullable: row.get(6),
                        primary_key: row.get(7),
                        unique: row.get(10),
                        comment: row.get(8),
                    });
                }
            }
            let lookup: std::collections::HashMap<_, _> = schema
                .tables
                .iter()
                .enumerate()
                .map(|(i, t)| ((t.schema.clone(), t.name.clone()), i))
                .collect();
            for row in c.query(FOREIGN_KEYS, &[]).map_err(connection_error)? {
                let key = (row.get::<_, String>(0), row.get::<_, String>(1));
                if let Some(&i) = lookup.get(&key) {
                    schema.tables[i].fks.push(ForeignKey {
                        columns: row.get(3),
                        ref_schema: row.get(4),
                        ref_table: row.get(5),
                        ref_columns: row.get(6),
                    });
                }
            }
            Ok(schema)
        })
    }
    fn run(
        &mut self,
        sql: &str,
        limit: usize,
        cancel: &Arc<AtomicBool>,
    ) -> Result<ResultSet, DbError> {
        let sql = sqlguard::check(Dialect::Postgres, sql).map_err(DbError::new)?;
        let timeout = self.spec.timeout_secs;
        self.operation(|client| execute(client, &sql, limit, cancel, timeout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn database() -> Pg {
        let host = std::env::var("DBVIEW_TEST_HOST").expect("DBVIEW_TEST_HOST");
        Pg::new(ConnSpec {
            info: ConnInfo {
                id: "test".into(),
                label: "test".into(),
                dialect: Dialect::Postgres,
                env: EnvKind::Dev,
                target: String::new(),
            },
            host,
            port: std::env::var("DBVIEW_TEST_PORT")
                .unwrap_or_else(|_| "5432".into())
                .parse()
                .unwrap(),
            database: std::env::var("DBVIEW_TEST_DATABASE").expect("DBVIEW_TEST_DATABASE"),
            user: std::env::var("DBVIEW_TEST_USER").expect("DBVIEW_TEST_USER"),
            password: std::env::var("DBVIEW_TEST_PASSWORD").expect("DBVIEW_TEST_PASSWORD"),
            secret: Secret::Plain,
            oracle_service: false,
            python: String::new(),
            bridge: String::new(),
            timeout_secs: 10,
            schemas: vec![],
        })
    }
    #[test]
    fn clips_unicode_cells() {
        assert_eq!(cell("áé"), "áé");
        assert_eq!(cell(&"á".repeat(4097)), format!("{}…", "á".repeat(4096)));
    }
    #[test]
    #[ignore = "requiere DBVIEW_TEST_* y acceso a PostgreSQL de desarrollo"]
    fn real_queries_errors_and_catalog() {
        let mut db = database();
        let cancel = Arc::new(AtomicBool::new(false));
        assert!(db.client.is_none());
        assert!(db.check().unwrap().starts_with("PostgreSQL"));
        let schema = db.schema().unwrap();
        assert!(!schema.tables.is_empty());
        for table in &schema.tables {
            assert!(table.rows.is_none_or(|n| n >= 0));
            if table.kind == TableKind::View {
                assert!(table.rows.is_none() && table.fks.is_empty());
            }
            for fk in &table.fks {
                assert_eq!(fk.columns.len(), fk.ref_columns.len());
                assert!(!fk.columns.is_empty());
                assert!(
                    fk.columns
                        .iter()
                        .all(|name| table.columns.iter().any(|c| c.name == *name))
                );
            }
        }
        for (sql, code) in [
            ("SELECT 'áé', columna_inexistente_dbview", "42703"),
            ("SELECT * FROM tabla_inexistente_dbview", "42P01"),
            ("SELECT 1 WHERE 1 = 'abc'", "22P02"),
            ("SELECT 1/0", "22012"),
        ] {
            let error = db.run(sql, 10, &cancel).unwrap_err();
            assert_eq!(error.code.as_deref(), Some(code), "{error:?}");
            if code == "42703" {
                assert_eq!(
                    error.position,
                    Some(sql[..sql.find("columna").unwrap()].chars().count() + 1)
                );
            }
        }
        let result = db.run("SELECT 1.25::numeric, now(), '{\"a\":1}'::json, ARRAY[1,2], '\\x1234'::bytea, '00000000-0000-0000-0000-000000000001'::uuid, NULL", 10, &cancel).unwrap();
        assert_eq!(
            result
                .columns
                .iter()
                .map(|c| c.type_name.as_str())
                .collect::<Vec<_>>(),
            [
                "numeric",
                "timestamptz",
                "json",
                "_int4",
                "bytea",
                "uuid",
                "text"
            ]
        );
        assert_eq!(result.rows[0][6], None);
        let rows = db.run("SELECT generate_series(1,4)", 2, &cancel).unwrap();
        assert_eq!(rows.rows.len(), 2);
        assert!(rows.truncated);
        assert!(!db.run("SELECT 1 -- fin", 1, &cancel).unwrap().truncated);
        assert!(db.run("SELECT 1", 0, &cancel).unwrap().truncated);
        assert_eq!(
            db.run("SELECT repeat('á', 5000)", 1, &cancel).unwrap().rows[0][0]
                .as_ref()
                .unwrap()
                .chars()
                .count(),
            4097
        );
        db.client = None;
        assert!(db.check().is_ok());
    }
    #[test]
    #[ignore = "requiere DBVIEW_TEST_* y acceso a PostgreSQL de desarrollo"]
    fn real_cancel() {
        let mut db = database();
        db.check().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = cancel.clone();
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            signal.store(true, Ordering::SeqCst);
        });
        let start = Instant::now();
        assert_eq!(
            db.run("SELECT pg_sleep(5)", 1, &cancel)
                .unwrap_err()
                .message,
            "Consulta cancelada"
        );
        setter.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(4));
        cancel.store(false, Ordering::SeqCst);
        assert!(db.run("SELECT 1", 1, &cancel).is_ok());
    }
    #[test]
    #[ignore = "requiere DBVIEW_TEST_* y acceso a PostgreSQL de desarrollo"]
    fn real_timeout() {
        let mut db = database();
        let cancel = Arc::new(AtomicBool::new(false));
        db.spec.timeout_secs = 1;
        db.client = None;
        assert_eq!(
            db.run("SELECT pg_sleep(5)", 1, &cancel)
                .unwrap_err()
                .message,
            "La consulta superó el límite de 1 s"
        );
    }
}
