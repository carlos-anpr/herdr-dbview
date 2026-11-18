//! Estado y lógica de la aplicación: conexiones, espacios de trabajo, hilos y eventos.

use crate::agent;
use crate::catalog::{self, Catalog};
use crate::config::{self, Config};
use crate::db::{self, Database};
use crate::editor::Editor;
use crate::explorer;
use crate::form::{Form, FormAct, TestState};
use crate::history;
use crate::model::*;
use crate::nl2sql;
use crate::notes;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Conns,
    Tables,
    Question,
    Sql,
    Result,
}

const ORDER: [Focus; 5] = [
    Focus::Conns,
    Focus::Tables,
    Focus::Question,
    Focus::Sql,
    Focus::Result,
];

#[derive(Clone, Debug, PartialEq)]
pub enum ConnStatus {
    Idle,
    Connecting,
    Ok(String),
    Failed(DbError),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Busy {
    No,
    Ai,
    Run,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AiMode {
    New,
    Adjust,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SchemaState {
    None,
    Loading,
    Ready,
    Failed(String),
}

#[derive(Default, Clone, Debug)]
pub struct TableView {
    pub row: usize,
    pub col: usize,
    pub top: usize,
    pub left: usize,
    pub sort: Option<(usize, bool)>,
    /// Orden de filas visibles (índices en `rows`).
    pub order: Vec<usize>,
    pub widths: Vec<u16>,
    pub numeric: Vec<bool>,
    /// Filas y columnas que cabían en el último dibujo.
    pub page_rows: usize,
    pub page_cols: usize,
}

/// Una pestaña de consulta (al estilo DBeaver): pregunta ①, SQL ② y resultado ③ van siempre juntos.
pub struct Tab {
    /// Número visible y estable (`Consulta 3`, `SQL 3`); no se reutiliza al cerrar pestañas.
    pub num: usize,
    /// Identificador del trabajo en curso (IA o ejecución) de esta pestaña; los mensajes de los hilos llegan con él.
    pub job: u64,
    pub question: Editor,
    pub sql: Editor,
    pub mode: AiMode,
    pub busy: Busy,
    pub ai_error: Option<String>,
    pub pending_generate: bool,
    pub cancel: Arc<AtomicBool>,
    pub result: Option<ResultSet>,
    pub error: Option<DbError>,
    /// SQL que se ejecutó (para situar el error).
    pub error_sql: String,
    pub tv: TableView,
    pub busy_since: Option<Instant>,
    pub ran_sql: String,
    /// Cadena «generar → ejecutar → corregir» en curso: el usuario solo ve el resultado final.
    pub chain: bool,
    /// Intentos de corrección automática usados.
    pub repair: u8,
    /// Error que la IA debe corregir en la próxima generación.
    pub fix_error: Option<DbError>,
    /// Plan de tablas de la última petición (se enseña al usuario) y ejemplos leídos.
    pub plan: Option<agent::Plan>,
    pub samples: Option<Arc<HashMap<String, String>>>,
    /// Fase de la generación para la cabecera del SQL.
    pub phase: String,
    /// Pasos de investigación del agente (visibles para el usuario).
    pub steps: Vec<String>,
    /// Conexión donde sí está lo que el usuario pide (sugerencia con botón).
    pub suggest: Option<usize>,
    /// Último SQL entregado por la IA y la pregunta que lo originó (para aprender de cómo lo usa el usuario).
    pub ai_sql: String,
    pub ai_question: String,
    pub learned: bool,
    /// Hechos descubiertos por el agente pendientes de guardar en las notas.
    pub lessons: Vec<String>,
}

/// Reparte rangos de números de trabajo distintos a cada `Ws` que se crea.
static NEXT_SERIAL_BASE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Reparte números únicos a cada intento de conexión.
static NEXT_ATTEMPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Pestañas que se recuerdan tras cerrarlas (Alt+U las reabre).
const CLOSED_KEEP: usize = 10;

impl Tab {
    fn new(num: usize) -> Self {
        Tab {
            num,
            job: 0,
            question: Editor::new(""),
            sql: Editor::new(""),
            mode: AiMode::New,
            busy: Busy::No,
            ai_error: None,
            pending_generate: false,
            cancel: Arc::new(AtomicBool::new(false)),
            result: None,
            error: None,
            error_sql: String::new(),
            tv: TableView::default(),
            busy_since: None,
            ran_sql: String::new(),
            chain: false,
            repair: 0,
            fix_error: None,
            plan: None,
            samples: None,
            phase: String::new(),
            steps: Vec::new(),
            suggest: None,
            ai_sql: String::new(),
            ai_question: String::new(),
            learned: false,
            lessons: Vec::new(),
        }
    }

    /// ¿La pestaña no tiene nada escrito ni resultado? (no merece recordarse al cerrarla)
    pub fn is_blank(&self) -> bool {
        self.question.is_empty() && self.sql.is_empty() && self.result.is_none()
    }

    /// Nombre de la pestaña ①: el principio de la pregunta, o `Consulta N` si está vacía.
    pub fn question_label(&self) -> String {
        let q = self.question.text();
        let first = q.lines().map(str::trim).find(|l| !l.is_empty());
        match first {
            Some(l) => crate::theme::trunc(l, 18),
            None => format!("Consulta {}", self.num),
        }
    }

    /// Nombre de la pestaña ②, enlazada a la ① con el mismo número.
    pub fn sql_label(&self) -> String {
        format!("SQL {}", self.num)
    }

    fn rebuild_view(&mut self) {
        let Some(r) = &self.result else {
            self.tv = TableView::default();
            return;
        };
        let mut tv = TableView {
            order: (0..r.rows.len()).collect(),
            ..Default::default()
        };
        for (c, col) in r.columns.iter().enumerate() {
            let mut w = col.name.chars().count();
            let mut numeric = !r.rows.is_empty();
            for row in r.rows.iter().take(300) {
                if let Some(Some(v)) = row.get(c) {
                    w = w.max(crate::theme::width(v));
                    if numeric && v.trim().parse::<f64>().is_err() {
                        numeric = false;
                    }
                }
            }
            tv.widths.push(w.clamp(4, 40) as u16);
            tv.numeric.push(numeric);
        }
        self.tv = tv;
    }

    pub fn sort_by(&mut self, col: usize) {
        let Some(r) = &self.result else { return };
        let desc = matches!(self.tv.sort, Some((c, false)) if c == col);
        if matches!(self.tv.sort, Some((c, true)) if c == col) {
            self.tv.sort = None;
            self.tv.order = (0..r.rows.len()).collect();
            return;
        }
        let numeric = self.tv.numeric.get(col).copied().unwrap_or(false);
        let mut order: Vec<usize> = (0..r.rows.len()).collect();
        order.sort_by(|a, b| {
            let (x, y) = (
                r.rows[*a].get(col).cloned().flatten(),
                r.rows[*b].get(col).cloned().flatten(),
            );
            let o = match (x, y) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, _) => std::cmp::Ordering::Less,
                (_, None) => std::cmp::Ordering::Greater,
                (Some(x), Some(y)) if numeric => x
                    .trim()
                    .parse::<f64>()
                    .unwrap_or(0.0)
                    .partial_cmp(&y.trim().parse::<f64>().unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal),
                (Some(x), Some(y)) => x.to_lowercase().cmp(&y.to_lowercase()),
            };
            if desc { o.reverse() } else { o }
        });
        self.tv.order = order;
        self.tv.sort = Some((col, !desc));
    }
}

/// Espacio de trabajo de una conexión: lo común (esquema, tablas, catálogo) y sus pestañas de consulta.
/// `Deref` a la pestaña activa: `ws.sql`, `ws.result`… son los de la pestaña que se está viendo.
pub struct Ws {
    pub spec: ConnSpec,
    pub db: Arc<Mutex<Box<dyn Database>>>,
    pub status: ConnStatus,
    pub schema: Option<Arc<Schema>>,
    pub schema_state: SchemaState,
    pub pinned: BTreeSet<String>,
    /// Contador de trabajos de la conexión (y de sus pestañas): cada uno recibe un número único.
    pub serial: u64,
    /// Intento de conexión vigente (ver `Msg::Connected`).
    pub conn_attempt: u64,
    pub tables_filter: String,
    pub tables_sel: usize,
    pub tables_top: usize,
    /// Primera columna visible del panel «Columnas» (se desplaza con la rueda) y de qué tabla es.
    pub cols_top: usize,
    pub cols_table: String,
    pub catalog: Option<Arc<Catalog>>,
    pub analysis: Analysis,
    pub analysis_cancel: Arc<AtomicBool>,
    /// Última vez que se leyó la estructura de la base (para el chequeo periódico).
    pub last_check: Instant,
    pub rechecking: bool,
    /// Pestañas de consulta (siempre hay al menos una) y la que se ve.
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub next_tab: usize,
    /// Pestañas cerradas recientemente.
    pub closed: Vec<Tab>,
}

impl std::ops::Deref for Ws {
    type Target = Tab;
    fn deref(&self) -> &Tab {
        &self.tabs[self.active.min(self.tabs.len() - 1)]
    }
}

impl std::ops::DerefMut for Ws {
    fn deref_mut(&mut self) -> &mut Tab {
        let i = self.active.min(self.tabs.len() - 1);
        &mut self.tabs[i]
    }
}

/// Progreso del análisis de la base para enseñarlo al usuario.
#[derive(Clone, Debug, PartialEq)]
pub struct AnaProg {
    pub phase: String,
    pub done: usize,
    pub total: usize,
    /// `true` si es el primer análisis de esta base (no hay memoria previa).
    pub first: bool,
    /// Tablas nuevas o modificadas que se están (re)analizando.
    pub pending: usize,
    pub tables: usize,
    pub since: Instant,
}

impl AnaProg {
    /// Fracción global 0..=1: estructura 5 %, tablas 90 %, resumen 5 %.
    pub fn fraction(&self) -> f32 {
        let f = if self.total == 0 {
            0.0
        } else {
            (self.done as f32 / self.total as f32).clamp(0.0, 1.0)
        };
        match self.phase.as_str() {
            "estructura" => 0.05 * f,
            "resumen" => 0.95 + 0.05 * f,
            _ => 0.05 + 0.90 * f,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Analysis {
    None,
    Running(AnaProg),
    Done,
    Failed(String),
}

impl Ws {
    fn new(spec: ConnSpec) -> Self {
        let db = db::open(&spec);
        Ws {
            spec,
            db: Arc::new(Mutex::new(db)),
            status: ConnStatus::Idle,
            schema: None,
            schema_state: SchemaState::None,
            pinned: BTreeSet::new(),
            // Rango propio por conexión creada: un mensaje de una conexión anterior (editada o recreada) nunca casa con un trabajo nuevo.
            serial: NEXT_SERIAL_BASE.fetch_add(1 << 32, Ordering::Relaxed),
            conn_attempt: 0,
            tables_filter: String::new(),
            tables_sel: 0,
            tables_top: 0,
            cols_top: 0,
            cols_table: String::new(),
            catalog: None,
            analysis: Analysis::None,
            analysis_cancel: Arc::new(AtomicBool::new(false)),
            last_check: Instant::now(),
            rechecking: false,
            tabs: vec![Tab::new(1)],
            active: 0,
            next_tab: 2,
            closed: Vec::new(),
        }
    }

    /// Tablas que pasan el filtro, como índices de `schema.tables`.
    pub fn filtered_tables(&self) -> Vec<usize> {
        let Some(s) = &self.schema else { return vec![] };
        let f = self.tables_filter.to_lowercase();
        s.tables
            .iter()
            .enumerate()
            .filter(|(_, t)| f.is_empty() || t.qualified().to_lowercase().contains(&f))
            .map(|(i, _)| i)
            .collect()
    }

    /// Abre una pestaña vacía al final y pasa a ella.
    pub fn new_tab(&mut self) {
        self.tabs.push(Tab::new(self.next_tab));
        self.next_tab += 1;
        self.active = self.tabs.len() - 1;
    }

    /// ¿Alguna pestaña está trabajando o esperando el esquema?
    pub fn any_work(&self) -> bool {
        self.tabs
            .iter()
            .any(|t| t.busy != Busy::No || t.chain || t.pending_generate)
    }
}

/// Intentos de corrección automática antes de enseñar el error al usuario.
pub const MAX_REPAIRS: u8 = 3;
/// Cada cuánto se comprueba si la estructura de la base ha cambiado (30 min).
pub const RECHECK_SECS: u64 = 1800;

pub enum Msg {
    /// El último número es el intento de conexión (`Ws::conn_attempt`): los de una conexión reemplazada o reintentada se descartan.
    Connected(String, Result<String, DbError>, u64),
    Schema(String, Result<Schema, DbError>, u64),
    AiText(String, u64, String),
    AiDone(String, u64, Result<String, String>),
    Ran(String, u64, Result<ResultSet, DbError>),
    FormTest(Result<(String, u64), DbError>, u64),
    AnalysisProgress(String, String, usize, usize),
    AnalysisDone(String, Result<Catalog, String>),
    Recheck(String, Result<Schema, DbError>),
    Step(String, u64, String),
    Learned(String, u64, Vec<String>),
    TableNotes(String, Vec<(String, String)>),
    Plan(
        String,
        u64,
        Option<agent::Plan>,
        Arc<HashMap<String, String>>,
    ),
    Phase(String, u64, String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    Conn(usize),
    NewConn,
    EditConn(usize),
    DelConn(usize),
    Focus(Focus),
    Generate,
    ToggleMode,
    /// Pestañas de consulta: ver la n, cerrar la n, abrir una nueva.
    Tab(usize),
    CloseTab(usize),
    NewTab,
    Run,
    Fix,
    Clear,
    Table(usize),
    Pin(usize),
    /// Clic en una columna del panel «Columnas»: la inserta en el SQL.
    Column(usize),
    Cell(usize, usize),
    Sort(usize),
    Help,
    Quit,
    Cancel,
    Export,
    ClearFilter,
    Notes,
    SaveLessons,
    SwitchAsk(usize),
    Info,
    Reanalyze,
    NotesSave,
    // formulario
    FormItem(usize),
    /// Clic en una opción de un selector del formulario (tipo de BD, entorno, modo Oracle).
    FormPick(usize, usize),
    FormTest,
    FormSave,
    FormCancel,
    ConfirmYes,
    ConfirmNo,
    CloseDialog,
}

/// Zonas de la pantalla del último dibujo (para arrastrar bordes con el ratón).
#[derive(Clone, Copy, Default)]
pub struct Geo {
    pub area: Rect,
    pub side: Rect,
    pub main: Rect,
    pub q: Rect,
    pub s: Rect,
    pub r: Rect,
    pub conns: Rect,
    pub tables: Rect,
    pub cols: Rect,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Drag {
    Side,
    Question,
    Sql,
    Conns,
    Cols,
}

pub enum Dialog {
    None,
    Form(Box<Form>),
    ConfirmDelete(usize),
    Help,
    Cell(String),
    Info(String),
    Notes(Box<NotesDlg>),
}

pub struct NotesDlg {
    pub conn: usize,
    pub ed: Editor,
}

pub struct Toast {
    pub text: String,
    pub good: bool,
    pub until: Instant,
}

pub struct App {
    pub cfg: Config,
    pub wss: Vec<Ws>,
    pub cur: usize,
    pub focus: Focus,
    pub dialog: Dialog,
    pub toast: Option<Toast>,
    pub tx: mpsc::Sender<Msg>,
    pub rx: mpsc::Receiver<Msg>,
    pub hits: Vec<(Rect, Act)>,
    pub quit: bool,
    pub tick: u64,
    pub clip: String,
    pub today: String,
    /// Hora (dd/mm HH:MM) en que se compiló el binario en ejecución: se ve en la cabecera para saber qué versión corre.
    pub build: String,
    pub form_serial: u64,
    pub conn_scroll: usize,
    /// Ajuste manual del ancho de la barra lateral (columnas).
    pub side_w: Option<u16>,
    pub q_h: Option<u16>,
    pub sql_h: Option<u16>,
    pub conns_h: Option<u16>,
    pub cols_h: Option<u16>,
    pub drag: Option<Drag>,
    pub zoom: bool,
    pub geo: Geo,
}

/// Fecha de modificación del propio ejecutable (= cuándo se compiló), `dd/mm HH:MM`.
fn build_stamp() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            std::process::Command::new("date")
                .arg("-r")
                .arg(exe)
                .arg("+%d/%m %H:%M")
                .output()
                .ok()
        })
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "?".into())
}

fn today() -> String {
    std::process::Command::new("date")
        .arg("+%F")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| s.len() == 10)
        .unwrap_or_else(|| "2026-01-01".into())
}

impl App {
    pub fn new(cfg: Config) -> Self {
        let (tx, rx) = mpsc::channel();
        let wss: Vec<Ws> = cfg.connections.iter().cloned().map(Ws::new).collect();
        let cur = cfg
            .default_conn
            .as_ref()
            .and_then(|d| cfg.connections.iter().position(|c| &c.info.id == d))
            .unwrap_or(0);
        let mut app = App {
            cfg,
            wss,
            cur,
            focus: Focus::Question,
            dialog: Dialog::None,
            toast: None,
            tx,
            rx,
            hits: vec![],
            quit: false,
            tick: 0,
            clip: String::new(),
            today: today(),
            build: build_stamp(),
            form_serial: 0,
            conn_scroll: 0,
            side_w: None,
            q_h: None,
            sql_h: None,
            conns_h: None,
            cols_h: None,
            drag: None,
            zoom: false,
            geo: Geo::default(),
        };
        if app.wss.is_empty() {
            app.dialog = Dialog::Form(Box::new(Form::new_blank()));
        } else {
            app.connect_current();
            // El resto de conexiones se leen también (solo el esquema) para poder sugerir dónde está cada tabla.
            for i in 0..app.wss.len() {
                if i != app.cur {
                    app.reconnect_idx(i);
                }
            }
        }
        app
    }

    pub fn ws(&self) -> Option<&Ws> {
        self.wss.get(self.cur)
    }

    pub fn ws_mut(&mut self) -> Option<&mut Ws> {
        self.wss.get_mut(self.cur)
    }

    pub fn toast(&mut self, text: impl Into<String>, good: bool) {
        self.toast = Some(Toast {
            text: text.into(),
            good,
            until: Instant::now() + Duration::from_secs(4),
        });
    }

    pub fn tick(&mut self) {
        self.tick += 1;
        if self
            .toast
            .as_ref()
            .is_some_and(|t| t.until < Instant::now())
        {
            self.toast = None;
        }
    }

    // ───────────── conexión y esquema ─────────────

    pub fn select_conn(&mut self, i: usize) {
        if i < self.wss.len() {
            self.cur = i;
            self.connect_current();
        }
    }

    fn connect_current(&mut self) {
        let Some(ws) = self.wss.get_mut(self.cur) else {
            return;
        };
        if !matches!(ws.status, ConnStatus::Idle) {
            return;
        }
        self.reconnect_current();
    }

    pub fn reconnect_current(&mut self) {
        self.reconnect_idx(self.cur);
    }

    pub fn reconnect_idx(&mut self, idx: usize) {
        let tx = self.tx.clone();
        let Some(ws) = self.wss.get_mut(idx) else {
            return;
        };
        ws.status = ConnStatus::Connecting;
        ws.schema_state = SchemaState::Loading;
        // Número único en todo el programa: ni un reintento ni una conexión editada con el mismo id lo repiten.
        let serial = NEXT_ATTEMPT.fetch_add(1, Ordering::Relaxed);
        ws.conn_attempt = serial;
        let (id, db) = (ws.spec.info.id.clone(), ws.db.clone());
        let schemas = ws.spec.schemas.clone();
        std::thread::spawn(move || {
            let t = Instant::now();
            let r = db
                .lock()
                .map_err(|_| DbError::new("Conexión bloqueada"))
                .and_then(|mut d| {
                    let v = d.check()?;
                    let _ = t;
                    Ok((v, d.schema().map(|s| s.restricted(&schemas))))
                });
            match r {
                Ok((v, schema)) => {
                    let _ = tx.send(Msg::Connected(id.clone(), Ok(v), serial));
                    let _ = tx.send(Msg::Schema(id, schema, serial));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Connected(id, Err(e), serial));
                }
            }
        });
    }

    // ───────────── IA ─────────────

    pub fn generate(&mut self) {
        if let Some(ws) = self.wss.get_mut(self.cur) {
            if ws.busy == Busy::No {
                ws.repair = 0;
                ws.fix_error = None;
            }
        }
        self.generate_inner();
    }

    fn generate_inner(&mut self) {
        let (cfg_ai, today) = (self.cfg.ai.clone(), self.today.clone());
        let tx = self.tx.clone();
        // ¿La pregunta nombra expresamente una tabla que no existe aquí pero sí en otra conexión? Responder ya, sin gastar pasos.
        let explicit = self
            .wss
            .get(self.cur)
            .filter(|w| w.busy == Busy::No && w.fix_error.is_none())
            .map(|w| w.question.text())
            .and_then(|q| self.cross_hint(&q, true));
        if let Some((idx, msg)) = explicit {
            if let Some(ws) = self.wss.get_mut(self.cur) {
                ws.ai_error = Some(msg);
                ws.suggest = Some(idx);
                ws.pending_generate = false;
            }
            return;
        }
        let Some(ws) = self.wss.get_mut(self.cur) else {
            return;
        };
        if ws.busy != Busy::No {
            return;
        }
        ws.suggest = None;
        let question = ws.question.text().trim().to_string();
        if question.is_empty() {
            // Sin pregunta no hay nada que esperar: se anula una generación diferida.
            ws.pending_generate = false;
            self.toast("Escribe primero qué quieres consultar", false);
            self.focus = Focus::Question;
            return;
        }
        if let Some(e) = &cfg_ai.error {
            let msg = format!("IA mal configurada: {e}");
            self.toast(msg, false);
            return;
        }
        let Some(schema) = ws.schema.clone() else {
            if ws.schema_state == SchemaState::Loading {
                ws.pending_generate = true;
                self.toast("Leyendo las tablas… se generará en cuanto estén", true);
            } else {
                self.toast(
                    "No hay esquema de la base: revisa la conexión (Ctrl+L la reintenta)",
                    false,
                );
            }
            return;
        };
        ws.pending_generate = false;
        ws.serial += 1;
        let serial = ws.serial;
        ws.job = serial;
        ws.busy = Busy::Ai;
        ws.busy_since = Some(Instant::now());
        ws.ai_error = None;
        ws.cancel = Arc::new(AtomicBool::new(false));
        let cancel = ws.cancel.clone();
        let fixing = ws.fix_error.is_some();
        let current = if (fixing || ws.mode == AiMode::Adjust) && !ws.sql.is_empty() {
            Some(ws.sql.text())
        } else {
            None
        };
        let last_error = if current.is_some() {
            ws.fix_error.take().or_else(|| ws.error.clone())
        } else {
            None
        };
        ws.chain = true;
        let mut pinned: Vec<String> = ws.pinned.iter().cloned().collect();
        if let (Some(sql), Some(_)) = (&current, &last_error) {
            // Al corregir, la IA necesita ver TODAS las columnas de las tablas que usa el SQL fallido.
            let up = sql.to_uppercase();
            for t in &schema.tables {
                if t.name.len() > 2 && up.contains(&t.name.to_uppercase()) {
                    pinned.push(t.qualified());
                }
            }
        }
        let engine = match &ws.status {
            ConnStatus::Ok(v) => v.clone(),
            _ => String::new(),
        };
        let dialect = ws.spec.info.dialect;
        let id = ws.spec.info.id.clone();
        ws.sql.checkpoint();
        let cat: Catalog = ws
            .catalog
            .as_ref()
            .map(|c| (**c).clone())
            .unwrap_or_default();
        let notes = {
            let mut n = notes::load(&id);
            let h = history::similar(&id, &question, 3);
            if !h.is_empty() {
                n.push_str("\n\nConsultas anteriores CONFIRMADAS por el usuario sobre esta misma base (úsalas como guía de qué tablas y columnas usar si la pregunta se parece):\n");
                n.push_str(&h);
            }
            let k = notes::knowledge(&id, &question, 5_000);
            if !k.is_empty() {
                n.push_str("\n\nDocumentación de referencia sobre esta base (fragmentos relevantes; fiable salvo que contradiga a los datos):\n");
                n.push_str(&k);
            }
            n
        };
        let (reuse_plan, reuse_samples) = if fixing {
            (ws.plan.clone(), ws.samples.clone())
        } else {
            (None, None)
        };
        if !fixing {
            ws.plan = None;
            ws.samples = None;
            ws.steps.clear();
            ws.lessons.clear();
        }
        ws.phase = if fixing {
            "corrigiendo la consulta".into()
        } else {
            "eligiendo las tablas".into()
        };
        let db = ws.db.clone();
        let engine_for_agent = engine.clone();
        let strong = cfg_ai.strong();
        std::thread::spawn(move || {
            // Vía principal: agente explorador (herramientas de solo lectura, prueba el SQL antes de entregarlo).
            if reuse_plan.is_none() && !fixing {
                let candidates: Vec<String> =
                    agent::rank_hybrid(&schema, &question, &pinned, &cat, 25)
                        .into_iter()
                        .map(|i| schema.tables[i].qualified())
                        .collect();
                let relations: HashMap<String, Vec<String>> = explorer::relation_map(&cat);
                let ctx = explorer::Ctx {
                    cfg: &cfg_ai,
                    dialect,
                    engine: &engine_for_agent,
                    today: &today,
                    question: &question,
                    schema: &schema,
                    summaries: &cat.tables,
                    overview: &cat.overview,
                    relations: &relations,
                    candidates: &candidates,
                    notes: &notes,
                    pinned: &pinned,
                    current_sql: current.as_deref(),
                    cancel: &cancel,
                };
                let txe = tx.clone();
                let ide = id.clone();
                let mut on_event = move |e: explorer::Event| match e {
                    explorer::Event::Step(t) => {
                        let _ = txe.send(Msg::Step(ide.clone(), serial, t));
                    }
                    explorer::Event::Tables(v) => {
                        let _ = txe.send(Msg::Plan(
                            ide.clone(),
                            serial,
                            Some(agent::Plan {
                                tables: v,
                                reason: String::new(),
                            }),
                            Arc::new(HashMap::new()),
                        ));
                    }
                    explorer::Event::Sql(sql) => {
                        let _ = txe.send(Msg::AiText(ide.clone(), serial, sql));
                    }
                };
                let result = match db.lock() {
                    Ok(mut g) => {
                        explorer::explore_smart(&ctx, strong.as_ref(), &mut **g, &mut on_event)
                    }
                    Err(_) => Err("Conexión bloqueada".into()),
                };
                match result {
                    Ok(out) if out.sql.trim().is_empty() => {
                        let _ = tx.send(Msg::AiDone(
                            id,
                            serial,
                            Err(format!("No lo encuentro en esta base: {}", out.explanation)),
                        ));
                        return;
                    }
                    Ok(out) => {
                        let _ = tx.send(Msg::Plan(
                            id.clone(),
                            serial,
                            Some(agent::Plan {
                                tables: out.tables.clone(),
                                reason: out.explanation.clone(),
                            }),
                            Arc::new(HashMap::new()),
                        ));
                        if !out.table_notes.is_empty() {
                            let _ = tx.send(Msg::TableNotes(id.clone(), out.table_notes.clone()));
                        }
                        if !out.lessons.is_empty() {
                            let _ = tx.send(Msg::Learned(id.clone(), serial, out.lessons.clone()));
                        }
                        let _ = tx.send(Msg::AiDone(id, serial, Ok(out.sql)));
                        return;
                    }
                    Err(e) if cancel.load(Ordering::Relaxed) || e.contains("cancelada") => {
                        let _ = tx.send(Msg::AiDone(id, serial, Err("Consulta cancelada".into())));
                        return;
                    }
                    Err(_) => {
                        let _ = tx.send(Msg::Step(
                            id.clone(),
                            serial,
                            "el agente no llegó a nada probado: modo simple".into(),
                        ));
                    }
                }
            }
            let (plan, samples): (Option<agent::Plan>, Arc<HashMap<String, String>>) =
                match reuse_plan {
                    Some(p) => (Some(p), reuse_samples.unwrap_or_default()),
                    None => {
                        let _ = tx.send(Msg::Phase(
                            id.clone(),
                            serial,
                            "eligiendo las tablas".into(),
                        ));
                        let plan = agent::plan(
                            &cfg_ai,
                            dialect,
                            &today,
                            &question,
                            &schema,
                            &cat,
                            &notes,
                            &pinned,
                            current.as_deref(),
                            &cancel,
                        )
                        .ok();
                        let chosen: Vec<String> = match &plan {
                            Some(p) => p.tables.clone(),
                            None => agent::rank_hybrid(&schema, &question, &pinned, &cat, 4)
                                .into_iter()
                                .map(|i| schema.tables[i].qualified())
                                .collect(),
                        };
                        let _ = tx.send(Msg::Phase(
                            id.clone(),
                            serial,
                            "leyendo columnas y ejemplos".into(),
                        ));
                        let samples = match db.lock() {
                            Ok(mut g) => {
                                agent::samples(&mut **g, dialect, &schema, &chosen, &cancel)
                            }
                            Err(_) => HashMap::new(),
                        };
                        let samples = Arc::new(samples);
                        let _ =
                            tx.send(Msg::Plan(id.clone(), serial, plan.clone(), samples.clone()));
                        (plan, samples)
                    }
                };
            if cancel.load(Ordering::Relaxed) {
                let _ = tx.send(Msg::AiDone(id, serial, Err("Consulta cancelada".into())));
                return;
            }
            let _ = tx.send(Msg::Phase(id.clone(), serial, "escribiendo el SQL".into()));
            let mut pinned = pinned;
            if let Some(p) = &plan {
                for t in &p.tables {
                    if !pinned.contains(t) {
                        pinned.push(t.clone());
                    }
                }
            }
            let req = nl2sql::Request {
                dialect,
                engine: &engine,
                catalog: Some(&cat),
                samples: Some(&samples),
                notes: &notes,
                plan: plan.as_ref().map(|p| p.reason.as_str()),
                today: &today,
                question: &question,
                schema: &schema,
                pinned: &pinned,
                current_sql: current.as_deref(),
                last_error: last_error.as_ref(),
            };
            let (system, user) = nl2sql::prompt(&req);
            let tx2 = tx.clone();
            let id2 = id.clone();
            let mut last = Instant::now() - Duration::from_secs(1);
            let r = crate::ai::chat(&cfg_ai, &system, &user, cancel, move |text| {
                if last.elapsed() > Duration::from_millis(60) {
                    last = Instant::now();
                    let _ = tx2.send(Msg::AiText(id2.clone(), serial, text.to_string()));
                }
            });
            let _ = tx.send(Msg::AiDone(id, serial, r.map_err(|e| e.to_string())));
        });
    }

    // ───────────── análisis del catálogo ─────────────

    /// Describe con la IA las tablas de la conexión (en segundo plano, con caché en disco).
    pub fn start_analysis(&mut self, idx: usize, force: bool) {
        let cfg_ai = self.cfg.ai.clone();
        let tx = self.tx.clone();
        let Some(ws) = self.wss.get_mut(idx) else {
            return;
        };
        let Some(schema) = ws.schema.clone() else {
            return;
        };
        if cfg_ai.error.is_some() {
            return;
        }
        if force {
            ws.analysis_cancel.store(true, Ordering::Relaxed);
        } else if matches!(ws.analysis, Analysis::Running(..)) {
            return;
        }
        let id = ws.spec.info.id.clone();
        let existing = catalog::load(&id);
        ws.catalog = existing.clone().map(Arc::new);
        if !force {
            // Modo normal: memoria estructural local e instantánea (relaciones); las descripciones se aprenden al consultar.
            let mut cat = existing.unwrap_or_default();
            // Si la conexión se limita a ciertos esquemas, la memoria no guarda tablas de los demás.
            let known: std::collections::HashSet<String> =
                schema.tables.iter().map(|t| t.qualified()).collect();
            cat.tables.retain(|t, _| known.contains(t));
            cat.signatures.retain(|t, _| known.contains(t));
            cat.relations = catalog::relations(&schema);
            let _ = catalog::save(&id, &cat);
            ws.catalog = Some(Arc::new(cat));
            ws.analysis = Analysis::Done;
            return;
        }
        let todo = catalog::pending(&schema, existing.as_ref()).len();
        if todo == 0 {
            ws.analysis = Analysis::Done;
            return;
        }
        let first = existing.as_ref().is_none_or(|c| c.tables.is_empty());
        ws.analysis = Analysis::Running(AnaProg {
            phase: "estructura".into(),
            done: 0,
            total: 1,
            first,
            pending: todo,
            tables: schema.tables.len(),
            since: Instant::now(),
        });
        ws.analysis_cancel = Arc::new(AtomicBool::new(false));
        let cancel = ws.analysis_cancel.clone();
        let (dialect, label) = (ws.spec.info.dialect, ws.spec.info.label.clone());
        std::thread::spawn(move || {
            let tx2 = tx.clone();
            let id2 = id.clone();
            let mut progress = move |p: catalog::Progress| {
                let _ = tx2.send(Msg::AnalysisProgress(
                    id2.clone(),
                    p.phase.to_string(),
                    p.done,
                    p.total,
                ));
            };
            let r = catalog::analyze(
                &cfg_ai,
                dialect,
                &id,
                &label,
                &schema,
                existing,
                &cancel,
                &mut progress,
            );
            let _ = tx.send(Msg::AnalysisDone(id, r));
        });
    }

    /// Pide a la IA que corrija el SQL actual con el error de la base.
    pub fn fix_with_ai(&mut self) {
        let Some(ws) = self.wss.get_mut(self.cur) else {
            return;
        };
        if ws.error.is_none() {
            self.toast("No hay ningún error que corregir", false);
            return;
        }
        ws.mode = AiMode::Adjust;
        if ws.question.is_empty() {
            ws.question.set_text("Corrige el error de la consulta");
        }
        self.generate();
    }

    // ───────────── ejecución ─────────────

    pub fn run(&mut self) {
        let max = self.cfg.max_rows;
        let tx = self.tx.clone();
        let Some(ws) = self.wss.get_mut(self.cur) else {
            return;
        };
        if ws.busy == Busy::Run {
            return;
        }
        let sql = ws.sql.text();
        if sql.trim().is_empty() {
            self.toast("No hay SQL que ejecutar", false);
            return;
        }
        ws.serial += 1;
        let serial = ws.serial;
        ws.job = serial;
        ws.busy = Busy::Run;
        ws.busy_since = Some(Instant::now());
        ws.cancel = Arc::new(AtomicBool::new(false));
        ws.error = None;
        ws.ran_sql = sql.clone();
        let (cancel, db, id) = (ws.cancel.clone(), ws.db.clone(), ws.spec.info.id.clone());
        std::thread::spawn(move || {
            let r = match db.lock() {
                Ok(mut d) => d.run(&sql, max, &cancel),
                Err(_) => Err(DbError::new("Conexión bloqueada")),
            };
            let _ = tx.send(Msg::Ran(id, serial, r));
        });
    }

    /// Para el trabajo de una pestaña. `Some(true)` si era una ejecución (la conexión se reabre), `None` si no hacía nada.
    fn cancel_tab(&mut self, wi: usize, ti: usize) -> Option<bool> {
        let ws = self.wss.get_mut(wi)?;
        let t = ws.tabs.get_mut(ti)?;
        if t.busy == Busy::No {
            return None;
        }
        t.cancel.store(true, Ordering::Relaxed);
        t.chain = false;
        t.fix_error = None;
        t.phase.clear();
        let was_run = t.busy == Busy::Run;
        t.busy = Busy::No;
        // El número de trabajo nuevo descarta lo que aún llegue del hilo cancelado.
        ws.serial += 1;
        let job = ws.serial;
        ws.tabs[ti].job = job;
        if was_run {
            // No esperamos a que el servidor responda: se descarta el resultado y se abre una conexión nueva.
            ws.db = Arc::new(Mutex::new(db::open(&ws.spec)));
        }
        Some(was_run)
    }

    pub fn cancel_work(&mut self) -> bool {
        let (wi, ti) = match self.wss.get(self.cur) {
            Some(ws) => (self.cur, ws.active),
            None => return false,
        };
        match self.cancel_tab(wi, ti) {
            None => false,
            Some(was_run) => {
                if was_run {
                    self.toast("Consulta cancelada", true);
                }
                true
            }
        }
    }

    // ───────────── pestañas ─────────────

    /// Ejecuta `f` como si la pestaña `ti` de la conexión `wi` fuera la que se ve (para terminar trabajos de pestañas de fondo).
    fn at_tab<R>(&mut self, wi: usize, ti: usize, f: impl FnOnce(&mut Self) -> R) -> R {
        let (cur, focus, prev) = (self.cur, self.focus, self.wss[wi].active);
        self.cur = wi;
        self.wss[wi].active = ti;
        let r = f(self);
        self.cur = cur;
        self.focus = focus;
        if let Some(w) = self.wss.get_mut(wi) {
            w.active = prev;
        }
        r
    }

    /// Pestaña (conexión, índice) a la que pertenece un trabajo.
    fn find_job(&self, id: &str, job: u64) -> Option<(usize, usize)> {
        self.wss.iter().enumerate().find_map(|(wi, w)| {
            if w.spec.info.id != id || job == 0 {
                return None;
            }
            w.tabs.iter().position(|t| t.job == job).map(|ti| (wi, ti))
        })
    }

    pub fn new_tab(&mut self) {
        if let Some(ws) = self.wss.get_mut(self.cur) {
            ws.new_tab();
            self.focus = Focus::Question;
        }
    }

    pub fn switch_tab(&mut self, i: usize) {
        if let Some(ws) = self.wss.get_mut(self.cur)
            && i < ws.tabs.len()
        {
            ws.active = i;
        }
    }

    /// Siguiente (+1) o anterior (-1) pestaña, dando la vuelta.
    pub fn step_tab(&mut self, d: isize) {
        if let Some(ws) = self.wss.get_mut(self.cur) {
            let n = ws.tabs.len() as isize;
            ws.active = (ws.active as isize + d).rem_euclid(n) as usize;
        }
    }

    /// Cierra una pestaña con su SQL y su resultado. Siempre queda una (vacía si era la última).
    pub fn close_tab(&mut self, ti: usize) {
        let wi = self.cur;
        if self.wss.get(wi).is_none_or(|w| ti >= w.tabs.len()) {
            return;
        }
        self.cancel_tab(wi, ti);
        let ws = &mut self.wss[wi];
        let mut t = ws.tabs.remove(ti);
        t.job = 0;
        t.pending_generate = false;
        t.chain = false;
        t.fix_error = None;
        t.phase.clear();
        if ws.tabs.is_empty() {
            ws.tabs.push(Tab::new(ws.next_tab));
            ws.next_tab += 1;
            ws.active = 0;
        } else if ti < ws.active {
            ws.active -= 1;
        } else {
            ws.active = ws.active.min(ws.tabs.len() - 1);
        }
        if !t.is_blank() {
            ws.closed.push(t);
            if ws.closed.len() > CLOSED_KEEP {
                ws.closed.remove(0);
            }
            self.toast("Pestaña cerrada (Alt+U la recupera)", true);
        }
    }

    pub fn reopen_tab(&mut self) {
        let Some(ws) = self.wss.get_mut(self.cur) else {
            return;
        };
        match ws.closed.pop() {
            Some(t) => {
                // Si la única pestaña está vacía, se sustituye en vez de dejarla de relleno.
                if ws.tabs.len() == 1 && ws.tabs[0].is_blank() {
                    ws.tabs.clear();
                }
                ws.tabs.push(t);
                ws.active = ws.tabs.len() - 1;
            }
            None => self.toast("No hay pestañas cerradas que recuperar", false),
        }
    }

    // ───────────── mensajes de hilos ─────────────

    pub fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Connected(id, r, serial) => {
                let Some(ws) = self.wss.iter_mut().find(|w| w.spec.info.id == id) else {
                    return;
                };
                if serial != ws.conn_attempt {
                    return;
                }
                match r {
                    Ok(v) => ws.status = ConnStatus::Ok(v),
                    Err(e) => {
                        ws.status = ConnStatus::Failed(e.clone());
                        ws.schema_state = SchemaState::Failed(e.message.clone());
                    }
                }
            }
            Msg::Schema(id, r, attempt) => {
                let mut waiting: Vec<(usize, usize)> = vec![];
                if let Some((wi, ws)) = self
                    .wss
                    .iter_mut()
                    .enumerate()
                    .find(|(_, w)| w.spec.info.id == id && w.conn_attempt == attempt)
                {
                    match r {
                        Ok(sc) => {
                            ws.schema = Some(Arc::new(sc));
                            ws.schema_state = SchemaState::Ready;
                            for (ti, t) in ws.tabs.iter().enumerate() {
                                if t.pending_generate {
                                    waiting.push((wi, ti));
                                }
                            }
                        }
                        Err(e) => ws.schema_state = SchemaState::Failed(e.message),
                    }
                }
                if let Some(i) = self.wss.iter().position(|w| w.spec.info.id == id) {
                    self.wss[i].last_check = Instant::now();
                    self.start_analysis(i, false);
                }
                for (wi, ti) in waiting {
                    self.at_tab(wi, ti, |a| a.generate());
                }
            }
            Msg::AiText(id, serial, text) => {
                if let Some((wi, ti)) = self.find_job(&id, serial) {
                    let sql = nl2sql::extract_sql(&text);
                    if !sql.is_empty() {
                        self.wss[wi].tabs[ti].sql.stream_set(&sql);
                    }
                }
            }
            Msg::AiDone(id, serial, r) => {
                let Some((wi, ti)) = self.find_job(&id, serial) else {
                    return;
                };
                let mut run = false;
                let mut post_hint = false;
                {
                    let tab = &mut self.wss[wi].tabs[ti];
                    tab.busy = Busy::No;
                    tab.phase.clear();
                    match r {
                        Ok(text) => {
                            let sql = nl2sql::extract_sql(&text);
                            if sql.is_empty() {
                                tab.ai_error =
                                    Some("La IA no devolvió ninguna consulta SQL".into());
                            } else {
                                tab.sql.stream_set(&sql);
                                tab.ai_sql = sql.clone();
                                tab.ai_question = tab.question.text();
                                tab.learned = false;
                                tab.mode = AiMode::Adjust;
                                run = true;
                            }
                        }
                        Err(e) => {
                            tab.ai_error = Some(e);
                            tab.chain = false;
                            post_hint = true;
                        }
                    }
                }
                let visible = wi == self.cur && self.wss[wi].active == ti;
                if post_hint && visible {
                    let q = self.wss[wi].question.text();
                    if let Some((idx, msg)) = self.cross_hint(&q, false) {
                        let ws = &mut self.wss[wi];
                        let base = ws.ai_error.take().unwrap_or_default();
                        ws.ai_error = Some(format!("{base} — {msg}"));
                        ws.suggest = Some(idx);
                    }
                }
                if run {
                    if visible {
                        self.focus = Focus::Sql;
                    }
                    self.at_tab(wi, ti, |a| a.run());
                }
            }
            Msg::Ran(id, serial, r) => {
                let Some((wi, ti)) = self.find_job(&id, serial) else {
                    return;
                };
                let mut regenerate = false;
                {
                    let tab = &mut self.wss[wi].tabs[ti];
                    tab.busy = Busy::No;
                    match r {
                        Ok(rs) => {
                            if !tab.learned
                                && !tab.ai_sql.is_empty()
                                && !rs.rows.is_empty()
                                && tab.ran_sql.trim() != tab.ai_sql.trim()
                            {
                                // El usuario corrigió el SQL de la IA y funciona: se recuerda para preguntas parecidas.
                                history::add(
                                    &id,
                                    &tab.ai_question,
                                    &tab.ran_sql,
                                    "corregida por el usuario",
                                );
                                tab.learned = true;
                            }
                            tab.error = None;
                            tab.result = Some(rs);
                            tab.rebuild_view();
                            tab.error_sql.clear();
                            tab.chain = false;
                            tab.fix_error = None;
                        }
                        Err(e) => {
                            if tab.chain
                                && !e.connection
                                && tab.repair < MAX_REPAIRS
                                && !e.message.contains("cancelad")
                            {
                                // Corrección silenciosa: el usuario no ve el error intermedio.
                                tab.repair += 1;
                                tab.fix_error = Some(e);
                                regenerate = true;
                            } else {
                                tab.chain = false;
                                tab.error_sql = tab.ran_sql.clone();
                                if let Some(p) = e.position {
                                    let lead = tab
                                        .ran_sql
                                        .chars()
                                        .take_while(|c| c.is_whitespace())
                                        .count();
                                    let pos = tab.sql.pos_of_offset(p + lead);
                                    tab.sql.move_cursor_to(pos);
                                }
                                tab.error = Some(e);
                            }
                        }
                    }
                }
                if regenerate {
                    self.at_tab(wi, ti, |a| a.generate_inner());
                }
            }
            Msg::AnalysisProgress(id, phase, d, t) => {
                if let Some(ws) = self.wss.iter_mut().find(|w| w.spec.info.id == id) {
                    if let Analysis::Running(p) = &mut ws.analysis {
                        p.phase = phase;
                        p.done = d;
                        p.total = t;
                    }
                }
            }
            Msg::AnalysisDone(id, r) => {
                let mut toast = None;
                if let Some(ws) = self.wss.iter_mut().find(|w| w.spec.info.id == id) {
                    match r {
                        Ok(c) => {
                            ws.catalog = Some(Arc::new(c));
                            ws.analysis = Analysis::Done;
                            toast = Some((
                                format!(
                                    "Análisis de «{}» terminado: la IA ya conoce para qué sirve cada tabla",
                                    ws.spec.info.label
                                ),
                                true,
                            ));
                        }
                        Err(e) => {
                            if let Some(c) = catalog::load(&id) {
                                ws.catalog = Some(Arc::new(c));
                            }
                            if !ws.analysis_cancel.load(Ordering::Relaxed) {
                                ws.analysis = Analysis::Failed(e.clone());
                                toast = Some((format!("No se pudo analizar la base: {e}"), false));
                            }
                        }
                    }
                }
                if let Some((t, good)) = toast {
                    self.toast(t, good);
                }
            }
            Msg::Recheck(id, r) => {
                let mut start = None;
                let mut note = None;
                if let Some((i, ws)) = self
                    .wss
                    .iter_mut()
                    .enumerate()
                    .find(|(_, w)| w.spec.info.id == id)
                {
                    ws.rechecking = false;
                    if let Ok(new) = r {
                        let (n_new, n_chg, n_del) = schema_changes(ws.schema.as_deref(), &new);
                        let changed = n_new + n_chg + n_del;
                        let df = catalog::Diff {
                            new_tables: n_new,
                            changed_tables: n_chg,
                            removed_tables: n_del,
                        };
                        let same = ws.schema.as_ref().is_some_and(|old| **old == new);
                        if !same {
                            ws.schema = Some(Arc::new(new));
                        }
                        if changed > 0 {
                            note = Some(format!(
                                "Estructura de «{}» cambiada: {} tablas nuevas, {} modificadas, {} eliminadas; actualizando lo que la IA sabe",
                                ws.spec.info.label,
                                df.new_tables,
                                df.changed_tables,
                                df.removed_tables
                            ));
                            start = Some(i);
                        }
                    }
                }
                if let Some(t) = note {
                    self.toast(t, true);
                }
                if let Some(i) = start {
                    self.start_analysis(i, false);
                }
            }
            Msg::Plan(id, serial, plan, samples) => {
                if let Some((wi, ti)) = self.find_job(&id, serial) {
                    let tab = &mut self.wss[wi].tabs[ti];
                    tab.plan = plan;
                    tab.samples = Some(samples);
                }
            }
            Msg::Step(id, serial, text) => {
                if let Some((wi, ti)) = self.find_job(&id, serial) {
                    let tab = &mut self.wss[wi].tabs[ti];
                    tab.phase = text.clone();
                    if tab.steps.last() != Some(&text) {
                        tab.steps.push(text);
                        if tab.steps.len() > 40 {
                            tab.steps.remove(0);
                        }
                    }
                }
            }
            Msg::TableNotes(id, notes) => {
                if let Some(ws) = self.wss.iter_mut().find(|w| w.spec.info.id == id) {
                    let mut cat: Catalog = ws
                        .catalog
                        .as_ref()
                        .map(|c| (**c).clone())
                        .unwrap_or_default();
                    for (t, d) in notes {
                        cat.tables.insert(t, d);
                    }
                    let _ = catalog::save(&id, &cat);
                    ws.catalog = Some(Arc::new(cat));
                }
            }
            Msg::Learned(id, serial, lessons) => {
                if let Some((wi, ti)) = self.find_job(&id, serial) {
                    self.wss[wi].tabs[ti].lessons = lessons;
                }
            }
            Msg::Phase(id, serial, phase) => {
                if let Some((wi, ti)) = self.find_job(&id, serial) {
                    self.wss[wi].tabs[ti].phase = phase;
                }
            }
            Msg::FormTest(r, serial) => {
                if serial != self.form_serial {
                    return;
                }
                if let Dialog::Form(f) = &mut self.dialog {
                    f.test = match r {
                        Ok((info, ms)) => TestState::Ok { info, ms },
                        Err(e) => TestState::Err(e),
                    };
                }
            }
        }
    }

    // ───────────── formulario de conexión ─────────────

    fn form_spec(&self, f: &Form) -> Result<ConnSpec, String> {
        let ids: Vec<String> = self
            .cfg
            .connections
            .iter()
            .map(|c| c.info.id.clone())
            .collect();
        let (python, bridge) = self
            .wss
            .first()
            .map(|w| (w.spec.python.clone(), w.spec.bridge.clone()))
            .or_else(|| {
                self.cfg
                    .connections
                    .first()
                    .map(|c| (c.python.clone(), c.bridge.clone()))
            })
            .unwrap_or_else(|| (self.cfg.python.clone(), self.cfg.bridge.clone()));
        let (python, bridge) = if python.is_empty() {
            (self.cfg.python.clone(), self.cfg.bridge.clone())
        } else {
            (python, bridge)
        };
        f.to_spec(&ids, self.cfg.timeout_secs, &python, &bridge)
    }

    fn form_test(&mut self) {
        let Dialog::Form(f) = &self.dialog else {
            return;
        };
        let spec = match self.form_spec(f) {
            Ok(s) => s,
            Err(e) => {
                if let Dialog::Form(f) = &mut self.dialog {
                    f.error = Some(e);
                }
                return;
            }
        };
        self.form_serial += 1;
        let serial = self.form_serial;
        let tx = self.tx.clone();
        if let Dialog::Form(f) = &mut self.dialog {
            f.error = None;
            f.test = TestState::Running;
        }
        std::thread::spawn(move || {
            let t = Instant::now();
            let mut d = db::open(&spec);
            let r = d.check().map(|v| (v, t.elapsed().as_millis() as u64));
            let _ = tx.send(Msg::FormTest(r, serial));
        });
    }

    fn form_save(&mut self) {
        let Dialog::Form(f) = &self.dialog else {
            return;
        };
        let spec = match self.form_spec(f) {
            Ok(s) => s,
            Err(e) => {
                if let Dialog::Form(f) = &mut self.dialog {
                    f.error = Some(e);
                }
                return;
            }
        };
        let editing = f.editing.clone();
        let id = spec.info.id.clone();
        match &editing {
            Some(old) => {
                if let Some(i) = self.cfg.connections.iter().position(|c| &c.info.id == old) {
                    self.cfg.connections[i] = spec.clone();
                    let ws = Ws::new(spec);
                    let old_ws = std::mem::replace(&mut self.wss[i], ws);
                    // Conserva lo escrito por el usuario: todas las pestañas, sin trabajos a medias.
                    let nw = &mut self.wss[i];
                    nw.tabs = old_ws.tabs;
                    nw.active = old_ws.active;
                    nw.next_tab = old_ws.next_tab;
                    nw.closed = old_ws.closed;
                    for t in &mut nw.tabs {
                        t.cancel.store(true, Ordering::Relaxed);
                        t.busy = Busy::No;
                        t.chain = false;
                        t.pending_generate = false;
                        t.phase.clear();
                        t.job = 0;
                    }
                    nw.pinned = old_ws.pinned;
                    self.cur = i;
                }
            }
            None => {
                self.cfg.connections.push(spec.clone());
                self.wss.push(Ws::new(spec));
                self.cur = self.wss.len() - 1;
            }
        }
        self.dialog = Dialog::None;
        match config::save(&self.cfg) {
            Ok(()) => self.toast(format!("Conexión «{id}» guardada"), true),
            Err(e) => self.toast(format!("No se pudo guardar la configuración: {e}"), false),
        }
        self.reconnect_current();
        self.focus = Focus::Question;
    }

    fn delete_conn(&mut self, i: usize) {
        if i >= self.wss.len() {
            return;
        }
        let name = self.wss[i].spec.info.label.clone();
        self.wss.remove(i);
        self.cfg.connections.remove(i);
        self.cur = self.cur.min(self.wss.len().saturating_sub(1));
        self.dialog = Dialog::None;
        match config::save(&self.cfg) {
            Ok(()) => self.toast(format!("Conexión «{name}» borrada"), true),
            Err(e) => self.toast(format!("No se pudo guardar la configuración: {e}"), false),
        }
        if self.wss.is_empty() {
            self.dialog = Dialog::Form(Box::new(Form::new_blank()));
        } else {
            self.connect_current();
        }
    }

    /// Palabras de la pregunta que podrían ser nombres de tabla (minúsculas, ≥ 4 letras).
    fn tokens(question: &str, explicit_only: bool) -> Vec<String> {
        let words: Vec<String> = question
            .to_lowercase()
            .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
            .filter(|w| !w.is_empty())
            .map(|w| w.trim_matches('.').to_string())
            .collect();
        if !explicit_only {
            return words
                .into_iter()
                .filter(|w| w.chars().count() >= 6)
                .collect();
        }
        // «la tabla X», «tabla de X», «la tabla llamada X»…
        let mut out = vec![];
        for (i, w) in words.iter().enumerate() {
            if w == "tabla" || w == "tablas" || w == "table" {
                for nxt in words.iter().skip(i + 1).take(3) {
                    if !matches!(
                        nxt.as_str(),
                        "de" | "la" | "el" | "los" | "las" | "llamada" | "que" | "se" | "es"
                    ) {
                        out.push(nxt.clone());
                        break;
                    }
                }
            }
        }
        out
    }

    fn has_table(schema: &Schema, token: &str) -> bool {
        let sing = token.strip_suffix('s').unwrap_or(token);
        schema.tables.iter().any(|t| {
            let n = t.name.to_lowercase();
            n == token || n == sing || t.qualified().to_lowercase() == token
        })
    }

    /// Si la pregunta nombra una tabla que NO existe en la conexión actual pero sí en otra, devuelve (índice, mensaje).
    pub fn cross_hint(&self, question: &str, explicit_only: bool) -> Option<(usize, String)> {
        let cur = self.wss.get(self.cur)?;
        let cur_schema = cur.schema.as_ref()?;
        for tok in Self::tokens(question, explicit_only) {
            if Self::has_table(cur_schema, &tok) {
                continue;
            }
            // Todas las conexiones que sí tienen la tabla; primero las del mismo entorno (PROD con PROD).
            let sing = tok.strip_suffix('s').unwrap_or(&tok).to_string();
            let mut found: Vec<(usize, String)> = vec![];
            for (i, ws) in self.wss.iter().enumerate() {
                if i == self.cur {
                    continue;
                }
                let Some(sc) = &ws.schema else { continue };
                if let Some(t) = sc.tables.iter().find(|t| {
                    let n = t.name.to_lowercase();
                    n == tok || n == sing
                }) {
                    found.push((i, t.qualified()));
                }
            }
            if found.is_empty() {
                continue;
            }
            let env = cur.spec.info.env;
            found.sort_by_key(|(i, _)| (self.wss[*i].spec.info.env != env) as u8);
            let (best, qual) = found[0].clone();
            let others = found
                .iter()
                .skip(1)
                .map(|(i, _)| format!("«{}»", self.wss[*i].spec.info.label))
                .collect::<Vec<_>>();
            let also = if others.is_empty() {
                String::new()
            } else {
                format!(" (también en {})", others.join(", "))
            };
            return Some((
                best,
                format!(
                    "«{}» no existe en «{}» ({}), pero sí en «{}» ({}): {}{}",
                    tok,
                    cur.spec.info.label,
                    cur.spec.info.dialect.name(),
                    self.wss[best].spec.info.label,
                    self.wss[best].spec.info.dialect.name(),
                    qual,
                    also
                ),
            ));
        }
        None
    }

    /// Pasa la pregunta a otra conexión y la lanza allí.
    fn switch_and_ask(&mut self, i: usize) {
        let Some(q) = self.wss.get(self.cur).map(|w| w.question.text()) else {
            return;
        };
        if i >= self.wss.len() {
            return;
        }
        if let Some(ws) = self.wss.get_mut(self.cur) {
            ws.suggest = None;
            ws.ai_error = None;
        }
        self.select_conn(i);
        if let Some(ws) = self.wss.get_mut(i) {
            // Pestaña nueva en la otra conexión: no pisa lo que ya hubiera escrito allí.
            ws.new_tab();
            ws.question.set_text(&q);
            ws.mode = AiMode::New;
        }
        self.focus = Focus::Question;
        self.generate();
    }

    /// Añade lo que el agente descubrió a las notas de la base (sección «Aprendido»).
    fn save_lessons(&mut self) {
        let Some(ws) = self.wss.get_mut(self.cur) else {
            return;
        };
        if ws.lessons.is_empty() {
            self.toast("No hay nada nuevo que guardar", false);
            return;
        }
        let id = ws.spec.info.id.clone();
        let mut text = notes::load(&id);
        if !text.contains("## Aprendido") {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str("\n## Aprendido (confirmado al consultar)\n");
        }
        for l in &ws.lessons {
            if !text.contains(l.as_str()) {
                text.push_str(&format!("- {l}\n"));
            }
        }
        let n = ws.lessons.len();
        ws.lessons.clear();
        match notes::save(&id, &text) {
            Ok(()) => self.toast(
                format!("{n} hecho(s) guardado(s) en las notas de la base (F3 para revisarlos)"),
                true,
            ),
            Err(e) => self.toast(format!("No se pudo guardar: {e}"), false),
        }
    }

    /// Detalle completo de la última investigación (nada recortado).
    pub fn open_info(&mut self) {
        let Some(ws) = self.wss.get(self.cur) else {
            return;
        };
        let mut t = String::new();
        if let Some(p) = &ws.plan {
            t.push_str(&format!("Tablas elegidas: {}\n", p.tables.join(", ")));
            if !p.reason.is_empty() {
                t.push_str(&format!("Por qué: {}\n", p.reason));
            }
        }
        if !ws.steps.is_empty() {
            t.push_str("\nInvestigación paso a paso:\n");
            for (i, st) in ws.steps.iter().enumerate() {
                t.push_str(&format!("  {}. {st}\n", i + 1));
            }
        }
        if !ws.lessons.is_empty() {
            t.push_str("\nLa IA ha descubierto (F6 los guarda en las notas):\n");
            for l in &ws.lessons {
                t.push_str(&format!("  • {l}\n"));
            }
        }
        if let Some(e) = &ws.ai_error {
            t.push_str(&format!("\nMensaje: {e}\n"));
        }
        if t.is_empty() {
            t = "Todavía no hay investigación que mostrar: pregunta algo primero.".into();
        }
        self.dialog = Dialog::Info(t);
    }

    pub fn open_notes(&mut self) {
        let Some(ws) = self.wss.get(self.cur) else {
            return;
        };
        let text = notes::load(&ws.spec.info.id);
        self.dialog = Dialog::Notes(Box::new(NotesDlg {
            conn: self.cur,
            ed: Editor::new(&text),
        }));
    }

    fn save_notes(&mut self) {
        let Dialog::Notes(n) = &self.dialog else {
            return;
        };
        let (conn, text) = (n.conn, n.ed.text());
        let Some(ws) = self.wss.get(conn) else { return };
        let r = notes::save(&ws.spec.info.id, &text);
        self.dialog = Dialog::None;
        match r {
            Ok(()) => self.toast("Notas guardadas: la IA las tendrá en cuenta", true),
            Err(e) => self.toast(format!("No se pudieron guardar las notas: {e}"), false),
        }
    }

    pub fn open_edit(&mut self, i: usize) {
        if let Some(w) = self.wss.get(i) {
            self.dialog = Dialog::Form(Box::new(Form::from_spec(&w.spec)));
        }
    }

    // ───────────── eventos ─────────────

    pub fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                self.on_key(k)
            }
            Event::Mouse(m) => self.on_mouse(m),
            Event::Paste(s) => self.on_paste(&s),
            _ => {}
        }
    }

    fn on_paste(&mut self, s: &str) {
        match &mut self.dialog {
            Dialog::Form(f) => f.paste(s),
            Dialog::Notes(n) => n.ed.insert_str(s),
            Dialog::None => {
                let focus = self.focus;
                if let Some(ws) = self.wss.get_mut(self.cur) {
                    match focus {
                        Focus::Question => ws.question.insert_str(s),
                        Focus::Sql => ws.sql.insert_str(s),
                        Focus::Tables => {
                            ws.tables_filter.push_str(s.lines().next().unwrap_or(""));
                            ws.tables_sel = 0;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn do_act(&mut self, a: Act) {
        match a {
            Act::Conn(i) => {
                self.select_conn(i);
                // Lo siguiente que se hace es preguntar: el foco va a la pregunta, no se queda en la lista.
                self.focus = Focus::Question;
            }
            Act::NewConn => self.dialog = Dialog::Form(Box::new(Form::new_blank())),
            Act::EditConn(i) => self.open_edit(i),
            Act::DelConn(i) => {
                if i < self.wss.len() {
                    self.dialog = Dialog::ConfirmDelete(i)
                }
            }
            Act::Focus(f) => self.focus = f,
            Act::Generate => self.generate(),
            Act::ToggleMode => {
                if let Some(ws) = self.ws_mut() {
                    ws.mode = if ws.mode == AiMode::New {
                        AiMode::Adjust
                    } else {
                        AiMode::New
                    };
                }
            }
            Act::Tab(i) => self.switch_tab(i),
            Act::CloseTab(i) => self.close_tab(i),
            Act::NewTab => self.new_tab(),
            Act::Run => self.run(),
            Act::Fix => self.fix_with_ai(),
            Act::Clear => {
                if let Some(ws) = self.ws_mut() {
                    ws.sql.checkpoint();
                    ws.sql.stream_set("");
                    ws.error = None;
                    ws.mode = AiMode::New;
                }
                self.focus = Focus::Sql;
            }
            Act::Table(i) => {
                self.focus = Focus::Tables;
                if let Some(ws) = self.ws_mut() {
                    ws.tables_sel = i;
                }
            }
            Act::Pin(i) => self.toggle_pin(i),
            Act::Column(i) => self.insert_column(i),
            Act::Cell(r, c) => {
                self.focus = Focus::Result;
                if let Some(ws) = self.ws_mut() {
                    ws.tv.row = r;
                    ws.tv.col = c;
                }
            }
            Act::Sort(c) => {
                if let Some(ws) = self.ws_mut() {
                    ws.sort_by(c);
                }
            }
            Act::Help => self.dialog = Dialog::Help,
            Act::Quit => self.quit = true,
            Act::Cancel => {
                self.cancel_work();
            }
            Act::Export => self.export(),
            Act::Notes => self.open_notes(),
            Act::SaveLessons => self.save_lessons(),
            Act::SwitchAsk(i) => self.switch_and_ask(i),
            Act::Info => self.open_info(),
            Act::NotesSave => self.save_notes(),
            Act::Reanalyze => {
                let i = self.cur;
                self.start_analysis(i, true);
                self.toast("Analizando de nuevo todas las tablas…", true);
            }
            Act::ClearFilter => {
                if let Some(ws) = self.ws_mut() {
                    ws.tables_filter.clear();
                }
            }
            Act::FormItem(i) => {
                if let Dialog::Form(f) = &mut self.dialog {
                    f.focus = i;
                }
            }
            Act::FormPick(item, idx) => {
                if let Dialog::Form(f) = &mut self.dialog {
                    f.pick(item, idx);
                }
            }
            Act::FormTest => self.form_test(),
            Act::FormSave => self.form_save(),
            Act::FormCancel | Act::CloseDialog => {
                if self.wss.is_empty() && matches!(self.dialog, Dialog::Form(_)) {
                    self.quit = true;
                }
                self.dialog = Dialog::None;
            }
            Act::ConfirmYes => {
                if let Dialog::ConfirmDelete(i) = self.dialog {
                    self.delete_conn(i);
                }
            }
            Act::ConfirmNo => self.dialog = Dialog::None,
        }
    }

    fn toggle_pin(&mut self, vis_idx: usize) {
        let Some(ws) = self.wss.get_mut(self.cur) else {
            return;
        };
        let list = ws.filtered_tables();
        let (Some(schema), Some(&ti)) = (ws.schema.clone(), list.get(vis_idx)) else {
            return;
        };
        let name = schema.tables[ti].qualified();
        if !ws.pinned.remove(&name) {
            ws.pinned.insert(name);
        }
    }

    /// Escribe en el SQL el nombre de la columna `ci` de la tabla elegida en el panel «Tablas».
    fn insert_column(&mut self, ci: usize) {
        let Some(ws) = self.wss.get_mut(self.cur) else {
            return;
        };
        let list = ws.filtered_tables();
        let name = ws
            .schema
            .as_ref()
            .and_then(|s| list.get(ws.tables_sel).map(|&i| &s.tables[i]))
            .and_then(|t| t.columns.get(ci))
            .map(|c| c.name.clone());
        if let Some(n) = name {
            ws.sql.insert_at_cursor(&n);
            self.focus = Focus::Sql;
        }
    }

    /// ¿Hay un borde redimensionable bajo el puntero?
    fn border_at(&self, x: u16, y: u16) -> Option<Drag> {
        let g = &self.geo;
        let within = |r: Rect, x: u16, y: u16| {
            x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
        };
        if within(g.side, g.side.x + g.side.width - 1, y)
            && (x == g.side.x + g.side.width.saturating_sub(1) || x == g.main.x)
        {
            return Some(Drag::Side);
        }
        if !self.zoom {
            let inside_main = x >= g.main.x && x < g.main.x + g.main.width;
            if inside_main && y == g.q.y + g.q.height.saturating_sub(1) {
                return Some(Drag::Question);
            }
            if inside_main && y == g.s.y + g.s.height.saturating_sub(1) {
                return Some(Drag::Sql);
            }
        }
        let inside_side = x >= g.side.x && x < g.side.x + g.side.width;
        if inside_side && y == g.conns.y + g.conns.height.saturating_sub(1) {
            return Some(Drag::Conns);
        }
        if inside_side && g.cols.height > 0 && y == g.cols.y {
            return Some(Drag::Cols);
        }
        None
    }

    fn apply_drag(&mut self, d: Drag, x: u16, y: u16) {
        let g = self.geo;
        match d {
            Drag::Side => {
                self.side_w = Some(
                    (x + 1)
                        .saturating_sub(g.side.x)
                        .clamp(24, g.area.width.saturating_sub(40).max(24)),
                )
            }
            Drag::Question => {
                self.q_h = Some(
                    (y + 1)
                        .saturating_sub(g.q.y)
                        .clamp(5, g.main.height.saturating_sub(10).max(5)),
                )
            }
            Drag::Sql => {
                self.sql_h = Some(
                    (y + 1)
                        .saturating_sub(g.s.y)
                        .clamp(6, g.main.height.saturating_sub(8).max(6)),
                )
            }
            Drag::Conns => {
                self.conns_h = Some(
                    (y + 1)
                        .saturating_sub(g.conns.y)
                        .clamp(5, g.side.height.saturating_sub(6).max(5)),
                )
            }
            Drag::Cols => {
                self.cols_h = Some(
                    (g.cols.y + g.cols.height)
                        .saturating_sub(y)
                        .min(g.side.height.saturating_sub(8)),
                )
            }
        }
    }

    /// Diagnóstico: si existe `~/.config/herdr-dbview/debug-mouse`, cada clic/rueda se anota en
    /// `~/.local/share/herdr-dbview/mouse.log` con lo que llegó (celda), el tamaño de pantalla y a qué acción cayó.
    fn log_mouse(&self, m: &MouseEvent) {
        if matches!(m.kind, MouseEventKind::Moved) {
            return;
        }
        let base = |var: &str, rest: &str| {
            std::env::var_os(var)
                .filter(|p| !p.is_empty())
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(rest))
                })
                .map(|p| p.join("herdr-dbview"))
        };
        let (Some(cfg), Some(data)) = (
            base("XDG_CONFIG_HOME", ".config").map(|d| d.join("debug-mouse")),
            base("XDG_DATA_HOME", ".local/share"),
        ) else {
            return;
        };
        if !cfg.exists() {
            return;
        }
        let hit = self
            .hits
            .iter()
            .rev()
            .find(|(r, _)| {
                m.column >= r.x
                    && m.column < r.x + r.width
                    && m.row >= r.y
                    && m.row < r.y + r.height
            })
            .map(|(_, a)| format!("{a:?}"))
            .unwrap_or_else(|| "-".into());
        let g = &self.geo;
        let line = format!(
            "{:?} col={} fila={} | pantalla {}x{} lateral x={} w={} y={} | foco={:?} cur={} dialogo={} | cae en: {} | borde: {:?}\n",
            m.kind,
            m.column,
            m.row,
            g.area.width,
            g.area.height,
            g.side.x,
            g.side.width,
            g.side.y,
            self.focus,
            self.cur,
            !matches!(self.dialog, Dialog::None),
            hit,
            self.border_at(m.column, m.row),
        );
        let path = data.join("mouse.log");
        if std::fs::metadata(&path).is_ok_and(|md| md.len() > 200_000) {
            let _ = std::fs::remove_file(&path);
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        self.log_mouse(&m);
        let (x, y) = (m.column, m.row);
        match m.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(d) = self.drag {
                    self.apply_drag(d, x, y);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => self.drag = None,
            // Clic derecho sobre una tabla: la fija o desfija para la IA (★).
            MouseEventKind::Down(MouseButton::Right) if matches!(self.dialog, Dialog::None) => {
                let hit = self
                    .hits
                    .iter()
                    .rev()
                    .find(|(r, a)| {
                        matches!(a, Act::Table(_) | Act::Pin(_))
                            && x >= r.x
                            && x < r.x + r.width
                            && y >= r.y
                            && y < r.y + r.height
                    })
                    .map(|(_, a)| a.clone());
                if let Some(Act::Table(i) | Act::Pin(i)) = hit {
                    self.toggle_pin(i);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if matches!(self.dialog, Dialog::None) && !self.wss.is_empty() {
                    if let Some(d) = self.border_at(x, y) {
                        self.drag = Some(d);
                        return;
                    }
                }
                let hit = self
                    .hits
                    .iter()
                    .rev()
                    .find(|(r, _)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
                    .map(|(_, a)| a.clone());
                if let Some(a) = hit
                    && (matches!(self.dialog, Dialog::None)
                        || matches!(
                            a,
                            Act::FormItem(_)
                                | Act::FormPick(..)
                                | Act::FormTest
                                | Act::FormSave
                                | Act::FormCancel
                                | Act::ConfirmYes
                                | Act::ConfirmNo
                                | Act::CloseDialog
                        ))
                {
                    // Pulsar dentro de un editor también coloca el cursor donde se pulsó.
                    let editor = match &a {
                        Act::Focus(Focus::Question) => Some(true),
                        Act::Focus(Focus::Sql) => Some(false),
                        _ => None,
                    };
                    self.do_act(a);
                    if let (Some(question), Some(ws), true) = (
                        editor,
                        self.wss.get_mut(self.cur),
                        matches!(self.dialog, Dialog::None),
                    ) {
                        if question {
                            ws.question.click(x, y, false);
                        } else {
                            ws.sql.click(x, y, false);
                        }
                    }
                    return;
                }
                if !matches!(self.dialog, Dialog::None) {
                    return;
                }
                if let Some(ws) = self.wss.get_mut(self.cur) {
                    if ws.question.click(x, y, false) {
                        self.focus = Focus::Question;
                    } else if ws.sql.click(x, y, false) {
                        self.focus = Focus::Sql;
                    }
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let d: isize = if matches!(m.kind, MouseEventKind::ScrollUp) {
                    -1
                } else {
                    1
                };
                if !matches!(self.dialog, Dialog::None) {
                    return;
                }
                let inside =
                    |r: Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
                if inside(self.geo.cols) || inside(self.geo.tables) {
                    let in_cols = inside(self.geo.cols);
                    if let Some(ws) = self.wss.get_mut(self.cur) {
                        if in_cols {
                            // El tope por abajo lo recorta el dibujo, que sabe cuántas filas caben.
                            ws.cols_top = (ws.cols_top as isize + d * 3).max(0) as usize;
                        } else {
                            let n = ws.filtered_tables().len();
                            ws.tables_sel = (ws.tables_sel as isize + d * 3)
                                .clamp(0, n.saturating_sub(1) as isize)
                                as usize;
                        }
                    }
                    return;
                }
                if let Some(ws) = self.wss.get_mut(self.cur) {
                    if inside(ws.sql.area) {
                        ws.sql.scroll_by(d * 3);
                    } else if inside(ws.question.area) {
                        ws.question.scroll_by(d * 3);
                    } else {
                        let hit = self
                            .hits
                            .iter()
                            .any(|(r, a)| inside(*r) && matches!(a, Act::Cell(..) | Act::Sort(_)));
                        if hit {
                            ws.tv.top = (ws.tv.top as isize + d * 3)
                                .clamp(0, ws.tv.order.len().saturating_sub(1) as isize)
                                as usize;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        // Diálogos: reciben todo.
        match &mut self.dialog {
            Dialog::Form(f) => {
                let a = f.on_key(k);
                match a {
                    FormAct::None => {}
                    FormAct::Test => self.form_test(),
                    FormAct::Save => self.form_save(),
                    FormAct::Cancel => self.do_act(Act::FormCancel),
                }
                return;
            }
            Dialog::ConfirmDelete(_) => {
                // Borrar exige Ctrl+D o pulsar el botón: ni Enter ni una letra suelta (al teclear una pregunta
                // «s», «y» o Enter confirmaban sin querer). Enter y Esc cancelan.
                match k.code {
                    KeyCode::Char('d') if ctrl => self.do_act(Act::ConfirmYes),
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('n') => {
                        self.do_act(Act::ConfirmNo)
                    }
                    _ => {}
                }
                return;
            }
            Dialog::Notes(n) => {
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                match k.code {
                    KeyCode::Esc => self.dialog = Dialog::None,
                    KeyCode::Char('s') if ctrl => self.save_notes(),
                    _ => {
                        n.ed.handle_key(k);
                    }
                }
                return;
            }
            Dialog::Help | Dialog::Cell(_) | Dialog::Info(_) => {
                if let (Dialog::Cell(t), KeyCode::Char('y')) = (&self.dialog, k.code) {
                    let t = t.clone();
                    self.copy(&t);
                }
                if matches!(
                    k.code,
                    KeyCode::Esc
                        | KeyCode::Enter
                        | KeyCode::Char('q')
                        | KeyCode::F(1)
                        | KeyCode::Char('?')
                        | KeyCode::Char(' ')
                ) {
                    self.dialog = Dialog::None;
                }
                return;
            }
            Dialog::None => {}
        }
        // Globales.
        match k.code {
            KeyCode::Char('q') if ctrl => return self.quit = true,
            KeyCode::F(1) => return self.dialog = Dialog::Help,
            KeyCode::F(2) => return self.open_edit(self.cur),
            KeyCode::F(3) => return self.open_notes(),
            KeyCode::F(4) => return self.do_act(Act::Reanalyze),
            KeyCode::F(6) => return self.do_act(Act::SaveLessons),
            KeyCode::F(7) => {
                if let Some(i) = self.ws().and_then(|w| w.suggest) {
                    return self.switch_and_ask(i);
                }
                return;
            }
            KeyCode::Char(',') if alt => {
                return self.side_w = Some(self.geo.side.width.saturating_sub(2).max(24));
            }
            KeyCode::Char('.') if alt => return self.side_w = Some(self.geo.side.width + 2),
            KeyCode::Char('[') if alt => {
                return self.q_h = Some(self.geo.q.height.saturating_sub(1).max(5));
            }
            KeyCode::Char(']') if alt => return self.q_h = Some(self.geo.q.height + 1),
            KeyCode::Char('-') if alt => {
                return self.sql_h = Some(self.geo.s.height.saturating_sub(2).max(6));
            }
            KeyCode::Char('=') if alt => return self.sql_h = Some(self.geo.s.height + 2),
            KeyCode::Char('z') if alt => return self.zoom = !self.zoom,
            KeyCode::Char('t') if alt => return self.new_tab(),
            KeyCode::Char('w') if alt => {
                let i = self.ws().map(|w| w.active).unwrap_or(0);
                return self.close_tab(i);
            }
            KeyCode::Char('u') if alt => return self.reopen_tab(),
            KeyCode::PageDown if ctrl || alt => return self.step_tab(1),
            KeyCode::PageUp if ctrl || alt => return self.step_tab(-1),
            KeyCode::Char('0') if alt => {
                self.side_w = None;
                self.q_h = None;
                self.sql_h = None;
                self.conns_h = None;
                self.cols_h = None;
                self.zoom = false;
                return;
            }
            KeyCode::F(8) => return self.open_info(),
            KeyCode::F(5) => return self.run(),
            KeyCode::Char('r') if ctrl => return self.run(),
            KeyCode::Char('g') if ctrl => return self.generate(),
            KeyCode::Char('e') if ctrl => return self.fix_with_ai(),
            KeyCode::Char('n') if ctrl => return self.do_act(Act::NewConn),
            KeyCode::Char('l') if ctrl => return self.reconnect_current(),
            KeyCode::Char('t') if ctrl => return self.do_act(Act::ToggleMode),
            KeyCode::Tab => return self.cycle_focus(1),
            KeyCode::BackTab => return self.cycle_focus(-1),
            KeyCode::Up if alt => return self.select_conn(self.cur.saturating_sub(1)),
            KeyCode::Down if alt => {
                return self.select_conn((self.cur + 1).min(self.wss.len().saturating_sub(1)));
            }
            KeyCode::Char(c @ '1'..='9') if alt => {
                return self.select_conn(c as usize - '1' as usize);
            }
            KeyCode::Esc if self.cancel_work() => return,
            KeyCode::Esc => {
                if self.focus == Focus::Tables
                    && self.ws().is_some_and(|w| !w.tables_filter.is_empty())
                {
                    return self.do_act(Act::ClearFilter);
                }
                return self.quit = true;
            }
            _ => {}
        }
        match self.focus {
            Focus::Conns => self.key_conns(k),
            Focus::Tables => self.key_tables(k),
            Focus::Question => self.key_question(k),
            Focus::Sql => self.key_sql(k),
            Focus::Result => self.key_result(k),
        }
    }

    fn cycle_focus(&mut self, d: isize) {
        let i = ORDER.iter().position(|f| *f == self.focus).unwrap_or(2) as isize;
        self.focus = ORDER[(i + d).rem_euclid(ORDER.len() as isize) as usize];
    }

    fn key_conns(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Up => self.select_conn(self.cur.saturating_sub(1)),
            KeyCode::Down => self.select_conn((self.cur + 1).min(self.wss.len().saturating_sub(1))),
            KeyCode::Enter => self.focus = Focus::Question,
            // Solo teclas que no se escriben por accidente: nada de letras sueltas que borren o cierren.
            // (Nueva: Ctrl+N · editar: F2 · reconectar: Ctrl+L · salir: Ctrl+Q.)
            KeyCode::Delete => self.do_act(Act::DelConn(self.cur)),
            // Cualquier letra que se teclee es que el usuario quiere escribir: pasa a la pregunta.
            KeyCode::Char(_) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.focus = Focus::Question;
                self.key_question(k);
            }
            _ => {}
        }
    }

    fn key_tables(&mut self, k: KeyEvent) {
        let Some(ws) = self.wss.get_mut(self.cur) else {
            return;
        };
        let n = ws.filtered_tables().len();
        match k.code {
            KeyCode::Up => ws.tables_sel = ws.tables_sel.saturating_sub(1),
            KeyCode::Down => ws.tables_sel = (ws.tables_sel + 1).min(n.saturating_sub(1)),
            KeyCode::PageUp => ws.tables_sel = ws.tables_sel.saturating_sub(10),
            KeyCode::PageDown => ws.tables_sel = (ws.tables_sel + 10).min(n.saturating_sub(1)),
            KeyCode::Home => ws.tables_sel = 0,
            KeyCode::End => ws.tables_sel = n.saturating_sub(1),
            KeyCode::Esc => ws.tables_filter.clear(),
            KeyCode::Backspace => {
                ws.tables_filter.pop();
                ws.tables_sel = 0;
            }
            KeyCode::Char(' ') | KeyCode::Char('*')
                if ws.tables_filter.is_empty() || k.code == KeyCode::Char('*') =>
            {
                let sel = ws.tables_sel;
                self.toggle_pin(sel);
            }
            KeyCode::Enter => {
                let list = ws.filtered_tables();
                if let (Some(schema), Some(&ti)) = (ws.schema.clone(), list.get(ws.tables_sel)) {
                    let name = schema.tables[ti].qualified();
                    ws.sql.insert_at_cursor(&name);
                    self.focus = Focus::Sql;
                }
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                ws.tables_filter.push(c);
                ws.tables_sel = 0;
            }
            _ => {}
        }
    }

    fn key_question(&mut self, k: KeyEvent) {
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('j') if ctrl => {
                if let Some(ws) = self.ws_mut() {
                    ws.question.newline();
                }
            }
            KeyCode::Enter if alt || ctrl || k.modifiers.contains(KeyModifiers::SHIFT) => {
                if let Some(ws) = self.ws_mut() {
                    ws.question.newline();
                }
            }
            KeyCode::Enter => self.generate(),
            KeyCode::Down
                if self
                    .ws()
                    .is_some_and(|w| w.question.row + 1 >= w.question.lines().len())
                    && !alt =>
            {
                self.focus = Focus::Sql
            }
            _ => {
                let clip = self.clip.clone();
                if let Some(ws) = self.wss.get_mut(self.cur) {
                    edit_common(&mut ws.question, k, &clip);
                }
                self.after_edit(k, true);
            }
        }
    }

    fn key_sql(&mut self, k: KeyEvent) {
        let clip = self.clip.clone();
        let Some(ws) = self.wss.get_mut(self.cur) else {
            return;
        };
        if k.code == KeyCode::Up
            && ws.sql.row == 0
            && ws.sql.scroll == 0
            && !k.modifiers.contains(KeyModifiers::ALT)
            && ws.sql.col == 0
        {
            self.focus = Focus::Question;
            return;
        }
        let changed = edit_common(&mut ws.sql, k, &clip);
        if changed {
            ws.error = None;
        }
        self.after_edit(k, false);
    }

    /// Copiar/cortar del editor activo al portapapeles.
    fn after_edit(&mut self, k: KeyEvent, question: bool) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if !ctrl {
            return;
        }
        let Some(ws) = self.wss.get_mut(self.cur) else {
            return;
        };
        let ed = if question {
            &mut ws.question
        } else {
            &mut ws.sql
        };
        match k.code {
            KeyCode::Char('c') | KeyCode::Char('x') => {
                if let Some(t) = ed.selected_text() {
                    if k.code == KeyCode::Char('x') {
                        ed.insert_str("");
                    }
                    self.copy(&t);
                }
            }
            _ => {}
        }
    }

    fn key_result(&mut self, k: KeyEvent) {
        let Some(ws) = self.wss.get_mut(self.cur) else {
            return;
        };
        let ncols = ws.result.as_ref().map(|r| r.columns.len()).unwrap_or(0);
        let nrows = ws.tv.order.len();
        let page = ws.tv.page_rows.max(1);
        match k.code {
            KeyCode::Up => ws.tv.row = ws.tv.row.saturating_sub(1),
            KeyCode::Down => ws.tv.row = (ws.tv.row + 1).min(nrows.saturating_sub(1)),
            KeyCode::Left => ws.tv.col = ws.tv.col.saturating_sub(1),
            KeyCode::Right => ws.tv.col = (ws.tv.col + 1).min(ncols.saturating_sub(1)),
            KeyCode::PageUp => ws.tv.row = ws.tv.row.saturating_sub(page),
            KeyCode::PageDown => ws.tv.row = (ws.tv.row + page).min(nrows.saturating_sub(1)),
            KeyCode::Home => ws.tv.col = 0,
            KeyCode::End => ws.tv.col = ncols.saturating_sub(1),
            KeyCode::Char('g') => ws.tv.row = 0,
            KeyCode::Char('G') => ws.tv.row = nrows.saturating_sub(1),
            KeyCode::Char('s') => {
                let c = ws.tv.col;
                ws.sort_by(c);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(v) = cell_text(ws, ws.tv.row, ws.tv.col) {
                    self.dialog = Dialog::Cell(v);
                }
            }
            KeyCode::Char('y') => {
                if let Some(v) = cell_text(ws, ws.tv.row, ws.tv.col) {
                    self.copy(&v);
                    self.learn_from_use();
                    self.toast("Celda copiada", true);
                }
            }
            KeyCode::Char('Y') => {
                if let (Some(r), Some(&ri)) = (&ws.result, ws.tv.order.get(ws.tv.row)) {
                    let line = r.rows[ri]
                        .iter()
                        .map(|v| v.clone().unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join("\t");
                    self.copy(&line);
                    self.toast("Fila copiada", true);
                }
            }
            KeyCode::Char('e') => self.export(),
            _ => {}
        }
    }

    /// Copiar o exportar el resultado de una consulta entregada por la IA cuenta como aprobación implícita.
    fn learn_from_use(&mut self) {
        let Some(ws) = self.wss.get_mut(self.cur) else {
            return;
        };
        if ws.learned
            || ws.ai_sql.is_empty()
            || ws.result.as_ref().is_none_or(|r| r.rows.is_empty())
        {
            return;
        }
        if ws.ran_sql.trim() == ws.ai_sql.trim() {
            history::add(
                &ws.spec.info.id,
                &ws.ai_question,
                &ws.ai_sql,
                "aprobada por el usuario",
            );
            ws.learned = true;
        }
    }

    pub fn copy(&mut self, text: &str) {
        self.clip = text.to_string();
        use std::io::Write;
        let seq = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
        let mut out = std::io::stdout();
        let _ = out.write_all(seq.as_bytes());
        let _ = out.flush();
    }

    pub fn export(&mut self) {
        let Some(ws) = self.wss.get(self.cur) else {
            return;
        };
        let Some(r) = &ws.result else {
            self.toast("No hay resultado que exportar", false);
            return;
        };
        let esc = |v: &str| {
            if v.contains([',', '"', '\n', ';']) {
                format!("\"{}\"", v.replace('"', "\"\""))
            } else {
                v.to_string()
            }
        };
        let mut out = r
            .columns
            .iter()
            .map(|c| esc(&c.name))
            .collect::<Vec<_>>()
            .join(",")
            + "\n";
        for &i in &ws.tv.order {
            out += &r.rows[i]
                .iter()
                .map(|v| v.as_deref().map(esc).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(",");
            out.push('\n');
        }
        let stamp = std::process::Command::new("date")
            .arg("+%Y%m%d-%H%M%S")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default();
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let dir = std::path::Path::new(&home).join("Descargas");
        let dir = if dir.is_dir() {
            dir
        } else {
            std::path::PathBuf::from(&home)
        };
        let path = dir.join(format!("dbview-{}-{}.csv", ws.spec.info.id, stamp.trim()));
        let n = r.rows.len();
        match std::fs::write(&path, out) {
            Ok(()) => {
                self.learn_from_use();
                let msg = format!("{n} filas exportadas a {}", path.display());
                self.toast(msg, true);
            }
            Err(e) => self.toast(format!("No se pudo exportar: {e}"), false),
        }
    }
}

/// Tablas nuevas, modificadas (columnas distintas) y eliminadas entre dos lecturas del esquema.
pub fn schema_changes(old: Option<&Schema>, new: &Schema) -> (usize, usize, usize) {
    let Some(old) = old else { return (0, 0, 0) };
    let key = |t: &TableInfo| {
        (
            t.qualified(),
            t.columns
                .iter()
                .map(|c| format!("{}:{}", c.name, c.type_name))
                .collect::<Vec<_>>(),
        )
    };
    let om: HashMap<String, Vec<String>> = old.tables.iter().map(key).collect();
    let nm: HashMap<String, Vec<String>> = new.tables.iter().map(key).collect();
    let added = nm.keys().filter(|k| !om.contains_key(*k)).count();
    let removed = om.keys().filter(|k| !nm.contains_key(*k)).count();
    let changed = nm
        .iter()
        .filter(|(k, v)| om.get(*k).is_some_and(|o| o != *v))
        .count();
    (added, changed, removed)
}

pub fn cell_text(ws: &Ws, row: usize, col: usize) -> Option<String> {
    let r = ws.result.as_ref()?;
    let ri = *ws.tv.order.get(row)?;
    Some(
        r.rows[ri]
            .get(col)?
            .clone()
            .unwrap_or_else(|| "NULL".into()),
    )
}

/// Teclas de edición comunes; `true` si cambió el texto.
fn edit_common(ed: &mut Editor, k: KeyEvent, clip: &str) -> bool {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && k.code == KeyCode::Char('v') {
        ed.insert_str(clip);
        return true;
    }
    if ctrl && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('x')) {
        return false;
    }
    ed.handle_key(k)
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for ch in data.chunks(3) {
        let n = (ch[0] as u32) << 16
            | (*ch.get(1).unwrap_or(&0) as u32) << 8
            | *ch.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if ch.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if ch.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form;

    /// Las pruebas NUNCA tocan `~/.config` ni `~/.local/share` reales: `form_save`, el catálogo, las notas y el
    /// historial escriben en disco, y una vez pisaron las conexiones del usuario con las de demo.
    fn aislar_disco() {
        static UNA_VEZ: std::sync::Once = std::sync::Once::new();
        UNA_VEZ.call_once(|| {
            let dir = std::env::temp_dir().join(format!("dbview-pruebas-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            // SAFETY: se ejecuta antes de crear ningún `App` y solo fija rutas de datos.
            unsafe {
                std::env::set_var("XDG_CONFIG_HOME", dir.join("config"));
                std::env::set_var("XDG_DATA_HOME", dir.join("data"));
            }
        });
    }

    fn app_demo() -> App {
        aislar_disco();
        let mut cfg = config::load();
        cfg.connections = db::demo::connections();
        cfg.default_conn = None;
        App::new(cfg)
    }

    fn rs(valor: &str) -> ResultSet {
        ResultSet {
            columns: vec![ColumnMeta {
                name: "c".into(),
                type_name: String::new(),
            }],
            rows: vec![vec![Some(valor.into())]],
            truncated: false,
            elapsed_ms: 1,
        }
    }

    fn pintar(app: &mut App) {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 44)).unwrap();
        term.draw(|f| crate::ui::draw(f, app)).unwrap();
    }

    fn clic(app: &mut App, x: u16, y: u16) {
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        });
    }

    fn centro_de(app: &App, quiero: &Act) -> Vec<(u16, u16)> {
        app.hits
            .iter()
            .filter(|(_, a)| a == quiero)
            .map(|(r, _)| (r.x + r.width / 2, r.y))
            .collect()
    }

    #[test]
    fn cada_pestana_conserva_su_pregunta_sql_y_resultado() {
        let mut app = app_demo();
        app.ws_mut().unwrap().question.set_text("clientes");
        app.ws_mut().unwrap().sql.set_text("SELECT 1");
        app.ws_mut().unwrap().result = Some(rs("uno"));
        app.new_tab();
        assert_eq!(app.ws().unwrap().tabs.len(), 2);
        // La pestaña nueva está limpia.
        assert!(app.ws().unwrap().question.is_empty());
        assert!(app.ws().unwrap().sql.is_empty());
        assert!(app.ws().unwrap().result.is_none());
        app.ws_mut().unwrap().question.set_text("pedidos");
        app.ws_mut().unwrap().sql.set_text("SELECT 2");
        // Volver a la primera: sus tres paneles son los suyos.
        app.switch_tab(0);
        let ws = app.ws().unwrap();
        assert_eq!(ws.question.text(), "clientes");
        assert_eq!(ws.sql.text(), "SELECT 1");
        assert!(ws.result.is_some());
        app.switch_tab(1);
        assert_eq!(app.ws().unwrap().sql.text(), "SELECT 2");
        assert!(app.ws().unwrap().result.is_none());
    }

    #[test]
    fn cerrar_pestana_borra_su_sql_y_su_resultado() {
        let mut app = app_demo();
        app.ws_mut().unwrap().sql.set_text("SELECT 1");
        app.new_tab();
        app.ws_mut().unwrap().sql.set_text("SELECT 2");
        app.ws_mut().unwrap().result = Some(rs("dos"));
        app.close_tab(1);
        let ws = app.ws().unwrap();
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(ws.sql.text(), "SELECT 1");
        assert!(ws.result.is_none());
        // Cerrar la última deja una vacía.
        app.close_tab(0);
        let ws = app.ws().unwrap();
        assert_eq!(ws.tabs.len(), 1);
        assert!(ws.sql.is_empty() && ws.question.is_empty());
        // Y se recuperan las cerradas, la última primero.
        app.reopen_tab();
        assert_eq!(app.ws().unwrap().sql.text(), "SELECT 1");
        app.new_tab();
        app.reopen_tab();
        assert_eq!(app.ws().unwrap().sql.text(), "SELECT 2");
        assert!(app.ws().unwrap().result.is_some());
    }

    #[test]
    fn cerrar_una_anterior_mantiene_la_activa() {
        let mut app = app_demo();
        app.new_tab();
        app.new_tab();
        app.ws_mut().unwrap().sql.set_text("tercera");
        assert_eq!(app.ws().unwrap().active, 2);
        app.close_tab(0);
        let ws = app.ws().unwrap();
        assert_eq!(ws.active, 1);
        assert_eq!(ws.sql.text(), "tercera");
    }

    #[test]
    fn el_resultado_llega_a_su_pestana_aunque_se_vea_otra() {
        let mut app = app_demo();
        let id = app.ws().unwrap().spec.info.id.clone();
        app.ws_mut().unwrap().sql.set_text("SELECT 1");
        app.ws_mut().unwrap().busy = Busy::Run;
        app.ws_mut().unwrap().job = 77;
        app.new_tab();
        app.on_msg(Msg::Ran(id.clone(), 77, Ok(rs("para la primera"))));
        let ws = app.ws().unwrap();
        assert_eq!(ws.active, 1);
        assert!(
            ws.result.is_none(),
            "la pestaña que se ve no debe recibirlo"
        );
        assert_eq!(ws.tabs[0].busy, Busy::No);
        assert!(ws.tabs[0].result.is_some());
        // Un trabajo de una pestaña ya cerrada se descarta sin tocar nada.
        app.ws_mut().unwrap().tabs[1].job = 78;
        app.close_tab(1);
        app.on_msg(Msg::Ran(id, 78, Ok(rs("huérfano"))));
        assert_eq!(app.ws().unwrap().tabs.len(), 1);
        assert_eq!(
            app.ws().unwrap().tabs[0].result.as_ref().unwrap().rows[0][0].as_deref(),
            Some("para la primera")
        );
    }

    #[test]
    fn la_tira_de_pestanas_responde_al_raton_en_los_dos_paneles() {
        let mut app = app_demo();
        pintar(&mut app);
        // ① y ② dibujan cada una su «+».
        let plus = centro_de(&app, &Act::NewTab);
        assert_eq!(plus.len(), 2, "un + por panel");
        clic(&mut app, plus[0].0, plus[0].1);
        assert_eq!(app.ws().unwrap().tabs.len(), 2);
        pintar(&mut app);
        // El «+» del panel ② hace lo mismo.
        let plus = centro_de(&app, &Act::NewTab);
        clic(&mut app, plus[1].0, plus[1].1);
        assert_eq!(app.ws().unwrap().tabs.len(), 3);
        pintar(&mut app);
        // Pestaña 1 desde la tira de ②; luego su ✕ desde ①.
        let tab0 = centro_de(&app, &Act::Tab(0));
        assert_eq!(tab0.len(), 2);
        clic(&mut app, tab0[1].0, tab0[1].1);
        assert_eq!(app.ws().unwrap().active, 0);
        pintar(&mut app);
        let x0 = centro_de(&app, &Act::CloseTab(1));
        clic(&mut app, x0[0].0, x0[0].1);
        assert_eq!(app.ws().unwrap().tabs.len(), 2);
    }

    #[test]
    fn los_botones_del_titulo_siguen_funcionando() {
        let mut app = app_demo();
        pintar(&mut app);
        let run = centro_de(&app, &Act::Run);
        assert_eq!(run.len(), 1);
        clic(&mut app, run[0].0, run[0].1);
        // Sin SQL, «Ejecutar» avisa: prueba de que el clic llegó al botón y no al panel.
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|t| t.text.contains("No hay SQL"))
        );
    }

    #[test]
    fn el_formulario_deja_elegir_oracle_y_produccion_con_clic() {
        let mut app = app_demo();
        app.do_act(Act::NewConn);
        pintar(&mut app);
        let oracle = centro_de(&app, &Act::FormPick(form::F_DIALECT, 1));
        assert_eq!(oracle.len(), 1);
        clic(&mut app, oracle[0].0, oracle[0].1);
        let prod = centro_de(&app, &Act::FormPick(form::F_ENV, 1));
        clic(&mut app, prod[0].0, prod[0].1);
        let Dialog::Form(f) = &app.dialog else {
            panic!("el formulario debía seguir abierto")
        };
        assert_eq!(f.dialect, Dialect::Oracle);
        assert_eq!(f.env, EnvKind::Prod);
        assert_eq!(f.port.text(), "1521");
        // Con el teclado también: en «Entorno», → pasa a Otro.
        if let Dialog::Form(f) = &mut app.dialog {
            f.focus = form::F_ENV;
        }
        app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        let Dialog::Form(f) = &app.dialog else {
            panic!()
        };
        assert_eq!(f.env, EnvKind::Other);
    }

    #[test]
    fn un_mensaje_de_una_conexion_editada_no_toca_pestanas_nuevas() {
        let mut app = app_demo();
        let id = app.ws().unwrap().spec.info.id.clone();
        app.ws_mut().unwrap().sql.set_text("SELECT 1");
        app.ws_mut().unwrap().busy = Busy::Ai;
        let viejo = app.ws().unwrap().serial + 1;
        app.ws_mut().unwrap().job = viejo;
        app.new_tab();
        app.open_edit(app.cur);
        app.form_save();
        // Tras editar, los números de trabajo nuevos no coinciden con los de antes.
        app.ws_mut().unwrap().serial += 1;
        let nuevo = app.ws().unwrap().serial;
        app.ws_mut().unwrap().job = nuevo;
        assert_ne!(nuevo, viejo);
        app.on_msg(Msg::AiText(id, viejo, "SELECT 999".into()));
        assert!(app.ws().unwrap().sql.is_empty());
        assert_eq!(app.ws().unwrap().tabs[0].sql.text(), "SELECT 1");
    }

    #[test]
    fn cerrar_una_pestana_que_esperaba_el_esquema_no_la_deja_pendiente() {
        let mut app = app_demo();
        app.ws_mut().unwrap().question.set_text("algo");
        app.ws_mut().unwrap().pending_generate = true;
        app.close_tab(0);
        app.reopen_tab();
        let ws = app.ws().unwrap();
        assert!(!ws.pending_generate);
        assert!(!ws.any_work());
    }

    #[test]
    fn una_pestana_de_fondo_no_roba_el_foco() {
        let mut app = app_demo();
        let id = app.ws().unwrap().spec.info.id.clone();
        app.ws_mut().unwrap().pending_generate = true; // pregunta vacía
        app.new_tab();
        app.focus = Focus::Result;
        let sc = app.ws().unwrap().schema.clone().unwrap_or_default();
        let intento = app.ws().unwrap().conn_attempt;
        app.on_msg(Msg::Schema(id, Ok((*sc).clone()), intento));
        assert_eq!(app.focus, Focus::Result);
        assert_eq!(app.ws().unwrap().active, 1);
    }

    #[test]
    fn el_editor_de_la_pregunta_sobrevive_en_60x18_con_error_y_sugerencia() {
        let mut app = app_demo();
        app.ws_mut().unwrap().question.set_text("PRIMERA\nSEGUNDA");
        app.ws_mut().unwrap().ai_error =
            Some("no lo encuentro aquí, ni allí, ni en ningún otro sitio de la base".into());
        app.ws_mut().unwrap().suggest = Some(1);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 18)).unwrap();
        term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        // El cursor queda en la última línea («SEGUNDA»), que no es el nombre de la pestaña: si aparece en la fila del editor, se dibuja.
        let area = app.ws().unwrap().question.area;
        assert!(
            area.height >= 1 && area.width > 0,
            "editor sin sitio: {area:?}"
        );
        let buf = term.backend().buffer().clone();
        let fila: String = (area.x..area.x + area.width)
            .map(|x| buf[(x, area.y)].symbol().to_string())
            .collect();
        assert!(fila.contains("SEGUNDA"), "{fila}");
    }

    #[test]
    fn respuestas_de_una_conexion_reemplazada_se_descartan() {
        let mut app = app_demo();
        let id = app.ws().unwrap().spec.info.id.clone();
        let viejo = app.ws().unwrap().conn_attempt;
        app.open_edit(app.cur);
        app.form_save();
        let nuevo = app.ws().unwrap().conn_attempt;
        assert_ne!(viejo, nuevo);
        app.ws_mut().unwrap().status = ConnStatus::Connecting;
        let antes = app.ws().unwrap().schema.is_some();
        app.on_msg(Msg::Connected(
            id.clone(),
            Err(DbError::new("viejo")),
            viejo,
        ));
        assert_eq!(app.ws().unwrap().status, ConnStatus::Connecting);
        let sc = Schema::default();
        app.on_msg(Msg::Schema(id, Ok(sc), viejo));
        assert_eq!(app.ws().unwrap().schema.is_some(), antes);
    }

    #[test]
    fn el_aviso_de_esquema_pendiente_no_tapa_el_editor() {
        let mut app = app_demo();
        app.ws_mut().unwrap().pending_generate = true;
        app.generate(); // pregunta vacía: anula la espera
        assert!(!app.ws().unwrap().pending_generate);
    }

    fn con_esquema() -> App {
        let mut app = app_demo();
        for _ in 0..50 {
            while let Ok(m) = app.rx.try_recv() {
                app.on_msg(m);
            }
            if app.ws().is_some_and(|w| w.schema.is_some()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            app.ws().unwrap().schema.is_some(),
            "el demo debía cargar el esquema"
        );
        app
    }

    fn rueda(app: &mut App, x: u16, y: u16, abajo: bool) {
        app.on_mouse(MouseEvent {
            kind: if abajo {
                MouseEventKind::ScrollDown
            } else {
                MouseEventKind::ScrollUp
            },
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        });
    }

    #[test]
    fn clic_en_el_texto_de_una_tabla_la_selecciona_y_no_la_fija() {
        let mut app = con_esquema();
        pintar(&mut app);
        let r = app
            .hits
            .iter()
            .find(|(_, a)| *a == Act::Table(2))
            .map(|(r, _)| *r)
            .unwrap();
        // En cualquier punto de la fila, también en el extremo derecho.
        clic(&mut app, r.x + r.width - 1, r.y);
        assert_eq!(app.ws().unwrap().tables_sel, 2);
        assert!(
            app.ws().unwrap().pinned.is_empty(),
            "no debe fijar sin querer"
        );
        // Clic derecho: la fija; clic en su ★: la quita.
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: r.x + 3,
            row: r.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.ws().unwrap().pinned.len(), 1);
        pintar(&mut app);
        let estrella = centro_de(&app, &Act::Pin(2));
        assert_eq!(estrella.len(), 1, "solo hay ★ clicable en las fijadas");
        clic(&mut app, estrella[0].0, estrella[0].1);
        assert!(app.ws().unwrap().pinned.is_empty());
    }

    #[test]
    fn la_ultima_columna_se_alcanza_con_la_rueda_y_se_inserta_con_clic() {
        let mut app = con_esquema();
        app.cols_h = Some(4);
        pintar(&mut app);
        let (ncols, ultima) = {
            let ws = app.ws().unwrap();
            let list = ws.filtered_tables();
            let t = &ws.schema.as_ref().unwrap().tables[list[ws.tables_sel]];
            (t.columns.len(), t.columns.last().unwrap().name.clone())
        };
        let g = app.geo.cols;
        let visibles = app
            .hits
            .iter()
            .filter(|(_, a)| matches!(a, Act::Column(_)))
            .count();
        assert!(
            ncols > visibles,
            "la tabla de prueba debe tener más columnas que filas"
        );
        assert!(centro_de(&app, &Act::Column(ncols - 1)).is_empty());
        for _ in 0..10 {
            rueda(&mut app, g.x + 3, g.y + 2, true);
        }
        pintar(&mut app);
        let ult = centro_de(&app, &Act::Column(ncols - 1));
        assert_eq!(ult.len(), 1, "la última columna debe quedar a la vista");
        clic(&mut app, ult[0].0, ult[0].1);
        assert!(app.ws().unwrap().sql.text().contains(&ultima));
        assert_eq!(app.focus, Focus::Sql);
        // Al cambiar de tabla vuelve al principio.
        app.ws_mut().unwrap().tables_sel = 1;
        pintar(&mut app);
        assert_eq!(app.ws().unwrap().cols_top, 0);
    }

    fn tecla(app: &mut App, c: KeyCode, m: KeyModifiers) {
        app.on_key(KeyEvent::new(c, m));
    }

    fn teclear(app: &mut App, texto: &str) {
        for ch in texto.chars() {
            tecla(app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }

    #[test]
    fn teclear_tras_pulsar_una_conexion_no_borra_ni_cierra_nada() {
        let mut app = app_demo();
        pintar(&mut app);
        let c = centro_de(&app, &Act::Conn(1));
        clic(&mut app, c[0].0, c[0].1);
        assert_eq!(app.cur, 1);
        // Antes, con el foco en la lista, la «d» pedía borrar y «s», «y» o Enter lo confirmaban.
        teclear(&mut app, "dime los datos de todos los clientes, y q");
        tecla(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.wss.len(), 3, "no debe borrarse ninguna conexión");
        assert!(matches!(app.dialog, Dialog::None));
        assert!(!app.quit);
        assert!(
            app.ws()
                .unwrap()
                .question
                .text()
                .starts_with("dime los datos")
        );
    }

    #[test]
    fn borrar_una_conexion_exige_ctrl_d_o_el_boton() {
        let mut app = app_demo();
        app.focus = Focus::Conns;
        tecla(&mut app, KeyCode::Delete, KeyModifiers::NONE);
        assert!(matches!(app.dialog, Dialog::ConfirmDelete(_)));
        for k in ['s', 'y'] {
            tecla(&mut app, KeyCode::Char(k), KeyModifiers::NONE);
        }
        assert_eq!(app.wss.len(), 3, "ni «s» ni «y» confirman");
        tecla(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(app.dialog, Dialog::None), "Enter cancela");
        assert_eq!(app.wss.len(), 3);
        tecla(&mut app, KeyCode::Delete, KeyModifiers::NONE);
        tecla(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(app.wss.len(), 2);
    }

    #[test]
    fn alternar_clics_entre_conexiones_y_tablas_elige_lo_pulsado() {
        let mut app = con_esquema();
        for (conn, tabla) in [(1usize, 2usize), (0, 1), (2, 0), (0, 3), (1, 1)] {
            pintar(&mut app);
            let c = centro_de(&app, &Act::Conn(conn));
            clic(&mut app, c[0].0, c[0].1);
            assert_eq!(app.cur, conn);
            // Las tablas de la conexión recién elegida pueden tardar en llegar: se espera su esquema.
            for _ in 0..50 {
                while let Ok(m) = app.rx.try_recv() {
                    app.on_msg(m);
                }
                if app.ws().is_some_and(|w| w.schema.is_some()) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            pintar(&mut app);
            let t = centro_de(&app, &Act::Table(tabla));
            assert_eq!(
                t.len(),
                1,
                "la tabla {tabla} de la conexión {conn} debe estar a la vista"
            );
            clic(&mut app, t[0].0, t[0].1);
            assert_eq!(app.cur, conn, "pulsar una tabla no cambia de conexión");
            assert_eq!(app.ws().unwrap().tables_sel, tabla);
            assert_eq!(app.wss.len(), 3);
        }
    }

    #[test]
    fn pulsar_en_cualquier_hueco_de_un_panel_lateral_le_da_el_foco() {
        let mut app = con_esquema();
        app.ws_mut().unwrap().tables_filter = "zzzz-no-existe".into();
        pintar(&mut app);
        let g = app.geo;
        // Hueco vacío bajo la lista de tablas, caja del filtro y fila libre de Conexiones.
        for (x, y, esperado) in [
            (
                g.tables.x + 5,
                g.tables.y + g.tables.height - 3,
                Focus::Tables,
            ),
            (g.tables.x + 5, g.tables.y + 1, Focus::Tables),
            (g.cols.x + 5, g.cols.y + 1, Focus::Tables),
            (g.conns.x + 5, g.conns.y + g.conns.height - 4, Focus::Conns),
        ] {
            app.focus = Focus::Sql;
            clic(&mut app, x, y);
            assert_eq!(
                app.focus, esperado,
                "clic en ({x},{y}) debía dar el foco a {esperado:?}"
            );
            pintar(&mut app);
        }
    }

    #[test]
    fn base64_correcto() {
        assert_eq!(base64(b"hola"), "aG9sYQ==");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(base64(b""), "");
    }
}
