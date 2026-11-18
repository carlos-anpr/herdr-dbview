//! Contrato común entre módulos. NO se cambia sin avisar al líder.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    Oracle,
}

impl Dialect {
    pub fn name(self) -> &'static str {
        match self {
            Dialect::Postgres => "PostgreSQL",
            Dialect::Oracle => "Oracle",
        }
    }
}

/// Tipo de entorno: solo decide el color y la etiqueta de la conexión (PROD en rojo).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvKind {
    Dev,
    Prod,
    Other,
}

/// Lo que la interfaz puede enseñar de una conexión. NUNCA lleva credenciales.
#[derive(Clone, Debug, PartialEq)]
pub struct ConnInfo {
    /// Identificador estable (`dev-ventas`).
    pub id: String,
    /// Nombre visible (`Ventas · DESARROLLO`).
    pub label: String,
    pub dialect: Dialect,
    pub env: EnvKind,
    /// `host:puerto/base` para enseñar, sin usuario ni clave.
    pub target: String,
}

/// De dónde sale la clave de una conexión (para guardar sin perder la referencia original).
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Secret {
    /// Escrita en el propio fichero de configuración (0600).
    #[default]
    Plain,
    /// Variable de entorno.
    Env(String),
    /// Clave `key` de un fichero `export CLAVE=valor`.
    File { path: String, key: String },
}

/// Datos para conectar. La clave ya viene resuelta; `Debug` no la enseña.
#[derive(Clone)]
pub struct ConnSpec {
    pub info: ConnInfo,
    pub host: String,
    pub port: u16,
    /// Base de datos (PostgreSQL) o SID/servicio (Oracle).
    pub database: String,
    pub user: String,
    pub password: String,
    /// Origen de la clave; el formulario de edición solo la sustituye si el usuario escribe una nueva.
    pub secret: Secret,
    /// Oracle: `true` si `database` es un SERVICE_NAME en vez de un SID.
    pub oracle_service: bool,
    /// Oracle: intérprete de Python con `oracledb` instalado.
    pub python: String,
    /// Oracle: ruta del puente `bridge/oracle_bridge.py`.
    pub bridge: String,
    /// Límite de duración de una consulta, en segundos.
    pub timeout_secs: u64,
    /// Esquemas que ve esta conexión (vacío = todos). Permite tener varias conexiones a la misma base,
    /// cada una con las tablas de una sola aplicación (p. ej. tasaciones por un lado y Engineering por otro).
    pub schemas: Vec<String>,
}

impl fmt::Debug for ConnSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnSpec")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnMeta {
    pub name: String,
    /// Nombre del tipo tal como lo da la base (`int4`, `VARCHAR2`…); puede estar vacío.
    pub type_name: String,
}

/// Resultado de una consulta. Todos los valores llegan ya como texto; `None` = NULL.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResultSet {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Option<String>>>,
    /// `true` si había más filas que el límite pedido.
    pub truncated: bool,
    pub elapsed_ms: u64,
}

/// Error de base de datos, tal como lo enseñaría DBeaver.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DbError {
    /// Mensaje principal, en el idioma de la base (`column "x" does not exist`).
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    /// SQLSTATE (`42703`) u `ORA-00904`.
    pub code: Option<String>,
    /// Posición 1-based, en CARACTERES (no bytes), del SQL ejecutado donde está el fallo.
    pub position: Option<usize>,
    /// Texto de error que no es de la consulta (red, autenticación…) frente a un fallo del SQL.
    pub connection: bool,
}

impl DbError {
    pub fn new(message: impl Into<String>) -> Self {
        DbError {
            message: message.into(),
            ..Default::default()
        }
    }
    pub fn conn(message: impl Into<String>) -> Self {
        DbError {
            message: message.into(),
            connection: true,
            ..Default::default()
        }
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for DbError {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TableKind {
    #[default]
    Table,
    View,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColInfo {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
    pub primary_key: bool,
    /// Clave única de UNA columna (no componentes de restricciones compuestas).
    pub unique: bool,
    pub comment: Option<String>,
}

impl Default for ColInfo {
    fn default() -> Self {
        Self {
            name: String::new(),
            type_name: String::new(),
            nullable: true,
            primary_key: false,
            unique: false,
            comment: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ForeignKey {
    pub columns: Vec<String>,
    pub ref_schema: String,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    pub kind: TableKind,
    pub columns: Vec<ColInfo>,
    pub comment: Option<String>,
    pub rows: Option<i64>,
    pub fks: Vec<ForeignKey>,
}

impl TableInfo {
    /// `esquema.tabla` tal como se escribe en SQL (sin comillas).
    pub fn qualified(&self) -> String {
        if self.schema.is_empty() {
            self.name.clone()
        } else {
            format!("{}.{}", self.schema, self.name)
        }
    }
}

/// Catálogo de la conexión, sin esquemas de sistema.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Schema {
    pub tables: Vec<TableInfo>,
}

impl Schema {
    /// Deja solo las tablas de los esquemas indicados (sin distinguir mayúsculas). Lista vacía = todo.
    pub fn restricted(mut self, schemas: &[String]) -> Self {
        if !schemas.is_empty() {
            self.tables
                .retain(|t| schemas.iter().any(|s| s.eq_ignore_ascii_case(&t.schema)));
        }
        self
    }
}
