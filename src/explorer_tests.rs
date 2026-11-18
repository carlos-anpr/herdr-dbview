//! Pruebas del agente explorador con `Database` y modelo de lenguaje falsos. (encargo tests)
//!
//! Nada de aquí toca red ni bases reales: `FakeDb` pasa cada SQL por
//! `sqlguard::check` (igual que `pg.rs`/`oracle.rs`) y el guion decide la
//! respuesta del LLM paso a paso. Hay dos dominios sintéticos (tasaciones y
//! biblioteca) para comprobar que el agente no depende de un negocio concreto.

use crate::config::AiConfig;
use crate::db::{Database, sqlguard};
use crate::explorer::{Ctx, Event, MAX_STEPS, Outcome, explore_with};
use crate::model::*;
use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const HOY: &str = "2026-11-17";

// ---------------------------------------------------------------------------
// Catálogo sintético
// ---------------------------------------------------------------------------

fn col(nombre: &str, tipo: &str, pk: bool) -> ColInfo {
    ColInfo {
        name: nombre.into(),
        type_name: tipo.into(),
        nullable: !pk,
        primary_key: pk,
        ..Default::default()
    }
}

fn tabla(esquema: &str, nombre: &str, columnas: Vec<ColInfo>) -> TableInfo {
    TableInfo {
        schema: esquema.into(),
        name: nombre.into(),
        kind: TableKind::Table,
        columns: columnas,
        comment: None,
        ..Default::default()
    }
}

/// Todo lo que `Ctx` necesita, con la vida atada al propio fixture.
struct Fixture {
    cfg: AiConfig,
    schema: Schema,
    summaries: HashMap<String, String>,
    overview: String,
    relations: HashMap<String, Vec<String>>,
    candidates: Vec<String>,
    notes: String,
    pinned: Vec<String>,
    current_sql: Option<String>,
    cancel: Arc<AtomicBool>,
    today: String,
    engine: String,
    question: String,
}

impl Fixture {
    fn nuevo(schema: Schema, question: &str) -> Self {
        let summaries: HashMap<String, String> = schema
            .tables
            .iter()
            .map(|t| {
                (
                    t.qualified(),
                    format!("datos de {} con {} columnas", t.name, t.columns.len()),
                )
            })
            .collect();
        let candidates = schema.tables.iter().map(TableInfo::qualified).collect();
        Self {
            cfg: AiConfig::default(),
            schema,
            summaries,
            overview: "Catálogo sintético de pruebas.".into(),
            relations: HashMap::new(),
            candidates,
            notes: String::new(),
            pinned: Vec::new(),
            current_sql: None,
            cancel: Arc::new(AtomicBool::new(false)),
            today: HOY.into(),
            engine: "FakeDB 1.0".into(),
            question: question.into(),
        }
    }

    fn ctx(&self, dialect: Dialect) -> Ctx<'_> {
        Ctx {
            cfg: &self.cfg,
            dialect,
            engine: &self.engine,
            today: &self.today,
            question: &self.question,
            schema: &self.schema,
            summaries: &self.summaries,
            overview: &self.overview,
            relations: &self.relations,
            candidates: &self.candidates,
            notes: &self.notes,
            pinned: &self.pinned,
            current_sql: self.current_sql.as_deref(),
            cancel: &self.cancel,
        }
    }
}

fn fixture_tasaciones() -> Fixture {
    let solicitudes = tabla(
        "ventas",
        "solicitudes",
        vec![
            col("numexp", "int8", true),
            col("codcli", "int4", false),
            col("fchenc", "date", false),
            col("fchrem", "date", false),
            col("codest", "varchar", false),
        ],
    );
    let estadisticas = tabla(
        "ventas",
        "estadisticas_tasaciones",
        vec![
            col("id", "int8", true),
            col("numexp", "int8", false),
            col("importe", "numeric", false),
        ],
    );
    let mut f = Fixture::nuevo(
        Schema {
            tables: vec![solicitudes, estadisticas],
        },
        "¿Cuántas solicitudes se han recibido en la última semana?",
    );
    f.relations.insert(
        "ventas.solicitudes".into(),
        vec!["numexp = ventas.estadisticas_tasaciones.numexp (fk)".into()],
    );
    f
}

/// Catálogo con nombres tal como los da Oracle (mayúsculas + una tabla mixta).
fn fixture_oracle() -> Fixture {
    let solicitudes = tabla(
        "VENTAS_APP",
        "SOLICITUDES",
        vec![
            col("NUMEXP", "NUMBER", true),
            col("CODCLI", "NUMBER", false),
            col("FCHENC", "DATE", false),
            col("FCHREM", "DATE", false),
            col("CODEST", "VARCHAR2", false),
        ],
    );
    let antiguas = tabla(
        "VENTAS_APP",
        "SolicitudesAntiguas",
        vec![col("NUMEXP", "NUMBER", true)],
    );
    Fixture::nuevo(
        Schema {
            tables: vec![solicitudes, antiguas],
        },
        "¿Cuántas solicitudes hay por estado?",
    )
}

/// Dominio ajeno a las tasaciones: biblioteca.
fn fixture_biblioteca() -> Fixture {
    let prestamos = tabla(
        "biblioteca",
        "prestamos",
        vec![
            col("id", "int8", true),
            col("libro_id", "int8", false),
            col("socio_id", "int8", false),
            col("fecha_prestamo", "date", false),
            col("fecha_devolucion", "date", false),
            col("estado", "varchar", false),
        ],
    );
    let libros = tabla(
        "biblioteca",
        "libros",
        vec![
            col("id", "int8", true),
            col("titulo", "varchar", false),
            col("autor", "varchar", false),
        ],
    );
    let socios = tabla(
        "biblioteca",
        "socios",
        vec![col("id", "int8", true), col("nombre", "varchar", false)],
    );
    Fixture::nuevo(
        Schema {
            tables: vec![prestamos, libros, socios],
        },
        "¿Qué préstamos se han hecho en la última semana?",
    )
}

// ---------------------------------------------------------------------------
// Database falso
// ---------------------------------------------------------------------------

type Respuesta = Result<ResultSet, DbError>;
type Respondedor = Box<dyn FnMut(&str, usize) -> Respuesta + Send>;

/// `Database` falso: guardia léxico real, respuestas por SQL y registro de todo
/// lo recibido. `run` devuelve `DbError` con el motivo si el guardia rechaza,
/// sin ejecutar nada, como `pg.rs`/`oracle.rs`.
struct FakeDb {
    dialect: Dialect,
    responde: Respondedor,
    /// Todas las SQL recibidas, incluidas las rechazadas por el guardia.
    recibidas: Vec<String>,
    /// SQL que pasaron el guardia y se ejecutaron con éxito.
    ejecutadas: Vec<String>,
    /// SQL rechazadas por `sqlguard::check`, con el motivo.
    rechazadas: Vec<(String, String)>,
}

impl FakeDb {
    fn new(
        dialect: Dialect,
        responde: impl FnMut(&str, usize) -> Respuesta + Send + 'static,
    ) -> Self {
        Self {
            dialect,
            responde: Box::new(responde),
            recibidas: Vec::new(),
            ejecutadas: Vec::new(),
            rechazadas: Vec::new(),
        }
    }

    fn recibio(&self, trozo: &str) -> bool {
        let trozo = trozo.to_lowercase();
        self.recibidas
            .iter()
            .any(|sql| sql.to_lowercase().contains(&trozo))
    }

    fn veces(&self, trozo: &str) -> usize {
        let trozo = trozo.to_lowercase();
        self.recibidas
            .iter()
            .filter(|sql| sql.to_lowercase().contains(&trozo))
            .count()
    }
}

impl Database for FakeDb {
    fn dialect(&self) -> Dialect {
        self.dialect
    }

    fn check(&mut self) -> Result<String, DbError> {
        Ok("FakeDB 1.0".into())
    }

    fn schema(&mut self) -> Result<Schema, DbError> {
        Err(DbError::new("el catálogo lo pone la prueba"))
    }

    fn run(
        &mut self,
        sql: &str,
        limit: usize,
        cancel: &Arc<AtomicBool>,
    ) -> Result<ResultSet, DbError> {
        self.recibidas.push(sql.to_string());
        if cancel.load(Ordering::Relaxed) {
            return Err(DbError::new("Consulta cancelada"));
        }
        let limpia = match sqlguard::check(self.dialect, sql) {
            Ok(limpia) => limpia,
            Err(motivo) => {
                self.rechazadas.push((sql.to_string(), motivo.clone()));
                return Err(DbError::new(motivo));
            }
        };
        let mut rs = (self.responde)(&limpia, limit)?;
        if rs.rows.len() > limit {
            rs.rows.truncate(limit);
            rs.truncated = true;
        }
        self.ejecutadas.push(limpia);
        Ok(rs)
    }
}

fn resultado(cabeceras: &[&str], filas: &[&[&str]]) -> ResultSet {
    ResultSet {
        columns: cabeceras
            .iter()
            .map(|n| ColumnMeta {
                name: (*n).to_string(),
                type_name: "text".into(),
            })
            .collect(),
        rows: filas
            .iter()
            .map(|fila| fila.iter().map(|v| Some((*v).to_string())).collect())
            .collect(),
        truncated: false,
        elapsed_ms: 2,
    }
}

fn respuesta_total(n: usize) -> Respuesta {
    let valor = n.to_string();
    Ok(resultado(&["total"], &[&[valor.as_str()]]))
}

fn respuesta_rango(ultimos_7: &str) -> Respuesta {
    Ok(resultado(
        &[
            "minimo",
            "maximo",
            "nulos",
            "ultimos_7_dias",
            "ultimos_30_dias",
        ],
        &[&["2025-09-01", "2026-11-16", "0", ultimos_7, "42"]],
    ))
}

fn es_recuento(sql: &str) -> bool {
    sql.trim_start()
        .to_lowercase()
        .starts_with("select count(*) as total from (")
}

fn responder_solicitudes(sql: &str, _limit: usize) -> Respuesta {
    let s = sql.to_lowercase();
    if es_recuento(sql) {
        return respuesta_total(if s.contains("fchrem") { 8 } else { 0 });
    }
    if s.contains("min(") {
        return respuesta_rango("8");
    }
    if s.contains("where fchenc") {
        return Ok(resultado(&["numexp"], &[]));
    }
    if s.contains("where fchrem") {
        return Ok(resultado(
            &["numexp", "fchrem"],
            &[
                &["9001", "2026-11-10"],
                &["9002", "2026-11-11"],
                &["9003", "2026-11-12"],
                &["9004", "2026-11-13"],
                &["9005", "2026-11-14"],
                &["9006", "2026-11-15"],
                &["9007", "2026-11-16"],
                &["9008", "2026-11-16"],
            ],
        ));
    }
    Ok(resultado(&["numexp", "codest"], &[&["1", "P"]]))
}

fn responder_basico(sql: &str, _limit: usize) -> Respuesta {
    let s = sql.to_lowercase();
    if es_recuento(sql) {
        return respuesta_total(3);
    }
    if s.contains("min(") {
        return respuesta_rango("3");
    }
    if s.contains("group by") {
        return Ok(resultado(&["valor", "n"], &[&["P", "12"], &["T", "3"]]));
    }
    Ok(resultado(
        &["numexp", "fchrem", "codest"],
        &[
            &["1", "2026-11-10", "P"],
            &["2", "2026-11-11", "P"],
            &["3", "2026-11-12", "T"],
        ],
    ))
}

fn responder_oracle(sql: &str, _limit: usize) -> Respuesta {
    let s = sql.to_lowercase();
    if es_recuento(sql) {
        return respuesta_total(2);
    }
    if s.contains("min(") {
        return respuesta_rango("2");
    }
    if s.contains("group by") {
        return Ok(resultado(&["valor", "n"], &[&["P", "7"]]));
    }
    Ok(resultado(
        &["numexp", "codest"],
        &[&["1", "P"], &["2", "P"]],
    ))
}

fn responder_biblioteca(sql: &str, _limit: usize) -> Respuesta {
    let s = sql.to_lowercase();
    if es_recuento(sql) {
        return respuesta_total(5);
    }
    if s.contains("min(") {
        return respuesta_rango("5");
    }
    Ok(resultado(
        &["id", "titulo", "fecha_prestamo"],
        &[
            &["11", "El Quijote", "2026-11-12"],
            &["12", "La Regenta", "2026-11-13"],
            &["13", "Fortunata y Jacinta", "2026-11-14"],
            &["14", "Misericordia", "2026-11-15"],
            &["15", "Tristana", "2026-11-16"],
        ],
    ))
}

// ---------------------------------------------------------------------------
// LLM falso, eventos y lanzadera
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Ev {
    Step(String),
    Tables(Vec<String>),
    Sql(String),
}

impl From<Event> for Ev {
    fn from(e: Event) -> Self {
        match e {
            Event::Step(s) => Ev::Step(s),
            Event::Tables(t) => Ev::Tables(t),
            Event::Sql(s) => Ev::Sql(s),
        }
    }
}

/// Guion: una respuesta JSON por llamada, en orden, y el `user` de cada turno.
struct Guion {
    respuestas: VecDeque<String>,
    prompts: Vec<String>,
    llamadas: usize,
}

impl Guion {
    fn nuevo(respuestas: Vec<String>) -> Self {
        Self {
            respuestas: respuestas.into(),
            prompts: Vec::new(),
            llamadas: 0,
        }
    }

    fn llm(
        &mut self,
    ) -> impl FnMut(&str, &str, &mut dyn FnMut(&str)) -> Result<String, String> + '_ {
        move |_sistema, user, on_text| {
            self.prompts.push(user.to_string());
            self.llamadas += 1;
            let Some(respuesta) = self.respuestas.pop_front() else {
                panic!(
                    "el guion se quedó sin respuestas en la llamada {}",
                    self.llamadas
                );
            };
            on_text(&respuesta);
            Ok(respuesta)
        }
    }
}

struct Corrida {
    resultado: Result<Outcome, String>,
    eventos: Vec<Ev>,
    prompts: Vec<String>,
    llamadas: usize,
}

impl Corrida {
    fn ok(&self) -> &Outcome {
        match &self.resultado {
            Ok(outcome) => outcome,
            Err(e) => panic!("se esperaba un Outcome y llegó Err({e})"),
        }
    }

    fn err(&self) -> &str {
        match &self.resultado {
            Err(e) => e,
            Ok(outcome) => panic!("se esperaba Err y llegó el SQL {:?}", outcome.sql),
        }
    }

    fn pasos(&self) -> Vec<&str> {
        self.eventos
            .iter()
            .filter_map(|e| match e {
                Ev::Step(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    fn tiene_paso(&self, trozo: &str) -> bool {
        self.pasos().iter().any(|s| s.contains(trozo))
    }

    fn pasos_con(&self, trozo: &str) -> usize {
        self.pasos().iter().filter(|s| s.contains(trozo)).count()
    }

    fn tablas(&self) -> Vec<String> {
        self.eventos
            .iter()
            .filter_map(|e| match e {
                Ev::Tables(t) => Some(t.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn tiene_tabla(&self, tabla: &str) -> bool {
        self.tablas().iter().any(|t| t == tabla)
    }

    fn sqls(&self) -> Vec<&str> {
        self.eventos
            .iter()
            .filter_map(|e| match e {
                Ev::Sql(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }
}

fn lote(respuestas: &[&str]) -> Vec<String> {
    respuestas.iter().map(|r| (*r).to_string()).collect()
}

fn ejecutar(ctx: &Ctx, db: &mut dyn Database, respuestas: Vec<String>) -> Corrida {
    let mut guion = Guion::nuevo(respuestas);
    let mut eventos = Vec::new();
    let resultado = {
        let mut on_event = |e: Event| eventos.push(Ev::from(e));
        let mut llm = guion.llm();
        explore_with(ctx, db, &mut on_event, &mut llm)
    };
    Corrida {
        resultado,
        eventos,
        prompts: std::mem::take(&mut guion.prompts),
        llamadas: guion.llamadas,
    }
}

// ---------------------------------------------------------------------------
// Casos obligatorios
// ---------------------------------------------------------------------------

#[test]
fn caso_1_cero_filas_cambia_la_columna_de_fecha() {
    let f = fixture_tasaciones();
    let ctx = f.ctx(Dialect::Postgres);
    let mut db = FakeDb::new(Dialect::Postgres, responder_solicitudes);
    let c = ejecutar(
        &ctx,
        &mut db,
        lote(&[
            r#"{"accion":"probar","sql":"SELECT numexp FROM ventas.solicitudes WHERE fchenc >= CURRENT_DATE - 7"}"#,
            r#"{"accion":"final","sql":"SELECT numexp FROM ventas.solicitudes WHERE fchenc >= CURRENT_DATE - 7","explicacion":"con fchenc no hay nada","aprendido":[]}"#,
            r#"{"accion":"rango","tabla":"ventas.solicitudes","columna":"fchrem"}"#,
            r#"{"accion":"probar","sql":"SELECT numexp, fchrem FROM ventas.solicitudes WHERE fchrem >= CURRENT_DATE - 7"}"#,
            r#"{"accion":"final","sql":"SELECT numexp, fchrem FROM ventas.solicitudes WHERE fchrem >= CURRENT_DATE - 7","explicacion":"la recepción real está en fchrem; fchenc no trae datos de la semana.","aprendido":["la fecha de recepción real es fchrem"]}"#,
        ]),
    );
    let o = c.ok();
    assert_eq!(
        o.sql,
        "SELECT numexp, fchrem FROM ventas.solicitudes WHERE fchrem >= CURRENT_DATE - 7"
    );
    assert!(!o.sql.contains("fchenc"), "entregó la consulta de 0 filas");
    assert_eq!(c.llamadas, 5);
    // El aviso por 0 filas llega al modelo en el turno siguiente...
    assert!(
        c.prompts[1].contains("Resultado vacío o 0"),
        "prompt: {}",
        c.prompts[1]
    );
    // ...y el intento de entregar el resultado vacío se rechaza.
    assert!(c.prompts[2].contains("Ese SQL devolvió 0 filas"));
    // Investigó el rango de la otra columna y encontró 8 filas.
    assert!(c.prompts[3].contains("Rango de solicitudes.fchrem"));
    assert!(
        c.prompts[4].contains("8 filas en total"),
        "{}",
        c.prompts[4]
    );
    assert!(db.recibio("min(fchrem)"), "historial: {:?}", db.recibidas);
    assert!(db.recibio("where fchenc"));
    assert!(db.recibio("where fchrem"));
    assert!(db.ejecutadas.iter().any(|s| s.contains("fchrem")));
}

#[test]
fn caso_2_final_sin_probar_se_rechaza() {
    let f = fixture_tasaciones();
    let ctx = f.ctx(Dialect::Postgres);
    let mut db = FakeDb::new(Dialect::Postgres, responder_basico);
    let sql = "SELECT numexp, codest FROM ventas.solicitudes WHERE codest = 'P'";
    let c = ejecutar(
        &ctx,
        &mut db,
        lote(&[
            r#"{"accion":"final","sql":"SELECT numexp, codest FROM ventas.solicitudes WHERE codest = 'P'","explicacion":"pendientes"}"#,
            r#"{"accion":"probar","sql":"SELECT numexp, codest FROM ventas.solicitudes WHERE codest = 'P'"}"#,
            r#"{"accion":"final","sql":"SELECT numexp, codest FROM ventas.solicitudes WHERE codest = 'P'","explicacion":"pendientes"}"#,
        ]),
    );
    let o = c.ok();
    assert_eq!(o.sql, sql);
    assert_eq!(c.llamadas, 3);
    assert!(
        c.prompts[1].contains("tienes que probar"),
        "prompt: {}",
        c.prompts[1]
    );
    // El `final` sin probar no ejecutó nada: solo la prueba posterior (consulta + recuento).
    assert_eq!(db.recibidas.len(), 2);
    assert!(o.lessons.is_empty());
}

#[test]
fn caso_3_solo_lectura_nunca_se_ejecuta() {
    let f = fixture_tasaciones();
    let ctx = f.ctx(Dialect::Postgres);
    let mut db = FakeDb::new(Dialect::Postgres, responder_basico);
    let borrado = "DELETE FROM ventas.solicitudes WHERE numexp = 1";
    let inyeccion = "SELECT numexp FROM ventas.solicitudes; DROP TABLE ventas.solicitudes";
    let bloqueo = "SELECT numexp FROM ventas.solicitudes FOR UPDATE";
    let buena = "SELECT numexp FROM ventas.solicitudes WHERE codest = 'P'";
    let c = ejecutar(
        &ctx,
        &mut db,
        lote(&[
            r#"{"accion":"probar","sql":"DELETE FROM ventas.solicitudes WHERE numexp = 1"}"#,
            r#"{"accion":"probar","sql":"SELECT numexp FROM ventas.solicitudes; DROP TABLE ventas.solicitudes"}"#,
            r#"{"accion":"probar","sql":"SELECT numexp FROM ventas.solicitudes FOR UPDATE"}"#,
            r#"{"accion":"final","sql":"DELETE FROM ventas.solicitudes WHERE numexp = 1","explicacion":"x"}"#,
            r#"{"accion":"probar","sql":"SELECT numexp FROM ventas.solicitudes WHERE codest = 'P'"}"#,
            r#"{"accion":"final","sql":"SELECT numexp FROM ventas.solicitudes WHERE codest = 'P'","explicacion":"pendientes"}"#,
        ]),
    );
    let o = c.ok();
    assert_eq!(o.sql, buena);
    assert_eq!(db.rechazadas.len(), 3, "rechazadas: {:?}", db.rechazadas);
    let motivos = db
        .rechazadas
        .iter()
        .map(|(_, m)| m.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(motivos.contains("SELECT o WITH"), "{motivos}");
    assert!(motivos.contains("una sentencia"), "{motivos}");
    // `FOR UPDATE` cae por la palabra prohibida UPDATE antes que por el bloqueo.
    assert!(motivos.contains("UPDATE"), "{motivos}");
    // El `final` con el DELETE también se rechaza y se avisa al modelo.
    assert!(c.prompts[4].contains("dio error al probarlo"));
    for mala in [borrado, inyeccion, bloqueo] {
        assert!(db.recibio(mala), "no llegó al Database: {mala}");
        assert!(
            !db.ejecutadas.iter().any(|s| s.eq_ignore_ascii_case(mala)),
            "se ejecutó: {mala}"
        );
    }
    // Todo lo que se ejecutó con éxito es de lectura y pasa el guardia.
    for sql in &db.ejecutadas {
        assert!(sqlguard::check(Dialect::Postgres, sql).is_ok(), "{sql}");
        let s = sql.to_lowercase();
        assert!(
            !s.contains("delete") && !s.contains("drop") && !s.contains("for update"),
            "se ejecutó algo que no es lectura: {sql}"
        );
    }
    assert!(!o.sql.to_lowercase().contains("delete"));
}

#[test]
fn caso_4_json_tolerante_y_tres_invalidas() {
    // (a) Prosa alrededor, vallas ```json y una respuesta inválida suelta: se tolera.
    let f = fixture_tasaciones();
    let ctx = f.ctx(Dialect::Postgres);
    let mut db = FakeDb::new(Dialect::Postgres, responder_basico);
    let c = ejecutar(
        &ctx,
        &mut db,
        lote(&[
            "No estoy seguro; primero miro la tabla.\n```json\n{\"accion\":\"describir\",\"tabla\":\"ventas.solicitudes\"}\n```\nEso es todo.",
            "Esto no es JSON: no puedo responder.",
            "Ahora sí: {\"accion\":\"probar\",\"sql\":\"SELECT numexp FROM ventas.solicitudes WHERE codest = 'P'\"} ¿vale?",
            "```json\n{\"accion\":\"final\",\"sql\":\"SELECT numexp FROM ventas.solicitudes WHERE codest = 'P'\",\"explicacion\":\"estado P\"}\n```\nListo.",
        ]),
    );
    let o = c.ok();
    assert!(o.sql.contains("codest = 'P'"));
    assert_eq!(c.llamadas, 4);
    assert!(c.tiene_paso("mirando las columnas de ventas.solicitudes"));

    // (b) Tres respuestas inválidas sin nada probado: Err.
    let mut db = FakeDb::new(Dialect::Postgres, responder_basico);
    let c = ejecutar(
        &ctx,
        &mut db,
        lote(&["no sé", "sigo sin saber", "??? ni idea"]),
    );
    assert!(
        c.err().contains("no llegó a una consulta probada"),
        "{}",
        c.err()
    );
    assert_eq!(c.llamadas, 3);
    assert!(db.recibidas.is_empty());

    // (c) Tres inválidas después de una consulta buena: se devuelve la última buena.
    let mut db = FakeDb::new(Dialect::Postgres, responder_basico);
    let c = ejecutar(
        &ctx,
        &mut db,
        lote(&[
            r#"{"accion":"probar","sql":"SELECT numexp FROM ventas.solicitudes WHERE codest = 'P'"}"#,
            "basura uno",
            "basura dos",
            "basura tres",
        ]),
    );
    let o = c.ok();
    assert_eq!(c.llamadas, 4);
    assert!(
        o.explanation.contains("Se agotaron los pasos"),
        "{}",
        o.explanation
    );
    assert!(o.sql.contains("codest = 'P'"));
}

#[test]
fn caso_5_accion_repetida_se_avisa() {
    let f = fixture_tasaciones();
    let ctx = f.ctx(Dialect::Postgres);
    let mut db = FakeDb::new(Dialect::Postgres, responder_basico);
    let c = ejecutar(
        &ctx,
        &mut db,
        lote(&[
            r#"{"accion":"describir","tabla":"ventas.solicitudes"}"#,
            r#"{"accion":"describir","tabla":"ventas.solicitudes"}"#,
            r#"{"accion":"probar","sql":"SELECT numexp FROM ventas.solicitudes"}"#,
            r#"{"accion":"final","sql":"SELECT numexp FROM ventas.solicitudes","explicacion":"todas"}"#,
        ]),
    );
    let o = c.ok();
    assert_eq!(o.sql, "SELECT numexp FROM ventas.solicitudes");
    assert_eq!(c.llamadas, 4);
    assert!(
        c.prompts[2].contains("Ya hiciste exactamente esto"),
        "prompt: {}",
        c.prompts[2]
    );
    // La acción repetida no se vuelve a ejecutar ni emite otro paso.
    assert_eq!(c.pasos_con("mirando las columnas"), 1);
    assert_eq!(db.veces("select * from ventas.solicitudes"), 1);
}

#[test]
fn caso_6_tope_de_pasos() {
    // (a) Con una consulta buena: devuelve la última que funcionó con filas.
    let f = fixture_tasaciones();
    let ctx = f.ctx(Dialect::Postgres);
    let mut db = FakeDb::new(Dialect::Postgres, responder_basico);
    let buena = "SELECT numexp FROM ventas.solicitudes";
    let mut respuestas =
        lote(&[r#"{"accion":"probar","sql":"SELECT numexp FROM ventas.solicitudes"}"#]);
    for i in 0..(MAX_STEPS - 1) {
        respuestas.push(format!(r#"{{"accion":"buscar_tablas","texto":"zzz{i}"}}"#));
    }
    let c = ejecutar(&ctx, &mut db, respuestas);
    let o = c.ok();
    assert_eq!(o.sql, buena);
    assert!(
        o.explanation.contains("Se agotaron los pasos"),
        "{}",
        o.explanation
    );
    assert!(o.lessons.is_empty());
    assert_eq!(c.llamadas, MAX_STEPS);

    // (b) Sin ninguna consulta con filas: sin SQL y con una explicación para el usuario.
    let mut respuestas = Vec::new();
    for i in 0..MAX_STEPS {
        respuestas.push(format!(r#"{{"accion":"buscar_tablas","texto":"nada{i}"}}"#));
    }
    let c = ejecutar(&ctx, &mut db, respuestas);
    assert_eq!(c.llamadas, MAX_STEPS);
    let o = c.ok();
    assert!(o.sql.is_empty());
    assert!(
        o.explanation.contains("no he encontrado"),
        "{}",
        o.explanation
    );
}

#[test]
fn caso_7_cancelacion() {
    // Cancelada antes de empezar: no se llama al modelo ni a la base.
    let f = fixture_tasaciones();
    f.cancel.store(true, Ordering::Relaxed);
    let ctx = f.ctx(Dialect::Postgres);
    let mut db = FakeDb::new(Dialect::Postgres, responder_basico);
    let c = ejecutar(
        &ctx,
        &mut db,
        lote(&[r#"{"accion":"buscar_tablas","texto":"x"}"#]),
    );
    assert!(c.err().contains("cancelada"), "{}", c.err());
    assert_eq!(c.llamadas, 0);
    assert!(db.recibidas.is_empty());
    assert_eq!(c.pasos().len(), 1, "solo el aviso inicial");

    // Cancelación a mitad: el aviso llega tras el primer turno y no hay más llamadas.
    let f = fixture_tasaciones();
    let ctx = f.ctx(Dialect::Postgres);
    let mut db = FakeDb::new(Dialect::Postgres, responder_basico);
    let cancelar = f.cancel.clone();
    let mut llamadas = 0usize;
    let mut eventos = Vec::new();
    let resultado = {
        let mut on_event = |e: Event| eventos.push(Ev::from(e));
        let mut llm = |_sistema: &str, _user: &str, on_text: &mut dyn FnMut(&str)| {
            llamadas += 1;
            cancelar.store(true, Ordering::Relaxed);
            let respuesta = r#"{"accion":"buscar_tablas","texto":"solicitudes"}"#.to_string();
            on_text(&respuesta);
            Ok(respuesta)
        };
        explore_with(&ctx, &mut db, &mut on_event, &mut llm)
    };
    match resultado {
        Err(e) => assert!(e.contains("cancelada"), "{e}"),
        Ok(o) => panic!("se esperaba cancelación y llegó {:?}", o.sql),
    }
    assert_eq!(llamadas, 1);
    assert_eq!(eventos.len(), 2, "aviso inicial + paso de búsqueda");
}

#[test]
fn caso_8_dominio_biblioteca() {
    let f = fixture_biblioteca();
    let ctx = f.ctx(Dialect::Postgres);
    let mut db = FakeDb::new(Dialect::Postgres, responder_biblioteca);
    let sql = "SELECT p.id, l.titulo, p.fecha_prestamo FROM biblioteca.prestamos p JOIN biblioteca.libros l ON l.id = p.libro_id WHERE p.fecha_prestamo >= CURRENT_DATE - 7";
    let c = ejecutar(
        &ctx,
        &mut db,
        lote(&[
            r#"{"accion":"rango","tabla":"biblioteca.prestamos","columna":"fecha_prestamo"}"#,
            r#"{"accion":"probar","sql":"SELECT p.id, l.titulo, p.fecha_prestamo FROM biblioteca.prestamos p JOIN biblioteca.libros l ON l.id = p.libro_id WHERE p.fecha_prestamo >= CURRENT_DATE - 7"}"#,
            r#"{"accion":"final","sql":"SELECT p.id, l.titulo, p.fecha_prestamo FROM biblioteca.prestamos p JOIN biblioteca.libros l ON l.id = p.libro_id WHERE p.fecha_prestamo >= CURRENT_DATE - 7","explicacion":"los préstamos de la semana están en prestamos.fecha_prestamo.","aprendido":["cada préstamo es una fila de biblioteca.prestamos"]}"#,
        ]),
    );
    let o = c.ok();
    assert_eq!(o.sql, sql);
    assert_eq!(c.llamadas, 3);
    assert!(
        c.prompts[1].contains("Rango de prestamos.fecha_prestamo"),
        "prompt: {}",
        c.prompts[1]
    );
    assert!(db.recibio("min(fecha_prestamo)"), "{:?}", db.recibidas);
    assert!(o.tables.iter().any(|t| t == "biblioteca.prestamos"));
    assert!(o.tables.iter().any(|t| t == "biblioteca.libros"));
    assert!(!o.sql.to_lowercase().contains("solicitudes"));
    assert!(db.ejecutadas.iter().any(|s| s == sql));
}

#[test]
fn caso_9_herramientas_errores_y_dialectos() {
    // (a) Tabla inexistente: mensaje claro; y en PG las muestras usan LIMIT.
    let f = fixture_tasaciones();
    let ctx = f.ctx(Dialect::Postgres);
    let mut db = FakeDb::new(Dialect::Postgres, responder_basico);
    let c = ejecutar(
        &ctx,
        &mut db,
        lote(&[
            r#"{"accion":"describir","tabla":"ventas.no_existe"}"#,
            r#"{"accion":"describir","tabla":"ventas.solicitudes"}"#,
            r#"{"accion":"probar","sql":"SELECT numexp FROM ventas.solicitudes"}"#,
            r#"{"accion":"final","sql":"SELECT numexp FROM ventas.solicitudes","explicacion":"todas"}"#,
        ]),
    );
    c.ok();
    assert!(
        c.prompts[1].contains("No existe la tabla «ventas.no_existe»"),
        "prompt: {}",
        c.prompts[1]
    );
    assert!(c.prompts[1].contains("buscar_tablas"));
    assert!(c.prompts[2].contains("Relaciones:"));
    assert!(db.recibio("select * from ventas.solicitudes limit 3"));
    assert!(!db.recibio("fetch first"));

    // (b) Columna inexistente en `valores`: mensaje claro y sin ejecutar SQL.
    let mut db = FakeDb::new(Dialect::Postgres, responder_basico);
    let c = ejecutar(
        &ctx,
        &mut db,
        lote(&[
            r#"{"accion":"valores","tabla":"ventas.solicitudes","columna":"inexistente"}"#,
            r#"{"accion":"probar","sql":"SELECT numexp FROM ventas.solicitudes"}"#,
            r#"{"accion":"final","sql":"SELECT numexp FROM ventas.solicitudes","explicacion":"todas"}"#,
        ]),
    );
    c.ok();
    assert!(
        c.prompts[1].contains("no tiene la columna «inexistente»"),
        "prompt: {}",
        c.prompts[1]
    );
    assert_eq!(db.recibidas.len(), 2, "solo la prueba y su recuento");

    // (c) `buscar_tablas` sin coincidencias.
    let mut db = FakeDb::new(Dialect::Postgres, responder_basico);
    let c = ejecutar(
        &ctx,
        &mut db,
        lote(&[
            r#"{"accion":"buscar_tablas","texto":"ornitorrinco"}"#,
            r#"{"accion":"probar","sql":"SELECT numexp FROM ventas.solicitudes"}"#,
            r#"{"accion":"final","sql":"SELECT numexp FROM ventas.solicitudes","explicacion":"todas"}"#,
        ]),
    );
    c.ok();
    assert!(
        c.prompts[1].contains("Sin coincidencias."),
        "prompt: {}",
        c.prompts[1]
    );

    // (d) Oracle: FETCH FIRST en vez de LIMIT y comillas solo en el nombre mixto.
    let f = fixture_oracle();
    let ctx = f.ctx(Dialect::Oracle);
    let mut db = FakeDb::new(Dialect::Oracle, responder_oracle);
    let c = ejecutar(
        &ctx,
        &mut db,
        lote(&[
            r#"{"accion":"describir","tabla":"VENTAS_APP.SOLICITUDES"}"#,
            r#"{"accion":"valores","tabla":"VENTAS_APP.SOLICITUDES","columna":"CODEST"}"#,
            r#"{"accion":"rango","tabla":"VENTAS_APP.SOLICITUDES","columna":"FCHREM"}"#,
            r#"{"accion":"describir","tabla":"VENTAS_APP.SolicitudesAntiguas"}"#,
            r#"{"accion":"probar","sql":"SELECT NUMEXP FROM VENTAS_APP.SOLICITUDES WHERE CODEST = 'P'"}"#,
            r#"{"accion":"final","sql":"SELECT NUMEXP FROM VENTAS_APP.SOLICITUDES WHERE CODEST = 'P'","explicacion":"pendientes"}"#,
        ]),
    );
    let o = c.ok();
    assert_eq!(
        o.sql,
        "SELECT NUMEXP FROM VENTAS_APP.SOLICITUDES WHERE CODEST = 'P'"
    );
    assert!(db.recibio("fetch first 3 rows only"), "{:?}", db.recibidas);
    assert!(db.recibio("fetch first 15 rows only"));
    assert!(db.recibio("trunc(sysdate) - 7"));
    assert!(
        db.recibidas
            .iter()
            .all(|sql| !sql.to_lowercase().contains("limit")),
        "Oracle no debe usar LIMIT: {:?}",
        db.recibidas
    );
    assert!(db.recibio(r#"ventas_app."SolicitudesAntiguas" fetch first 3 rows only"#));
    assert!(!db.recibio(r#""SOLICITUDES""#));
    assert!(!o.sql.to_uppercase().contains("LIMIT"));
}

#[test]
fn caso_10_eventos_de_paso_tablas_y_sql() {
    let f = fixture_tasaciones();
    let ctx = f.ctx(Dialect::Postgres);
    let mut db = FakeDb::new(Dialect::Postgres, responder_basico);
    let sql = "SELECT s.numexp FROM ventas.solicitudes s JOIN ventas.estadisticas_tasaciones e ON e.numexp = s.numexp WHERE s.codest = 'P'";
    let c = ejecutar(
        &ctx,
        &mut db,
        lote(&[
            r#"{"accion":"buscar_tablas","texto":"solicitudes"}"#,
            r#"{"accion":"describir","tabla":"ventas.solicitudes"}"#,
            r#"{"accion":"valores","tabla":"ventas.solicitudes","columna":"codest"}"#,
            r#"{"accion":"rango","tabla":"ventas.solicitudes","columna":"fchrem"}"#,
            r#"{"accion":"probar","sql":"SELECT s.numexp FROM ventas.solicitudes s JOIN ventas.estadisticas_tasaciones e ON e.numexp = s.numexp WHERE s.codest = 'P'"}"#,
            r#"{"accion":"final","sql":"SELECT s.numexp FROM ventas.solicitudes s JOIN ventas.estadisticas_tasaciones e ON e.numexp = s.numexp WHERE s.codest = 'P'","explicacion":"solicitudes pendientes con su estadística"}"#,
        ]),
    );
    let o = c.ok();
    assert_eq!(o.sql, sql);
    assert!(c.pasos()[0].contains("analizando la petición"));
    assert_eq!(c.pasos_con("buscando tablas: «solicitudes»"), 1);
    assert_eq!(c.pasos_con("mirando las columnas"), 1);
    assert_eq!(c.pasos_con("valores de ventas.solicitudes.codest"), 1);
    assert_eq!(c.pasos_con("rango de fechas"), 1);
    assert_eq!(c.pasos_con("probando la consulta"), 1);
    assert!(c.tiene_tabla("ventas.solicitudes"));
    assert!(c.tiene_tabla("ventas.estadisticas_tasaciones"));
    let sqls = c.sqls();
    assert!(
        sqls.iter().any(|s| s.contains("SELECT s.numexp")),
        "sqls emitidos: {sqls:?}"
    );
}

#[test]
fn caso_11_aprendido_llega_a_lessons() {
    let f = fixture_tasaciones();
    let ctx = f.ctx(Dialect::Postgres);
    let mut db = FakeDb::new(Dialect::Postgres, responder_basico);
    let c = ejecutar(
        &ctx,
        &mut db,
        lote(&[
            r#"{"accion":"probar","sql":"SELECT numexp FROM ventas.solicitudes"}"#,
            r#"{"accion":"final","sql":"SELECT numexp FROM ventas.solicitudes","explicacion":"todas","aprendido":["corto","las solicitudes viven en ventas.solicitudes","la fecha de recepción es fchrem","el estado codest=P es pendiente","los importes están en estadisticas_tasaciones","este quinto hecho sobra"]}"#,
        ]),
    );
    let o = c.ok();
    assert_eq!(o.lessons.len(), 4);
    assert_eq!(o.lessons[0], "las solicitudes viven en ventas.solicitudes");
    assert!(
        o.lessons
            .iter()
            .all(|l| l != "corto" && !l.contains("sobra"))
    );
}

// ---------------------------------------------------------------------------
// Bugs detectados en `explorer.rs` (no se arreglan aquí; ver docs/notas-tests.md)
// ---------------------------------------------------------------------------

/// `explorer.rs` mira `o.lines().nth(1)` (la cabecera de la tabla) para detectar
/// un valor único 0, así que un recuento 0 no dispara el aviso y se entrega.
#[test]

fn bug_recuento_cero_no_avisa() {
    let f = fixture_tasaciones();
    let ctx = f.ctx(Dialect::Postgres);
    let mut db = FakeDb::new(Dialect::Postgres, |sql, _limit| {
        if es_recuento(sql) {
            return respuesta_total(1);
        }
        Ok(resultado(&["total"], &[&["0"]]))
    });
    let c = ejecutar(
        &ctx,
        &mut db,
        lote(&[
            r#"{"accion":"probar","sql":"SELECT COUNT(*) AS total FROM ventas.solicitudes WHERE fchenc >= CURRENT_DATE - 7"}"#,
            r#"{"accion":"final","sql":"SELECT COUNT(*) AS total FROM ventas.solicitudes WHERE fchenc >= CURRENT_DATE - 7","explicacion":"no hay ninguna","aprendido":[]}"#,
            r#"{"accion":"final","sql":"SELECT COUNT(*) AS total FROM ventas.solicitudes WHERE fchenc >= CURRENT_DATE - 7","explicacion":"confirmo que no hay ninguna","aprendido":[]}"#,
        ]),
    );
    assert_eq!(
        c.llamadas, 3,
        "un recuento 0 debía pedir investigar antes de entregar"
    );
}

/// En el último paso, `explorer.rs` entrega un SQL que ya había dado error al
/// probarlo (`Some(None) if !last_step` deja pasar el caso en `last_step`).
#[test]

fn bug_ultimo_paso_entrega_sql_con_error() {
    let f = fixture_tasaciones();
    let ctx = f.ctx(Dialect::Postgres);
    let mut db = FakeDb::new(Dialect::Postgres, |sql, limite| {
        if sql.to_lowercase().contains("inexistente") {
            return Err(DbError::new("column \"inexistente\" does not exist"));
        }
        responder_basico(sql, limite)
    });
    let mut respuestas =
        lote(&[r#"{"accion":"probar","sql":"SELECT inexistente FROM ventas.solicitudes"}"#]);
    for i in 0..(MAX_STEPS - 2) {
        respuestas.push(format!(r#"{{"accion":"buscar_tablas","texto":"zzz{i}"}}"#));
    }
    respuestas.push(
        r#"{"accion":"final","sql":"SELECT inexistente FROM ventas.solicitudes","explicacion":"x"}"#
            .into(),
    );
    let c = ejecutar(&ctx, &mut db, respuestas);
    assert!(
        c.resultado.is_err(),
        "entregó un SQL que la base había rechazado"
    );
}

/// `parse_json` se queda con el primer `{` del texto: si la prosa lleva una
/// llave suelta antes del objeto, el JSON válido posterior se descarta.
#[test]

fn bug_json_tras_llaves_en_la_prosa() {
    let respuesta = r#"Uso el esquema {ventas} y respondo: {"accion":"probar","sql":"SELECT numexp FROM ventas.solicitudes"}"#;
    assert!(
        crate::explorer::parse_json(respuesta).is_some(),
        "la prosa con llaves hace perder el JSON válido"
    );
}

/// Regresión (17-nov): el agente se quedó sin pasos y entregó como «consulta que funciona» un listado de
/// `information_schema.tables`. El catálogo del sistema nunca puede ser el resultado final.
#[test]
fn nunca_entrega_consultas_al_catalogo_del_sistema() {
    let f = fixture_tasaciones();
    let ctx = f.ctx(Dialect::Postgres);
    let mut db = FakeDb::new(Dialect::Postgres, |sql, limite| {
        responder_basico(sql, limite)
    });
    let cat = "SELECT table_schema, table_name FROM information_schema.tables";
    let mut respuestas = lote(&[
        &format!(r#"{{"accion":"probar","sql":"{cat}"}}"#),
        &format!(r#"{{"accion":"final","sql":"{cat}","explicacion":"x"}}"#),
    ]);
    for i in 0..MAX_STEPS {
        respuestas.push(format!(r#"{{"accion":"buscar_tablas","texto":"zzz{i}"}}"#));
    }
    let c = ejecutar(&ctx, &mut db, respuestas);
    match &c.resultado {
        Ok(o) => assert!(
            o.sql.is_empty(),
            "no debía entregar SQL al catálogo: {}",
            o.sql
        ),
        Err(_) => {}
    }
}

/// `sin_respuesta` termina el bucle sin SQL y con la explicación.
#[test]
fn sin_respuesta_explica_sin_sql() {
    let f = fixture_tasaciones();
    let ctx = f.ctx(Dialect::Postgres);
    let mut db = FakeDb::new(Dialect::Postgres, |sql, limite| {
        responder_basico(sql, limite)
    });
    let c = ejecutar(
        &ctx,
        &mut db,
        lote(&[
            r#"{"accion":"buscar_tablas","texto":"zzz"}"#,
            r#"{"accion":"sin_respuesta","explicacion":"no hay tabla de pedidos"}"#,
        ]),
    );
    let o = c.resultado.expect("debía terminar bien");
    assert!(o.sql.is_empty());
    assert!(o.explanation.contains("pedidos"));
}
