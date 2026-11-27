//! Búsqueda web gratuita y sin clave, a través del navegador ya controlado por CDP.
//!
//! No se usa una API con key (Brave la exige) ni TLS propio (el registro de
//! crates disponible no resuelve `rustls`/`native-tls`): Chrome hace la red y
//! aquí solo se interpreta el resultado. Se respeta el servicio:
//! - motores en orden de preferencia (DuckDuckGo → Brave → Bing),
//! - pausa mínima entre peticiones y caché de 10 minutos.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::browser::Browser;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    DuckDuckGo,
    Brave,
    Bing,
}

impl Engine {
    pub fn all() -> Vec<Engine> {
        vec![Engine::DuckDuckGo, Engine::Brave, Engine::Bing]
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_lowercase().as_str() {
            "duckduckgo" | "ddg" => Some(Engine::DuckDuckGo),
            "brave" => Some(Engine::Brave),
            "bing" => Some(Engine::Bing),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Engine::DuckDuckGo => "duckduckgo",
            Engine::Brave => "brave",
            Engine::Bing => "bing",
        }
    }

    fn url(&self, query: &str) -> String {
        let q = urlencode(query);
        match self {
            Engine::DuckDuckGo => format!("https://html.duckduckgo.com/html/?q={q}"),
            Engine::Brave => format!("https://search.brave.com/search?q={q}"),
            Engine::Bing => format!("https://www.bing.com/search?q={q}"),
        }
    }

    fn script(&self, limit: usize) -> String {
        match self {
            Engine::Bing => format!(
                r#"(() => {{
  const items = Array.from(document.querySelectorAll('#b_results > li.b_algo')).slice(0, {limit});
  return JSON.stringify(items.map(li => ({{
    title: ((li.querySelector('h2') || {{}}).innerText || '').trim().slice(0, 160),
    url: (li.querySelector('h2 a') || {{}}).href || '',
    snippet: ((li.querySelector('.b_caption p') || li.querySelector('p') || {{}}).innerText || '').trim().slice(0, 300)
  }})));
}})()"#
            ),
            Engine::DuckDuckGo => format!(
                r#"(() => {{
  const items = Array.from(document.querySelectorAll('.result')).slice(0, {limit});
  return JSON.stringify(items.map(r => ({{
    title: ((r.querySelector('.result__a') || {{}}).innerText || '').trim().slice(0, 160),
    url: (r.querySelector('.result__a') || {{}}).href || '',
    snippet: ((r.querySelector('.result__snippet') || {{}}).innerText || '').trim().slice(0, 300)
  }})));
}})()"#
            ),
            // Brave cambia el marcado a menudo: extractor genérico por anclas.
            Engine::Brave => format!(
                r#"(() => {{
  const out = [];
  const seen = new Set();
  for (const a of document.querySelectorAll('#results a[href^="http"]')) {{
    const url = a.href;
    const title = (a.innerText || '').trim();
    if (seen.has(url) || title.length < 8) continue;
    seen.add(url);
    const box = a.closest('div');
    out.push({{
      title: title.slice(0, 160),
      url: url,
      snippet: ((box ? box.innerText : '') || '').replace(/\s+/g, ' ').trim().slice(0, 300)
    }});
    if (out.length >= {limit}) break;
  }}
  return JSON.stringify(out);
}})()"#
            ),
        }
    }
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct Hit {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub snippet: String,
}

/// Estado compartido de las herramientas web: ritmo y caché.
pub struct WebState {
    last: Option<Instant>,
    cache: Vec<(String, Vec<Hit>, Instant)>,
    min_interval: Duration,
    ttl: Duration,
}

impl WebState {
    pub fn new() -> Self {
        let ms = std::env::var("AGENT_WEB_MIN_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(2000);
        Self {
            last: None,
            cache: Vec::new(),
            min_interval: Duration::from_millis(ms),
            ttl: Duration::from_secs(600),
        }
    }

    pub fn cached(&self, query: &str) -> Option<Vec<Hit>> {
        let key = query.trim().to_lowercase();
        self.cache
            .iter()
            .find(|(q, _, at)| *q == key && at.elapsed() < self.ttl)
            .map(|(_, hits, _)| hits.clone())
    }

    pub fn store(&mut self, query: &str, hits: &[Hit]) {
        let key = query.trim().to_lowercase();
        self.cache.retain(|(q, _, _)| *q != key);
        self.cache.insert(0, (key, hits.to_vec(), Instant::now()));
        self.cache.truncate(16);
    }

    /// Espera lo necesario para no saturar el motor (se hace fuera del navegador).
    pub fn wait(&mut self) {
        self.throttle();
    }

    fn throttle(&mut self) {
        if let Some(last) = self.last {
            let elapsed = last.elapsed();
            if elapsed < self.min_interval {
                std::thread::sleep(self.min_interval - elapsed);
            }
        }
        self.last = Some(Instant::now());
    }
}

impl Default for WebState {
    fn default() -> Self {
        Self::new()
    }
}

/// Busca con el primer motor que devuelva resultados.
pub fn search(
    browser: &mut Browser,
    engines: &[Engine],
    query: &str,
    limit: usize,
) -> Result<Vec<Hit>> {
    if query.trim().is_empty() {
        bail!("consulta vacía");
    }
    let limit = limit.clamp(1, 10);
    let mut problems = Vec::new();
    for engine in engines {
        match one_engine(browser, *engine, query, limit) {
            Ok(hits) if !hits.is_empty() => return Ok(hits),
            Ok(_) => problems.push(format!("{}: sin resultados", engine.as_str())),
            Err(err) => problems.push(format!("{}: {err:#}", engine.as_str())),
        }
    }
    bail!("ningún motor devolvió resultados ({})", problems.join(" | "))
}

fn one_engine(
    browser: &mut Browser,
    engine: Engine,
    query: &str,
    limit: usize,
) -> Result<Vec<Hit>> {
    let script = engine.script(limit);
    browser.open(&engine.url(query))?;
    // Los resultados se pintan con JavaScript; si aún no están, se reintenta.
    for attempt in 0..2 {
        std::thread::sleep(Duration::from_millis(if attempt == 0 { 900 } else { 1200 }));
        let raw = browser.evaluate(&script)?;
        let parsed: Result<Vec<Hit>, _> = serde_json::from_str(raw.trim());
        let Ok(mut hits) = parsed else {
            if attempt == 0 {
                continue;
            }
            anyhow::bail!(
                "respuesta no-JSON de {}: {}",
                engine.as_str(),
                head(&raw, 200)
            );
        };
        hits.retain(|h| !h.url.is_empty() && !h.title.is_empty());
        for hit in hits.iter_mut() {
            hit.url = real_url(&hit.url);
        }
        if !hits.is_empty() {
            hits.truncate(limit);
            return Ok(hits);
        }
    }
    Ok(Vec::new())
}

/// Bing (y DDG) devuelven redirecciones; se resuelve la URL real.
fn real_url(href: &str) -> String {
    if href.contains("bing.com/ck/a")
        && let Some(pos) = href.find("&u=a1")
    {
        let rest = &href[pos + 5..];
        let end = rest.find('&').unwrap_or(rest.len());
        if let Some(bytes) = base64url_decode(&rest[..end])
            && let Ok(text) = String::from_utf8(bytes)
            && text.starts_with("http")
        {
            return text;
        }
    }
    if let Some(pos) = href.find("uddg=") {
        let rest = &href[pos + 5..];
        let end = rest.find('&').unwrap_or(rest.len());
        return percent_decode(&rest[..end]);
    }
    href.to_string()
}

fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        buffer = ((buffer << 6) | value) & 0x00FF_FFFF;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Descarga una página y devuelve su texto visible.
pub fn fetch(browser: &mut Browser, url: &str, max_chars: usize) -> Result<String> {
    let max = max_chars.clamp(500, 20_000);
    let info = browser.open(url)?;
    let script = format!(
        "(() => {{ const b = document.body ? document.body.innerText : ''; return b.slice(0, {max}); }})()"
    );
    let text = browser.evaluate(&script)?;
    Ok(format!("{info}\n\n{}", text.trim()))
}

pub fn format_hits(query: &str, hits: &[Hit]) -> String {
    let mut out = format!("Resultados para «{query}» ({}):\n", hits.len());
    for (i, hit) in hits.iter().enumerate() {
        out.push_str(&format!(
            "{}. {}\n   {}\n   {}\n",
            i + 1,
            hit.title,
            hit.url,
            hit.snippet
        ));
    }
    out
}

pub fn urlencode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn head(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_escapa_lo_justo() {
        assert_eq!(urlencode("rust lang"), "rust+lang");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencode("ñ"), "%C3%B1");
    }

    #[test]
    fn parsea_resultados_del_buscador() {
        let raw = r#"[{"title":"Rust","url":"https://rust-lang.org","snippet":"lenguaje"}]"#;
        let hits: Vec<Hit> = serde_json::from_str(raw).unwrap();
        assert_eq!(hits[0].url, "https://rust-lang.org");
        let text = format_hits("rust", &hits);
        assert!(text.contains("rust-lang.org"));
    }

    #[test]
    fn motores_se_resuelven_por_nombre() {
        assert_eq!(Engine::parse("ddg"), Some(Engine::DuckDuckGo));
        assert_eq!(Engine::parse("BING"), Some(Engine::Bing));
        assert_eq!(Engine::parse("google"), None);
        assert_eq!(Engine::all().len(), 3);
    }

    #[test]
    fn resuelve_redirecciones_del_buscador() {
        let bing = "https://www.bing.com/ck/a?!&&p=abc&u=a1aHR0cHM6Ly9ydXN0LWxhbmcub3JnLw&ntb=1";
        assert_eq!(real_url(bing), "https://rust-lang.org/");
        let ddg = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&rut=x";
        assert_eq!(real_url(ddg), "https://example.com/a?b=1");
        assert_eq!(real_url("https://directo.es"), "https://directo.es");
    }

    #[test]
    fn cache_y_ttl() {
        let mut st = WebState::new();
        assert!(st.cached("rust").is_none());
        st.store(
            "rust",
            &[Hit {
                title: "Rust".into(),
                url: "https://rust-lang.org".into(),
                snippet: String::new(),
            }],
        );
        assert_eq!(st.cached(" RUST ").unwrap().len(), 1);
    }
}
