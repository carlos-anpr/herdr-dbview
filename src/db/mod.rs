//! Acceso a bases de datos. Todo es SOLO LECTURA: ni la IA ni el usuario pueden escribir desde aquí.

pub mod demo;
pub mod oracle;
pub mod pg;
pub mod sqlguard;

use crate::model::*;
use std::sync::{Arc, atomic::AtomicBool};

/// Una conexión viva (o capaz de abrirse sola al primer uso).
pub trait Database: Send {
    fn dialect(&self) -> Dialect;
    /// «Probar conexión»: conecta y devuelve la versión del servidor
    /// (`PostgreSQL 16.14`, `Oracle Database 19c…`). Errores de red/credenciales → `DbError::conn`.
    fn check(&mut self) -> Result<String, DbError>;
    /// Lee el catálogo (tablas, vistas y columnas; sin esquemas de sistema).
    fn schema(&mut self) -> Result<Schema, DbError>;
    /// Ejecuta UNA sentencia de solo lectura y devuelve como mucho `limit` filas
    /// (`truncated` si había más). Debe pasar antes por `sqlguard::check`.
    /// Si `cancel` pasa a `true` mientras corre, aborta y devuelve `DbError::new("Consulta cancelada")`.
    fn run(
        &mut self,
        sql: &str,
        limit: usize,
        cancel: &Arc<AtomicBool>,
    ) -> Result<ResultSet, DbError>;
}

/// Crea la conexión de la especificación (sin conectar todavía).
pub fn open(spec: &ConnSpec) -> Box<dyn Database> {
    if spec.host == "demo" {
        return Box::new(demo::DemoDb::for_spec(spec));
    }
    match spec.info.dialect {
        Dialect::Postgres => Box::new(pg::Pg::new(spec.clone())),
        Dialect::Oracle => Box::new(oracle::OracleDb::new(spec.clone())),
    }
}
