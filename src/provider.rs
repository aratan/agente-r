//! Patrón **Strategy**: un único contrato (`LlmProvider`) para dos backends locales.
//!
//! - `Ollama` habla la API nativa `/api/chat` (puerto 11434).
//! - `LlamaCpp` es un **Adapter** sobre `llama-server`, que expone la API compatible
//!   con OpenAI `/v1/chat/completions` (puerto 8083).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// Llamada a herramienta pedida por el modelo.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

/// Mensaje neutro: cada proveedor lo traduce a su formato de cable.
#[derive(Debug, Clone)]
pub enum Msg {
    System(String),
    User(String),
    Assistant {
        text: String,
        calls: Vec<ToolCall>,
    },
    Tool {
        id: String,
        name: String,
        content: String,
    },
}

/// Respuesta de un turno del modelo.
#[derive(Debug, Clone)]
pub struct Turn {
    pub text: String,
    pub calls: Vec<ToolCall>,
}

pub trait LlmProvider: Send + Sync {
    /// `tools` son definiciones neutras: `{name, description, parameters}`.
    fn chat(&self, msgs: &[Msg], tools: &[Value]) -> Result<Turn>;
}

/// Backend elegido en el arranque: sabe fabricar proveedores con otro modelo,
/// lo que permite que cada subagente use el modelo que le corresponde.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Ollama,
    LlamaCpp,
}

#[derive(Debug, Clone)]
pub struct Backend {
    pub kind: Kind,
    pub url: String,
}

impl Backend {
    pub fn ollama(url: impl Into<String>) -> Self {
        Self {
            kind: Kind::Ollama,
            url: url.into().trim_end_matches('/').to_string(),
        }
    }

    pub fn llamacpp(url: impl Into<String>) -> Self {
        Self {
            kind: Kind::LlamaCpp,
            url: url.into().trim_end_matches('/').to_string(),
        }
    }

    pub fn provider(&self, model: &str) -> Box<dyn LlmProvider> {
        match self.kind {
            Kind::Ollama => Box::new(Ollama::new(self.url.clone(), model)),
            Kind::LlamaCpp => Box::new(LlamaCpp::new(self.url.clone(), model)),
        }
    }

    pub fn label(&self) -> &'static str {
        match self.kind {
            Kind::Ollama => "ollama",
            Kind::LlamaCpp => "llama.cpp",
        }
    }
}

/// Proveedor de embeddings (solo Ollama): permite RAG semántico.
#[derive(Debug, Clone)]
pub struct Embedder {
    base: String,
    model: String,
    http: ureq::Agent,
}

impl Embedder {
    /// Devuelve un embedder solo si el servidor tiene un modelo de embeddings.
    pub fn probe(base: &str, preferred: &str) -> Option<Self> {
        let base = base.trim_end_matches('/');
        let tags = get_json(&http_agent(), &format!("{base}/api/tags"))?;
        let names: Vec<String> = tags["models"]
            .as_array()?
            .iter()
            .filter_map(|m| m["name"].as_str().map(|s| s.to_string()))
            .collect();
        let model = names
            .iter()
            .find(|n| n.starts_with(preferred))
            .or_else(|| {
                names
                    .iter()
                    .find(|n| n.contains("embed") || n.contains("bert"))
            })?
            .clone();
        Some(Self {
            base: base.to_string(),
            model,
            http: http_agent(),
        })
    }

    pub fn label(&self) -> String {
        format!("{} · {}", self.model, self.base)
    }

    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let body = json!({"model": self.model, "input": text});
        let value = post_json(&self.http, &format!("{}/api/embed", self.base), &body)?;
        let vector: Vec<f32> = value["embeddings"]
            .as_array()
            .and_then(|e| e.first())
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_f64().map(|f| f as f32))
                    .collect()
            })
            .unwrap_or_default();
        if vector.is_empty() {
            bail!("el modelo de embeddings no devolvió vector");
        }
        Ok(vector)
    }
}

fn http_agent() -> ureq::Agent {
    // Los modelos locales pueden tardar; el timeout global es holgado.
    // Los errores HTTP se inspeccionan aquí para poder mostrar el cuerpo del servidor.
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(600)))
        .http_status_as_error(false)
        .build();
    ureq::Agent::new_with_config(config)
}

fn post_json(agent: &ureq::Agent, url: &str, body: &Value) -> Result<Value> {
    let mut res = agent
        .post(url)
        .send_json(body)
        .with_context(|| format!("no se pudo conectar con {url}"))?;
    let status = res.status().as_u16();
    let text = res
        .body_mut()
        .read_to_string()
        .with_context(|| format!("respuesta ilegible de {url}"))?;
    if !(200..300).contains(&status) {
        bail!("{url} respondió HTTP {status}: {}", head(&text, 400));
    }
    serde_json::from_str(&text).with_context(|| {
        format!(
            "respuesta no-JSON de {url}: {}",
            text.chars().take(300).collect::<String>()
        )
    })
}

fn get_json(agent: &ureq::Agent, url: &str) -> Option<Value> {
    let mut res = agent.get(url).call().ok()?;
    let text = res.body_mut().read_to_string().ok()?;
    serde_json::from_str(&text).ok()
}

fn wrap_tools(tools: &[Value]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|t| json!({"type": "function", "function": t}))
            .collect(),
    )
}

/// Algunos modelos devuelven los argumentos como objeto y otros como string JSON.
fn args_of(value: &Value) -> Value {
    match value {
        Value::Object(_) => value.clone(),
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
        _ => json!({}),
    }
}

fn head(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

// ---------------------------------------------------------------------------
// Ollama (API nativa)
// ---------------------------------------------------------------------------

pub struct Ollama {
    base: String,
    model: String,
    http: ureq::Agent,
}

impl Ollama {
    pub fn new(base: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            model: model.into(),
            http: http_agent(),
        }
    }
}

fn to_ollama(m: &Msg) -> Value {
    match m {
        Msg::System(t) => json!({"role": "system", "content": t}),
        Msg::User(t) => json!({"role": "user", "content": t}),
        Msg::Assistant { text, calls } => {
            let mut msg = json!({"role": "assistant", "content": text});
            if !calls.is_empty() {
                msg["tool_calls"] = Value::Array(
                    calls
                        .iter()
                        .map(|c| json!({"function": {"name": c.name, "arguments": c.args}}))
                        .collect(),
                );
            }
            msg
        }
        Msg::Tool { content, .. } => json!({"role": "tool", "content": content}),
    }
}

impl LlmProvider for Ollama {
    fn chat(&self, msgs: &[Msg], tools: &[Value]) -> Result<Turn> {
        if self.model.is_empty() {
            bail!("modelo vacío para Ollama (usa --model o arranca `ollama pull`)");
        }
        let messages: Vec<Value> = msgs.iter().map(to_ollama).collect();
        let mut body = json!({"model": self.model, "stream": false, "messages": messages});
        if !tools.is_empty() {
            body["tools"] = wrap_tools(tools);
        }
        let v = post_json(&self.http, &format!("{}/api/chat", self.base), &body)?;
        let message = &v["message"];
        Ok(Turn {
            text: message["content"].as_str().unwrap_or_default().to_string(),
            calls: parse_calls(&message["tool_calls"], |i| format!("call_{i}")),
        })
    }
}

// ---------------------------------------------------------------------------
// llama.cpp (llama-server, compatible OpenAI)
// ---------------------------------------------------------------------------

pub struct LlamaCpp {
    base: String,
    model: String,
    http: ureq::Agent,
}

impl LlamaCpp {
    pub fn new(base: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            model: model.into(),
            http: http_agent(),
        }
    }
}

fn to_openai(m: &Msg) -> Value {
    match m {
        Msg::System(t) => json!({"role": "system", "content": t}),
        Msg::User(t) => json!({"role": "user", "content": t}),
        Msg::Assistant { text, calls } => {
            let mut msg = json!({"role": "assistant", "content": text});
            if !calls.is_empty() {
                msg["tool_calls"] = Value::Array(
                    calls
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "type": "function",
                                "function": {"name": c.name, "arguments": c.args.to_string()}
                            })
                        })
                        .collect(),
                );
            }
            msg
        }
        Msg::Tool { id, name, content } => {
            json!({"role": "tool", "tool_call_id": id, "name": name, "content": content})
        }
    }
}

impl LlmProvider for LlamaCpp {
    fn chat(&self, msgs: &[Msg], tools: &[Value]) -> Result<Turn> {
        let messages: Vec<Value> = msgs.iter().map(to_openai).collect();
        let mut body = json!({"model": self.model, "stream": false, "messages": messages});
        if !tools.is_empty() {
            body["tools"] = wrap_tools(tools);
        }
        let url = format!("{}/v1/chat/completions", self.base);
        let v = post_json(&self.http, &url, &body)?;
        if let Some(err) = v["error"]["message"].as_str() {
            bail!("llama.cpp: {}", head(err, 400));
        }
        let message = &v["choices"][0]["message"];
        Ok(Turn {
            text: message["content"].as_str().unwrap_or_default().to_string(),
            calls: parse_calls(&message["tool_calls"], |i| format!("call_{i}")),
        })
    }
}

fn parse_calls(list: &Value, fallback_id: impl Fn(usize) -> String) -> Vec<ToolCall> {
    let Some(arr) = list.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .enumerate()
        .filter_map(|(i, call)| {
            let function = &call["function"];
            let name = function["name"].as_str()?.to_string();
            let id = match call["id"].as_str() {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => fallback_id(i),
            };
            Some(ToolCall {
                id,
                name,
                args: args_of(&function["arguments"]),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argumentos_string_y_objeto_se_normalizan() {
        assert_eq!(args_of(&json!("{\"a\":1}")), json!({"a": 1}));
        assert_eq!(args_of(&json!({"b": 2})), json!({"b": 2}));
        assert_eq!(args_of(&Value::Null), json!({}));
    }

    #[test]
    fn tool_calls_sin_id_reciben_id_sintetico() {
        let calls = parse_calls(
            &json!([{"function": {"name": "shell", "arguments": {"command": "ls"}}}]),
            |i| format!("call_{i}"),
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_0");
        assert_eq!(calls[0].args["command"], "ls");
    }
}
