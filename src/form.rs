//! Formulario de conexión (nueva / editar), con «Probar conexión».

use crate::editor::Editor;
use crate::model::*;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub const F_NAME: usize = 0;
pub const F_DIALECT: usize = 1;
pub const F_ENV: usize = 2;
pub const F_HOST: usize = 3;
pub const F_PORT: usize = 4;
pub const F_DB: usize = 5;
pub const F_MODE: usize = 6;
pub const F_SCHEMAS: usize = 7;
pub const F_USER: usize = 8;
pub const F_PASS: usize = 9;
pub const F_TEST: usize = 10;
pub const F_SAVE: usize = 11;
pub const F_CANCEL: usize = 12;
pub const N_ITEMS: usize = 13;

#[derive(Clone, Debug, PartialEq)]
pub enum TestState {
    Idle,
    Running,
    Ok { info: String, ms: u64 },
    Err(DbError),
}

pub enum FormAct {
    None,
    Test,
    Save,
    Cancel,
}

pub struct Form {
    /// `Some(id)` si se edita una conexión existente.
    pub editing: Option<String>,
    pub name: Editor,
    pub host: Editor,
    pub port: Editor,
    pub database: Editor,
    /// Esquemas que ve la conexión, separados por comas (vacío = todos).
    pub schemas: Editor,
    pub user: Editor,
    pub password: Editor,
    pub dialect: Dialect,
    pub env: EnvKind,
    pub oracle_service: bool,
    pub show_pw: bool,
    pub focus: usize,
    pub secret: Secret,
    /// Clave ya resuelta de la conexión original (se usa si no se escribe otra).
    pub old_password: String,
    pub test: TestState,
    pub error: Option<String>,
    pub spec_extra: Option<ConnSpec>,
}

fn ed(t: &str) -> Editor {
    Editor::new(t)
}

impl Form {
    pub fn new_blank() -> Self {
        Form {
            editing: None,
            name: ed(""),
            host: ed(""),
            port: ed("5432"),
            database: ed(""),
            schemas: ed(""),
            user: ed(""),
            password: ed(""),
            dialect: Dialect::Postgres,
            env: EnvKind::Dev,
            oracle_service: false,
            show_pw: false,
            focus: F_NAME,
            secret: Secret::Plain,
            old_password: String::new(),
            test: TestState::Idle,
            error: None,
            spec_extra: None,
        }
    }

    pub fn from_spec(s: &ConnSpec) -> Self {
        let mut f = Form::new_blank();
        f.editing = Some(s.info.id.clone());
        f.name = ed(&s.info.label);
        f.host = ed(&s.host);
        f.port = ed(&s.port.to_string());
        f.database = ed(&s.database);
        f.schemas = ed(&s.schemas.join(", "));
        f.user = ed(&s.user);
        f.dialect = s.info.dialect;
        f.env = s.info.env;
        f.oracle_service = s.oracle_service;
        f.secret = s.secret.clone();
        f.old_password = s.password.clone();
        f.spec_extra = Some(s.clone());
        f
    }

    pub fn is_oracle(&self) -> bool {
        self.dialect == Dialect::Oracle
    }

    pub fn password_note(&self) -> Option<String> {
        if !self.password.is_empty() || self.editing.is_none() {
            return None;
        }
        Some(match &self.secret {
            Secret::Plain => "se mantiene la guardada".into(),
            Secret::Env(v) => format!("se mantiene la de la variable {v}"),
            Secret::File { key, .. } => format!("se mantiene la de {key}"),
        })
    }

    fn editor_mut(&mut self, i: usize) -> Option<&mut Editor> {
        match i {
            F_NAME => Some(&mut self.name),
            F_HOST => Some(&mut self.host),
            F_PORT => Some(&mut self.port),
            F_DB => Some(&mut self.database),
            F_SCHEMAS => Some(&mut self.schemas),
            F_USER => Some(&mut self.user),
            F_PASS => Some(&mut self.password),
            _ => None,
        }
    }

    pub fn editor_of(&self, i: usize) -> Option<&Editor> {
        match i {
            F_NAME => Some(&self.name),
            F_HOST => Some(&self.host),
            F_PORT => Some(&self.port),
            F_DB => Some(&self.database),
            F_SCHEMAS => Some(&self.schemas),
            F_USER => Some(&self.user),
            F_PASS => Some(&self.password),
            _ => None,
        }
    }

    fn visible(&self, i: usize) -> bool {
        i != F_MODE || self.is_oracle()
    }

    pub fn step(&mut self, delta: isize) {
        let mut i = self.focus as isize;
        loop {
            i = (i + delta).rem_euclid(N_ITEMS as isize);
            if self.visible(i as usize) {
                break;
            }
        }
        self.focus = i as usize;
    }

    pub fn set_dialect(&mut self, d: Dialect) {
        if self.dialect == d {
            return;
        }
        let default_old = if self.dialect == Dialect::Postgres {
            "5432"
        } else {
            "1521"
        };
        if self.port.text() == default_old || self.port.is_empty() {
            self.port = ed(if d == Dialect::Postgres {
                "5432"
            } else {
                "1521"
            });
        }
        self.dialect = d;
        self.test = TestState::Idle;
    }

    /// Elige directamente una opción de un selector (clic): `idx` es la posición de la opción.
    pub fn pick(&mut self, item: usize, idx: usize) {
        self.focus = item;
        match item {
            F_DIALECT => self.set_dialect(if idx == 0 {
                Dialect::Postgres
            } else {
                Dialect::Oracle
            }),
            F_ENV => {
                if let Some(e) = [EnvKind::Dev, EnvKind::Prod, EnvKind::Other].get(idx) {
                    self.env = *e;
                }
            }
            F_MODE => self.oracle_service = idx == 1,
            _ => {}
        }
    }

    fn cycle(&mut self, dir: isize) {
        match self.focus {
            F_DIALECT => {
                let d = if self.dialect == Dialect::Postgres {
                    Dialect::Oracle
                } else {
                    Dialect::Postgres
                };
                self.set_dialect(d);
            }
            F_ENV => {
                let all = [EnvKind::Dev, EnvKind::Prod, EnvKind::Other];
                let i = all.iter().position(|e| *e == self.env).unwrap_or(0) as isize;
                self.env = all[(i + dir).rem_euclid(3) as usize];
            }
            F_MODE => self.oracle_service = !self.oracle_service,
            _ => {}
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) -> FormAct {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => return FormAct::Cancel,
            KeyCode::Char('s') if ctrl => return FormAct::Save,
            KeyCode::Char('t') if ctrl => return FormAct::Test,
            KeyCode::Char('p') if ctrl => {
                self.show_pw = !self.show_pw;
                return FormAct::None;
            }
            KeyCode::Tab | KeyCode::Down => {
                self.step(1);
                return FormAct::None;
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.step(-1);
                return FormAct::None;
            }
            _ => {}
        }
        let f = self.focus;
        if matches!(f, F_DIALECT | F_ENV | F_MODE) {
            match k.code {
                KeyCode::Left => self.cycle(-1),
                KeyCode::Right | KeyCode::Char(' ') => self.cycle(1),
                KeyCode::Enter => self.step(1),
                _ => {}
            }
            return FormAct::None;
        }
        if matches!(f, F_TEST | F_SAVE | F_CANCEL) {
            match k.code {
                KeyCode::Enter | KeyCode::Char(' ') => {
                    return match f {
                        F_TEST => FormAct::Test,
                        F_SAVE => FormAct::Save,
                        _ => FormAct::Cancel,
                    };
                }
                KeyCode::Left => self.step(-1),
                KeyCode::Right => self.step(1),
                _ => {}
            }
            return FormAct::None;
        }
        if k.code == KeyCode::Enter {
            self.step(1);
            return FormAct::None;
        }
        let numeric = f == F_PORT;
        if let (true, KeyCode::Char(c)) = (numeric, k.code)
            && !c.is_ascii_digit()
        {
            return FormAct::None;
        }
        if let Some(e) = self.editor_mut(f) {
            e.handle_key(k);
        }
        self.test = TestState::Idle;
        FormAct::None
    }

    pub fn paste(&mut self, s: &str) {
        let f = self.focus;
        let one: String = s.lines().next().unwrap_or("").to_string();
        if let Some(e) = self.editor_mut(f) {
            e.insert_str(&one);
            self.test = TestState::Idle;
        }
    }

    /// Construye la especificación o dice qué falta.
    pub fn to_spec(
        &self,
        existing_ids: &[String],
        timeout_secs: u64,
        python: &str,
        bridge: &str,
    ) -> Result<ConnSpec, String> {
        let label = self.name.text().trim().to_string();
        if label.is_empty() {
            return Err("Pon un nombre a la conexión".into());
        }
        let host = self.host.text().trim().to_string();
        if host.is_empty() {
            return Err("Falta el servidor (host)".into());
        }
        let port: u16 = self
            .port
            .text()
            .trim()
            .parse()
            .map_err(|_| "El puerto debe ser un número entre 1 y 65535".to_string())?;
        if port == 0 {
            return Err("El puerto debe ser un número entre 1 y 65535".into());
        }
        let database = self.database.text().trim().to_string();
        if database.is_empty() {
            return Err(if self.is_oracle() {
                "Falta el SID o servicio".into()
            } else {
                "Falta el nombre de la base de datos".into()
            });
        }
        let user = self.user.text().trim().to_string();
        if user.is_empty() {
            return Err("Falta el usuario".into());
        }
        let (password, secret) = if self.password.is_empty() && self.editing.is_some() {
            (self.old_password.clone(), self.secret.clone())
        } else {
            (self.password.text(), Secret::Plain)
        };
        let id = match &self.editing {
            Some(id) => id.clone(),
            None => unique_id(&label, existing_ids),
        };
        Ok(ConnSpec {
            info: ConnInfo {
                id,
                label,
                dialect: self.dialect,
                env: self.env,
                target: format!("{host}:{port}/{database}"),
            },
            host,
            port,
            database,
            user,
            password,
            secret,
            oracle_service: self.is_oracle() && self.oracle_service,
            python: python.to_string(),
            bridge: bridge.to_string(),
            timeout_secs,
            schemas: self
                .schemas
                .text()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        })
    }
}

pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let c = c.to_lowercase().next().unwrap_or(c);
        let c = match c {
            'á' | 'à' | 'ä' => 'a',
            'é' | 'è' | 'ë' => 'e',
            'í' | 'ï' => 'i',
            'ó' | 'ö' => 'o',
            'ú' | 'ü' => 'u',
            'ñ' => 'n',
            c => c,
        };
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "conexion".into()
    } else {
        out
    }
}

fn unique_id(label: &str, existing: &[String]) -> String {
    let base = slug(label);
    if !existing.contains(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| !existing.contains(c))
        .unwrap_or(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_y_unicos() {
        assert_eq!(slug("Ventas · DESARROLLO"), "ventas-desarrollo");
        assert_eq!(slug("Año ñoño"), "ano-nono");
        assert_eq!(unique_id("Mi BD", &["mi-bd".into()]), "mi-bd-2");
    }

    #[test]
    fn validacion() {
        let mut f = Form::new_blank();
        assert!(f.to_spec(&[], 30, "python3", "b.py").is_err());
        f.name = Editor::new("Demo");
        f.host = Editor::new("h");
        f.database = Editor::new("d");
        f.user = Editor::new("u");
        let s = f.to_spec(&[], 30, "python3", "b.py").unwrap();
        assert_eq!(s.info.target, "h:5432/d");
        f.port = Editor::new("99999");
        assert!(f.to_spec(&[], 30, "python3", "b.py").is_err());
    }

    #[test]
    fn puerto_por_dialecto() {
        let mut f = Form::new_blank();
        f.set_dialect(Dialect::Oracle);
        assert_eq!(f.port.text(), "1521");
    }
}
