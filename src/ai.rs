//! Transporte de la IA: `curl` por stdin + SSE. La clave viaja en la configuración de curl
//! (stdin), nunca en `argv`, y no aparece en los mensajes de error.

use crate::config::{AI_URL, AiConfig};
use anyhow::{Context, Result, bail};
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

const RESPONSE_MAX: usize = 16_000;
const MAX_TOKENS: u32 = 2_048;
const TOTAL_TIMEOUT_SECS: u64 = 90;
const HARD_TIMEOUT: Duration = Duration::from_secs(TOTAL_TIMEOUT_SECS + 10);

/// Pide una respuesta a la IA en streaming. `on_text` recibe el TEXTO ACUMULADO hasta el momento.
/// Bloquea hasta terminar; devuelve el texto final. Se aborta si `cancel` pasa a `true`.
pub fn chat(
    cfg: &AiConfig,
    system: &str,
    user: &str,
    cancel: Arc<AtomicBool>,
    mut on_text: impl FnMut(&str),
) -> Result<String> {
    if let Some(error) = &cfg.error {
        bail!("{error}");
    }
    let key = cfg.api_key.as_deref().filter(|key| !key.trim().is_empty());
    if key.is_some_and(|key| key.chars().any(char::is_control)) {
        bail!("La clave de IA no es válida");
    }
    if cfg.url == AI_URL && key.is_none() {
        bail!(
            "Falta la clave de OpenCode Go: configura DBVIEW_AI_API_KEY_VALUE o el almacén de credenciales de OpenCode"
        );
    }
    if cancel.load(Ordering::Relaxed) {
        bail!("Consulta cancelada");
    }
    let body = build_body(cfg, system, user);
    match stream(cfg, &body, &cancel, &mut on_text) {
        Ok(text) => Ok(text),
        // Algunos proveedores rechazan `temperature`; se reintenta una vez sin ella.
        Err(error) if error.status == 400 && body.contains("\"temperature\"") => {
            let retry = without_temperature(&body);
            stream(cfg, &retry, &cancel, &mut on_text)
                .map_err(|error| anyhow::anyhow!(error.message))
        }
        Err(error) => bail!("{}", error.message),
    }
}

/// Cuerpo de la petición (puro, sin red).
pub fn build_body(cfg: &AiConfig, system: &str, user: &str) -> String {
    let mut body = json!({
        "model": cfg.model,
        "stream": true,
        "max_tokens": MAX_TOKENS,
        "temperature": 0,
        "reasoning_effort": cfg.effort,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
    });
    if cfg.model.to_lowercase().contains("deepseek") {
        body["thinking"] =
            json!({"type": if cfg.effort == "none" { "disabled" } else { "enabled" }});
    }
    body.to_string()
}

/// Cabeceras de la petición (puras, sin red). Incluyen la clave: nunca se imprimen.
pub fn build_headers(cfg: &AiConfig) -> Vec<(String, String)> {
    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    if let Some(key) = cfg.api_key.as_deref().filter(|key| !key.trim().is_empty()) {
        headers.push(("Authorization".to_string(), format!("Bearer {key}")));
    }
    if cfg.url == AI_URL {
        headers.push((
            "x-opencode-session".to_string(),
            format!("herdr-dbview-{}", std::process::id()),
        ));
    }
    headers
}

/// Quita `temperature` del cuerpo, si está. Puro.
fn without_temperature(body: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.to_string();
    };
    if let Some(map) = value.as_object_mut() {
        map.remove("temperature");
    }
    value.to_string()
}

fn quote(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}

/// Configuración de curl con la URL, las cabeceras y el cuerpo, todo por stdin.
fn curl_config(cfg: &AiConfig, body: &str) -> String {
    let mut config = format!("url = {}\ndata-binary = {}\n", quote(&cfg.url), quote(body));
    for (name, value) in build_headers(cfg) {
        config.push_str(&format!(
            "header = {}\n",
            quote(&format!("{name}: {value}"))
        ));
    }
    config
}

#[derive(Debug)]
struct Failure {
    message: String,
    status: u16,
}
impl Failure {
    fn new(message: impl Into<String>) -> Self {
        Failure {
            message: message.into(),
            status: 0,
        }
    }
}

fn stream(
    cfg: &AiConfig,
    body: &str,
    cancel: &AtomicBool,
    on_text: &mut impl FnMut(&str),
) -> std::result::Result<String, Failure> {
    let config = curl_config(cfg, body);
    let max_time = TOTAL_TIMEOUT_SECS.to_string();
    let mut process = Command::new("curl")
        .args([
            "--disable",
            "--silent",
            "--show-error",
            "--no-buffer",
            "--fail",
            "--connect-timeout",
            "10",
            "--max-time",
            &max_time,
            "--proto",
            "=http,https",
            "--write-out",
            "\n%{http_code}\n",
            "--config",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            Failure::new(format!(
                "No se pudo iniciar curl para consultar la IA: {error}"
            ))
        })?;
    let write = process
        .stdin
        .take()
        .context("Sin entrada para curl")
        .and_then(|mut input| {
            input
                .write_all(config.as_bytes())
                .context("No se pudo enviar la consulta")
        });
    if let Err(error) = write {
        let _ = process.kill();
        let _ = process.wait();
        return Err(Failure::new(error.to_string()));
    }
    let stdout = match process.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = process.kill();
            let _ = process.wait();
            return Err(Failure::new("Sin salida de curl"));
        }
    };
    let (tx, rx) = mpsc::sync_channel(32);
    let reader = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut bytes = Vec::new();
            match reader.by_ref().take(262_145).read_until(b'\n', &mut bytes) {
                Ok(0) => break,
                Ok(_) if bytes.len() <= 262_144 => {
                    if tx.send(String::from_utf8(bytes).map_err(|_| ())).is_err() {
                        break;
                    }
                }
                _ => {
                    let _ = tx.send(Err(()));
                    break;
                }
            }
        }
    });

    let mut text = String::new();
    let mut http_status = 0u16;
    let started = Instant::now();
    let result = (|| -> std::result::Result<String, Failure> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Failure::new("Consulta cancelada"));
            }
            if started.elapsed() > HARD_TIMEOUT {
                return Err(Failure::new(format!(
                    "La IA ha tardado demasiado ({TOTAL_TIMEOUT_SECS} s)"
                )));
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Ok(line)) => {
                    let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                        if let Ok(code) = line.trim().parse::<u16>() {
                            http_status = code;
                        }
                        continue;
                    };
                    if data == "[DONE]" {
                        if text.trim().is_empty() {
                            return Err(Failure::new("La IA no devolvió ninguna respuesta"));
                        }
                        return Ok(text.trim().to_string());
                    }
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                        return Err(Failure::new(
                            "La respuesta de IA está incompleta o no es válida",
                        ));
                    };
                    if value.get("error").is_some() {
                        return Err(Failure::new("El proveedor no pudo completar la respuesta"));
                    }
                    if let Some(delta) = value["choices"][0]["delta"]["content"].as_str() {
                        text.push_str(delta);
                        if text.chars().count() > RESPONSE_MAX {
                            return Err(Failure::new(
                                "La respuesta de IA superó el tamaño permitido",
                            ));
                        }
                        on_text(&text);
                    }
                }
                Ok(Err(())) => return Err(Failure::new("La respuesta de IA no es válida")),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let status = process.wait().map_err(|error| {
                        Failure::new(format!("No se pudo esperar a curl: {error}"))
                    })?;
                    if status.code() == Some(28) {
                        return Err(Failure::new(format!(
                            "La IA ha tardado demasiado ({TOTAL_TIMEOUT_SECS} s)"
                        )));
                    }
                    if http_status >= 400 {
                        return Err(Failure {
                            message: format!(
                                "El proveedor de IA respondió HTTP {http_status}; comprueba la clave, el modelo y la cuota"
                            ),
                            status: http_status,
                        });
                    }
                    return Err(Failure::new(
                        "No se pudo completar la consulta de IA; comprueba la conexión, el modelo y la clave",
                    ));
                }
            }
        }
    })();
    let _ = process.kill();
    let _ = process.wait();
    drop(rx);
    let _ = reader.join();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn config(url: String) -> AiConfig {
        AiConfig {
            url,
            api_key: Some("clave-de-prueba".into()),
            ..AiConfig::default()
        }
    }

    fn read_request(stream: &mut std::net::TcpStream) -> (String, String) {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut headers = String::new();
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap();
            }
            headers.push_str(&line);
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        (headers, String::from_utf8(body).unwrap())
    }

    fn sse_reply(stream: &mut std::net::TcpStream, text: &str) {
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len()).unwrap();
    }

    #[test]
    fn cuerpo_y_cabeceras() {
        let cfg = AiConfig {
            api_key: Some("privada".into()),
            ..AiConfig::default()
        };
        let body: serde_json::Value =
            serde_json::from_str(&build_body(&cfg, "sistema", "usuario")).unwrap();
        assert_eq!(body["model"], "deepseek-v4.1-flash");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 2048);
        assert_eq!(body["temperature"], 0);
        assert_eq!(body["reasoning_effort"], "none");
        assert_eq!(body["thinking"]["type"], "disabled");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "sistema");
        assert_eq!(body["messages"][1]["content"], "usuario");
        assert!(!build_body(&cfg, "s", "u").contains("privada"));

        let cfg = AiConfig {
            effort: "max".into(),
            ..AiConfig::default()
        };
        let body: serde_json::Value = serde_json::from_str(&build_body(&cfg, "s", "u")).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");

        let headers = build_headers(&AiConfig::default());
        assert!(headers.iter().all(|(name, _)| name != "Authorization"));
        assert!(headers.iter().any(|(name, _)| name == "x-opencode-session"));
        let custom = AiConfig {
            url: "http://localhost:8080/chat".into(),
            ..AiConfig::default()
        };
        assert!(
            build_headers(&custom)
                .iter()
                .all(|(name, _)| name != "x-opencode-session")
        );
        let headers = build_headers(&AiConfig {
            api_key: Some("privada".into()),
            ..AiConfig::default()
        });
        assert!(headers.contains(&("Authorization".into(), "Bearer privada".into())));
        assert!(headers.iter().any(
            |(name, value)| name == "x-opencode-session" && value.starts_with("herdr-dbview-")
        ));
        assert!(!format!("{:?}", cfg).contains("privada"));
    }

    #[test]
    fn sin_temperature() {
        let body = build_body(&AiConfig::default(), "s", "u");
        let retry = without_temperature(&body);
        assert!(!retry.contains("temperature"));
        let value: serde_json::Value = serde_json::from_str(&retry).unwrap();
        assert_eq!(value["max_tokens"], 2048);
        assert_eq!(without_temperature("no es json"), "no es json");
    }

    #[test]
    fn sse_local_y_cancelacion() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (headers, body) = read_request(&mut stream);
            assert!(
                headers
                    .to_lowercase()
                    .contains("authorization: bearer clave-de-prueba")
            );
            let value: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(value["messages"][1]["content"], "pregunta");
            let text = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"ignorado\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"SELECT \"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"1\"}}]}\n\ndata: [DONE]\n\n";
            sse_reply(&mut stream, text);
        });
        let cfg = config(format!("http://{address}/chat"));
        let mut partial = String::new();
        let result = chat(
            &cfg,
            "sistema",
            "pregunta",
            Arc::new(AtomicBool::new(false)),
            |text| partial = text.to_string(),
        )
        .unwrap();
        assert_eq!(result, "SELECT 1");
        assert_eq!(partial, "SELECT 1");
        server.join().unwrap();

        let err = chat(&cfg, "s", "u", Arc::new(AtomicBool::new(true)), |_| {}).unwrap_err();
        assert_eq!(err.to_string(), "Consulta cancelada");
    }

    #[test]
    fn reintenta_sin_temperature() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let (_, body) = read_request(&mut first);
            assert!(body.contains("\"temperature\""));
            write!(
                first,
                "HTTP/1.1 400 Bad Request\r\nContent-Length: 7\r\nConnection: close\r\n\r\nrechazo"
            )
            .unwrap();
            let (mut second, _) = listener.accept().unwrap();
            let (_, body) = read_request(&mut second);
            assert!(!body.contains("\"temperature\""));
            sse_reply(
                &mut second,
                "data: {\"choices\":[{\"delta\":{\"content\":\"SELECT 2\"}}]}\n\ndata: [DONE]\n\n",
            );
        });
        let cfg = config(format!("http://{address}/chat"));
        let result = chat(&cfg, "s", "u", Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
        assert_eq!(result, "SELECT 2");
        server.join().unwrap();
    }

    #[test]
    fn errores_sin_filtrar_la_clave() {
        let cfg = AiConfig {
            api_key: Some("clave-secreta".into()),
            error: Some("modelo roto".into()),
            ..AiConfig::default()
        };
        let error = chat(&cfg, "s", "u", Arc::new(AtomicBool::new(false)), |_| {})
            .unwrap_err()
            .to_string();
        assert_eq!(error, "modelo roto");
        let cfg = AiConfig {
            api_key: None,
            ..AiConfig::default()
        };
        let error = chat(&cfg, "s", "u", Arc::new(AtomicBool::new(false)), |_| {})
            .unwrap_err()
            .to_string();
        assert!(error.contains("Falta la clave"));
        assert!(!error.contains("clave-secreta"));
    }
}
