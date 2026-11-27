//! Control de navegador por **CDP** (Chrome DevTools Protocol) sobre WebSocket.
//!
//! Casi todas las acciones se resuelven con `Runtime.evaluate`, así que el
//! navegador se maneja con media docena de métodos en vez de un driver completo.

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const READ_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Browser {
    port: u16,
    profile: PathBuf,
    child: Option<Child>,
    ws: Option<WebSocket<MaybeTlsStream<TcpStream>>>,
    next_id: u64,
}

impl Browser {
    pub fn new(port: u16, profile: PathBuf) -> Self {
        Self {
            port,
            profile,
            child: None,
            ws: None,
            next_id: 1,
        }
    }

    fn endpoint(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Conecta al navegador; lo arranca en headless si aún no escucha.
    fn ensure(&mut self) -> Result<()> {
        if self.ws.is_some() {
            return Ok(());
        }
        if http_get(&self.endpoint("/json/version")).is_none() {
            self.spawn()?;
        }
        let ws_url = self.page_ws_url()?;
        let (mut ws, _) =
            tungstenite::connect(&ws_url).with_context(|| format!("handshake CDP falló: {ws_url}"))?;
        if let MaybeTlsStream::Plain(stream) = ws.get_mut() {
            stream.set_read_timeout(Some(READ_TIMEOUT))?;
        }
        self.ws = Some(ws);
        Ok(())
    }

    fn spawn(&mut self) -> Result<()> {
        let bin = std::env::var_os("AGENT_BROWSER")
            .map(PathBuf::from)
            .or_else(|| {
                find_in_path(&[
                    "google-chrome",
                    "google-chrome-stable",
                    "chromium",
                    "chromium-browser",
                    "microsoft-edge",
                ])
            })
            .context("no se encontró Chrome/Chromium; define AGENT_BROWSER=/ruta/al/binario")?;
        std::fs::create_dir_all(&self.profile)?;
        let child = Command::new(&bin)
            .arg(format!("--remote-debugging-port={}", self.port))
            .arg(format!("--user-data-dir={}", self.profile.display()))
            .args([
                "--headless=new",
                "--disable-gpu",
                "--no-sandbox",
                "--no-first-run",
                "--no-default-browser-check",
                "--window-size=1280,800",
                "about:blank",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("no se pudo lanzar {}", bin.display()))?;
        self.child = Some(child);

        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) {
            if http_get(&self.endpoint("/json/version")).is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        bail!("el navegador no abrió el puerto CDP {}", self.port)
    }

    fn page_ws_url(&self) -> Result<String> {
        let raw = http_get(&self.endpoint("/json/list")).context("no se pudo listar pestañas CDP")?;
        let list: Value = serde_json::from_str(&raw)?;
        if let Some(pages) = list.as_array() {
            for page in pages {
                if page["type"] == "page"
                    && let Some(url) = page["webSocketDebuggerUrl"].as_str()
                {
                    return Ok(url.to_string());
                }
            }
        }
        bail!("no hay pestañas disponibles en el navegador")
    }

    /// Envía un comando CDP y espera la respuesta con el mismo `id`.
    fn cdp(&mut self, method: &str, params: Value) -> Result<Value> {
        self.ensure()?;
        let id = self.next_id;
        self.next_id += 1;
        let payload = json!({"id": id, "method": method, "params": params});
        let ws = self.ws.as_mut().expect("sesión CDP abierta");
        ws.send(Message::Text(payload.to_string().into()))
            .with_context(|| format!("no se pudo enviar {method}"))?;

        loop {
            let msg = ws.read().map_err(|e| match e {
                tungstenite::Error::Io(io) => {
                    anyhow!("timeout o conexión perdida esperando {method}: {io}")
                }
                other => anyhow!("{method}: {other}"),
            })?;
            let text = match msg {
                Message::Text(t) => t.to_string(),
                Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                Message::Close(_) => bail!("el navegador cerró la conexión CDP"),
                _ => continue,
            };
            let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if value["id"].as_u64() != Some(id) {
                continue; // evento asíncrono: se ignora (KISS, sin cola de eventos)
            }
            if !value["error"].is_null() {
                bail!("CDP {method}: {}", value["error"]);
            }
            return Ok(value["result"].clone());
        }
    }

    pub fn evaluate(&mut self, expression: &str) -> Result<String> {
        let result = self.cdp(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
                "userGesture": true,
            }),
        )?;
        if let Some(text) = result["exceptionDetails"]["text"].as_str() {
            bail!("error de JavaScript: {text}");
        }
        let value = &result["result"]["value"];
        Ok(match value {
            Value::String(s) => s.clone(),
            Value::Null => "null".to_string(),
            other => other.to_string(),
        })
    }

    pub fn open(&mut self, url: &str) -> Result<String> {
        if !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("file://"))
        {
            bail!("url no soportada: {url}");
        }
        let result = self.cdp("Page.navigate", json!({"url": url}))?;
        if let Some(err) = result["errorText"].as_str() {
            bail!("navegación fallida: {err}");
        }
        // Espera a que el documento termine de cargar (sin cola de eventos).
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(150));
            if self
                .evaluate("document.readyState")
                .map(|s| s.contains("complete"))
                .unwrap_or(false)
            {
                break;
            }
        }
        let title = self.evaluate("document.title").unwrap_or_default();
        let href = self.evaluate("location.href").unwrap_or_default();
        Ok(format!("{title} — {href}"))
    }

    pub fn text(&mut self) -> Result<String> {
        self.evaluate("document.body ? document.body.innerText.slice(0, 20000) : ''")
    }

    pub fn click(&mut self, selector: &str) -> Result<String> {
        let sel = serde_json::to_string(selector)?;
        let js = format!(
            "(() => {{ const e = document.querySelector({sel}); \
             if (!e) return 'NOT_FOUND'; e.scrollIntoView({{block:'center'}}); e.click(); \
             return 'CLICKED'; }})()"
        );
        self.evaluate(&js)
    }

    pub fn type_text(&mut self, selector: &str, text: &str) -> Result<String> {
        let sel = serde_json::to_string(selector)?;
        let value = serde_json::to_string(text)?;
        let js = format!(
            "(() => {{ const e = document.querySelector({sel}); \
             if (!e) return 'NOT_FOUND'; e.focus(); const v = {value}; \
             if ('value' in e) {{ e.value = v; }} else {{ e.textContent = v; }} \
             e.dispatchEvent(new Event('input', {{bubbles:true}})); \
             e.dispatchEvent(new Event('change', {{bubbles:true}})); \
             return 'TYPED'; }})()"
        );
        self.evaluate(&js)
    }

    pub fn screenshot(&mut self, path: &Path) -> Result<u64> {
        let result = self.cdp("Page.captureScreenshot", json!({"format": "png"}))?;
        let data = result["data"]
            .as_str()
            .ok_or_else(|| anyhow!("la captura no devolvió datos"))?;
        let bytes = base64_decode(data)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, &bytes)?;
        Ok(bytes.len() as u64)
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn http_get(url: &str) -> Option<String> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(3)))
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut res = agent.get(url).call().ok()?;
    res.body_mut().read_to_string().ok()
}

fn find_in_path(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn base64_decode(input: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b'\n' | b'\r' | b' ' => continue,
            _ => bail!("base64 inválido en la captura"),
        } as u32;
        buffer = ((buffer << 6) | value) & 0x00FF_FFFF;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decodifica_png_cabecera() {
        // "iVBORw0KGgo=" es el inicio de un PNG.
        let bytes = base64_decode("iVBORw0KGgo=").unwrap();
        assert_eq!(&bytes[..4], b"\x89PNG");
    }

    /// Verificación real del canal CDP con un fichero local (no requiere red).
    #[test]
    #[ignore = "lanza Chrome headless"]
    fn abre_un_fichero_local_y_lee_el_texto() {
        let dir = std::env::temp_dir().join(format!("agent-cdp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let page = dir.join("smoke.html");
        std::fs::write(&page, "<html><body><h1>HOLA-AGENTE</h1></body></html>").unwrap();

        let mut browser = Browser::new(9333, dir.join("profile"));
        browser
            .open(&format!("file://{}", page.display()))
            .unwrap();
        let text = browser.text().unwrap();
        assert!(text.contains("HOLA-AGENTE"), "texto inesperado: {text}");
        let clicked = browser.click("h1").unwrap();
        assert_eq!(clicked, "CLICKED");
    }
}
