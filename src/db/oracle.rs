//! Oracle por medio del puente Python con `oracledb` (modo thin).

use super::{Database, sqlguard};
use crate::model::*;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

pub struct OracleDb {
    spec: ConnSpec,
}

impl OracleDb {
    pub fn new(spec: ConnSpec) -> Self {
        OracleDb { spec }
    }

    fn payload(&self) -> Value {
        json!({
            "host": self.spec.host,
            "port": self.spec.port,
            "database": self.spec.database,
            "service": self.spec.oracle_service,
            "user": self.spec.user,
            "password": self.spec.password,
            "timeout_secs": self.spec.timeout_secs,
        })
    }

    /// Lanza el puente para UNA operación, le pasa el JSON por stdin y lee su única línea JSON.
    /// La clave nunca va en `argv` ni en el entorno.
    fn exchange(
        &self,
        operation: &str,
        payload: &Value,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<Value, DbError> {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(DbError::new("Consulta cancelada"));
        }
        if self.spec.python.trim().is_empty() {
            return Err(DbError::conn(
                "No hay intérprete de Python configurado para Oracle (ajusta `oracle_python`)",
            ));
        }
        if self.spec.bridge.trim().is_empty() {
            return Err(DbError::conn(
                "No hay ruta configurada para el puente de Oracle (`bridge`)",
            ));
        }
        if !Path::new(&self.spec.bridge).is_file() {
            return Err(DbError::conn(format!(
                "No se encuentra el puente de Oracle en «{}»",
                self.spec.bridge
            )));
        }
        let mut child = Command::new(&self.spec.python)
            .arg(&self.spec.bridge)
            .arg(operation)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => DbError::conn(format!(
                    "No se encontró el intérprete de Python para Oracle («{}»); configura `oracle_python` o instala el entorno con oracledb",
                    self.spec.python
                )),
                _ => DbError::conn(format!("No se pudo iniciar el puente de Oracle: {error}")),
            })?;

        if let Some(mut input) = child.stdin.take() {
            let data = serde_json::to_vec(payload)
                .map_err(|_| DbError::conn("La petición al puente Oracle no es válida"))?;
            if let Err(error) = input.write_all(&data) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(DbError::conn(format!(
                    "No se pudo enviar la consulta al puente Oracle: {error}"
                )));
            }
        }

        let out_reader = child.stdout.take().map(|mut out| {
            std::thread::spawn(move || {
                let mut buffer = Vec::new();
                let _ = out.read_to_end(&mut buffer);
                buffer
            })
        });
        let err_reader = child.stderr.take().map(|mut err| {
            std::thread::spawn(move || {
                let mut buffer = Vec::new();
                let _ = err.read_to_end(&mut buffer);
                buffer
            })
        });

        let deadline = Instant::now()
            + Duration::from_secs(self.spec.timeout_secs.clamp(1, 3600).saturating_add(20));
        let status = loop {
            if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(DbError::new("Consulta cancelada"));
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(DbError::conn(format!(
                        "No se pudo esperar al puente Oracle: {error}"
                    )));
                }
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(DbError::conn(format!(
                    "El puente Oracle no respondió en {} s",
                    self.spec.timeout_secs.saturating_add(20)
                )));
            }
            std::thread::sleep(Duration::from_millis(40));
        };

        let stdout = out_reader
            .map(|reader| reader.join().unwrap_or_default())
            .unwrap_or_default();
        let stderr = err_reader
            .map(|reader| reader.join().unwrap_or_default())
            .unwrap_or_default();
        let stdout = String::from_utf8_lossy(&stdout);
        let stderr = String::from_utf8_lossy(&stderr);
        let Some(line) = stdout.lines().map(str::trim).find(|line| !line.is_empty()) else {
            return Err(bridge_failure(&status.to_string(), &stderr));
        };
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => return Err(bridge_failure(&status.to_string(), &stderr)),
        };
        if value.get("ok").and_then(Value::as_bool) == Some(false) {
            return Err(error_from_json(value.get("error").unwrap_or(&Value::Null)));
        }
        Ok(value)
    }
}

/// Error del puente sin JSON utilizable: se queda con la última línea de stderr, que es la
/// más informativa y nunca contiene la clave.
fn bridge_failure(status: &str, stderr: &str) -> DbError {
    let last = stderr
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or("");
    let mut message = if last.is_empty() {
        format!("El puente Oracle no devolvió una respuesta válida (estado {status})")
    } else {
        let short: String = last.chars().take(300).collect();
        format!("El puente Oracle falló: {short}")
    };
    if message.chars().count() > 400 {
        message = message.chars().take(400).collect();
    }
    DbError::conn(message)
}

fn error_from_json(error: &Value) -> DbError {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .unwrap_or("Error de Oracle")
        .to_string();
    DbError {
        message,
        code: error
            .get("code")
            .and_then(Value::as_str)
            .filter(|code| !code.trim().is_empty())
            .map(str::to_string),
        position: error
            .get("position")
            .and_then(Value::as_u64)
            .filter(|position| *position > 0)
            .map(|position| position as usize),
        connection: error
            .get("connection")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        ..Default::default()
    }
}

fn table_from_json(value: &Value) -> Option<TableInfo> {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())?
        .to_string();
    let schema = value
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let kind = match value
        .get("kind")
        .and_then(Value::as_str)
        .map(str::to_ascii_uppercase)
        .as_deref()
    {
        Some("VIEW") => TableKind::View,
        _ => TableKind::Table,
    };
    let columns = value
        .get("columns")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(column_from_json).collect())
        .unwrap_or_default();
    let comment = value
        .get("comment")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|comment| !comment.is_empty())
        .map(str::to_string);
    Some(TableInfo {
        schema,
        name,
        kind,
        columns,
        comment,
        rows: (kind == TableKind::Table)
            .then(|| {
                value
                    .get("rows")
                    .and_then(Value::as_i64)
                    .filter(|n| *n >= 0)
            })
            .flatten(),
        fks: if kind == TableKind::View {
            Vec::new()
        } else {
            value
                .get("fks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|v| serde_json::from_value::<ForeignKey>(v.clone()).ok())
                .filter(|fk| {
                    !fk.ref_table.is_empty()
                        && !fk.columns.is_empty()
                        && fk.columns.len() == fk.ref_columns.len()
                })
                .collect()
        },
    })
}

fn column_from_json(value: &Value) -> Option<ColInfo> {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())?
        .to_string();
    Some(ColInfo {
        name,
        type_name: value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        nullable: value
            .get("nullable")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        primary_key: value.get("pk").and_then(Value::as_bool).unwrap_or(false),
        unique: value
            .get("unique")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        comment: value
            .get("comment")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

impl Database for OracleDb {
    fn dialect(&self) -> Dialect {
        Dialect::Oracle
    }

    fn check(&mut self) -> Result<String, DbError> {
        let value = self.exchange("check", &self.payload(), None)?;
        value
            .get("version")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|version| !version.is_empty())
            .map(str::to_string)
            .ok_or_else(|| DbError::conn("El puente Oracle no devolvió la versión del servidor"))
    }

    fn schema(&mut self) -> Result<Schema, DbError> {
        let value = self.exchange("schema", &self.payload(), None)?;
        let Some(tables) = value.get("tables").and_then(Value::as_array) else {
            return Err(DbError::conn("El puente Oracle no devolvió el catálogo"));
        };
        Ok(Schema {
            tables: tables.iter().filter_map(table_from_json).collect(),
        })
    }

    fn run(
        &mut self,
        sql: &str,
        limit: usize,
        cancel: &Arc<AtomicBool>,
    ) -> Result<ResultSet, DbError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(DbError::new("Consulta cancelada"));
        }
        let sql = sqlguard::check(Dialect::Oracle, sql).map_err(DbError::new)?;
        let mut payload = self.payload();
        payload["sql"] = Value::String(sql);
        payload["limit"] = json!(limit.max(1));
        let started = Instant::now();
        let value = self.exchange("run", &payload, Some(cancel))?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let columns: Vec<ColumnMeta> = value
            .get("columns")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let name = item.get("name").and_then(Value::as_str)?.to_string();
                        Some(ColumnMeta {
                            name,
                            type_name: item
                                .get("type")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let rows: Vec<Vec<Option<String>>> = value
            .get("rows")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|row| {
                        row.as_array()
                            .map(|cells| {
                                cells
                                    .iter()
                                    .map(|cell| match cell {
                                        Value::Null => None,
                                        Value::String(text) => Some(text.clone()),
                                        other => Some(other.to_string()),
                                    })
                                    .collect()
                            })
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .unwrap_or_default();
        let truncated = value
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(ResultSet {
            columns,
            rows,
            truncated,
            elapsed_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(python: &str) -> ConnSpec {
        ConnSpec {
            info: ConnInfo {
                id: "oracle".into(),
                label: "Oracle · PRODUCCIÓN".into(),
                dialect: Dialect::Oracle,
                env: EnvKind::Prod,
                target: "oraclehost:1521/ORCL".into(),
            },
            host: "db.example.com".into(),
            port: 1521,
            database: "ORCL".into(),
            user: "lector".into(),
            password: "secreta".into(),
            secret: Secret::Plain,
            oracle_service: false,
            python: python.into(),
            bridge: concat!(env!("CARGO_MANIFEST_DIR"), "/bridge/oracle_bridge.py").into(),
            timeout_secs: 10,
            schemas: vec![],
        }
    }

    #[test]
    fn catalogo_desde_json() {
        let value = json!({
            "schema": "VENTAS",
            "name": "CLIENTES",
            "kind": "VIEW",
            "comment": "Clientes dados de alta",
            "columns": [
                {"name": "ID", "type": "NUMBER(10)", "nullable": false, "pk": true},
                {"name": "NOMBRE", "type": "VARCHAR2(120)", "nullable": true, "pk": false},
                {"name": "", "type": "X"}
            ]
        });
        let table = table_from_json(&value).unwrap();
        assert_eq!(table.schema, "VENTAS");
        assert_eq!(table.qualified(), "VENTAS.CLIENTES");
        assert_eq!(table.kind, TableKind::View);
        assert_eq!(table.comment.as_deref(), Some("Clientes dados de alta"));
        assert_eq!(table.columns.len(), 2);
        assert!(table.columns[0].primary_key);
        assert!(!table.columns[0].nullable);
        assert_eq!(table.columns[1].type_name, "VARCHAR2(120)");
        assert!(table_from_json(&json!({"schema": "X"})).is_none());
    }

    #[test]
    fn rich_catalog_is_backward_compatible_and_composite_fks_keep_order() {
        let value = json!({"schema":"LIB","name":"LIBROS","kind":"TABLE","rows":0,
            "columns":[{"name":"AUTOR_ID","type":"NUMBER(10)","comment":"Autor de la obra","unique":true}],
            "fks":[{"columns":["TENANT_ID","AUTOR_ID"],"ref_schema":"LIB","ref_table":"AUTORES","ref_columns":["TENANT_ID","ID"]},
                {"columns":["X"],"ref_table":"INVALIDA","ref_columns":[]}]});
        let t = table_from_json(&value).unwrap();
        assert_eq!(t.rows, Some(0));
        assert_eq!(t.columns[0].comment.as_deref(), Some("Autor de la obra"));
        assert!(t.columns[0].unique);
        assert_eq!(t.fks.len(), 1);
        assert_eq!(t.fks[0].columns, ["TENANT_ID", "AUTOR_ID"]);
        assert_eq!(t.fks[0].ref_columns, ["TENANT_ID", "ID"]);
        let mut view = value;
        view["kind"] = json!("VIEW");
        let v = table_from_json(&view).unwrap();
        assert!(v.rows.is_none() && v.fks.is_empty());
        let old = table_from_json(&json!({"name":"ANTIGUA","columns":[{"name":"ID"}]})).unwrap();
        assert!(
            old.rows.is_none()
                && old.fks.is_empty()
                && old.columns[0].comment.is_none()
                && !old.columns[0].unique
        );
        assert_eq!(
            table_from_json(&json!({"name":"T","rows":-1}))
                .unwrap()
                .rows,
            None
        );
    }
    #[test]
    fn bridge_reads_rich_metadata_in_five_bulk_queries() {
        let script = r#"
import importlib.util, json, sys
spec=importlib.util.spec_from_file_location('bridge',sys.argv[1])
m=importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
class Cursor:
    def execute(self,q):
        connection.queries.append(q)
        if 'FROM all_tables' in q:
            self.rows=[('LIB','AUTORES','TABLE',10),('LIB','LIBROS','TABLE',0),('LIB','V_LIBROS','VIEW',None)]
            assert 'num_rows' in q
        elif 'FROM all_tab_columns' in q:
            assert 'all_col_comments' in q
            self.rows=[('LIB','AUTORES','TENANT_ID','NUMBER',None,10,0,'N',None),('LIB','AUTORES','ID','NUMBER',None,10,0,'N',None),
                ('LIB','AUTORES','NOMBRE','VARCHAR2',80,None,None,'N','Nombre del autor'),
                ('LIB','LIBROS','TENANT_ID','NUMBER',None,10,0,'N',None),('LIB','LIBROS','AUTOR_ID','NUMBER',None,10,0,'N','Autor de la obra')]
        elif 'FROM all_tab_comments' in q:
            self.rows=[('LIB','LIBROS','Catálogo bibliográfico')]
        elif "c.constraint_type IN ('P','U')" in q:
            self.rows=[('LIB','AUTORES','PK_AUTOR','P','TENANT_ID'),('LIB','AUTORES','PK_AUTOR','P','ID'),('LIB','AUTORES','U_NOMBRE','U','NOMBRE')]
        elif "c.constraint_type = 'R'" in q:
            assert 'rc.position = cc.position' in q
            self.rows=[('LIB','LIBROS','FK_AUTOR','TENANT_ID','LIB','AUTORES','TENANT_ID'),('LIB','LIBROS','FK_AUTOR','AUTOR_ID','LIB','AUTORES','ID')]
        else: raise AssertionError(q)
    def __iter__(self): return iter(self.rows)
class Connection:
    queries=[]
    closed=False
    def cursor(self): return Cursor()
    def close(self): self.closed=True
connection=Connection(); m.connect=lambda _:connection
result=m.op_schema({})
assert connection.closed and len(connection.queries)==5
print(json.dumps(result))
"#;
        let output = std::process::Command::new("python3")
            .args(["-B", "-c", script, &spec("python3").bridge])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        let tables: Vec<_> = value["tables"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(table_from_json)
            .collect();
        let books = tables.iter().find(|t| t.name == "LIBROS").unwrap();
        assert_eq!(books.rows, Some(0));
        assert_eq!(books.comment.as_deref(), Some("Catálogo bibliográfico"));
        assert_eq!(
            books.columns[1].comment.as_deref(),
            Some("Autor de la obra")
        );
        assert_eq!(books.fks[0].columns, ["TENANT_ID", "AUTOR_ID"]);
        assert_eq!(books.fks[0].ref_columns, ["TENANT_ID", "ID"]);
        let authors = tables.iter().find(|t| t.name == "AUTORES").unwrap();
        assert!(authors.columns[0].primary_key && authors.columns[1].primary_key);
        assert!(!authors.columns[0].unique && authors.columns[2].unique);
        assert!(
            tables
                .iter()
                .find(|t| t.name == "V_LIBROS")
                .unwrap()
                .rows
                .is_none()
        );
    }
    #[test]
    fn error_desde_json() {
        let error = error_from_json(&json!({
            "message": "ORA-00904: \"NO_EXISTE\": identificador no válido",
            "code": "ORA-00904",
            "position": 8,
            "connection": false
        }));
        assert_eq!(error.code.as_deref(), Some("ORA-00904"));
        assert_eq!(error.position, Some(8));
        assert!(!error.connection);
        assert!(error.message.contains("ORA-00904"));
        let error = error_from_json(&Value::Null);
        assert_eq!(error.message, "Error de Oracle");
        assert_eq!(error.position, None);
        assert!(!error.connection);
    }

    #[test]
    fn la_clave_no_aparece_en_debug() {
        let db = OracleDb::new(spec("/usr/bin/python3"));
        assert!(db.payload().to_string().contains("secreta"));
        assert!(!format!("{:?}", db.spec).contains("secreta"));
    }

    #[test]
    fn python_inexistente_es_error_de_conexion() {
        let mut db = OracleDb::new(spec("/no/existe/python3"));
        let error = db.check().unwrap_err();
        assert!(error.connection);
        assert!(error.message.contains("intérprete de Python"));
    }

    #[test]
    fn puente_inexistente_es_error_de_conexion() {
        let mut spec = spec("/usr/bin/python3");
        spec.bridge = "/no/existe/oracle_bridge.py".into();
        let error = OracleDb::new(spec).check().unwrap_err();
        assert!(error.connection);
        assert!(error.message.contains("No se encuentra el puente"));
    }

    #[test]
    fn cancelada_antes_de_arrancar() {
        let mut db = OracleDb::new(spec("/usr/bin/python3"));
        let cancel = Arc::new(AtomicBool::new(true));
        let error = db.run("SELECT 1 FROM dual", 10, &cancel).unwrap_err();
        assert_eq!(error.message, "Consulta cancelada");
        assert!(!error.connection);
    }

    #[test]
    #[ignore = "consulta real a Oracle producción; requiere ORACLE_TEST_PASSWORD"]
    fn oracle_produccion() {
        let password = std::env::var("ORACLE_TEST_PASSWORD").expect("falta ORACLE_TEST_PASSWORD");
        let python = std::env::var("ORACLE_TEST_PYTHON").unwrap_or_else(|_| "python3".into());
        let mut db = OracleDb::new(ConnSpec {
            password,
            python,
            ..spec("/usr/bin/python3")
        });

        let version = db.check().unwrap();
        assert!(
            version.to_lowercase().contains("oracle"),
            "versión inesperada: {version}"
        );

        let schema = db.schema().unwrap();
        assert!(!schema.tables.is_empty(), "el catálogo llegó vacío");
        assert!(schema.tables.iter().any(|table| !table.columns.is_empty()));
        for table in &schema.tables {
            if table.kind == TableKind::View {
                assert!(table.rows.is_none() && table.fks.is_empty());
            }
            assert!(table.rows.is_none_or(|n| n >= 0));
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

        let cancel = Arc::new(AtomicBool::new(false));
        let result = db.run("SELECT * FROM dual", 10, &cancel).unwrap();
        assert_eq!(result.columns.len(), 1);
        assert_eq!(result.columns[0].name, "DUMMY");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0].as_deref(), Some("X"));
        assert!(!result.truncated);

        let error = db
            .run("SELECT no_existe FROM dual", 10, &cancel)
            .unwrap_err();
        assert_eq!(error.code.as_deref(), Some("ORA-00904"));
        assert_eq!(error.position, Some(8));
        assert!(!error.connection);

        let result = db
            .run(
                "SELECT LEVEL AS n FROM dual CONNECT BY LEVEL <= 10",
                3,
                &cancel,
            )
            .unwrap();
        assert_eq!(result.columns[0].name, "N");
        assert_eq!(result.rows.len(), 3);
        assert!(result.truncated);
    }
}
