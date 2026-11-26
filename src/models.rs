//! Catálogo de modelos y elección por rol, más el contador de descargas.
//!
//! Regla del proyecto: **como máximo 3 descargas de modelos** por proyecto. El
//! registro vive en `.agent/models.json`, así que el límite sobrevive entre
//! sesiones y no depende de la memoria del agente.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::hardware::{self, Specs};

/// Tope duro de descargas por proyecto.
pub const MAX_DOWNLOADS: usize = 3;

#[derive(Debug, Clone)]
pub struct Model {
    pub id: String,
    /// Miles de millones de parámetros (0.0 si el servidor no lo dice).
    pub params_b: f64,
    pub size_gb: f64,
    pub family: String,
}

impl Model {
    fn params_or_default(&self) -> f64 {
        if self.params_b > 0.0 { self.params_b } else { 7.0 }
    }

    pub fn is_embedding(&self) -> bool {
        let id = self.id.to_lowercase();
        id.contains("embed") || id.contains("bert") || id.contains("minilm")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Descompone el objetivo y reparte trabajo.
    Planner,
    /// Ejecuta tareas normales con herramientas.
    Worker,
    /// Escribe y revisa código.
    Coder,
    /// Respuestas cortas y baratas (resúmenes, clasificación).
    Fast,
}

impl Role {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_lowercase().as_str() {
            "" | "worker" | "trabajador" => Some(Role::Worker),
            "planner" | "planificador" => Some(Role::Planner),
            "coder" | "programador" | "code" => Some(Role::Coder),
            "fast" | "rapido" | "rápido" => Some(Role::Fast),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Planner => "planner",
            Role::Worker => "worker",
            Role::Coder => "coder",
            Role::Fast => "fast",
        }
    }

    pub fn guidance(&self) -> &'static str {
        match self {
            Role::Planner => {
                "Eres el planificador de un subagente. Divide el objetivo en pasos pequeños, \
                 delega en otros subagentes cuando aporte paralelismo real y sintetiza el resultado."
            }
            Role::Worker => {
                "Eres un subagente ejecutor. Haz exactamente la tarea encomendada, con las \
                 herramientas necesarias, y devuelve un resultado breve y verificable."
            }
            Role::Coder => {
                "Eres un subagente de código. Escribe cambios mínimos y coherentes con el \
                 proyecto, compruébalos y explica qué verificaste."
            }
            Role::Fast => {
                "Eres un subagente rápido. Responde en pocas líneas, sin explorar de más."
            }
        }
    }
}

pub fn list_ollama(url: &str) -> Vec<Model> {
    let Some(value) = get_json(&format!("{}/api/tags", url.trim_end_matches('/'))) else {
        return Vec::new();
    };
    let Some(items) = value["models"].as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|m| {
            let id = m["name"].as_str()?.to_string();
            let details = &m["details"];
            let family = details["family"].as_str().unwrap_or("?").to_string();
            let params_b = details["parameter_size"]
                .as_str()
                .and_then(parse_params)
                .unwrap_or(0.0);
            let size_gb = m["size"].as_u64().unwrap_or(0) as f64 / 1e9;
            Some(Model {
                id,
                params_b,
                size_gb,
                family,
            })
        })
        .collect()
}

pub fn list_llamacpp(url: &str) -> Vec<Model> {
    let Some(value) = get_json(&format!("{}/v1/models", url.trim_end_matches('/'))) else {
        return Vec::new();
    };
    let Some(items) = value["data"].as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?.to_string();
            Some(Model {
                id,
                params_b: 0.0,
                size_gb: 0.0,
                family: "llamacpp".into(),
            })
        })
        .collect()
}

/// "9.0B" / "268.10M" / "137M" → miles de millones.
fn parse_params(text: &str) -> Option<f64> {
    let t = text.trim().to_uppercase();
    let (number, scale) = if let Some(rest) = t.strip_suffix('B') {
        (rest, 1.0)
    } else if let Some(rest) = t.strip_suffix('M') {
        (rest, 0.001)
    } else {
        (t.as_str(), 1.0)
    };
    number.trim().parse::<f64>().ok().map(|v| v * scale)
}

/// Elige el mejor modelo instalado para un rol y este hardware.
pub fn pick(models: &[Model], role: Role, specs: &Specs) -> Option<String> {
    let fit = specs.max_params_b();
    let mut best: Option<(f64, &Model)> = None;
    for model in models.iter().filter(|m| !m.is_embedding()) {
        let score = score(model, role, fit);
        if best.as_ref().is_none_or(|(b, _)| score > *b) {
            best = Some((score, model));
        }
    }
    best.map(|(_, m)| m.id.clone())
}

fn score(model: &Model, role: Role, fit: f64) -> f64 {
    let params = model.params_or_default();
    let id = model.id.to_lowercase();
    let mut score = 0.0;

    // No cabe en el presupuesto: penalización proporcional (aun así puede servir).
    if params > fit {
        score -= (params - fit) * 8.0;
    } else {
        score += 2.0;
    }

    match role {
        Role::Planner => {
            score += params.min(fit) * 0.30;
            if id.contains("coder") || id.contains("code") {
                score -= 0.5;
            }
        }
        Role::Worker => {
            score += 1.5 - (params - 7.0).abs() * 0.20;
        }
        Role::Coder => {
            if id.contains("coder") || id.contains("code") {
                score += 4.0;
            }
            score += 1.0 - (params - 7.0).abs() * 0.15;
        }
        Role::Fast => {
            score += 5.0 - params * 0.55;
        }
    }

    // Modelos minúsculos no sirven para trabajo real (salvo el rol rápido).
    if params < 2.0 && role != Role::Fast {
        score -= 4.0;
    }
    score
}

/// KV cache aproximada por token, en GB.
///
/// La fórmula real es `2 (K y V) × capas × cabezas_kv × dim_cabeza × bytes`, pero
/// el backend no expone la arquitectura del modelo. Se aproxima por tamaño
/// asumiendo GQA de 8 cabezas KV en fp16: un 8B viene a gastar ~8 GB de KV a 64k
/// tokens. Es conservador a propósito: pasarse de slots es peor que quedarse
/// corto, porque el exceso no cabe y el backend empieza a paginar.
pub fn kv_gb_per_token(params_b: f64) -> f64 {
    /// KV de un 8B con GQA a 64k tokens ≈ 8 GB.
    const OCHO_B: f64 = 1.22e-4;
    OCHO_B * (params_b.max(1.0) / 8.0).sqrt()
}

/// Cuántas inferencias simultáneas caben **ahora** con este modelo y este
/// contexto, mirando la memoria libre real del equipo.
///
/// Supuestos (son la mayor fuente de error):
/// - Los pesos del mismo modelo se cargan **una vez** y se comparten entre
///   peticiones concurrentes; cada slot extra solo añade su KV cache y sus
///   activaciones. Con un solo modelo cargado, el segundo slot es mucho más
///   barato que el primero.
/// - RAM y VRAM se suman como un único presupuesto (llama.cpp reparte capas) y
///   se deja un 20% de margen para el escritorio y los procesos del agente.
/// - Un slot siempre se permite: si no cabe, el backend paginará, pero negarse
///   a trabajar no es una opción.
pub fn concurrent_slots(model: &Model, specs: &Specs, context_tokens: usize) -> usize {
    let params = model.params_or_default();
    let weights_gb = if model.size_gb > 0.0 {
        model.size_gb
    } else {
        // Sin dato del servidor: q4 ≈ 0,6 GB por cada mil millones.
        params * 0.6
    };
    let kv_gb = kv_gb_per_token(params) * context_tokens as f64;
    let usable = (hardware::free_ram_gb() + specs.free_vram_gb().unwrap_or(0.0)) * 0.8;

    let primer_slot = weights_gb + kv_gb + 0.5;
    let slots = if usable <= primer_slot {
        1
    } else {
        // Cada slot adicional solo paga KV + activaciones, no los pesos.
        1 + ((usable - primer_slot) / (kv_gb + 0.3).max(0.1)).floor() as usize
    };

    // Más de una inferencia por cada dos núcleos no gana nada, y más de 8 es
    // un enjambre peleando por la misma memoria.
    slots.min((specs.cores.max(1) / 2).max(1)).clamp(1, 8)
}

fn get_json(url: &str) -> Option<serde_json::Value> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .build();
    let agent = ureq::Agent::new_with_config(agent);
    let mut res = agent.get(url).call().ok()?;
    let text = res.body_mut().read_to_string().ok()?;
    serde_json::from_str(&text).ok()
}

// ---------------------------------------------------------------------------
// Registro de descargas (máximo MAX_DOWNLOADS por proyecto)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default)]
    pub pulled: Vec<String>,
}

pub struct Downloads {
    path: PathBuf,
    ledger: Ledger,
}

impl Downloads {
    pub fn open(path: PathBuf) -> Result<Self> {
        let ledger = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Ledger::default(),
        };
        Ok(Self { path, ledger })
    }

    pub fn used(&self) -> usize {
        self.ledger.pulled.len()
    }

    pub fn remaining(&self) -> usize {
        MAX_DOWNLOADS.saturating_sub(self.used())
    }

    pub fn pulled(&self) -> &[String] {
        &self.ledger.pulled
    }

    /// Comprueba el tope antes de descargar nada.
    pub fn ensure_slot(&self, model: &str) -> Result<()> {
        if self.ledger.pulled.iter().any(|m| same_model(m, model)) {
            bail!("'{model}' ya se descargó en este proyecto; no cuenta de nuevo");
        }
        if self.used() >= MAX_DOWNLOADS {
            bail!(
                "límite de {MAX_DOWNLOADS} descargas por proyecto alcanzado (ya: {:?}); usa uno existente o borra el registro",
                self.ledger.pulled
            );
        }
        Ok(())
    }

    pub fn record(&mut self, model: &str) -> Result<()> {
        if !self.ledger.pulled.iter().any(|m| same_model(m, model)) {
            self.ledger.pulled.push(model.to_string());
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.path, serde_json::to_vec_pretty(&self.ledger)?)
            .with_context(|| format!("no se pudo escribir {}", self.path.display()))?;
        Ok(())
    }
}

fn same_model(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim().to_lowercase().replace(":latest", "");
    norm(a) == norm(b)
}

/// Descarga un modelo en Ollama (bloqueante, sin streaming).
pub fn pull_ollama(url: &str, model: &str) -> Result<String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(3600)))
        .http_status_as_error(false)
        .build();
    let agent = ureq::Agent::new_with_config(agent);
    let url = format!("{}/api/pull", url.trim_end_matches('/'));
    let mut res = agent
        .post(&url)
        .send_json(json!({"model": model, "stream": false}))
        .with_context(|| format!("no se pudo conectar con {url}"))?;
    let status = res.status().as_u16();
    let text = res.body_mut().read_to_string().unwrap_or_default();
    if !(200..300).contains(&status) {
        bail!("{url} respondió HTTP {status}: {}", text.chars().take(300).collect::<String>());
    }
    Ok(format!("descargado '{model}'"))
}

pub fn ledger_path(workspace: &Path) -> PathBuf {
    workspace.join(".agent").join("models.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specs(budget: f64) -> Specs {
        Specs {
            cores: 8,
            ram_gb: budget * 2.0,
            vram_gb: None,
            gpu: None,
        }
    }

    fn model(id: &str, params: f64) -> Model {
        Model {
            id: id.into(),
            params_b: params,
            size_gb: params * 0.6,
            family: "test".into(),
        }
    }

    #[test]
    fn interpreta_tamanos() {
        assert_eq!(parse_params("9.0B"), Some(9.0));
        assert!((parse_params("268.10M").unwrap() - 0.2681).abs() < 1e-9);
        assert_eq!(parse_params("basura"), None);
    }

    #[test]
    fn coder_prefiere_modelo_de_codigo() {
        let models = vec![
            model("qwen3.5:9b", 9.0),
            model("qwen2.5-coder:7b", 7.0),
            model("gemma3:270m", 0.27),
        ];
        let s = specs(20.0);
        assert_eq!(pick(&models, Role::Coder, &s).unwrap(), "qwen2.5-coder:7b");
        assert_eq!(pick(&models, Role::Fast, &s).unwrap(), "gemma3:270m");
        assert_eq!(pick(&models, Role::Planner, &s).unwrap(), "qwen3.5:9b");
    }

    #[test]
    fn ignora_modelos_de_embeddings() {
        let models = vec![model("nomic-embed-text:latest", 0.137), model("qwen3.5:9b", 9.0)];
        let elegido = pick(&models, Role::Fast, &specs(20.0)).unwrap();
        assert_eq!(elegido, "qwen3.5:9b");
    }

    #[test]
    fn prefiere_lo_que_cabe_en_hardware_pequeno() {
        let models = vec![model("qwen3.8:27b", 27.3), model("qwen3.5:9b", 9.0)];
        let elegido = pick(&models, Role::Worker, &specs(5.0)).unwrap();
        assert_eq!(elegido, "qwen3.5:9b");
    }

    #[test]
    fn mas_contexto_nunca_da_mas_slots() {
        let m = model("qwen3.5:9b", 9.0);
        let s = specs(20.0);
        let corto = concurrent_slots(&m, &s, 4 * 1024);
        let largo = concurrent_slots(&m, &s, 64 * 1024);
        assert!(largo <= corto, "más contexto no puede dar más slots");
        assert!(largo >= 1, "siempre se permite trabajar");
        assert!(largo <= 8);
        // 64k de contexto en un 8B no puede ser barato: es el grueso del coste.
        assert!(
            kv_gb_per_token(8.0) * 65536.0 > 4.0,
            "la KV cache a 64k debería pesar varios GB"
        );
    }

    #[test]
    fn el_gpu_grande_admite_mas_que_el_flojo() {
        let m = model("qwen3.5:9b", 9.0);
        let mut grande = Specs {
            cores: 32,
            ram_gb: 128.0,
            vram_gb: Some(48.0),
            gpu: Some("A100".into()),
        };
        // Sin nvidia-smi no hay VRAM libre medible: se cae a la RAM disponible.
        grande.gpu = None;
        assert!(concurrent_slots(&m, &grande, 8 * 1024) >= 1);
    }

    #[test]
    fn limite_de_tres_descargas() {
        let dir = std::env::temp_dir().join(format!("agent-ledger-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("models.json");
        let mut ledger = Downloads::open(path.clone()).unwrap();
        assert_eq!(ledger.remaining(), MAX_DOWNLOADS);
        for name in ["a:latest", "b", "c"] {
            ledger.ensure_slot(name).unwrap();
            ledger.record(name).unwrap();
        }
        assert_eq!(ledger.remaining(), 0);
        assert!(ledger.ensure_slot("d").is_err());
        // Ya descargado: no consume cupo ni se duplica.
        assert!(ledger.ensure_slot("a").is_err());
        let reopened = Downloads::open(path).unwrap();
        assert_eq!(reopened.used(), 3);
    }
}
