//! Conexiones de demostración sin red (`--demo`): esquemas realistas y resultados
//! deterministas, incluidos errores de tabla/columna con posición.

use super::{Database, sqlguard};
use crate::model::*;
use std::collections::BTreeSet;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Ventas,
    Clientes,
    Catastro,
}

impl Kind {
    fn dialect(self) -> Dialect {
        match self {
            Kind::Ventas | Kind::Clientes => Dialect::Postgres,
            Kind::Catastro => Dialect::Oracle,
        }
    }

    fn schema(self) -> &'static str {
        match self {
            Kind::Ventas | Kind::Clientes => "public",
            Kind::Catastro => "CATASTRO",
        }
    }

    fn tables(self) -> &'static [DemoTable] {
        match self {
            Kind::Ventas => VENTAS,
            Kind::Clientes => CLIENTES,
            Kind::Catastro => CATASTRO,
        }
    }
}

pub struct DemoDb {
    kind: Kind,
}

impl DemoDb {
    pub fn for_spec(spec: &ConnSpec) -> Self {
        let key = format!("{} {}", spec.info.id, spec.database).to_ascii_lowercase();
        let kind = if spec.info.dialect == Dialect::Oracle || key.contains("catastro") {
            Kind::Catastro
        } else if key.contains("clientes") {
            Kind::Clientes
        } else {
            Kind::Ventas
        };
        DemoDb { kind }
    }
}

impl Database for DemoDb {
    fn dialect(&self) -> Dialect {
        self.kind.dialect()
    }

    fn check(&mut self) -> Result<String, DbError> {
        Ok(match self.kind {
            Kind::Ventas => "PostgreSQL 16.14 (demo · ventas)".into(),
            Kind::Clientes => "PostgreSQL 16.14 (demo · clientes)".into(),
            Kind::Catastro => "Oracle Database 19c Enterprise Edition (demo)".into(),
        })
    }

    fn schema(&mut self) -> Result<Schema, DbError> {
        let schema = self.kind.schema();
        Ok(Schema {
            tables: self
                .kind
                .tables()
                .iter()
                .map(|table| TableInfo {
                    schema: schema.into(),
                    name: table.name.into(),
                    kind: table.kind,
                    rows: (table.kind == TableKind::Table).then(|| {
                        custom_rows(self.kind, table.name, table.columns)
                            .map_or(5, |rows| rows.len()) as i64
                    }),
                    fks: Vec::new(),
                    comment: (!table.comment.is_empty()).then(|| table.comment.to_string()),
                    columns: table
                        .columns
                        .iter()
                        .map(|column| ColInfo {
                            name: column.name.into(),
                            type_name: column.type_name.into(),
                            nullable: column.nullable,
                            primary_key: column.pk,
                            unique: false,
                            comment: None,
                        })
                        .collect(),
                })
                .collect(),
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
        let sql = sqlguard::check(self.kind.dialect(), sql).map_err(DbError::new)?;
        let tokens = tokens(&masked(&sql));
        let referenced = check_semantics(self.kind, &tokens)?;
        let limit = limit.max(1);
        let dialect = self.kind.dialect();

        if let Some(label) = count_star_label(&tokens) {
            let rows = referenced
                .first()
                .copied()
                .and_then(|table| custom_rows(self.kind, table.name, table.columns))
                .map_or(5, |rows| rows.len());
            let type_name = if dialect == Dialect::Oracle {
                "NUMBER"
            } else {
                "int8"
            };
            return Ok(ResultSet {
                columns: vec![ColumnMeta {
                    name: label,
                    type_name: type_name.into(),
                }],
                rows: vec![vec![Some(rows.to_string())]],
                truncated: false,
                elapsed_ms: 2,
            });
        }

        if referenced.is_empty() {
            if tokens
                .iter()
                .any(|token| token.kind == TokenKind::Word && token.lower == "dual")
            {
                return Ok(ResultSet {
                    columns: vec![ColumnMeta {
                        name: "DUMMY".into(),
                        type_name: "VARCHAR2".into(),
                    }],
                    rows: vec![vec![Some("X".into())]],
                    truncated: false,
                    elapsed_ms: 1,
                });
            }
            return Ok(ResultSet {
                columns: vec![ColumnMeta {
                    name: "resultado".into(),
                    type_name: "text".into(),
                }],
                rows: (1..=5).map(|i| vec![Some(format!("fila {i}"))]).collect(),
                truncated: false,
                elapsed_ms: 1,
            });
        }

        let table = referenced[0];
        let indices = selected_columns(&tokens, table);
        let rows = custom_rows(self.kind, table.name, table.columns)
            .unwrap_or_else(|| generic_rows(table));
        let columns = indices
            .iter()
            .map(|(index, label)| ColumnMeta {
                name: label.clone(),
                type_name: table.columns[*index].type_name.into(),
            })
            .collect();
        let mut rows: Vec<Vec<Option<String>>> = rows
            .into_iter()
            .map(|row| {
                indices
                    .iter()
                    .map(|(index, _)| row.get(*index).cloned().flatten())
                    .collect()
            })
            .collect();
        let sql_limit = sql_limit(&tokens);
        let effective = sql_limit.map_or(limit, |value| value.min(limit));
        let truncated = rows.len() > effective && sql_limit.is_none_or(|value| value > limit);
        rows.truncate(effective);
        Ok(ResultSet {
            columns,
            rows,
            truncated,
            elapsed_ms: 3,
        })
    }
}

/// Conexiones ficticias; `host == "demo"` hace que `db::open` use `DemoDb`.
pub fn connections() -> Vec<ConnSpec> {
    vec![
        demo(
            "demo-ventas",
            "Tienda online · DESARROLLO",
            Dialect::Postgres,
            EnvKind::Dev,
            "ventas",
        ),
        demo(
            "demo-clientes",
            "Tienda online · PRODUCCIÓN",
            Dialect::Postgres,
            EnvKind::Prod,
            "clientes",
        ),
        demo(
            "demo-catastro",
            "Catastro · Oracle",
            Dialect::Oracle,
            EnvKind::Prod,
            "CATASTRO",
        ),
    ]
}

fn demo(id: &str, label: &str, dialect: Dialect, env: EnvKind, database: &str) -> ConnSpec {
    ConnSpec {
        info: ConnInfo {
            id: id.into(),
            label: label.into(),
            dialect,
            env,
            target: format!("demo/{database}"),
        },
        host: "demo".into(),
        port: if dialect == Dialect::Oracle {
            1521
        } else {
            5432
        },
        database: database.into(),
        user: "demo".into(),
        password: String::new(),
        secret: Secret::Plain,
        oracle_service: dialect == Dialect::Oracle,
        python: String::new(),
        bridge: String::new(),
        timeout_secs: 30,
        schemas: vec![],
    }
}

// ───────────────────────────── esquemas ─────────────────────────────

struct DemoCol {
    name: &'static str,
    type_name: &'static str,
    nullable: bool,
    pk: bool,
}

struct DemoTable {
    name: &'static str,
    kind: TableKind,
    comment: &'static str,
    columns: &'static [DemoCol],
}

const fn col(name: &'static str, type_name: &'static str, nullable: bool, pk: bool) -> DemoCol {
    DemoCol {
        name,
        type_name,
        nullable,
        pk,
    }
}
const fn table(
    name: &'static str,
    kind: TableKind,
    comment: &'static str,
    columns: &'static [DemoCol],
) -> DemoTable {
    DemoTable {
        name,
        kind,
        comment,
        columns,
    }
}

static VENTAS: &[DemoTable] = &[
    table(
        "almacenes",
        TableKind::Table,
        "Almacenes propios y alquilados",
        &[
            col("id", "int4", false, true),
            col("nombre", "text", false, false),
            col("ciudad", "text", true, false),
            col("capacidad", "int4", true, false),
        ],
    ),
    table(
        "categorias",
        TableKind::Table,
        "",
        &[
            col("id", "int4", false, true),
            col("nombre", "text", false, false),
            col("descripcion", "text", true, false),
        ],
    ),
    table(
        "clientes",
        TableKind::Table,
        "Clientes facturables",
        &[
            col("id", "int4", false, true),
            col("nombre", "text", false, false),
            col("nif", "varchar(20)", false, false),
            col("email", "text", true, false),
            col("telefono", "varchar(20)", true, false),
            col("alta", "date", false, false),
            col("activo", "bool", false, false),
        ],
    ),
    table(
        "facturas",
        TableKind::Table,
        "Facturas emitidas",
        &[
            col("id", "int4", false, true),
            col("pedido_id", "int4", false, false),
            col("numero", "varchar(20)", false, false),
            col("fecha", "date", false, false),
            col("base", "numeric(12,2)", false, false),
            col("iva", "numeric(12,2)", false, false),
            col("total", "numeric(12,2)", false, false),
            col("pagada", "bool", false, false),
        ],
    ),
    table(
        "lineas_pedido",
        TableKind::Table,
        "",
        &[
            col("id", "int4", false, true),
            col("pedido_id", "int4", false, false),
            col("producto_id", "int4", false, false),
            col("cantidad", "int4", false, false),
            col("precio", "numeric(12,2)", false, false),
        ],
    ),
    table(
        "pagos",
        TableKind::Table,
        "",
        &[
            col("id", "int4", false, true),
            col("factura_id", "int4", false, false),
            col("fecha", "date", false, false),
            col("importe", "numeric(12,2)", false, false),
            col("metodo", "varchar(20)", false, false),
        ],
    ),
    table(
        "pedidos",
        TableKind::Table,
        "Pedidos de venta",
        &[
            col("id", "int4", false, true),
            col("cliente_id", "int4", false, false),
            col("fecha", "timestamp", false, false),
            col("estado", "varchar(20)", false, false),
            col("total", "numeric(12,2)", false, false),
        ],
    ),
    table(
        "productos",
        TableKind::Table,
        "Catálogo",
        &[
            col("id", "int4", false, true),
            col("nombre", "text", false, false),
            col("categoria_id", "int4", false, false),
            col("precio", "numeric(12,2)", false, false),
            col("stock", "int4", false, false),
            col("activo", "bool", false, false),
        ],
    ),
    table(
        "proveedores",
        TableKind::View,
        "Vista de proveedores activos",
        &[
            col("id", "int4", false, true),
            col("nombre", "text", false, false),
            col("nif", "varchar(20)", false, false),
            col("email", "text", true, false),
            col("activo", "bool", false, false),
        ],
    ),
    table(
        "stock",
        TableKind::Table,
        "",
        &[
            col("id", "int4", false, true),
            col("producto_id", "int4", false, false),
            col("almacen_id", "int4", false, false),
            col("unidades", "int4", false, false),
            col("actualizado", "timestamp", false, false),
        ],
    ),
];

static CLIENTES: &[DemoTable] = &[
    table(
        "campanas",
        TableKind::Table,
        "",
        &[
            col("id", "int4", false, true),
            col("nombre", "text", false, false),
            col("canal", "varchar(20)", false, false),
            col("inicio", "date", false, false),
            col("fin", "date", true, false),
            col("presupuesto", "numeric(12,2)", true, false),
        ],
    ),
    table(
        "clientes",
        TableKind::Table,
        "Cartera de clientes",
        &[
            col("id", "int4", false, true),
            col("nombre", "text", false, false),
            col("nif", "varchar(20)", false, false),
            col("sector", "varchar(40)", true, false),
            col("empleados", "int4", true, false),
            col("alta", "date", false, false),
            col("activo", "bool", false, false),
        ],
    ),
    table(
        "contactos",
        TableKind::Table,
        "",
        &[
            col("id", "int4", false, true),
            col("cliente_id", "int4", false, false),
            col("nombre", "text", false, false),
            col("cargo", "varchar(60)", true, false),
            col("email", "text", true, false),
            col("telefono", "varchar(20)", true, false),
        ],
    ),
    table(
        "contratos",
        TableKind::Table,
        "Contratos de mantenimiento",
        &[
            col("id", "int4", false, true),
            col("cliente_id", "int4", false, false),
            col("numero", "varchar(20)", false, false),
            col("inicio", "date", false, false),
            col("fin", "date", true, false),
            col("importe", "numeric(12,2)", false, false),
            col("estado", "varchar(20)", false, false),
        ],
    ),
    table(
        "direcciones",
        TableKind::Table,
        "",
        &[
            col("id", "int4", false, true),
            col("cliente_id", "int4", false, false),
            col("calle", "text", false, false),
            col("ciudad", "text", false, false),
            col("cp", "varchar(10)", true, false),
            col("pais", "varchar(40)", false, false),
        ],
    ),
    table(
        "interacciones",
        TableKind::Table,
        "",
        &[
            col("id", "int4", false, true),
            col("cliente_id", "int4", false, false),
            col("fecha", "timestamp", false, false),
            col("tipo", "varchar(20)", false, false),
            col("resumen", "text", true, false),
        ],
    ),
    table(
        "oportunidades",
        TableKind::Table,
        "Oportunidades comerciales",
        &[
            col("id", "int4", false, true),
            col("cliente_id", "int4", false, false),
            col("nombre", "text", false, false),
            col("importe", "numeric(12,2)", false, false),
            col("fase", "varchar(20)", false, false),
            col("cierre", "date", true, false),
        ],
    ),
    table(
        "segmentos",
        TableKind::View,
        "Vista de segmentos",
        &[
            col("id", "int4", false, true),
            col("nombre", "text", false, false),
            col("descripcion", "text", true, false),
        ],
    ),
    table(
        "tickets",
        TableKind::Table,
        "",
        &[
            col("id", "int4", false, true),
            col("cliente_id", "int4", false, false),
            col("asunto", "text", false, false),
            col("prioridad", "varchar(10)", false, false),
            col("abierto", "bool", false, false),
            col("creado", "timestamp", false, false),
        ],
    ),
    table(
        "usuarios",
        TableKind::Table,
        "",
        &[
            col("id", "int4", false, true),
            col("nombre", "text", false, false),
            col("email", "text", false, false),
            col("rol", "varchar(20)", false, false),
            col("activo", "bool", false, false),
        ],
    ),
];

static CATASTRO: &[DemoTable] = &[
    table(
        "CATASTRO_REF",
        TableKind::Table,
        "",
        &[
            col("ID", "NUMBER(10)", false, true),
            col("REF_CATASTRAL", "VARCHAR2(20)", false, false),
            col("PROVINCIA", "VARCHAR2(60)", false, false),
            col("HOJA", "VARCHAR2(20)", true, false),
        ],
    ),
    table(
        "EXPEDIENTES",
        TableKind::Table,
        "Expedientes catastrales",
        &[
            col("ID", "NUMBER(10)", false, true),
            col("INMUEBLE_ID", "NUMBER(10)", false, false),
            col("NUMERO", "VARCHAR2(30)", false, false),
            col("ESTADO", "VARCHAR2(20)", false, false),
            col("ABIERTO", "DATE", false, false),
        ],
    ),
    table(
        "INMUEBLES",
        TableKind::Table,
        "Inmuebles registrados",
        &[
            col("ID", "NUMBER(10)", false, true),
            col("REF_CATASTRAL", "VARCHAR2(20)", false, false),
            col("DIRECCION", "VARCHAR2(200)", false, false),
            col("MUNICIPIO_ID", "NUMBER(10)", true, false),
            col("TIPO_ID", "NUMBER(5)", true, false),
            col("SUPERFICIE", "NUMBER(10,2)", true, false),
            col("VALOR_SUELO", "NUMBER(14,2)", true, false),
            col("FECHA_ALTA", "DATE", false, false),
        ],
    ),
    table(
        "LOCALIZACIONES",
        TableKind::Table,
        "",
        &[
            col("ID", "NUMBER(10)", false, true),
            col("INMUEBLE_ID", "NUMBER(10)", false, false),
            col("LATITUD", "NUMBER(9,6)", true, false),
            col("LONGITUD", "NUMBER(9,6)", true, false),
            col("ALTITUD", "NUMBER(6)", true, false),
        ],
    ),
    table(
        "MUNICIPIOS",
        TableKind::Table,
        "",
        &[
            col("ID", "NUMBER(10)", false, true),
            col("NOMBRE", "VARCHAR2(100)", false, false),
            col("PROVINCIA", "VARCHAR2(60)", false, false),
            col("CODIGO_POSTAL", "VARCHAR2(10)", true, false),
        ],
    ),
    table(
        "PROPIETARIOS",
        TableKind::Table,
        "Titulares catastrales",
        &[
            col("ID", "NUMBER(10)", false, true),
            col("NIF", "VARCHAR2(20)", false, false),
            col("NOMBRE", "VARCHAR2(120)", false, false),
            col("TIPO_PERSONA", "VARCHAR2(10)", false, false),
        ],
    ),
    table(
        "TIPOS_INMUEBLE",
        TableKind::Table,
        "",
        &[
            col("ID", "NUMBER(5)", false, true),
            col("DESCRIPCION", "VARCHAR2(80)", false, false),
        ],
    ),
    table(
        "TITULARIDAD",
        TableKind::Table,
        "",
        &[
            col("ID", "NUMBER(10)", false, true),
            col("INMUEBLE_ID", "NUMBER(10)", false, false),
            col("PROPIETARIO_ID", "NUMBER(10)", false, false),
            col("PORCENTAJE", "NUMBER(5,2)", false, false),
            col("DESDE", "DATE", false, false),
        ],
    ),
    table(
        "USOS",
        TableKind::Table,
        "",
        &[
            col("ID", "NUMBER(5)", false, true),
            col("INMUEBLE_ID", "NUMBER(10)", false, false),
            col("USO", "VARCHAR2(40)", false, false),
            col("DESDE", "DATE", false, false),
        ],
    ),
    table(
        "VALORACIONES",
        TableKind::Table,
        "Valoraciones de referencia",
        &[
            col("ID", "NUMBER(10)", false, true),
            col("INMUEBLE_ID", "NUMBER(10)", false, false),
            col("FECHA", "DATE", false, false),
            col("VALOR", "NUMBER(14,2)", false, false),
            col("METODO", "VARCHAR2(30)", true, false),
        ],
    ),
];

// ───────────────────────────── datos ─────────────────────────────

fn row(columns: &[DemoCol], values: &[(&str, Option<&str>)]) -> Vec<Option<String>> {
    columns
        .iter()
        .map(|column| {
            values
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(column.name))
                .and_then(|(_, value)| value.map(str::to_string))
        })
        .collect()
}

fn custom_rows(kind: Kind, name: &str, columns: &[DemoCol]) -> Option<Vec<Vec<Option<String>>>> {
    let upper = name.to_ascii_uppercase();
    let rows = match (kind, upper.as_str()) {
        (Kind::Ventas, "CLIENTES") => vec![
            row(
                columns,
                &[
                    ("id", Some("1")),
                    ("nombre", Some("Talleres Rivas S.L.")),
                    ("nif", Some("B12345678")),
                    ("email", Some("compras@rivas.es")),
                    ("telefono", Some("910000001")),
                    ("alta", Some("2023-02-14")),
                    ("activo", Some("true")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("2")),
                    ("nombre", Some("Distribuciones Norte S.A.")),
                    ("nif", Some("A87654321")),
                    ("email", Some("admin@dnorte.es")),
                    ("telefono", Some("944000002")),
                    ("alta", Some("2022-11-03")),
                    ("activo", Some("true")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("3")),
                    ("nombre", Some("Clínica San Lucas")),
                    ("nif", Some("B55667788")),
                    ("email", Some("facturacion@sanlucas.es")),
                    ("telefono", Some("963000003")),
                    ("alta", Some("2024-01-20")),
                    ("activo", Some("true")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("4")),
                    ("nombre", Some("Hotel Miramar")),
                    ("nif", Some("B99887766")),
                    ("email", Some("reservas@miramar.es")),
                    ("telefono", Some("952000004")),
                    ("alta", Some("2021-06-30")),
                    ("activo", Some("false")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("5")),
                    ("nombre", Some("Panificadora La Espiga")),
                    ("nif", Some("B33445566")),
                    ("email", Some("pedidos@laespiga.es")),
                    ("telefono", Some("915000005")),
                    ("alta", Some("2025-03-11")),
                    ("activo", Some("true")),
                ],
            ),
        ],
        (Kind::Ventas, "PEDIDOS") => vec![
            row(
                columns,
                &[
                    ("id", Some("1")),
                    ("cliente_id", Some("1")),
                    ("fecha", Some("2026-10-02 09:15:00")),
                    ("estado", Some("entregado")),
                    ("total", Some("1240.50")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("2")),
                    ("cliente_id", Some("2")),
                    ("fecha", Some("2026-10-05 12:40:00")),
                    ("estado", Some("pendiente")),
                    ("total", Some("870.00")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("3")),
                    ("cliente_id", Some("1")),
                    ("fecha", Some("2026-10-19 16:05:00")),
                    ("estado", Some("entregado")),
                    ("total", Some("315.75")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("4")),
                    ("cliente_id", Some("3")),
                    ("fecha", Some("2026-11-02 08:30:00")),
                    ("estado", Some("en tránsito")),
                    ("total", Some("2490.00")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("5")),
                    ("cliente_id", Some("5")),
                    ("fecha", Some("2026-11-10 11:20:00")),
                    ("estado", Some("pendiente")),
                    ("total", Some("96.40")),
                ],
            ),
        ],
        (Kind::Ventas, "PRODUCTOS") => vec![
            row(
                columns,
                &[
                    ("id", Some("1")),
                    ("nombre", Some("Monitor 27\"")),
                    ("categoria_id", Some("3")),
                    ("precio", Some("219.90")),
                    ("stock", Some("24")),
                    ("activo", Some("true")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("2")),
                    ("nombre", Some("Teclado mecánico")),
                    ("categoria_id", Some("3")),
                    ("precio", Some("89.00")),
                    ("stock", Some("130")),
                    ("activo", Some("true")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("3")),
                    ("nombre", Some("Silla ergonómica")),
                    ("categoria_id", Some("2")),
                    ("precio", Some("149.50")),
                    ("stock", Some("8")),
                    ("activo", Some("true")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("4")),
                    ("nombre", Some("Lámpara LED")),
                    ("categoria_id", Some("2")),
                    ("precio", Some("34.95")),
                    ("stock", Some("0")),
                    ("activo", Some("false")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("5")),
                    ("nombre", Some("Disco SSD 1 TB")),
                    ("categoria_id", Some("3")),
                    ("precio", Some("74.99")),
                    ("stock", Some("56")),
                    ("activo", Some("true")),
                ],
            ),
        ],
        (Kind::Ventas, "FACTURAS") => vec![
            row(
                columns,
                &[
                    ("id", Some("1")),
                    ("pedido_id", Some("1")),
                    ("numero", Some("FAC-2026-0001")),
                    ("fecha", Some("2026-10-02")),
                    ("base", Some("1025.21")),
                    ("iva", Some("215.29")),
                    ("total", Some("1240.50")),
                    ("pagada", Some("true")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("2")),
                    ("pedido_id", Some("2")),
                    ("numero", Some("FAC-2026-0002")),
                    ("fecha", Some("2026-10-06")),
                    ("base", Some("719.01")),
                    ("iva", Some("150.99")),
                    ("total", Some("870.00")),
                    ("pagada", Some("false")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("3")),
                    ("pedido_id", Some("3")),
                    ("numero", Some("FAC-2026-0003")),
                    ("fecha", Some("2026-10-20")),
                    ("base", Some("260.95")),
                    ("iva", Some("54.80")),
                    ("total", Some("315.75")),
                    ("pagada", Some("true")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("4")),
                    ("pedido_id", Some("4")),
                    ("numero", Some("FAC-2026-0004")),
                    ("fecha", Some("2026-11-03")),
                    ("base", Some("2057.85")),
                    ("iva", Some("432.15")),
                    ("total", Some("2490.00")),
                    ("pagada", Some("false")),
                ],
            ),
        ],
        (Kind::Ventas, "PAGOS") => vec![
            row(
                columns,
                &[
                    ("id", Some("1")),
                    ("factura_id", Some("1")),
                    ("fecha", Some("2026-10-04")),
                    ("importe", Some("1240.50")),
                    ("metodo", Some("transferencia")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("2")),
                    ("factura_id", Some("3")),
                    ("fecha", Some("2026-10-22")),
                    ("importe", Some("315.75")),
                    ("metodo", Some("tarjeta")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("3")),
                    ("factura_id", Some("2")),
                    ("fecha", Some("2026-10-08")),
                    ("importe", Some("400.00")),
                    ("metodo", Some("transferencia")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("4")),
                    ("factura_id", Some("4")),
                    ("fecha", Some("2026-11-05")),
                    ("importe", Some("1000.00")),
                    ("metodo", Some("transferencia")),
                ],
            ),
        ],
        (Kind::Ventas, "PROVEEDORES") => vec![
            row(
                columns,
                &[
                    ("id", Some("1")),
                    ("nombre", Some("Suministros Vega S.L.")),
                    ("nif", Some("B11112222")),
                    ("email", Some("ventas@vega.es")),
                    ("activo", Some("true")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("2")),
                    ("nombre", Some("TecnoMayorista S.A.")),
                    ("nif", Some("A33334444")),
                    ("email", Some("pedidos@tecnomayorista.es")),
                    ("activo", Some("true")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("3")),
                    ("nombre", Some("Muebles Duero")),
                    ("nif", Some("B55556666")),
                    ("email", Some("info@mueblesduero.es")),
                    ("activo", Some("false")),
                ],
            ),
        ],
        (Kind::Clientes, "CLIENTES") => vec![
            row(
                columns,
                &[
                    ("id", Some("1")),
                    ("nombre", Some("Grupo Altamira")),
                    ("nif", Some("A28001111")),
                    ("sector", Some("Energía")),
                    ("empleados", Some("420")),
                    ("alta", Some("2019-04-10")),
                    ("activo", Some("true")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("2")),
                    ("nombre", Some("Nexo Salud S.L.")),
                    ("nif", Some("B28002222")),
                    ("sector", Some("Sanidad")),
                    ("empleados", Some("85")),
                    ("alta", Some("2021-09-15")),
                    ("activo", Some("true")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("3")),
                    ("nombre", Some("Logística Peninsular")),
                    ("nif", Some("A28003333")),
                    ("sector", Some("Transporte")),
                    ("empleados", Some("230")),
                    ("alta", Some("2018-01-23")),
                    ("activo", Some("true")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("4")),
                    ("nombre", Some("Estudio Delta")),
                    ("nif", Some("B28004444")),
                    ("sector", Some("Publicidad")),
                    ("empleados", Some("32")),
                    ("alta", Some("2023-06-01")),
                    ("activo", Some("true")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("5")),
                    ("nombre", Some("Cooperativa Levante")),
                    ("nif", Some("B28005555")),
                    ("sector", Some("Agroalimentación")),
                    ("empleados", Some("140")),
                    ("alta", Some("2020-02-17")),
                    ("activo", Some("false")),
                ],
            ),
        ],
        (Kind::Clientes, "CONTACTOS") => vec![
            row(
                columns,
                &[
                    ("id", Some("1")),
                    ("cliente_id", Some("1")),
                    ("nombre", Some("Marta Ibáñez")),
                    ("cargo", Some("Directora financiera")),
                    ("email", Some("marta@altamira.es")),
                    ("telefono", Some("911000001")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("2")),
                    ("cliente_id", Some("2")),
                    ("nombre", Some("Javier Ortega")),
                    ("cargo", Some("Responsable de compras")),
                    ("email", Some("jortega@nexosalud.es")),
                    ("telefono", Some("932000002")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("3")),
                    ("cliente_id", Some("3")),
                    ("nombre", Some("Lucía Serrano")),
                    ("cargo", Some("Directora de operaciones")),
                    ("email", Some("lserrano@logpen.es")),
                    ("telefono", Some("963000003")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("4")),
                    ("cliente_id", Some("4")),
                    ("nombre", Some("Pablo Vidal")),
                    ("cargo", Some("Socio")),
                    ("email", Some("pvidal@estudiodelta.es")),
                    ("telefono", Some("915000004")),
                ],
            ),
        ],
        (Kind::Clientes, "OPORTUNIDADES") => vec![
            row(
                columns,
                &[
                    ("id", Some("1")),
                    ("cliente_id", Some("1")),
                    ("nombre", Some("Renovación de flota")),
                    ("importe", Some("180000.00")),
                    ("fase", Some("negociación")),
                    ("cierre", Some("2026-12-15")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("2")),
                    ("cliente_id", Some("2")),
                    ("nombre", Some("Implantación HIS")),
                    ("importe", Some("95000.00")),
                    ("fase", Some("propuesta")),
                    ("cierre", Some("2026-11-30")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("3")),
                    ("cliente_id", Some("3")),
                    ("nombre", Some("Automatización de almacén")),
                    ("importe", Some("240000.00")),
                    ("fase", Some("ganada")),
                    ("cierre", Some("2026-09-20")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("4")),
                    ("cliente_id", Some("5")),
                    ("nombre", Some("Cuadro de mando")),
                    ("importe", Some("18000.00")),
                    ("fase", Some("perdida")),
                    ("cierre", Some("2026-08-01")),
                ],
            ),
        ],
        (Kind::Clientes, "CONTRATOS") => vec![
            row(
                columns,
                &[
                    ("id", Some("1")),
                    ("cliente_id", Some("1")),
                    ("numero", Some("CT-2024-001")),
                    ("inicio", Some("2024-01-01")),
                    ("fin", Some("2026-12-31")),
                    ("importe", Some("60000.00")),
                    ("estado", Some("vigente")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("2")),
                    ("cliente_id", Some("2")),
                    ("numero", Some("CT-2025-014")),
                    ("inicio", Some("2025-03-01")),
                    ("fin", Some("2027-02-28")),
                    ("importe", Some("24000.00")),
                    ("estado", Some("vigente")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("3")),
                    ("cliente_id", Some("3")),
                    ("numero", Some("CT-2023-007")),
                    ("inicio", Some("2023-07-01")),
                    ("fin", Some("2026-06-30")),
                    ("importe", Some("78000.00")),
                    ("estado", Some("renovación")),
                ],
            ),
        ],
        (Kind::Clientes, "TICKETS") => vec![
            row(
                columns,
                &[
                    ("id", Some("1")),
                    ("cliente_id", Some("2")),
                    ("asunto", Some("Error al exportar informes")),
                    ("prioridad", Some("alta")),
                    ("abierto", Some("true")),
                    ("creado", Some("2026-11-14 08:42:00")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("2")),
                    ("cliente_id", Some("1")),
                    ("asunto", Some("Solicitud de usuario adicional")),
                    ("prioridad", Some("media")),
                    ("abierto", Some("true")),
                    ("creado", Some("2026-11-10 15:10:00")),
                ],
            ),
            row(
                columns,
                &[
                    ("id", Some("3")),
                    ("cliente_id", Some("4")),
                    ("asunto", Some("Cambio de dirección fiscal")),
                    ("prioridad", Some("baja")),
                    ("abierto", Some("false")),
                    ("creado", Some("2026-10-28 11:05:00")),
                ],
            ),
        ],
        (Kind::Catastro, "INMUEBLES") => vec![
            row(
                columns,
                &[
                    ("ID", Some("1")),
                    ("REF_CATASTRAL", Some("2807901VK4721A0001XY")),
                    ("DIRECCION", Some("Calle Mayor 14")),
                    ("MUNICIPIO_ID", Some("1")),
                    ("TIPO_ID", Some("1")),
                    ("SUPERFICIE", Some("340.50")),
                    ("VALOR_SUELO", Some("182000.00")),
                    ("FECHA_ALTA", Some("2004-05-18")),
                ],
            ),
            row(
                columns,
                &[
                    ("ID", Some("2")),
                    ("REF_CATASTRAL", Some("0801902DF3800B0002JK")),
                    ("DIRECCION", Some("Avenida Diagonal 220")),
                    ("MUNICIPIO_ID", Some("2")),
                    ("TIPO_ID", Some("2")),
                    ("SUPERFICIE", Some("1250.00")),
                    ("VALOR_SUELO", Some("940500.00")),
                    ("FECHA_ALTA", Some("1998-11-02")),
                ],
            ),
            row(
                columns,
                &[
                    ("ID", Some("3")),
                    ("REF_CATASTRAL", Some("4601503YJ2700C0003LM")),
                    ("DIRECCION", Some("Plaza del Ayuntamiento 8")),
                    ("MUNICIPIO_ID", Some("3")),
                    ("TIPO_ID", Some("2")),
                    ("SUPERFICIE", Some("780.25")),
                    ("VALOR_SUELO", Some("512300.00")),
                    ("FECHA_ALTA", Some("2011-02-25")),
                ],
            ),
            row(
                columns,
                &[
                    ("ID", Some("4")),
                    ("REF_CATASTRAL", Some("4109104SE5100D0004NP")),
                    ("DIRECCION", Some("Calle Sierpes 45")),
                    ("MUNICIPIO_ID", Some("4")),
                    ("TIPO_ID", Some("3")),
                    ("SUPERFICIE", Some("210.00")),
                    ("VALOR_SUELO", Some("98750.00")),
                    ("FECHA_ALTA", Some("1975-07-09")),
                ],
            ),
            row(
                columns,
                &[
                    ("ID", Some("5")),
                    ("REF_CATASTRAL", Some("2807905VK4721A0005QR")),
                    ("DIRECCION", Some("Calle Alcalá 101")),
                    ("MUNICIPIO_ID", Some("1")),
                    ("TIPO_ID", Some("1")),
                    ("SUPERFICIE", Some("412.75")),
                    ("VALOR_SUELO", Some("320400.00")),
                    ("FECHA_ALTA", Some("2009-09-30")),
                ],
            ),
        ],
        (Kind::Catastro, "PROPIETARIOS") => vec![
            row(
                columns,
                &[
                    ("ID", Some("1")),
                    ("NIF", Some("12345678A")),
                    ("NOMBRE", Some("Ana Martín Ruiz")),
                    ("TIPO_PERSONA", Some("FISICA")),
                ],
            ),
            row(
                columns,
                &[
                    ("ID", Some("2")),
                    ("NIF", Some("B28001111")),
                    ("NOMBRE", Some("Inversiones Altamira S.L.")),
                    ("TIPO_PERSONA", Some("JURIDICA")),
                ],
            ),
            row(
                columns,
                &[
                    ("ID", Some("3")),
                    ("NIF", Some("87654321B")),
                    ("NOMBRE", Some("José Luis Navarro")),
                    ("TIPO_PERSONA", Some("FISICA")),
                ],
            ),
            row(
                columns,
                &[
                    ("ID", Some("4")),
                    ("NIF", Some("A28002222")),
                    ("NOMBRE", Some("Patrimonial Delta S.A.")),
                    ("TIPO_PERSONA", Some("JURIDICA")),
                ],
            ),
        ],
        (Kind::Catastro, "VALORACIONES") => vec![
            row(
                columns,
                &[
                    ("ID", Some("1")),
                    ("INMUEBLE_ID", Some("1")),
                    ("FECHA", Some("2025-01-15 00:00:00")),
                    ("VALOR", Some("205000.00")),
                    ("METODO", Some("MERCADO")),
                ],
            ),
            row(
                columns,
                &[
                    ("ID", Some("2")),
                    ("INMUEBLE_ID", Some("2")),
                    ("FECHA", Some("2025-02-20 00:00:00")),
                    ("VALOR", Some("1150000.00")),
                    ("METODO", Some("MERCADO")),
                ],
            ),
            row(
                columns,
                &[
                    ("ID", Some("3")),
                    ("INMUEBLE_ID", Some("3")),
                    ("FECHA", Some("2024-11-05 00:00:00")),
                    ("VALOR", Some("640000.00")),
                    ("METODO", Some("COSTE")),
                ],
            ),
            row(
                columns,
                &[
                    ("ID", Some("4")),
                    ("INMUEBLE_ID", Some("4")),
                    ("FECHA", Some("2023-06-30 00:00:00")),
                    ("VALOR", Some("120000.00")),
                    ("METODO", Some("MERCADO")),
                ],
            ),
        ],
        (Kind::Catastro, "EXPEDIENTES") => vec![
            row(
                columns,
                &[
                    ("ID", Some("1")),
                    ("INMUEBLE_ID", Some("1")),
                    ("NUMERO", Some("EXP-2024-0001")),
                    ("ESTADO", Some("TRAMITACION")),
                    ("ABIERTO", Some("2024-03-01")),
                ],
            ),
            row(
                columns,
                &[
                    ("ID", Some("2")),
                    ("INMUEBLE_ID", Some("2")),
                    ("NUMERO", Some("EXP-2024-0042")),
                    ("ESTADO", Some("FINALIZADO")),
                    ("ABIERTO", Some("2024-05-10")),
                ],
            ),
            row(
                columns,
                &[
                    ("ID", Some("3")),
                    ("INMUEBLE_ID", Some("5")),
                    ("NUMERO", Some("EXP-2025-0107")),
                    ("ESTADO", Some("TRAMITACION")),
                    ("ABIERTO", Some("2025-01-22")),
                ],
            ),
        ],
        (Kind::Catastro, "MUNICIPIOS") => vec![
            row(
                columns,
                &[
                    ("ID", Some("1")),
                    ("NOMBRE", Some("Madrid")),
                    ("PROVINCIA", Some("Madrid")),
                    ("CODIGO_POSTAL", Some("28001")),
                ],
            ),
            row(
                columns,
                &[
                    ("ID", Some("2")),
                    ("NOMBRE", Some("Barcelona")),
                    ("PROVINCIA", Some("Barcelona")),
                    ("CODIGO_POSTAL", Some("08019")),
                ],
            ),
            row(
                columns,
                &[
                    ("ID", Some("3")),
                    ("NOMBRE", Some("Valencia")),
                    ("PROVINCIA", Some("Valencia")),
                    ("CODIGO_POSTAL", Some("46002")),
                ],
            ),
            row(
                columns,
                &[
                    ("ID", Some("4")),
                    ("NOMBRE", Some("Sevilla")),
                    ("PROVINCIA", Some("Sevilla")),
                    ("CODIGO_POSTAL", Some("41004")),
                ],
            ),
        ],
        (Kind::Catastro, "TIPOS_INMUEBLE") => vec![
            row(
                columns,
                &[("ID", Some("1")), ("DESCRIPCION", Some("VIVIENDA"))],
            ),
            row(
                columns,
                &[("ID", Some("2")), ("DESCRIPCION", Some("LOCAL COMERCIAL"))],
            ),
            row(
                columns,
                &[("ID", Some("3")), ("DESCRIPCION", Some("OFICINA"))],
            ),
        ],
        _ => return None,
    };
    Some(rows)
}

const NOMBRES: &[&str] = &[
    "Ana García",
    "Luis Pérez",
    "Marta Ruiz",
    "Jorge Sáez",
    "Elena Díaz",
];
const CIUDADES: &[&str] = &["Madrid", "Barcelona", "Valencia", "Sevilla", "Bilbao"];
const PROVINCIAS: &[&str] = &["Madrid", "Barcelona", "Valencia", "Sevilla", "Bizkaia"];

fn generic_rows(table: &DemoTable) -> Vec<Vec<Option<String>>> {
    (0..5)
        .map(|index| {
            table
                .columns
                .iter()
                .map(|column| generic_value(column, index))
                .collect()
        })
        .collect()
}

fn generic_value(column: &DemoCol, index: usize) -> Option<String> {
    let name = column.name.to_ascii_lowercase();
    let type_name = column.type_name.to_ascii_lowercase();
    if type_name.contains("bool") {
        return Some(index.is_multiple_of(2).to_string());
    }
    if name == "id" {
        return Some((index + 1).to_string());
    }
    if is_date_type(&type_name) {
        return Some(format!(
            "2026-{:02}-{:02}",
            index + 1,
            (index * 3 + 5).min(28)
        ));
    }
    if type_name.contains("int") || type_name.contains("number") || type_name.contains("numeric") {
        return Some((index * 10 + 15).to_string());
    }
    if name.contains("nombre") {
        return Some(NOMBRES[index % NOMBRES.len()].to_string());
    }
    if name.contains("email") {
        return Some(format!("contacto{}@ejemplo.es", index + 1));
    }
    if name.contains("nif") {
        return Some(format!("B{:08}", 10_000_000 + index));
    }
    if name.contains("ciudad") || name.contains("municipio") {
        return Some(CIUDADES[index % CIUDADES.len()].to_string());
    }
    if name.contains("provincia") {
        return Some(PROVINCIAS[index % PROVINCIAS.len()].to_string());
    }
    if name.contains("estado") || name.contains("fase") {
        return Some(["pendiente", "activo", "cerrado"][index % 3].to_string());
    }
    Some(format!("{} {}", name.replace('_', " "), index + 1))
}

fn is_date_type(type_name: &str) -> bool {
    type_name.contains("date") || type_name.contains("timestamp") || type_name.contains("time")
}

// ───────────────────────────── SQL de mentira ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenKind {
    Word,
    LParen,
    RParen,
    Comma,
    Dot,
    Star,
}

struct Token {
    text: String,
    lower: String,
    start: usize,
    kind: TokenKind,
}

fn masked(sql: &str) -> Vec<char> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = chars.clone();
    let length = chars.len();
    let mut index = 0;
    while index < length {
        let current = chars[index];
        if current == '\'' || current == '"' {
            let quote = current;
            let mut end = index + 1;
            while end < length {
                if chars[end] == quote {
                    if quote == '\'' && end + 1 < length && chars[end + 1] == '\'' {
                        end += 2;
                        continue;
                    }
                    break;
                }
                end += 1;
            }
            let last = end.min(length.saturating_sub(1));
            for slot in out.iter_mut().take(last + 1).skip(index) {
                if *slot != '\n' {
                    *slot = ' ';
                }
            }
            index = end + 1;
        } else if current == '-' && index + 1 < length && chars[index + 1] == '-' {
            while index < length && chars[index] != '\n' {
                out[index] = ' ';
                index += 1;
            }
        } else if current == '/' && index + 1 < length && chars[index + 1] == '*' {
            out[index] = ' ';
            out[index + 1] = ' ';
            index += 2;
            while index < length {
                if chars[index] == '*' && index + 1 < length && chars[index + 1] == '/' {
                    out[index] = ' ';
                    out[index + 1] = ' ';
                    index += 2;
                    break;
                }
                if chars[index] != '\n' {
                    out[index] = ' ';
                }
                index += 1;
            }
        } else {
            index += 1;
        }
    }
    out
}

fn tokens(chars: &[char]) -> Vec<Token> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let current = chars[index];
        if current.is_alphanumeric() || matches!(current, '_' | '$' | '#') {
            let start = index;
            index += 1;
            while index < chars.len()
                && (chars[index].is_alphanumeric() || matches!(chars[index], '_' | '$' | '#'))
            {
                index += 1;
            }
            let text: String = chars[start..index].iter().collect();
            let lower = text.to_lowercase();
            out.push(Token {
                text,
                lower,
                start,
                kind: TokenKind::Word,
            });
        } else {
            let kind = match current {
                '(' => Some(TokenKind::LParen),
                ')' => Some(TokenKind::RParen),
                ',' => Some(TokenKind::Comma),
                '.' => Some(TokenKind::Dot),
                '*' => Some(TokenKind::Star),
                _ => None,
            };
            if let Some(kind) = kind {
                out.push(Token {
                    text: current.to_string(),
                    lower: current.to_string(),
                    start: index,
                    kind,
                });
            }
            index += 1;
        }
    }
    out
}

fn is_keyword(word: &str) -> bool {
    matches!(
        word,
        "select"
            | "distinct"
            | "all"
            | "from"
            | "where"
            | "and"
            | "or"
            | "not"
            | "null"
            | "is"
            | "in"
            | "like"
            | "ilike"
            | "between"
            | "order"
            | "by"
            | "group"
            | "having"
            | "limit"
            | "offset"
            | "fetch"
            | "first"
            | "next"
            | "rows"
            | "only"
            | "as"
            | "asc"
            | "desc"
            | "join"
            | "inner"
            | "left"
            | "right"
            | "full"
            | "cross"
            | "outer"
            | "on"
            | "using"
            | "union"
            | "intersect"
            | "except"
            | "with"
            | "recursive"
            | "case"
            | "when"
            | "then"
            | "else"
            | "end"
            | "cast"
            | "exists"
            | "any"
            | "some"
            | "true"
            | "false"
            | "current_date"
            | "current_timestamp"
            | "sysdate"
            | "systimestamp"
            | "rownum"
            | "level"
            | "connect"
            | "start"
            | "prior"
            | "interval"
            | "extract"
            | "year"
            | "month"
            | "day"
            | "hour"
            | "minute"
            | "second"
            | "over"
            | "partition"
            | "window"
            | "range"
            | "escape"
            | "int"
            | "integer"
            | "bigint"
            | "smallint"
            | "numeric"
            | "decimal"
            | "real"
            | "double"
            | "precision"
            | "text"
            | "varchar"
            | "char"
            | "date"
            | "timestamp"
            | "boolean"
            | "bool"
    )
}

fn is_number(word: &str) -> bool {
    word.chars()
        .next()
        .is_some_and(|first| first.is_ascii_digit())
}

fn cte_names(tokens: &[Token]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut depth = 0i32;
    let mut in_with = false;
    let mut expect_name = false;
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        match token.kind {
            TokenKind::LParen => {
                depth += 1;
                index += 1;
            }
            TokenKind::RParen => {
                depth -= 1;
                index += 1;
            }
            TokenKind::Comma if depth == 0 && in_with => {
                expect_name = true;
                index += 1;
            }
            TokenKind::Word if depth == 0 => {
                if token.lower == "select" {
                    in_with = false;
                } else if token.lower == "with" {
                    in_with = true;
                    expect_name = true;
                } else if token.lower == "recursive" {
                    // `WITH RECURSIVE nombre AS …`
                } else if expect_name {
                    names.insert(token.lower.clone());
                    expect_name = false;
                    let mut next = index + 1;
                    if next < tokens.len() && tokens[next].kind == TokenKind::LParen {
                        let mut inner = 0i32;
                        while next < tokens.len() {
                            if tokens[next].kind == TokenKind::LParen {
                                inner += 1;
                            } else if tokens[next].kind == TokenKind::RParen {
                                inner -= 1;
                                if inner == 0 {
                                    next += 1;
                                    break;
                                }
                            }
                            next += 1;
                        }
                        index = next;
                        continue;
                    }
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    names
}

fn relation_error(dialect: Dialect, name: &str, start: usize) -> DbError {
    match dialect {
        Dialect::Postgres => DbError {
            message: format!("relation \"{}\" does not exist", name.to_ascii_lowercase()),
            code: Some("42P01".into()),
            position: Some(start + 1),
            ..Default::default()
        },
        Dialect::Oracle => DbError {
            message: format!(
                "ORA-00942: la tabla o vista no existe («{}»)",
                name.to_ascii_uppercase()
            ),
            code: Some("ORA-00942".into()),
            position: Some(start + 1),
            ..Default::default()
        },
    }
}

fn column_error(dialect: Dialect, name: &str, start: usize) -> DbError {
    match dialect {
        Dialect::Postgres => DbError {
            message: format!("column \"{}\" does not exist", name.to_ascii_lowercase()),
            code: Some("42703".into()),
            position: Some(start + 1),
            ..Default::default()
        },
        Dialect::Oracle => DbError {
            message: format!(
                "ORA-00904: \"{}\": identificador no válido",
                name.to_ascii_uppercase()
            ),
            code: Some("ORA-00904".into()),
            position: Some(start + 1),
            ..Default::default()
        },
    }
}

fn check_semantics(kind: Kind, tokens: &[Token]) -> Result<Vec<&'static DemoTable>, DbError> {
    let dialect = kind.dialect();
    let tables = kind.tables();
    let ctes = cte_names(tokens);
    let mut known_names: BTreeSet<String> = tables
        .iter()
        .map(|table| table.name.to_ascii_lowercase())
        .collect();
    known_names.insert("dual".into());
    known_names.extend(ctes.iter().cloned());
    let mut aliases = BTreeSet::new();
    let mut referenced: Vec<&'static DemoTable> = Vec::new();

    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        if token.kind == TokenKind::Word && (token.lower == "from" || token.lower == "join") {
            let mut cursor = index + 1;
            while let Some(first) = tokens.get(cursor) {
                if first.kind != TokenKind::Word || is_keyword(&first.lower) {
                    break;
                }
                let mut name = first.lower.clone();
                let mut name_index = cursor;
                if cursor + 2 < tokens.len()
                    && tokens[cursor + 1].kind == TokenKind::Dot
                    && tokens[cursor + 2].kind == TokenKind::Word
                {
                    name = tokens[cursor + 2].lower.clone();
                    name_index = cursor + 2;
                }
                if name != "dual" && !ctes.contains(&name) {
                    match tables
                        .iter()
                        .find(|table| table.name.eq_ignore_ascii_case(&name))
                    {
                        Some(table) => referenced.push(table),
                        None => {
                            return Err(relation_error(
                                dialect,
                                &tokens[name_index].text,
                                tokens[name_index].start,
                            ));
                        }
                    }
                }
                let mut after = name_index + 1;
                if let Some(alias) = tokens.get(after)
                    && alias.kind == TokenKind::Word
                    && !is_keyword(&alias.lower)
                {
                    aliases.insert(alias.lower.clone());
                    after += 1;
                }
                cursor = after;
                if tokens
                    .get(cursor)
                    .is_some_and(|token| token.kind == TokenKind::Comma)
                {
                    cursor += 1;
                    continue;
                }
                break;
            }
            index = cursor.saturating_sub(1);
        }
        index += 1;
    }

    let known_columns: BTreeSet<String> = if referenced.is_empty() {
        tables
            .iter()
            .flat_map(|table| table.columns.iter())
            .map(|column| column.name.to_ascii_lowercase())
            .collect()
    } else {
        referenced
            .iter()
            .flat_map(|table| table.columns.iter())
            .map(|column| column.name.to_ascii_lowercase())
            .collect()
    };

    let mut in_select = false;
    let mut depth = 0i32;
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        match token.kind {
            TokenKind::LParen => {
                depth += 1;
                index += 1;
            }
            TokenKind::RParen => {
                depth -= 1;
                index += 1;
            }
            TokenKind::Word => {
                let word = token.lower.as_str();
                if is_keyword(word) {
                    if depth == 0 && word == "select" {
                        in_select = true;
                    } else if depth == 0 && word == "from" {
                        in_select = false;
                    } else if word == "as"
                        && let Some(alias) = tokens.get(index + 1)
                        && alias.kind == TokenKind::Word
                    {
                        aliases.insert(alias.lower.clone());
                    }
                    index += 1;
                    continue;
                }
                if word.is_empty() || is_number(word) {
                    index += 1;
                    continue;
                }
                if known_names.contains(word) || aliases.contains(word) {
                    index += 1;
                    continue;
                }
                let previous = index
                    .checked_sub(1)
                    .and_then(|position| tokens.get(position));
                let next = tokens.get(index + 1);
                if next.is_some_and(|token| token.kind == TokenKind::LParen) {
                    index += 1;
                    continue;
                }
                if next.is_some_and(|token| token.kind == TokenKind::Dot) {
                    index += 1;
                    continue;
                }
                if previous.is_some_and(|token| token.kind == TokenKind::Dot) {
                    if known_columns.contains(word) {
                        index += 1;
                        continue;
                    }
                    return Err(column_error(dialect, &token.text, token.start));
                }
                if known_columns.contains(word) {
                    index += 1;
                    continue;
                }
                if in_select
                    && previous.is_some_and(|token| expression_end(token, &known_columns, &aliases))
                {
                    aliases.insert(word.to_string());
                    index += 1;
                    continue;
                }
                if !in_select && previous.is_some_and(|token| token.kind == TokenKind::RParen) {
                    aliases.insert(word.to_string());
                    index += 1;
                    continue;
                }
                return Err(column_error(dialect, &token.text, token.start));
            }
            _ => index += 1,
        }
    }
    Ok(referenced)
}

fn expression_end(
    token: &Token,
    known_columns: &BTreeSet<String>,
    aliases: &BTreeSet<String>,
) -> bool {
    match token.kind {
        TokenKind::RParen | TokenKind::Star => true,
        TokenKind::Word => {
            !is_keyword(&token.lower)
                && (known_columns.contains(&token.lower)
                    || aliases.contains(&token.lower)
                    || is_number(&token.lower))
        }
        _ => false,
    }
}

fn count_star_label(tokens: &[Token]) -> Option<String> {
    for (index, token) in tokens.iter().enumerate() {
        if token.kind != TokenKind::Word || token.lower != "count" {
            continue;
        }
        let mut cursor = index + 1;
        if tokens
            .get(cursor)
            .is_some_and(|token| token.kind == TokenKind::LParen)
        {
            cursor += 1;
        }
        if !tokens
            .get(cursor)
            .is_some_and(|token| token.kind == TokenKind::Star)
        {
            continue;
        }
        cursor += 1;
        if !tokens
            .get(cursor)
            .is_some_and(|token| token.kind == TokenKind::RParen)
        {
            continue;
        }
        cursor += 1;
        if tokens.get(cursor).is_some_and(|token| token.lower == "as") {
            cursor += 1;
        }
        if let Some(alias) = tokens.get(cursor)
            && alias.kind == TokenKind::Word
            && !is_keyword(&alias.lower)
        {
            return Some(alias.text.clone());
        }
        return Some("count".into());
    }
    None
}

fn sql_limit(tokens: &[Token]) -> Option<usize> {
    for (index, token) in tokens.iter().enumerate() {
        if token.kind != TokenKind::Word {
            continue;
        }
        if token.lower == "limit"
            && let Some(value) = tokens
                .get(index + 1)
                .filter(|token| token.kind == TokenKind::Word)
                .and_then(|token| token.lower.parse::<usize>().ok())
        {
            return Some(value.max(1));
        }
        if token.lower == "fetch" {
            let mut cursor = index + 1;
            if tokens
                .get(cursor)
                .is_some_and(|token| token.lower == "first" || token.lower == "next")
            {
                cursor += 1;
            }
            if let Some(value) = tokens
                .get(cursor)
                .filter(|token| token.kind == TokenKind::Word)
                .and_then(|token| token.lower.parse::<usize>().ok())
            {
                return Some(value.max(1));
            }
        }
    }
    None
}

fn selected_columns(tokens: &[Token], table: &DemoTable) -> Vec<(usize, String)> {
    let all = || -> Vec<(usize, String)> {
        table
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| (index, column.name.to_string()))
            .collect()
    };
    let mut depth = 0i32;
    let mut collecting = false;
    let mut items: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for token in tokens {
        match token.kind {
            TokenKind::LParen => {
                depth += 1;
                if collecting {
                    return all();
                }
                continue;
            }
            TokenKind::RParen => {
                depth -= 1;
                if collecting {
                    return all();
                }
                continue;
            }
            _ => {}
        }
        if depth != 0 {
            continue;
        }
        if token.kind == TokenKind::Word && token.lower == "select" {
            collecting = true;
            continue;
        }
        if token.kind == TokenKind::Word && token.lower == "from" && collecting {
            break;
        }
        if !collecting {
            continue;
        }
        match token.kind {
            TokenKind::Star => return all(),
            TokenKind::Comma => items.push(std::mem::take(&mut current)),
            TokenKind::Word => current.push(token.lower.clone()),
            TokenKind::Dot => current.push(".".into()),
            _ => return all(),
        }
    }
    if collecting {
        items.push(current);
    }
    if items.is_empty() {
        return all();
    }
    let mut selected = Vec::new();
    for item in &items {
        let words: Vec<&String> = item
            .iter()
            .filter(|word| word.as_str() != "." && word.as_str() != "distinct")
            .collect();
        if words.is_empty() {
            return all();
        }
        let alias_position = words.iter().position(|word| word.as_str() == "as");
        let (base_words, alias) = match alias_position {
            Some(position) if position + 1 < words.len() => {
                (&words[..position], Some(words[position + 1].as_str()))
            }
            Some(_) => return all(),
            None => (&words[..], None),
        };
        let base = base_words
            .iter()
            .rev()
            .find(|word| !is_keyword(word) && !is_number(word))
            .and_then(|word| {
                table
                    .columns
                    .iter()
                    .position(|column| column.name.eq_ignore_ascii_case(word))
            });
        let Some(index) = base else { return all() };
        let label = alias.map(str::to_string).or_else(|| {
            let last = words.last()?;
            let is_column = table
                .columns
                .iter()
                .any(|column| column.name.eq_ignore_ascii_case(last));
            (!is_keyword(last) && !is_number(last) && !is_column).then(|| (*last).clone())
        });
        selected.push((
            index,
            label.unwrap_or_else(|| table.columns[index].name.to_string()),
        ));
    }
    if selected.is_empty() { all() } else { selected }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_db(id: &str) -> DemoDb {
        DemoDb::for_spec(
            &connections()
                .into_iter()
                .find(|spec| spec.info.id == id)
                .unwrap(),
        )
    }

    fn run(db: &mut DemoDb, sql: &str, limit: usize) -> Result<ResultSet, DbError> {
        db.run(sql, limit, &Arc::new(AtomicBool::new(false)))
    }

    #[test]
    fn conexiones_ficticias() {
        let connections = connections();
        assert_eq!(connections.len(), 3);
        assert!(connections.iter().all(|spec| spec.host == "demo"));
        assert_eq!(connections[0].info.label, "Tienda online · DESARROLLO");
        assert_eq!(connections[1].info.env, EnvKind::Prod);
        assert_eq!(connections[2].info.dialect, Dialect::Oracle);
        assert_eq!(connections[2].info.target, "demo/CATASTRO");
    }

    #[test]
    fn esquemas_realistas() {
        for id in ["demo-ventas", "demo-clientes", "demo-catastro"] {
            let mut db = demo_db(id);
            let schema = db.schema().unwrap();
            assert!(
                (8..=12).contains(&schema.tables.len()),
                "{id}: {} tablas",
                schema.tables.len()
            );
            assert!(
                schema
                    .tables
                    .windows(2)
                    .all(|pair| pair[0].name <= pair[1].name)
            );
            assert!(schema.tables.iter().all(|table| !table.columns.is_empty()));
            assert!(
                schema
                    .tables
                    .iter()
                    .any(|table| table.columns.iter().any(|column| column.primary_key))
            );
            assert!(schema.tables.iter().any(|table| table.comment.is_some()));
        }
        let schema = demo_db("demo-catastro").schema().unwrap();
        assert!(schema.tables.iter().all(|table| table.schema == "CATASTRO"));
        assert!(
            schema
                .tables
                .iter()
                .all(|table| table.name.chars().all(|c| !c.is_lowercase()))
        );
        let schema = demo_db("demo-ventas").schema().unwrap();
        assert!(schema.tables.iter().all(|table| table.schema == "public"));
    }

    #[test]
    fn consultas_con_datos() {
        let mut db = demo_db("demo-ventas");
        let result = run(&mut db, "SELECT * FROM clientes ORDER BY nombre", 500).unwrap();
        assert_eq!(result.columns.len(), 7);
        assert_eq!(result.rows.len(), 5);
        assert_eq!(result.rows[0][1].as_deref(), Some("Talleres Rivas S.L."));
        assert!(!result.truncated);

        let result = run(&mut db, "SELECT nombre, email FROM clientes LIMIT 2", 500).unwrap();
        assert_eq!(
            result
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["nombre", "email"]
        );
        assert_eq!(result.rows.len(), 2);
        assert!(!result.truncated);

        let result = run(
            &mut db,
            "select c.nombre as cliente, c.nif from clientes c where c.activo = true",
            3,
        )
        .unwrap();
        assert_eq!(result.columns[0].name, "cliente");
        assert_eq!(result.rows.len(), 3);
        assert!(result.truncated);

        let result = run(&mut db, "SELECT COUNT(*) FROM pedidos", 100).unwrap();
        assert_eq!(result.columns[0].name, "count");
        assert_eq!(result.rows, vec![vec![Some("5".into())]]);

        let result = run(&mut db, "SELECT * FROM productos", 2).unwrap();
        assert_eq!(result.rows.len(), 2);
        assert!(result.truncated);

        let result = run(&mut db, "SELECT * FROM facturas", 500).unwrap();
        assert_eq!(result.rows.len(), 4);
        assert_eq!(result.rows[0][2].as_deref(), Some("FAC-2026-0001"));
    }

    #[test]
    fn datos_deterministas() {
        let mut first = demo_db("demo-ventas");
        let mut second = demo_db("demo-ventas");
        let a = run(&mut first, "SELECT * FROM pedidos", 100).unwrap();
        let b = run(&mut second, "SELECT * FROM pedidos", 100).unwrap();
        assert_eq!(a, b);
        assert!(a.rows.iter().all(|row| row.len() == a.columns.len()));
    }

    #[test]
    fn errores_con_posicion() {
        let mut db = demo_db("demo-ventas");
        let error = run(&mut db, "SELECT * FROM tabla_falsa", 10).unwrap_err();
        assert_eq!(error.position, Some(15));
        assert_eq!(error.code.as_deref(), Some("42P01"));
        assert_eq!(error.message, "relation \"tabla_falsa\" does not exist");
        assert!(!error.connection);

        let error = run(&mut db, "SELECT columna_inexistente FROM clientes", 10).unwrap_err();
        assert_eq!(error.position, Some(8));
        assert_eq!(error.code.as_deref(), Some("42703"));
        assert_eq!(
            error.message,
            "column \"columna_inexistente\" does not exist"
        );

        let error = run(&mut db, "SELECT * FROM clientes WHERE nope = 1", 10).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("42703"));
        assert_eq!(error.position, Some(30));

        let mut oracle = demo_db("demo-catastro");
        let error = run(&mut oracle, "SELECT * FROM NO_EXISTE", 10).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("ORA-00942"));
        assert!(error.message.contains("ORA-00942"));
        let error = run(&mut oracle, "SELECT MALA FROM INMUEBLES", 10).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("ORA-00904"));
        assert_eq!(error.position, Some(8));
    }

    #[test]
    fn oracle_y_genericas() {
        let mut db = demo_db("demo-catastro");
        let result = run(&mut db, "SELECT * FROM INMUEBLES", 2).unwrap();
        assert_eq!(result.columns[0].name, "ID");
        assert_eq!(result.columns.len(), 8);
        assert_eq!(result.rows.len(), 2);
        assert!(result.truncated);
        assert_eq!(result.rows[0][1].as_deref(), Some("2807901VK4721A0001XY"));

        let result = run(&mut db, "SELECT * FROM VALORACIONES", 10).unwrap();
        assert_eq!(result.rows.len(), 4);
        assert_eq!(result.rows[0][3].as_deref(), Some("205000.00"));

        let result = run(&mut db, "SELECT * FROM TIPOS_INMUEBLE", 10).unwrap();
        assert_eq!(result.rows.len(), 3);

        let result = run(&mut db, "SELECT * FROM dual", 10).unwrap();
        assert_eq!(result.columns[0].name, "DUMMY");
        assert_eq!(result.rows[0][0].as_deref(), Some("X"));

        let mut ventas = demo_db("demo-ventas");
        let result = run(&mut ventas, "SELECT nombre FROM almacenes", 10).unwrap();
        assert_eq!(result.rows.len(), 5);
        assert_eq!(result.columns[0].name, "nombre");
        assert!(result.rows.iter().all(|row| row[0].is_some()));

        let result = run(&mut ventas, "SELECT 1 + 1", 10).unwrap();
        assert_eq!(result.columns[0].name, "resultado");
        assert_eq!(result.rows.len(), 5);
    }

    #[test]
    fn consultas_con_cte_y_join() {
        let mut db = demo_db("demo-ventas");
        let result = run(
            &mut db,
            "WITH activos AS (SELECT * FROM clientes) SELECT nombre FROM activos",
            10,
        )
        .unwrap();
        assert_eq!(result.rows.len(), 5);
        let result = run(
            &mut db,
            "SELECT c.nombre FROM clientes c JOIN pedidos p ON p.cliente_id = c.id",
            10,
        )
        .unwrap();
        assert_eq!(result.columns[0].name, "nombre");
        assert_eq!(result.rows.len(), 5);
    }

    #[test]
    fn cancelacion() {
        let mut db = demo_db("demo-ventas");
        let error = db
            .run(
                "SELECT * FROM clientes",
                10,
                &Arc::new(AtomicBool::new(true)),
            )
            .unwrap_err();
        assert_eq!(error.message, "Consulta cancelada");
    }

    #[test]
    fn versiones() {
        assert!(
            demo_db("demo-ventas")
                .check()
                .unwrap()
                .contains("PostgreSQL")
        );
        assert!(demo_db("demo-catastro").check().unwrap().contains("Oracle"));
    }
}
