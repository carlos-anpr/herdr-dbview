//! Configuración: `~/.config/herdr-dbview/config.toml`. Todo es opcional: si el fichero
//! falta, está roto o no se puede leer, se usan los valores por defecto y ninguna conexión.

use crate::model::*;
use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use toml::{Table, Value};

pub const AI_URL: &str = "https://opencode.ai/inference/go/openai/v1/chat/completions";
/// Variables de entorno que cambian la IA.
pub const AI_ENV_KEYS: &[&str] = &[
    "DBVIEW_AI_MODEL",
    "DBVIEW_AI_EFFORT",
    "DBVIEW_AI_API_URL",
    "DBVIEW_AI_API_KEY_VALUE",
];

const DIR_NAME: &str = "herdr-dbview";
const FILE_NAME: &str = "config.toml";
const BRIDGE_REL: &str = "bridge/oracle_bridge.py";
const MAX_ROWS_MIN: i64 = 1;
const MAX_ROWS_MAX: i64 = 10_000;
const TIMEOUT_MIN: i64 = 1;
const TIMEOUT_MAX: i64 = 3_600;

#[derive(Clone, PartialEq)]
pub struct AiConfig {
    pub model: String,
    pub effort: String,
    pub url: String,
    pub api_key: Option<String>,
    /// Problema de configuración (variable inválida…) para enseñarlo en pantalla.
    pub error: Option<String>,
}
impl std::fmt::Debug for AiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiConfig")
            .field("model", &self.model)
            .field("effort", &self.effort)
            .finish_non_exhaustive()
    }
}
impl Default for AiConfig {
    fn default() -> Self {
        Self {
            model: "deepseek-v4.1-flash".into(),
            effort: "none".into(),
            url: AI_URL.into(),
            api_key: None,
            error: None,
        }
    }
}

impl AiConfig {
    /// Modelo más potente para revisar y reintentar solo cuando el primer intento no convence (sin configuración:
    /// `deepseek-v4-pro` en OpenCode Go). `DBVIEW_AI_MODEL_STRONG` lo cambia; vacío u `off` lo desactiva.
    pub fn strong(&self) -> Option<AiConfig> {
        let model = match std::env::var("DBVIEW_AI_MODEL_STRONG") {
            Ok(v) if v.trim().is_empty() || v.trim().eq_ignore_ascii_case("off") => return None,
            Ok(v) => v.trim().to_string(),
            Err(_) if self.url == AI_URL => "deepseek-v4-pro".to_string(),
            Err(_) => return None,
        };
        if model == self.model || self.error.is_some() {
            return None;
        }
        Some(AiConfig {
            model,
            ..self.clone()
        })
    }

    /// `[ai]` del TOML; la clave nunca se lee del fichero.
    fn apply_toml(&mut self, table: &Table) {
        let text = |key: &str| {
            table
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        };
        if let Some(model) = text("model") {
            self.model = model.to_string();
        }
        if let Some(effort) = text("effort") {
            self.effort = effort.to_string();
        }
        if let Some(url) = text("url") {
            self.url = url.to_string();
        }
    }

    /// El entorno gana sobre el TOML. `var` se inyecta para poder probar sin tocar el entorno real.
    fn apply_env(&mut self, mut var: impl FnMut(&str) -> Option<String>) {
        let mut problem = None;
        for (key, target) in [
            (AI_ENV_KEYS[0], &mut self.model),
            (AI_ENV_KEYS[1], &mut self.effort),
            (AI_ENV_KEYS[2], &mut self.url),
        ] {
            if let Some(value) = var(key) {
                if value.trim().is_empty() {
                    problem = Some(format!("{key} está vacía"));
                } else {
                    *target = value.trim().to_string();
                }
            }
        }
        if let Some(key) = var(AI_ENV_KEYS[3]) {
            if key.trim().is_empty() || key.chars().any(char::is_control) {
                problem = Some(format!("{} no es válida", AI_ENV_KEYS[3]));
            } else {
                self.api_key = Some(key);
            }
        }
        self.validate_with(problem);
    }

    fn validate(&mut self) {
        self.validate_with(None);
    }

    /// Recalcula `error` desde cero: un valor corregido deja de dar error.
    fn validate_with(&mut self, problem: Option<String>) {
        let control = |s: &str| s.chars().any(char::is_control);
        self.error = problem.or_else(|| {
            if self.model.trim().is_empty() || control(&self.model) {
                return Some("El modelo de IA no es válido".into());
            }
            if !matches!(self.effort.as_str(), "none" | "minimal" | "low" | "medium" | "high" | "max" | "xhigh") {
                return Some("El esfuerzo de IA no es válido: usa none, minimal, low, medium, high, max o xhigh".into());
            }
            if !(self.url.starts_with("https://") || self.url.starts_with("http://")) || control(&self.url) {
                return Some("La URL de la IA debe ser HTTP(S)".into());
            }
            if self.api_key.as_ref().is_some_and(|key| key.trim().is_empty() || control(key)) {
                return Some("La clave de IA no es válida".into());
            }
            None
        });
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub connections: Vec<ConnSpec>,
    pub ai: AiConfig,
    /// Filas máximas que se traen por consulta.
    pub max_rows: usize,
    /// Si es `true`, tras generar el SQL con la IA se ejecuta solo.
    pub auto_run: bool,
    /// Id de la conexión con la que se arranca.
    pub default_conn: Option<String>,
    /// Tiempo máximo por consulta (s).
    pub timeout_secs: u64,
    /// Intérprete de Python con `oracledb` (valor por defecto para conexiones nuevas).
    pub python: String,
    /// Ruta del puente `bridge/oracle_bridge.py` (valor por defecto para conexiones nuevas).
    pub bridge: String,
}

impl PartialEq for Config {
    fn eq(&self, other: &Self) -> bool {
        self.ai == other.ai
            && self.max_rows == other.max_rows
            && self.auto_run == other.auto_run
            && self.default_conn == other.default_conn
            && self.timeout_secs == other.timeout_secs
            && self.python == other.python
            && self.bridge == other.bridge
            && self.connections.len() == other.connections.len()
            && self
                .connections
                .iter()
                .zip(&other.connections)
                .all(|(a, b)| conn_eq(a, b))
    }
}

fn conn_eq(a: &ConnSpec, b: &ConnSpec) -> bool {
    a.info == b.info
        && a.host == b.host
        && a.port == b.port
        && a.database == b.database
        && a.user == b.user
        && a.password == b.password
        && a.secret == b.secret
        && a.oracle_service == b.oracle_service
        && a.python == b.python
        && a.bridge == b.bridge
        && a.timeout_secs == b.timeout_secs
        && a.schemas == b.schemas
}

impl Default for Config {
    fn default() -> Self {
        let var = |key: &str| std::env::var(key).ok();
        Config::base(&Env {
            home: home_dir(),
            bridge: resolve_bridge(),
            var: &var,
        })
    }
}

/// Entorno mínimo para interpretar el TOML. Se inyecta en las pruebas.
struct Env<'a> {
    home: Option<PathBuf>,
    bridge: String,
    var: &'a dyn Fn(&str) -> Option<String>,
}

impl Config {
    fn base(env: &Env<'_>) -> Self {
        Config {
            connections: Vec::new(),
            ai: AiConfig::default(),
            max_rows: 500,
            auto_run: true,
            default_conn: None,
            timeout_secs: 30,
            python: default_python(env.home.as_deref()),
            bridge: env.bridge.clone(),
        }
    }
}

/// Carga la configuración. Cualquier problema devuelve los valores por defecto.
pub fn load() -> Config {
    let var = |key: &str| std::env::var(key).ok();
    let env = Env {
        home: home_dir(),
        bridge: resolve_bridge(),
        var: &var,
    };
    let mut cfg = match config_file_path().map(std::fs::read_to_string) {
        Some(Ok(src)) => parse(&src, &env),
        _ => Config::base(&env),
    };
    cfg.ai.apply_env(&var);
    if cfg.ai.api_key.is_none() && cfg.ai.error.is_none() && cfg.ai.url == AI_URL {
        cfg.ai.api_key = auth_file_path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|raw| shared_ai_key(&raw));
    }
    cfg
}

/// Guarda la configuración completa en `~/.config/herdr-dbview/config.toml` de forma atómica.
/// Conserva el origen de cada clave (`Plain`/`Env`/`File`) y deja `config.toml.bak`.
pub fn save(cfg: &Config) -> io::Result<()> {
    let path = config_file_path().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "No se pudo determinar la ruta de configuración (HOME o XDG_CONFIG_HOME)",
        )
    })?;
    write_config(&path, cfg)
}

/// Clave compartida del almacén de OpenCode: primero la propia y, si no, la de OpenCode Go.
fn shared_ai_key(raw: &str) -> Option<String> {
    let auth: serde_json::Value = serde_json::from_str(raw).ok()?;
    ["herdr-dbview", "herdr-gitview", "opencode-go"]
        .into_iter()
        .find_map(|entry| {
            auth.get(entry)
                .and_then(|v| v.get("key"))
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
        })
}

fn auth_file_path() -> Option<PathBuf> {
    let root = std::env::var_os("XDG_DATA_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".local/share")))?;
    Some(root.join("opencode/auth.json"))
}

fn default_python(_home: Option<&Path>) -> String {
    "python3".into()
}

/// Ruta del puente: relativa al ejecutable, con respaldo en `$HERDR_PLUGIN_ROOT` y en el
/// directorio actual. Si no existe en ninguno, devuelve la del ejecutable (para poder avisar).
fn resolve_bridge() -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("../..").join(BRIDGE_REL)));
    if let Some(path) = exe.as_ref().filter(|path| path.exists()) {
        return normalize(path).to_string_lossy().into_owned();
    }
    if let Some(root) = std::env::var_os("HERDR_PLUGIN_ROOT").filter(|s| !s.is_empty()) {
        let path = PathBuf::from(root).join(BRIDGE_REL);
        if path.exists() {
            return normalize(&path).to_string_lossy().into_owned();
        }
    }
    if let Some(path) = std::env::current_dir()
        .ok()
        .map(|dir| dir.join(BRIDGE_REL))
        .filter(|path| path.exists())
    {
        return normalize(&path).to_string_lossy().into_owned();
    }
    exe.map(|path| normalize(&path).to_string_lossy().into_owned())
        .unwrap_or_else(|| BRIDGE_REL.into())
}

/// Resuelve `.` y `..` sin tocar el disco.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

fn expand_tilde(path: &str, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return PathBuf::from(path);
    };
    match path {
        "~" => home.to_path_buf(),
        _ => match path.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(path),
        },
    }
}

fn config_path(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    match xdg.filter(|value| !value.is_empty()) {
        Some(value) => Some(Path::new(value).join(DIR_NAME).join(FILE_NAME)),
        None => home.map(|value| {
            Path::new(value)
                .join(".config")
                .join(DIR_NAME)
                .join(FILE_NAME)
        }),
    }
}

fn config_file_path() -> Option<PathBuf> {
    config_path(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

#[derive(Clone, Debug)]
struct ConnDefaults {
    python: String,
    bridge: String,
    timeout_secs: u64,
}

/// Mezcla el TOML con los valores por defecto. Lo que no encaja se ignora.
fn parse(src: &str, env: &Env<'_>) -> Config {
    let mut cfg = Config::base(env);
    let Ok(doc) = src.parse::<Table>() else {
        return cfg;
    };

    if let Some(value) = doc.get("max_rows").and_then(Value::as_integer) {
        cfg.max_rows = value.clamp(MAX_ROWS_MIN, MAX_ROWS_MAX) as usize;
    }
    if let Some(value) = doc.get("auto_run").and_then(Value::as_bool) {
        cfg.auto_run = value;
    }
    if let Some(value) = text(&doc, "default_conn") {
        cfg.default_conn = Some(value);
    }
    if let Some(value) = doc.get("timeout_secs").and_then(Value::as_integer) {
        cfg.timeout_secs = clamp_timeout(value);
    }
    if let Some(value) = text(&doc, "oracle_python") {
        cfg.python = expand_tilde(&value, env.home.as_deref())
            .to_string_lossy()
            .into_owned();
    }
    if let Some(table) = doc.get("ai").and_then(Value::as_table) {
        cfg.ai.apply_toml(table);
    }
    cfg.ai.validate();

    if let Some(items) = doc.get("connections").and_then(Value::as_array) {
        let defaults = ConnDefaults {
            python: cfg.python.clone(),
            bridge: cfg.bridge.clone(),
            timeout_secs: cfg.timeout_secs,
        };
        let mut connections: Vec<ConnSpec> = Vec::new();
        for item in items {
            let Some(spec) = item
                .as_table()
                .and_then(|table| parse_connection(table, env, &defaults))
            else {
                continue;
            };
            if !connections
                .iter()
                .any(|known| known.info.id == spec.info.id)
            {
                connections.push(spec);
            }
        }
        cfg.connections = connections;
    }
    cfg
}

/// Cadena no vacía del TOML, ya recortada.
fn text(table: &Table, key: &str) -> Option<String> {
    table
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn clamp_timeout(value: i64) -> u64 {
    value.clamp(TIMEOUT_MIN, TIMEOUT_MAX) as u64
}

fn parse_connection(table: &Table, env: &Env<'_>, defaults: &ConnDefaults) -> Option<ConnSpec> {
    let id = text(table, "id")?;
    let host = text(table, "host")?;
    let label = text(table, "label").unwrap_or_else(|| id.clone());
    let dialect = match text(table, "dialect")?.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" => Dialect::Postgres,
        "oracle" => Dialect::Oracle,
        _ => return None,
    };
    let port = table
        .get("port")
        .and_then(Value::as_integer)
        .filter(|value| (1..=65535).contains(value))
        .map_or_else(
            || match dialect {
                Dialect::Postgres => 5432,
                Dialect::Oracle => 1521,
            },
            |value| value as u16,
        );
    let env_kind = match text(table, "env")
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("dev") => EnvKind::Dev,
        Some("prod") => EnvKind::Prod,
        _ => EnvKind::Other,
    };
    let database = text(table, "database").unwrap_or_default();
    let user = text(table, "user").unwrap_or_default();
    let (password, secret) = resolve_password(table, env);
    let timeout_secs = table
        .get("timeout_secs")
        .and_then(Value::as_integer)
        .map_or(defaults.timeout_secs, clamp_timeout);
    let python = text(table, "python")
        .map(|value| {
            expand_tilde(&value, env.home.as_deref())
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_else(|| defaults.python.clone());
    let bridge = text(table, "bridge")
        .map(|value| {
            expand_tilde(&value, env.home.as_deref())
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_else(|| defaults.bridge.clone());

    Some(ConnSpec {
        info: ConnInfo {
            id,
            label,
            dialect,
            env: env_kind,
            target: format!("{host}:{port}/{database}"),
        },
        host,
        port,
        database,
        user,
        password,
        secret,
        oracle_service: table
            .get("service")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        python,
        bridge,
        timeout_secs,
        schemas: schema_list(table),
    })
}

/// `schemas = ["A", "B"]` (o `"A, B"`): esquemas que ve la conexión. Ausente o vacío = todos.
fn schema_list(table: &Table) -> Vec<String> {
    let raw: Vec<String> = match table.get("schemas") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(s)) => s.split(',').map(str::to_string).collect(),
        _ => vec![],
    };
    raw.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Orden: `password`, `password_env`, `password_file` + `password_key`. Si no se resuelve,
/// la conexión se mantiene con clave vacía para que la interfaz enseñe el error de autenticación.
fn resolve_password(table: &Table, env: &Env<'_>) -> (String, Secret) {
    if let Some(password) = table.get("password").and_then(Value::as_str) {
        return (password.to_string(), Secret::Plain);
    }
    if let Some(name) = text(table, "password_env") {
        let value = (env.var)(&name).unwrap_or_default();
        return (value, Secret::Env(name));
    }
    if let Some(path) = text(table, "password_file") {
        let key = text(table, "password_key").unwrap_or_default();
        let file = expand_tilde(&path, env.home.as_deref());
        let value = std::fs::read_to_string(file)
            .ok()
            .and_then(|src| secret_from_file(&src, &key))
            .unwrap_or_default();
        return (value, Secret::File { path, key });
    }
    (String::new(), Secret::Plain)
}

/// Busca `CLAVE=valor` o `export CLAVE=valor` en el contenido de un fichero de credenciales.
fn secret_from_file(src: &str, key: &str) -> Option<String> {
    if key.trim().is_empty() {
        return None;
    }
    for line in src.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim() != key.trim() {
            continue;
        }
        let value = value.trim();
        let quoted = value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')));
        return Some(if quoted {
            value[1..value.len() - 1].to_string()
        } else {
            value.to_string()
        });
    }
    None
}

fn dialect_name(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Postgres => "postgres",
        Dialect::Oracle => "oracle",
    }
}

fn env_name(env: EnvKind) -> &'static str {
    match env {
        EnvKind::Dev => "dev",
        EnvKind::Prod => "prod",
        EnvKind::Other => "other",
    }
}

/// Cadena básica de TOML, correctamente escapada.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04X}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// TOML completo. Las conexiones conservan el origen de su clave: una edición que no la
/// toca no la convierte en texto plano.
fn serialize(cfg: &Config) -> String {
    let mut out = String::new();
    out.push_str("# herdr-dbview — configuración (contiene claves: permisos 0600).\n");
    out.push_str(&format!("max_rows = {}\n", cfg.max_rows));
    out.push_str(&format!("auto_run = {}\n", cfg.auto_run));
    if let Some(id) = &cfg.default_conn {
        out.push_str(&format!("default_conn = {}\n", toml_string(id)));
    }
    out.push_str(&format!("timeout_secs = {}\n", cfg.timeout_secs));
    out.push_str(&format!("oracle_python = {}\n\n", toml_string(&cfg.python)));

    let defaults = AiConfig::default();
    if cfg.ai.model != defaults.model
        || cfg.ai.effort != defaults.effort
        || cfg.ai.url != defaults.url
    {
        out.push_str("[ai]\n");
        out.push_str(&format!("model = {}\n", toml_string(&cfg.ai.model)));
        out.push_str(&format!("effort = {}\n", toml_string(&cfg.ai.effort)));
        out.push_str(&format!("url = {}\n\n", toml_string(&cfg.ai.url)));
    }

    for conn in &cfg.connections {
        out.push_str("[[connections]]\n");
        out.push_str(&format!("id = {}\n", toml_string(&conn.info.id)));
        out.push_str(&format!("label = {}\n", toml_string(&conn.info.label)));
        out.push_str(&format!(
            "dialect = {}\n",
            toml_string(dialect_name(conn.info.dialect))
        ));
        out.push_str(&format!("env = {}\n", toml_string(env_name(conn.info.env))));
        out.push_str(&format!("host = {}\n", toml_string(&conn.host)));
        out.push_str(&format!("port = {}\n", conn.port));
        out.push_str(&format!("database = {}\n", toml_string(&conn.database)));
        out.push_str(&format!("user = {}\n", toml_string(&conn.user)));
        if conn.oracle_service {
            out.push_str("service = true\n");
        }
        match &conn.secret {
            Secret::Plain => out.push_str(&format!("password = {}\n", toml_string(&conn.password))),
            Secret::Env(name) => out.push_str(&format!("password_env = {}\n", toml_string(name))),
            Secret::File { path, key } => {
                out.push_str(&format!("password_file = {}\n", toml_string(path)));
                out.push_str(&format!("password_key = {}\n", toml_string(key)));
            }
        }
        if !conn.schemas.is_empty() {
            let list: Vec<String> = conn.schemas.iter().map(|s| toml_string(s)).collect();
            out.push_str(&format!("schemas = [{}]\n", list.join(", ")));
        }
        if conn.timeout_secs != cfg.timeout_secs {
            out.push_str(&format!("timeout_secs = {}\n", conn.timeout_secs));
        }
        if conn.python != cfg.python {
            out.push_str(&format!("python = {}\n", toml_string(&conn.python)));
        }
        if conn.bridge != cfg.bridge {
            out.push_str(&format!("bridge = {}\n", toml_string(&conn.bridge)));
        }
        out.push('\n');
    }
    out
}

fn write_config(path: &Path, cfg: &Config) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    let data = serialize(cfg);
    let written = (|| -> io::Result<()> {
        let mut file = private_file(&tmp)?;
        file.write_all(data.as_bytes())?;
        file.sync_all()
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    if path.exists() {
        let backup = path.with_extension("toml.bak");
        std::fs::copy(path, &backup)?;
        set_mode(&backup, 0o600)?;
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    set_mode(path, 0o600)
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    set_mode(dir, 0o700)
}

fn private_file(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    set_mode(path, 0o600)?;
    Ok(file)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn parse_with_vars(src: &str, vars: &BTreeMap<String, String>) -> Config {
        let var = |key: &str| vars.get(key).cloned();
        let env = Env {
            home: Some(PathBuf::from("/home/u")),
            bridge: "/proyecto/bridge/oracle_bridge.py".into(),
            var: &var,
        };
        parse(src, &env)
    }

    fn parse_with(src: &str) -> Config {
        let vars = BTreeMap::new();
        parse_with_vars(src, &vars)
    }

    fn conn(id: &str, dialect: Dialect, password: &str, secret: Secret) -> ConnSpec {
        ConnSpec {
            info: ConnInfo {
                id: id.into(),
                label: format!("{id} · prueba"),
                dialect,
                env: EnvKind::Dev,
                target: String::new(),
            },
            host: "db.local".into(),
            port: 5432,
            database: "base".into(),
            user: "usuario".into(),
            password: password.into(),
            secret,
            oracle_service: false,
            python: "/py3".into(),
            bridge: "/puente.py".into(),
            timeout_secs: 30,
            schemas: vec![],
        }
    }

    #[test]
    fn vacio_es_por_defecto() {
        let cfg = parse_with("");
        assert_eq!(cfg.max_rows, 500);
        assert!(cfg.auto_run);
        assert_eq!(cfg.default_conn, None);
        assert_eq!(cfg.timeout_secs, 30);
        assert_eq!(
            cfg.python,
            "python3"
        );
        assert_eq!(cfg.bridge, "/proyecto/bridge/oracle_bridge.py");
        assert!(cfg.connections.is_empty());
        assert_eq!(parse_with("# solo comentarios\n"), cfg);
        assert_eq!(parse_with("max_rows = "), cfg);
    }

    #[test]
    fn claves_simples() {
        let cfg = parse_with(
            "max_rows = 1000\nauto_run = false\ndefault_conn = \"dev-ventas\"\ntimeout_secs = 120\noracle_python = \"~/py3\"\n",
        );
        assert_eq!(cfg.max_rows, 1000);
        assert!(!cfg.auto_run);
        assert_eq!(cfg.default_conn.as_deref(), Some("dev-ventas"));
        assert_eq!(cfg.timeout_secs, 120);
        assert_eq!(cfg.python, "/home/u/py3");
    }

    #[test]
    fn limites_y_tipos() {
        assert_eq!(parse_with("max_rows = 0").max_rows, 1);
        assert_eq!(parse_with("max_rows = -5").max_rows, 1);
        assert_eq!(parse_with("max_rows = 20000").max_rows, 10_000);
        assert_eq!(parse_with("timeout_secs = 0").timeout_secs, 1);
        assert_eq!(parse_with("timeout_secs = 99999").timeout_secs, 3600);
        let cfg = parse_with(
            "max_rows = \"500\"\nauto_run = 1\ndefault_conn = 3\ntimeout_secs = \"x\"\nconnections = 4\n",
        );
        assert_eq!((cfg.max_rows, cfg.timeout_secs), (500, 30));
        assert!(cfg.auto_run);
        assert_eq!(cfg.default_conn, None);
        assert!(cfg.connections.is_empty());
    }

    #[test]
    fn conexiones() {
        let cfg = parse_with(
            r#"
            [[connections]]
            id = "dev-ventas"
            label = "Ventas · DESARROLLO"
            dialect = "postgres"
            env = "dev"
            host = "10.0.0.1"
            port = 5432
            database = "app_db"
            user = "app_db"
            password = "secreta"

            [[connections]]
            id = "prod-oracle"
            dialect = "oracle"
            env = "prod"
            host = "oraclehost"
            database = "ORCL"
            user = "lector"
            service = true
            password_env = "ORACLE_PASSWORD"
            "#,
        );
        assert_eq!(cfg.connections.len(), 2);
        let first = &cfg.connections[0];
        assert_eq!(first.info.label, "Ventas · DESARROLLO");
        assert_eq!(first.info.target, "10.0.0.1:5432/app_db");
        assert_eq!(first.password, "secreta");
        assert_eq!(first.secret, Secret::Plain);
        assert_eq!(first.timeout_secs, 30);
        assert_eq!(
            first.python,
            "python3"
        );
        let second = &cfg.connections[1];
        assert_eq!(second.info.label, "prod-oracle");
        assert_eq!(second.port, 1521);
        assert_eq!(second.info.target, "oraclehost:1521/ORCL");
        assert!(second.oracle_service);
        assert_eq!(second.secret, Secret::Env("ORACLE_PASSWORD".into()));
        assert_eq!(second.password, "");
    }

    #[test]
    fn conexiones_invalidas_se_ignoran() {
        let cfg = parse_with(
            r#"
            [[connections]]
            id = ""
            host = "x"
            dialect = "postgres"

            [[connections]]
            id = "sin-host"
            dialect = "postgres"

            [[connections]]
            id = "sin-dialecto"
            host = "x"

            [[connections]]
            id = "dialecto-raro"
            host = "x"
            dialect = "mysql"

            [[connections]]
            id = "repetida"
            host = "a"
            dialect = "postgres"

            [[connections]]
            id = "repetida"
            host = "b"
            dialect = "oracle"
            "#,
        );
        assert_eq!(cfg.connections.len(), 1);
        assert_eq!(cfg.connections[0].host, "a");
        assert_eq!(cfg.connections[0].info.env, EnvKind::Other);
    }

    #[test]
    fn claves_env_y_fichero() {
        let mut vars = BTreeMap::new();
        vars.insert("PG_PASS".to_string(), "desde-el-entorno".to_string());
        let cfg = parse_with_vars(
            r#"
            [[connections]]
            id = "env"
            host = "a"
            dialect = "postgres"
            password_env = "PG_PASS"

            [[connections]]
            id = "env-vacia"
            host = "b"
            dialect = "postgres"
            password_env = "NO_EXISTE"

            [[connections]]
            id = "fichero"
            host = "c"
            dialect = "postgres"
            password_file = "~/no-existe.env"
            password_key = "CLAVE"
            "#,
            &vars,
        );
        assert_eq!(cfg.connections[0].password, "desde-el-entorno");
        assert_eq!(cfg.connections[0].secret, Secret::Env("PG_PASS".into()));
        assert_eq!(cfg.connections[1].password, "");
        assert_eq!(cfg.connections[1].secret, Secret::Env("NO_EXISTE".into()));
        assert_eq!(cfg.connections[2].password, "");
        assert_eq!(
            cfg.connections[2].secret,
            Secret::File {
                path: "~/no-existe.env".into(),
                key: "CLAVE".into()
            }
        );
    }

    #[test]
    fn fichero_de_credenciales() {
        let src =
            "# claves\nexport PG_PASS=\"con espacios\"\nOTRA='comillas'\nPG_PASS2=sin_comillas\n";
        assert_eq!(
            secret_from_file(src, "PG_PASS").as_deref(),
            Some("con espacios")
        );
        assert_eq!(secret_from_file(src, "OTRA").as_deref(), Some("comillas"));
        assert_eq!(
            secret_from_file(src, "PG_PASS2").as_deref(),
            Some("sin_comillas")
        );
        assert_eq!(secret_from_file(src, "NO_EXISTE"), None);
        assert_eq!(secret_from_file(src, ""), None);
    }

    #[test]
    fn claves_por_conexion() {
        let cfg = parse_with(
            r#"
            timeout_secs = 20
            oracle_python = "/global/python"
            [[connections]]
            id = "a"
            host = "x"
            dialect = "oracle"
            timeout_secs = 90
            python = "~/venv/bin/python3"
            bridge = "~/bridge/oracle_bridge.py"
            "#,
        );
        let conn = &cfg.connections[0];
        assert_eq!(conn.timeout_secs, 90);
        assert_eq!(conn.python, "/home/u/venv/bin/python3");
        assert_eq!(conn.bridge, "/home/u/bridge/oracle_bridge.py");
        assert_eq!(cfg.timeout_secs, 20);
        assert_eq!(cfg.python, "/global/python");
    }

    #[test]
    fn ai_toml() {
        let cfg = parse_with(
            "[ai]\nmodel = \"otro\"\neffort = \"max\"\nurl = \"http://localhost:8080/v1\"\n",
        );
        assert_eq!(
            (
                cfg.ai.model.as_str(),
                cfg.ai.effort.as_str(),
                cfg.ai.url.as_str()
            ),
            ("otro", "max", "http://localhost:8080/v1")
        );
        assert_eq!(cfg.ai.api_key, None);
        let cfg = parse_with("[ai]\neffort = \"imposible\"\n");
        assert!(cfg.ai.error.is_some());
        let cfg = parse_with("[ai]\nurl = \"file:///etc/passwd\"\n");
        assert!(cfg.ai.error.is_some());
    }

    #[test]
    fn entorno_gana_sobre_toml() {
        let mut cfg = parse_with("[ai]\nmodel = \"toml\"\neffort = \"low\"\n");
        cfg.ai.apply_env(|key| match key {
            "DBVIEW_AI_MODEL" => Some("entorno".into()),
            "DBVIEW_AI_EFFORT" => Some("high".into()),
            "DBVIEW_AI_API_KEY_VALUE" => Some("clave-de-prueba".into()),
            _ => None,
        });
        assert_eq!(
            (cfg.ai.model.as_str(), cfg.ai.effort.as_str()),
            ("entorno", "high")
        );
        assert_eq!(cfg.ai.api_key.as_deref(), Some("clave-de-prueba"));
        assert_eq!(cfg.ai.error, None);
        assert!(!format!("{:?}", cfg.ai).contains("clave-de-prueba"));
    }

    #[test]
    fn entorno_invalido() {
        let mut cfg = parse_with("");
        cfg.ai
            .apply_env(|key| (key == "DBVIEW_AI_EFFORT").then(|| "imposible".to_string()));
        assert!(cfg.ai.error.is_some());
        let mut cfg = parse_with("");
        cfg.ai
            .apply_env(|key| (key == "DBVIEW_AI_MODEL").then(String::new));
        assert!(cfg.ai.error.is_some());
        let mut cfg = parse_with("");
        cfg.ai
            .apply_env(|key| (key == "DBVIEW_AI_API_KEY_VALUE").then(|| "con\nsalto".to_string()));
        assert!(cfg.ai.error.is_some());
        // Una variable mala no impide corregir otra.
        let mut cfg = parse_with("[ai]\neffort = \"imposible\"\n");
        cfg.ai
            .apply_env(|key| (key == "DBVIEW_AI_EFFORT").then(|| "max".to_string()));
        assert_eq!(cfg.ai.error, None);
    }

    #[test]
    fn clave_compartida() {
        let raw = r#"{"herdr-dbview":{"key":"propia"},"opencode-go":{"key":"respaldo"}}"#;
        assert_eq!(shared_ai_key(raw).as_deref(), Some("propia"));
        assert_eq!(
            shared_ai_key(r#"{"opencode-go":{"key":"respaldo"}}"#).as_deref(),
            Some("respaldo")
        );
        assert_eq!(shared_ai_key(r#"{"opencode-go":{"key":" "}}"#), None);
        assert_eq!(shared_ai_key("roto"), None);
    }

    #[test]
    fn cadenas_toml_escapadas() {
        let value = "comillas \" y barra \\ y salto\n y tabulador\t y control \u{7}";
        let quoted = toml_string(value);
        let doc: Table = format!("clave = {quoted}").parse().unwrap();
        assert_eq!(doc.get("clave").and_then(Value::as_str), Some(value));
    }

    #[test]
    fn esquemas_de_la_conexion_ida_y_vuelta() {
        let vars = BTreeMap::new();
        let cfg = parse_with_vars(
            r#"
            [[connections]]
            id = "tasaciones"
            label = "Oracle · Tasaciones"
            dialect = "oracle"
            host = "h"
            port = 1521
            database = "ORCL"
            user = "u"
            password = "p"
            schemas = ["VENTAS_APP", " VENTAS_AUX ", ""]

            [[connections]]
            id = "engineering"
            label = "Oracle · Engineering"
            dialect = "oracle"
            host = "h"
            port = 1521
            database = "ORCL"
            user = "u"
            password = "p"
            schemas = "CERT, CERT_AUX"

            [[connections]]
            id = "todo"
            label = "Todo"
            dialect = "postgres"
            host = "h"
            port = 5432
            database = "d"
            user = "u"
            password = "p"
            "#,
            &vars,
        );
        assert_eq!(cfg.connections[0].schemas, ["VENTAS_APP", "VENTAS_AUX"]);
        assert_eq!(cfg.connections[1].schemas, ["CERT", "CERT_AUX"]);
        assert!(cfg.connections[2].schemas.is_empty(), "vacío = todos");
        // Se escribe y se vuelve a leer igual.
        let texto = serialize(&cfg);
        let otra = parse_with_vars(&texto, &vars);
        assert_eq!(otra.connections[0].schemas, cfg.connections[0].schemas);
        assert_eq!(otra.connections[1].schemas, cfg.connections[1].schemas);
        assert!(otra.connections[2].schemas.is_empty());
    }

    #[test]
    fn el_esquema_se_restringe_sin_distinguir_mayusculas() {
        let t = |schema: &str, name: &str| TableInfo {
            schema: schema.into(),
            name: name.into(),
            kind: TableKind::Table,
            columns: vec![],
            comment: None,
            rows: None,
            fks: vec![],
        };
        let sc = Schema {
            tables: vec![
                t("CERT", "DOCUMENTOS"),
                t("VENTAS_AUX", "DOCUMENTOS"),
                t("VENTAS_APP", "SOLICITUDES"),
            ],
        };
        let tasaciones = sc
            .clone()
            .restricted(&["ventas_aux".into(), "VENTAS_APP".into()]);
        let nombres: Vec<String> = tasaciones.tables.iter().map(|x| x.qualified()).collect();
        assert_eq!(nombres, ["VENTAS_AUX.DOCUMENTOS", "VENTAS_APP.SOLICITUDES"]);
        assert_eq!(sc.restricted(&[]).tables.len(), 3);
    }

    #[test]
    fn ida_y_vuelta() {
        let mut vars = BTreeMap::new();
        vars.insert(
            "PGPROD_PASSWORD".to_string(),
            "clave-del-entorno".to_string(),
        );
        let mut cfg = parse_with_vars("", &vars);
        cfg.max_rows = 1000;
        cfg.auto_run = false;
        cfg.default_conn = Some("dev".into());
        cfg.timeout_secs = 45;
        cfg.ai.model = "otro-modelo".into();
        cfg.ai.effort = "max".into();
        cfg.ai.url = "http://localhost:9000/chat".into();
        cfg.connections = vec![
            ConnSpec {
                info: ConnInfo {
                    id: "dev".into(),
                    label: "Ventas \"dev\" · DESARROLLO".into(),
                    dialect: Dialect::Postgres,
                    env: EnvKind::Dev,
                    target: "10.0.0.1:5432/app_db".into(),
                },
                host: "10.0.0.1".into(),
                port: 5432,
                database: "app_db".into(),
                user: "app_db".into(),
                password: "con \"comillas\"\\ y\nsalto".into(),
                secret: Secret::Plain,
                oracle_service: false,
                python: "python3".into(),
                bridge: "/proyecto/bridge/oracle_bridge.py".into(),
                timeout_secs: 45,
                schemas: vec![],
            },
            ConnSpec {
                info: ConnInfo {
                    id: "prod".into(),
                    label: "Producción".into(),
                    dialect: Dialect::Oracle,
                    env: EnvKind::Prod,
                    target: "oraclehost:1521/ORCL".into(),
                },
                host: "oraclehost".into(),
                port: 1521,
                database: "ORCL".into(),
                user: "lector".into(),
                password: "clave-del-entorno".into(),
                secret: Secret::Env("PGPROD_PASSWORD".into()),
                oracle_service: true,
                python: "/opt/python3".into(),
                bridge: "/opt/oracle_bridge.py".into(),
                timeout_secs: 12,
                schemas: vec![],
            },
            ConnSpec {
                info: ConnInfo {
                    id: "fichero".into(),
                    label: "Fichero".into(),
                    dialect: Dialect::Postgres,
                    env: EnvKind::Other,
                    target: "pg:5432/base".into(),
                },
                host: "pg".into(),
                port: 5432,
                database: "base".into(),
                user: "usuario".into(),
                password: String::new(),
                secret: Secret::File {
                    path: "~/.config/dbview/secrets.env".into(),
                    key: "CLAVE".into(),
                },
                oracle_service: false,
                python: "python3".into(),
                bridge: "/proyecto/bridge/oracle_bridge.py".into(),
                timeout_secs: 45,
                schemas: vec![],
            },
        ];
        cfg.ai.api_key = Some("no-se-guarda".into());
        let serialized = serialize(&cfg);
        assert!(!serialized.contains("no-se-guarda"));
        let mut back = parse_with_vars(&serialized, &vars);
        // La clave de entorno y la compartida no están en el TOML: se resuelven igual al cargar.
        back.ai.api_key = cfg.ai.api_key.clone();
        assert_eq!(back, cfg);
        assert!(serialized.contains("password_env = \"PGPROD_PASSWORD\""));
        assert!(serialized.contains("password_file = \"~/.config/dbview/secrets.env\""));
        assert!(!serialized.contains("password = \"clave-del-entorno\""));
    }

    #[test]
    fn guardar_respeta_permisos_y_copia() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("herdr-dbview-prueba-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        let mut cfg = parse_with("max_rows = 123");
        cfg.connections = vec![conn("dev", Dialect::Postgres, "secreta", Secret::Plain)];

        write_config(&path, &cfg).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("max_rows = 123"));
        assert!(saved.contains("password = \"secreta\""));

        cfg.max_rows = 456;
        write_config(&path, &cfg).unwrap();
        let backup = std::fs::read_to_string(dir.join("config.toml.bak")).unwrap();
        assert!(backup.contains("max_rows = 123"));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("max_rows = 456")
        );
        assert!(!dir.join("config.toml.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ruta_de_configuracion() {
        let xdg = config_path(Some(OsStr::new("/xdg")), Some(OsStr::new("/home/u")));
        assert_eq!(xdg, Some(PathBuf::from("/xdg/herdr-dbview/config.toml")));
        let vacio = config_path(Some(OsStr::new("")), Some(OsStr::new("/home/u")));
        assert_eq!(
            vacio,
            Some(PathBuf::from("/home/u/.config/herdr-dbview/config.toml"))
        );
        assert_eq!(config_path(None, Some(OsStr::new("/home/u"))), vacio);
        assert_eq!(config_path(None, None), None);
    }

    #[test]
    fn rutas_normalizadas() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./bridge.py")),
            PathBuf::from("/a/c/bridge.py")
        );
        assert_eq!(
            normalize(Path::new("bridge/./oracle_bridge.py")),
            PathBuf::from("bridge/oracle_bridge.py")
        );
        assert_eq!(normalize(Path::new("/a/../../b")), PathBuf::from("/b"));
    }
}
