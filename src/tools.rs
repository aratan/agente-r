//! Herramientas del agente: ficheros, shell, navegador, web, memoria/RAG,
//! delegación, agenda y descargas de modelos.
//!
//! Patrón **Command**: cada herramienta implementa `Tool`. Patrón **Registry**:
//! `ToolRegistry` expone los esquemas y despacha por nombre, con un juego de
//! herramientas distinto según el rol del agente.

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::browser::Browser;
use crate::ext::{self, McpCallTool, McpCreateTool, McpServersTool, McpToolsTool};
use crate::ext::{SkillCreateTool, SkillListTool, SkillReadTool};
use crate::ext::{ToolCreateTool, ToolListTool, ToolRemoveTool};
use crate::media::{GuiTool, MailTool, MediaTool, TelegramTool, VoiceTool};
use crate::memory::Memory;
use crate::models::{Downloads, MAX_DOWNLOADS, Role};
use crate::orchestra::{Mandate, Orchestra};
use crate::provider::Kind;
use crate::scheduler::{self, Scheduler};
use crate::web::{self, Engine, WebState};

const MAX_OUTPUT: usize = 8000;

// ---------------------------------------------------------------------------
// Sandbox de rutas
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let root = std::fs::canonicalize(path.as_ref())
            .with_context(|| format!("workspace inexistente: {}", path.as_ref().display()))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resuelve una ruta del modelo dentro del workspace, rechazando escapes.
    pub fn resolve(&self, raw: &str) -> Result<PathBuf> {
        let raw = raw.trim();
        let candidate = if raw.is_empty() {
            self.root.clone()
        } else {
            PathBuf::from(raw)
        };
        let joined = if candidate.is_absolute() {
            candidate
        } else {
            self.root.join(candidate)
        };

        let mut parts: Vec<String> = Vec::new();
        for component in joined.components() {
            match component {
                Component::RootDir => parts.push("/".to_string()),
                Component::CurDir => {}
                Component::ParentDir => {
                    if parts.len() <= 1 {
                        bail!("ruta fuera del workspace: {raw}");
                    }
                    parts.pop();
                }
                Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
                Component::Prefix(_) => bail!("rutas con prefijo no soportadas: {raw}"),
            }
        }
        let mut normalized = PathBuf::new();
        for part in parts {
            normalized.push(part);
        }
        if !normalized.starts_with(&self.root) {
            bail!(
                "ruta fuera del workspace: {raw} (raíz {})",
                self.root.display()
            );
        }

        // La comprobación anterior es léxica: un symlink dentro del workspace
        // puede apuntar fuera. Se canonicaliza la parte que ya existe y se
        // recompone el resto, porque `fs_write` crea ficheros que aún no están.
        let mut existing = normalized;
        let mut rest: Vec<std::ffi::OsString> = Vec::new();
        let mut real = loop {
            match std::fs::canonicalize(&existing) {
                Ok(real) => break real,
                Err(_) => {
                    let name = existing.file_name().map(|n| n.to_os_string());
                    let parent = existing.parent().map(|p| p.to_path_buf());
                    let (Some(name), Some(parent)) = (name, parent) else {
                        bail!("ruta fuera del workspace: {raw}");
                    };
                    rest.push(name);
                    existing = parent;
                }
            }
        };
        for part in rest.iter().rev() {
            real.push(part);
        }
        if !real.starts_with(&self.root) {
            bail!(
                "ruta fuera del workspace: {raw} (los symlinks la llevan fuera de {})",
                self.root.display()
            );
        }
        Ok(real)
    }

    pub fn rel(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string())
    }
}

#[cfg(test)]
pub fn workspace_for_tests(tag: &str) -> Workspace {
    let dir = std::env::temp_dir().join(format!("agent-ws-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Workspace::new(&dir).unwrap()
}

// ---------------------------------------------------------------------------
// Contrato + registro
// ---------------------------------------------------------------------------

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn run(&self, args: &Value) -> Result<String>;
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn empty() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn add<T: Tool + 'static>(&mut self, tool: T) {
        self.tools.push(Box::new(tool));
    }

    pub fn defs(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name(),
                    "description": t.description(),
                    "parameters": t.parameters(),
                })
            })
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }

    pub fn run(&self, name: &str, args: &Value) -> Result<String> {
        let tool = self
            .tools
            .iter()
            .find(|t| t.name() == name)
            .ok_or_else(|| anyhow!("herramienta desconocida: {name}"))?;
        let out = tool.run(args)?;
        Ok(truncate(&out, MAX_OUTPUT))
    }

    /// Juego de herramientas según rol, nivel de anidamiento y mandato.
    pub fn for_role(orch: &Arc<Orchestra>, depth: usize, role: Role, mandate: Mandate) -> Self {
        let mut reg = Self::empty();
        let ws = orch.workspace.clone();
        let browser = orch.browser.clone();
        let web_state = orch.web.clone();
        let memory = orch.memory.clone();
        let embedder = orch.embedder.clone();
        let can_delegate = depth < orch.limits.max_depth;

        let writer = matches!(role, Role::Worker | Role::Coder);
        let web_full = !matches!(role, Role::Fast);

        reg.add(MemorySearchTool {
            memory: memory.clone(),
            embedder: embedder.clone(),
        });
        reg.add(MemorySaveTool {
            memory: memory.clone(),
            embedder: embedder.clone(),
        });
        reg.add(FsReadTool { ws: ws.clone() });
        reg.add(FsListTool { ws: ws.clone() });
        reg.add(WebSearchTool {
            browser: browser.clone(),
            state: web_state.clone(),
            ws: ws.clone(),
        });
        // Solo el agente raíz puede cerrar el objetivo global. Un subagente
        // entrega evidencias a su padre, pero no puede finalizar la misión.
        if depth == 0 {
            reg.add(GoalPlanTool { goal: orch.goal.clone() });
            reg.add(GoalFailureTool { goal: orch.goal.clone() });
            reg.add(GoalVerifyTool { goal: orch.goal.clone(), ws: ws.clone() });
            reg.add(GoalDoneTool {
                goal: orch.goal.clone(),
            });
        }
        reg.add(MediaTool);
        reg.add(TelegramTool);
        reg.add(VoiceTool);
        reg.add(ToolListTool { ws: ws.clone() });
        reg.add(ToolCreateTool { ws: ws.clone() });
        reg.add(SkillListTool { ws: ws.clone() });
        reg.add(SkillReadTool { ws: ws.clone() });
        reg.add(McpServersTool { ws: ws.clone() });
        reg.add(McpToolsTool { ws: ws.clone() });
        reg.add(McpCallTool { ws: ws.clone() });
        if !matches!(role, Role::Fast) {
            reg.add(GuiTool);
            reg.add(MailTool);
            reg.add(ToolRemoveTool { ws: ws.clone() });
            reg.add(SkillCreateTool { ws: ws.clone() });
            reg.add(McpCreateTool { ws: ws.clone() });
        }
        // Herramientas creadas por el propio agente (Python).
        for meta in ext::load_python_tools(ws.root()) {
            reg.add(ext::PythonTool::new(meta, ws.root()));
        }

        if web_full {
            reg.add(MemoryListTool {
                memory: memory.clone(),
            });
            reg.add(MemoryForgetTool {
                memory: memory.clone(),
            });
            reg.add(MemoryFeedbackTool {
                memory: memory.clone(),
            });
            reg.add(WebFetchTool {
                browser: browser.clone(),
                ws: ws.clone(),
            });
            reg.add(BrowserTool {
                browser: browser.clone(),
                ws: ws.clone(),
            });
        }
        if writer {
            reg.add(FsWriteTool { ws: ws.clone() });
            reg.add(FsMkdirTool { ws: ws.clone() });
            reg.add(FsDeleteTool { ws: ws.clone() });
            reg.add(ShellTool { ws: ws.clone() });
            reg.add(RagIndexTool {
                ws: ws.clone(),
                memory: memory.clone(),
                embedder: embedder.clone(),
            });
        }
        if can_delegate && !matches!(role, Role::Fast) {
            reg.add(DelegateTool {
                orch: orch.clone(),
                depth,
                mandate,
            });
            reg.add(DelegateManyTool {
                orch: orch.clone(),
                depth,
                mandate,
            });
        }
        if matches!(role, Role::Planner | Role::Worker) {
            reg.add(SystemInfoTool { orch: orch.clone() });
            reg.add(ScheduleAddTool { ws: ws.clone() });
            reg.add(ScheduleListTool { ws: ws.clone() });
            reg.add(ScheduleRemoveTool { ws: ws.clone() });
        }
        if matches!(role, Role::Planner) {
            reg.add(ModelPullTool { orch: orch.clone() });
        }
        reg
    }

    /// Solo memoria: la usa el módulo de auto-mejora (RSI).
    pub fn memory_only(orch: &Arc<Orchestra>) -> Self {
        let mut reg = Self::empty();
        let memory = orch.memory.clone();
        let embedder = orch.embedder.clone();
        reg.add(MemorySaveTool {
            memory: memory.clone(),
            embedder: embedder.clone(),
        });
        reg.add(MemorySearchTool { memory: memory.clone(), embedder });
        reg.add(MemoryListTool { memory });
        reg
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str("\n… (recortado)");
    out
}

fn str_arg(args: &Value, key: &str) -> Option<String> {
    args[key]
        .as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn req_str(args: &Value, key: &str) -> Result<String> {
    str_arg(args, key).ok_or_else(|| anyhow!("falta el parámetro '{key}'"))
}

fn usize_arg(args: &Value, key: &str, default: usize) -> usize {
    args[key]
        .as_u64()
        .map(|v| v as usize)
        .unwrap_or(default)
}

fn tags_arg(args: &Value) -> Vec<String> {
    match &args["tags"] {
        Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Value::String(text) => text
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

fn browser_port() -> u16 {
    std::env::var("AGENT_BROWSER_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9333)
}

/// Acceso serializado al navegador compartido (los subagentes comparten uno).
fn with_browser<T>(
    slot: &Arc<Mutex<Option<Browser>>>,
    ws: &Workspace,
    f: impl FnOnce(&mut Browser) -> Result<T>,
) -> Result<T> {
    let mut guard = slot.lock().map_err(|_| anyhow!("navegador: lock envenenado"))?;
    if guard.is_none() {
        let profile = ws.root().join(".agent-browser");
        *guard = Some(Browser::new(browser_port(), profile));
    }
    let browser = guard.as_mut().expect("navegador inicializado");
    f(browser)
}

// ---------------------------------------------------------------------------
// Shell y ficheros
// ---------------------------------------------------------------------------

fn run_command(command: &str, cwd: &Path, timeout: Duration) -> Result<String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("no se pudo ejecutar: {command}"))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut stream) = stdout {
            let _ = stream.read_to_end(&mut buf);
        }
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut stream) = stderr {
            let _ = stream.read_to_end(&mut buf);
        }
        buf
    });

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let stdout = String::from_utf8_lossy(&out_handle.join().unwrap_or_default()).to_string();
    let stderr = String::from_utf8_lossy(&err_handle.join().unwrap_or_default()).to_string();
    let mut out = String::new();
    if !stdout.trim().is_empty() {
        out.push_str(&stdout);
    }
    if !stderr.trim().is_empty() {
        if !out.is_empty() {
            out.push_str("\n[stderr]\n");
        }
        out.push_str(&stderr);
    }
    match status {
        Some(status) if status.success() => {
            if out.trim().is_empty() {
                out = "(sin salida, exit 0)".to_string();
            }
        }
        Some(status) => {
            out.push_str(&format!("\n[exit {status}]"));
        }
        None => {
            out.push_str(&format!(
                "\n[timeout tras {}s; proceso terminado]",
                timeout.as_secs()
            ));
        }
    }
    Ok(out)
}

struct ShellTool {
    ws: Workspace,
}

impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }
    fn description(&self) -> &str {
        "Ejecuta un comando de shell no interactivo en la raíz del workspace y devuelve stdout/stderr."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Comando para `sh -c`"},
                "timeout_secs": {"type": "integer", "description": "Timeout en segundos (defecto 60)"}
            },
            "required": ["command"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let command = req_str(args, "command")?;
        let secs = args["timeout_secs"].as_u64().unwrap_or(60).clamp(1, 600);
        run_command(&command, self.ws.root(), Duration::from_secs(secs))
    }
}

struct FsReadTool {
    ws: Workspace,
}

impl Tool for FsReadTool {
    fn name(&self) -> &str {
        "fs_read"
    }
    fn description(&self) -> &str {
        "Lee un fichero de texto del workspace."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "max_chars": {"type": "integer", "description": "Máximo de caracteres (defecto 20000)"}
            },
            "required": ["path"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let path = self.ws.resolve(&req_str(args, "path")?)?;
        let max = usize_arg(args, "max_chars", 20_000).clamp(100, 200_000);
        let bytes = std::fs::read(&path)
            .with_context(|| format!("no se pudo leer {}", self.ws.rel(&path)))?;
        match String::from_utf8(bytes) {
            Ok(text) => Ok(truncate(&text, max)),
            Err(_) => bail!("el fichero no es texto UTF-8: {}", self.ws.rel(&path)),
        }
    }
}

struct FsWriteTool {
    ws: Workspace,
}

impl Tool for FsWriteTool {
    fn name(&self) -> &str {
        "fs_write"
    }
    fn description(&self) -> &str {
        "Escribe (o añade) texto en un fichero del workspace, creando carpetas."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"},
                "append": {"type": "boolean", "description": "Si true, añade al final"}
            },
            "required": ["path", "content"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let path = self.ws.resolve(&req_str(args, "path")?)?;
        let content = args["content"].as_str().unwrap_or_default();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if args["append"].as_bool().unwrap_or(false) {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            file.write_all(content.as_bytes())?;
            Ok(format!("añadidos {} bytes a {}", content.len(), self.ws.rel(&path)))
        } else {
            std::fs::write(&path, content)?;
            Ok(format!("escritos {} bytes en {}", content.len(), self.ws.rel(&path)))
        }
    }
}

struct FsListTool {
    ws: Workspace,
}

impl Tool for FsListTool {
    fn name(&self) -> &str {
        "fs_list"
    }
    fn description(&self) -> &str {
        "Lista el contenido de una carpeta del workspace (carpetas con '/' final)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"path": {"type": "string", "description": "Carpeta (defecto '.')"}}
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let path = self.ws.resolve(str_arg(args, "path").unwrap_or_else(|| ".".into()).as_str())?;
        let mut rows = Vec::new();
        for entry in std::fs::read_dir(&path)
            .with_context(|| format!("no se pudo listar {}", self.ws.rel(&path)))?
        {
            let entry = entry?;
            let meta = entry.metadata()?;
            let name = entry.file_name().to_string_lossy().to_string();
            if meta.is_dir() {
                rows.push(format!("{name}/"));
            } else {
                rows.push(format!("{name} ({} bytes)", meta.len()));
            }
        }
        rows.sort();
        if rows.is_empty() {
            return Ok(format!("{} está vacía", self.ws.rel(&path)));
        }
        Ok(rows.join("\n"))
    }
}

struct FsMkdirTool {
    ws: Workspace,
}

impl Tool for FsMkdirTool {
    fn name(&self) -> &str {
        "fs_mkdir"
    }
    fn description(&self) -> &str {
        "Crea una carpeta (y las intermedias) dentro del workspace."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]})
    }
    fn run(&self, args: &Value) -> Result<String> {
        let path = self.ws.resolve(&req_str(args, "path")?)?;
        std::fs::create_dir_all(&path)?;
        Ok(format!("carpeta lista: {}", self.ws.rel(&path)))
    }
}

struct FsDeleteTool {
    ws: Workspace,
}

impl Tool for FsDeleteTool {
    fn name(&self) -> &str {
        "fs_delete"
    }
    fn description(&self) -> &str {
        "Borra un fichero o carpeta del workspace. Requiere `recursive` para carpetas no vacías."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "recursive": {"type": "boolean"}
            },
            "required": ["path"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let path = self.ws.resolve(&req_str(args, "path")?)?;
        if path == self.ws.root() {
            bail!("no se puede borrar la raíz del workspace");
        }
        let meta = std::fs::metadata(&path)
            .with_context(|| format!("no existe {}", self.ws.rel(&path)))?;
        if meta.is_dir() {
            if args["recursive"].as_bool().unwrap_or(false) {
                std::fs::remove_dir_all(&path)?;
            } else {
                std::fs::remove_dir(&path)
                    .with_context(|| "la carpeta no está vacía; usa recursive=true")?;
            }
        } else {
            std::fs::remove_file(&path)?;
        }
        Ok(format!("borrado: {}", self.ws.rel(&path)))
    }
}

// ---------------------------------------------------------------------------
// Navegador y web
// ---------------------------------------------------------------------------

struct BrowserTool {
    browser: Arc<Mutex<Option<Browser>>>,
    ws: Workspace,
}

impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }
    fn description(&self) -> &str {
        "Controla Chrome (CDP): action=open|text|click|type|eval|screenshot."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["open", "text", "click", "type", "eval", "screenshot"]},
                "url": {"type": "string"},
                "selector": {"type": "string"},
                "text": {"type": "string"},
                "expression": {"type": "string", "description": "JavaScript para action=eval"},
                "path": {"type": "string", "description": "Destino del PNG para action=screenshot"}
            },
            "required": ["action"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let action = req_str(args, "action")?;
        with_browser(&self.browser, &self.ws, |browser| match action.as_str() {
            "open" => browser.open(&req_str(args, "url")?),
            "text" => browser.text(),
            "click" => browser.click(&req_str(args, "selector")?),
            "type" => browser.type_text(&req_str(args, "selector")?, &req_str(args, "text")?),
            "eval" => browser.evaluate(&req_str(args, "expression")?),
            "screenshot" => {
                let raw = str_arg(args, "path").unwrap_or_else(|| "captura.png".into());
                let path = self.ws.resolve(&raw)?;
                let bytes = browser.screenshot(&path)?;
                Ok(format!("PNG de {bytes} bytes en {}", self.ws.rel(&path)))
            }
            other => bail!("acción desconocida: {other}"),
        })
    }
}

struct WebSearchTool {
    browser: Arc<Mutex<Option<Browser>>>,
    state: Arc<Mutex<WebState>>,
    ws: Workspace,
}

impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Busca en internet gratis (DuckDuckGo → Brave → Bing) y devuelve título, URL y resumen. Usa web_fetch para leer una página."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer", "description": "1-10 (defecto 5)"},
                "engine": {"type": "string", "enum": ["duckduckgo", "brave", "bing"]}
            },
            "required": ["query"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let query = req_str(args, "query")?;
        let limit = usize_arg(args, "limit", 5).clamp(1, 10);
        let engines = match str_arg(args, "engine").and_then(|e| Engine::parse(&e)) {
            Some(engine) => vec![engine],
            None => Engine::all(),
        };

        {
            let mut state = self.state.lock().map_err(|_| anyhow!("web: lock envenenado"))?;
            if let Some(hits) = state.cached(&query) {
                return Ok(format!(
                    "[caché] {}",
                    web::format_hits(&query, &hits[..hits.len().min(limit)])
                ));
            }
            // Pausa de cortesía antes de tocar el motor.
            // Pausa de cortesía antes de tocar el motor (serializa búsquedas).
            state.wait();
        }

        let hits = with_browser(&self.browser, &self.ws, |browser| {
            web::search(browser, &engines, &query, limit)
        })?;
        if let Ok(mut state) = self.state.lock() {
            state.store(&query, &hits);
        }
        Ok(web::format_hits(&query, &hits))
    }
}

struct WebFetchTool {
    browser: Arc<Mutex<Option<Browser>>>,
    ws: Workspace,
}

impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "Abre una URL con el navegador y devuelve su texto visible (útil tras web_search)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "max_chars": {"type": "integer", "description": "Defecto 8000"}
            },
            "required": ["url"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let url = req_str(args, "url")?;
        let max = usize_arg(args, "max_chars", 8000).clamp(500, 20_000);
        with_browser(&self.browser, &self.ws, |browser| {
            web::fetch(browser, &url, max)
        })
    }
}

// ---------------------------------------------------------------------------
// Memoria y RAG
// ---------------------------------------------------------------------------

struct MemorySaveTool {
    memory: Arc<Mutex<Memory>>,
    embedder: Option<crate::provider::Embedder>,
}

impl Tool for MemorySaveTool {
    fn name(&self) -> &str {
        "memory_save"
    }
    fn description(&self) -> &str {
        "Guarda algo reutilizable para futuras sesiones: kind=fact (dato), lesson (aprendizaje/corrección), skill (procedimiento). Idempotente."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "Contenido conciso y autosuficiente"},
                "kind": {"type": "string", "enum": ["fact", "lesson", "skill"]},
                "tags": {"type": "array", "items": {"type": "string"}},
                "source": {"type": "string", "description": "De dónde sale (fichero, URL, tarea)"},
                "replaces": {"type": "string", "description": "id de un recuerdo que esta versión mejora"}
            },
            "required": ["text"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let text = req_str(args, "text")?;
        let kind = str_arg(args, "kind").unwrap_or_else(|| "fact".into());
        let source = str_arg(args, "source").unwrap_or_else(|| "sesión".into());
        let vector = match &self.embedder {
            Some(embedder) => embedder.embed(&text).unwrap_or_default(),
            None => Vec::new(),
        };
        let mut memory = self.memory.lock().map_err(|_| anyhow!("memoria: lock envenenado"))?;
        memory.remember(
            &kind,
            &text,
            tags_arg(args),
            &source,
            args["replaces"].as_str(),
            vector,
        )
    }
}

struct MemorySearchTool {
    memory: Arc<Mutex<Memory>>,
    embedder: Option<crate::provider::Embedder>,
}

impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }
    fn description(&self) -> &str {
        "Busca en la memoria persistente y en los documentos indexados (RAG híbrido léxico + semántico)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "kind": {"type": "string", "enum": ["fact", "lesson", "skill", "doc"]},
                "limit": {"type": "integer", "description": "Defecto 5"}
            },
            "required": ["query"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let query = req_str(args, "query")?;
        let limit = usize_arg(args, "limit", 5).clamp(1, 20);
        let kind = str_arg(args, "kind");
        let qvec = match &self.embedder {
            Some(embedder) => embedder.embed(&query).ok(),
            None => None,
        };
        let memory = self.memory.lock().map_err(|_| anyhow!("memoria: lock envenenado"))?;
        let hits = memory.search(&query, kind.as_deref(), limit, qvec.as_deref());
        if hits.is_empty() {
            return Ok("Sin resultados en memoria.".to_string());
        }
        let mut out = String::new();
        for (score, entry) in hits {
            out.push_str(&format!(
                "[{}·v{}] id={} (relevancia {:.2}, usos {})\n  {}\n  fuente: {}\n",
                entry.kind,
                entry.version,
                entry.id,
                score,
                entry.hits,
                crate::memory::truncate(&entry.text, 400),
                entry.source
            ));
        }
        Ok(out)
    }
}

struct MemoryListTool {
    memory: Arc<Mutex<Memory>>,
}

impl Tool for MemoryListTool {
    fn name(&self) -> &str {
        "memory_list"
    }
    fn description(&self) -> &str {
        "Lista los recuerdos guardados (más recientes primero)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": {"type": "string"},
                "limit": {"type": "integer", "description": "Defecto 15"}
            }
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let limit = usize_arg(args, "limit", 15).clamp(1, 100);
        let kind = str_arg(args, "kind");
        let memory = self.memory.lock().map_err(|_| anyhow!("memoria: lock envenenado"))?;
        let entries = memory.list(kind.as_deref(), limit);
        if entries.is_empty() {
            return Ok("Memoria vacía.".to_string());
        }
        let mut out = format!("{} recuerdos (total {}):\n", entries.len(), memory.len());
        for entry in entries {
            out.push_str(&format!(
                "- {} [{}·v{}] id={} {} usos: {}\n",
                entry.id,
                entry.kind,
                entry.version,
                crate::memory::truncate(&entry.text, 120),
                entry.hits,
                ""
            ));
        }
        Ok(out)
    }
}

struct MemoryForgetTool {
    memory: Arc<Mutex<Memory>>,
}

impl Tool for MemoryForgetTool {
    fn name(&self) -> &str {
        "memory_forget"
    }
    fn description(&self) -> &str {
        "Olvida un recuerdo por id (queda archivado, reversible en el JSON)."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]})
    }
    fn run(&self, args: &Value) -> Result<String> {
        let id = req_str(args, "id")?;
        let mut memory = self.memory.lock().map_err(|_| anyhow!("memoria: lock envenenado"))?;
        if memory.forget(&id)? {
            Ok(format!("olvidado: {id}"))
        } else {
            Ok(format!("no existe el recuerdo {id}"))
        }
    }
}

struct MemoryFeedbackTool {
    memory: Arc<Mutex<Memory>>,
}

impl Tool for MemoryFeedbackTool {
    fn name(&self) -> &str {
        "memory_feedback"
    }
    fn description(&self) -> &str {
        "Valora si un recuerdo sirvió (sube o baja su peso en futuras búsquedas)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"id": {"type": "string"}, "useful": {"type": "boolean"}},
            "required": ["id", "useful"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let id = req_str(args, "id")?;
        let useful = args["useful"].as_bool().unwrap_or(true);
        let mut memory = self.memory.lock().map_err(|_| anyhow!("memoria: lock envenenado"))?;
        if memory.feedback(&id, useful)? {
            Ok(format!("feedback aplicado a {id} (useful={useful})"))
        } else {
            Ok(format!("no existe el recuerdo {id}"))
        }
    }
}

struct RagIndexTool {
    ws: Workspace,
    memory: Arc<Mutex<Memory>>,
    embedder: Option<crate::provider::Embedder>,
}

const TEXT_EXT: [&str; 20] = [
    "rs", "md", "txt", "toml", "json", "yaml", "yml", "py", "js", "ts", "tsx", "html", "css", "sh",
    "c", "h", "cpp", "go", "java", "sql",
];

impl Tool for RagIndexTool {
    fn name(&self) -> &str {
        "rag_index"
    }
    fn description(&self) -> &str {
        "Indexa ficheros de texto del workspace en la memoria (kind=doc) para que `memory_search` los recupere con RAG."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Carpeta o fichero (defecto '.')"},
                "max_files": {"type": "integer", "description": "Defecto 100"},
                "chunk_chars": {"type": "integer", "description": "Defecto 1200"}
            }
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let raw = str_arg(args, "path").unwrap_or_else(|| ".".into());
        let path = self.ws.resolve(&raw)?;
        let max_files = usize_arg(args, "max_files", 100).clamp(1, 500);
        let chunk_chars = usize_arg(args, "chunk_chars", 1200).clamp(300, 4000);

        let mut files = Vec::new();
        collect_files(&path, &mut files, max_files);
        if files.is_empty() {
            return Ok("No hay ficheros de texto que indexar.".to_string());
        }

        let mut chunks_total = 0;
        let mut files_ok = 0;
        for file in &files {
            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            if text.trim().is_empty() {
                continue;
            }
            let rel = self.ws.rel(file);
            files_ok += 1;
            for chunk in chunk_text(&text, chunk_chars, 200) {
                let vector = match &self.embedder {
                    Some(embedder) if chunks_total < 200 => {
                        embedder.embed(&chunk).unwrap_or_default()
                    }
                    _ => Vec::new(),
                };
                let tag = file
                    .extension()
                    .map(|e| e.to_string_lossy().to_string())
                    .unwrap_or_default();
                let mut memory = self
                    .memory
                    .lock()
                    .map_err(|_| anyhow!("memoria: lock envenenado"))?;
                memory.remember(
                    "doc",
                    &chunk,
                    vec![tag].into_iter().filter(|t| !t.is_empty()).collect(),
                    &rel,
                    None,
                    vector,
                )?;
                chunks_total += 1;
            }
        }
        let memory = self.memory.lock().map_err(|_| anyhow!("memoria: lock envenenado"))?;
        Ok(format!(
            "indexados {chunks_total} fragmentos de {files_ok} ficheros (memoria total: {})",
            memory.len()
        ))
    }
}

fn collect_files(start: &Path, out: &mut Vec<PathBuf>, max: usize) {
    if out.len() >= max {
        return;
    }
    if start.is_file() {
        if is_text_file(start) {
            out.push(start.to_path_buf());
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(start) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    children.sort();
    for child in children {
        if out.len() >= max {
            return;
        }
        let name = child.file_name().unwrap_or_default().to_string_lossy().to_string();
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        if child.is_dir() {
            collect_files(&child, out, max);
        } else if is_text_file(&child) {
            out.push(child);
        }
    }
}

fn is_text_file(path: &Path) -> bool {
    let Some(ext) = path.extension() else {
        return false;
    };
    let ext = ext.to_string_lossy().to_lowercase();
    TEXT_EXT.contains(&ext.as_str())
}

fn chunk_text(text: &str, size: usize, overlap: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + size).min(chars.len());
        let chunk: String = chars[start..end].iter().collect();
        if !chunk.trim().is_empty() {
            chunks.push(chunk);
        }
        if end == chars.len() {
            break;
        }
        start = end.saturating_sub(overlap).max(start + 1);
    }
    chunks
}

// ---------------------------------------------------------------------------
// Orquestación
// ---------------------------------------------------------------------------

struct DelegateTool {
    orch: Arc<Orchestra>,
    depth: usize,
    mandate: Mandate,
}

impl Tool for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }
    fn description(&self) -> &str {
        "Encarga una subtarea a un subagente (nivel siguiente) con su propio modelo según rol: planner|worker|coder|fast. El hijo recibe la mitad de tu presupuesto de agentes y pasos."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "goal": {"type": "string", "description": "Objetivo concreto y autocontenido"},
                "role": {"type": "string", "enum": ["planner", "worker", "coder", "fast"]},
                "context": {"type": "string", "description": "Datos que el subagente necesita"}
            },
            "required": ["goal"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let goal = req_str(args, "goal")?;
        let role = str_arg(args, "role").unwrap_or_else(|| "worker".into());
        let context = str_arg(args, "context").unwrap_or_default();
        let out = self
            .orch
            .spawn(self.depth + 1, &role, &goal, &context, self.mandate.split())?;
        Ok(format!("[subagente {role} · nivel {}]\n{out}", self.depth + 1))
    }
}

struct DelegateManyTool {
    orch: Arc<Orchestra>,
    depth: usize,
    mandate: Mandate,
}

impl Tool for DelegateManyTool {
    fn name(&self) -> &str {
        "delegate_many"
    }
    fn description(&self) -> &str {
        "Lanza varios subagentes en paralelo (solo tareas independientes). El hardware limita cuántos corren a la vez y cada hijo recibe su parte del presupuesto de agentes y pasos."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "description": "Lista de {goal, role?, context?}",
                    "items": {
                        "type": "object",
                        "properties": {
                            "goal": {"type": "string"},
                            "role": {"type": "string"},
                            "context": {"type": "string"}
                        },
                        "required": ["goal"]
                    }
                }
            },
            "required": ["tasks"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let Some(items) = args["tasks"].as_array() else {
            bail!("falta el array 'tasks'");
        };
        let mut tasks: Vec<(String, String, String)> = Vec::new();
        for item in items {
            let goal = item["goal"].as_str().unwrap_or("").trim().to_string();
            if goal.is_empty() {
                continue;
            }
            let role = item["role"].as_str().unwrap_or("worker").to_string();
            let context = item["context"].as_str().unwrap_or("").to_string();
            tasks.push((role, goal, context));
        }
        if tasks.is_empty() {
            bail!("no hay tareas válidas");
        }
        let plan = tasks.len();
        if plan > self.orch.agents_left() + self.orch.running() {
            eprintln!(
                "  aviso: {plan} tareas y {} huecos; las sobrantes esperarán turno",
                self.orch.agents_left()
            );
        }

        let orch = self.orch.clone();
        let depth = self.depth;
        // Hermanos: cada uno la mitad de su parte justa del mandato.
        let share = self.mandate.share(tasks.len());
        let mut results: Vec<(usize, Result<String>)> = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for (i, (role, goal, context)) in tasks.into_iter().enumerate() {
                let orch = orch.clone();
                let label_role = role.clone();
                let label_goal = goal.clone();
                handles.push((
                    i,
                    label_role,
                    label_goal,
                    scope.spawn(move || orch.spawn(depth + 1, &role, &goal, &context, share)),
                ));
            }
            handles
                .into_iter()
                .map(|(i, role, goal, handle)| {
                    let out = handle
                        .join()
                        .unwrap_or_else(|_| Err(anyhow!("el subagente entró en pánico")));
                    (i, out.map(|text| format!("[{role}] {goal}\n{text}")))
                })
                .collect()
        });
        results.sort_by_key(|(i, _)| *i);

        let mut out = String::new();
        let mut failures = 0;
        for (i, result) in results {
            match result {
                Ok(text) => out.push_str(&format!("\n### tarea {}\n{}\n", i + 1, text)),
                Err(err) => {
                    failures += 1;
                    out.push_str(&format!("\n### tarea {} FALLÓ\n{err:#}\n", i + 1));
                }
            }
        }
        if failures > 0 {
            out.push_str(&format!("\n({failures} de {plan} subtareas fallaron)"));
        }
        Ok(out)
    }
}

struct SystemInfoTool {
    orch: Arc<Orchestra>,
}

impl Tool for SystemInfoTool {
    fn name(&self) -> &str {
        "system_info"
    }
    fn description(&self) -> &str {
        "Muestra hardware medido, modelos instalados, modelo elegido por rol y huecos de orquestación."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn run(&self, _args: &Value) -> Result<String> {
        let orch = &self.orch;
        let mut out = format!(
            "Hardware: {}\nBackend: {} ({})\nSubagentes: {} libres de {}, {} en ejecución, profundidad máx {}\n",
            orch.specs.summary(),
            orch.backend.label(),
            orch.backend.url,
            orch.agents_left(),
            orch.limits.max_agents,
            orch.running(),
            orch.limits.max_depth
        );
        out.push_str("Modelos instalados:\n");
        for model in &orch.models {
            out.push_str(&format!(
                "- {} ({:.1}B, {:.2} GB, {})\n",
                model.id, model.params_b, model.size_gb, model.family
            ));
        }
        for role in [Role::Planner, Role::Worker, Role::Coder, Role::Fast] {
            out.push_str(&format!("  {} → {}\n", role.as_str(), orch.model_for(role)));
        }
        let downloads = Downloads::open(orch.downloads.clone())?;
        out.push_str(&format!(
            "Descargas de modelos: {}/{} usadas {:?}\n",
            downloads.used(),
            MAX_DOWNLOADS,
            downloads.pulled()
        ));
        if let Ok(memory) = orch.memory.lock() {
            out.push_str(&format!(
                "Memoria: {} recuerdos en {}\n",
                memory.len(),
                memory.path().display()
            ));
        }
        if let Ok(sched) = Scheduler::open(scheduler::schedule_path(orch.workspace.root())) {
            out.push_str(&format!("Agenda: {} tarea(s)\n", sched.tasks().len()));
            for task in sched.tasks() {
                out.push_str(&format!(
                    "- {} cada {} ({} ejecuciones): {}\n",
                    task.id,
                    scheduler::human_interval(task.every_secs),
                    task.runs,
                    task.goal
                ));
            }
        }
        Ok(out)
    }
}

struct ModelPullTool {
    orch: Arc<Orchestra>,
}

impl Tool for ModelPullTool {
    fn name(&self) -> &str {
        "model_pull"
    }
    fn description(&self) -> &str {
        "Descarga un modelo en Ollama. Hay un tope de 3 descargas por proyecto."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"name": {"type": "string", "description": "p. ej. qwen2.5-coder:7b"}},
            "required": ["name"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let name = req_str(args, "name")?;
        if self.orch.backend.kind != Kind::Ollama {
            bail!("las descargas solo se gestionan con Ollama; en llama.cpp los GGUF se colocan a mano");
        }
        if self.orch.models.iter().any(|m| m.id == name) {
            return Ok(format!("'{name}' ya está instalado"));
        }
        let mut ledger = Downloads::open(self.orch.downloads.clone())?;
        ledger.ensure_slot(&name)?;
        let message = crate::models::pull_ollama(&self.orch.backend.url, &name)?;
        ledger.record(&name)?;
        Ok(format!(
            "{message}. Descargas usadas en este proyecto: {}/{}",
            ledger.used(),
            MAX_DOWNLOADS
        ))
    }
}

// ---------------------------------------------------------------------------
// Agenda y metas
// ---------------------------------------------------------------------------

struct ScheduleAddTool {
    ws: Workspace,
}

impl Tool for ScheduleAddTool {
    fn name(&self) -> &str {
        "schedule_add"
    }
    fn description(&self) -> &str {
        "Programa una tarea recurrente que ejecutará un subagente (se ejecuta con --daemon)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "goal": {"type": "string"},
                "every": {"type": "string", "description": "30s, 5m, 2h, 1d"},
                "role": {"type": "string", "enum": ["planner", "worker", "coder", "fast"]}
            },
            "required": ["goal", "every"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let goal = req_str(args, "goal")?;
        let every_text = req_str(args, "every")?;
        let every = scheduler::parse_interval(&every_text)
            .ok_or_else(|| anyhow!("intervalo no válido: {every_text} (usa 30s, 5m, 2h, 1d)"))?;
        let role = str_arg(args, "role").unwrap_or_else(|| "worker".into());
        let mut sched = Scheduler::open(scheduler::schedule_path(self.ws.root()))?;
        let task = sched.add(&goal, every, &role)?;
        Ok(format!(
            "tarea {} programada cada {} (rol {role}). Se ejecuta con `--daemon`.",
            task.id,
            scheduler::human_interval(task.every_secs)
        ))
    }
}

struct ScheduleListTool {
    ws: Workspace,
}

impl Tool for ScheduleListTool {
    fn name(&self) -> &str {
        "schedule_list"
    }
    fn description(&self) -> &str {
        "Lista las tareas programadas con su intervalo y última ejecución."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn run(&self, _args: &Value) -> Result<String> {
        let sched = Scheduler::open(scheduler::schedule_path(self.ws.root()))?;
        if sched.tasks().is_empty() {
            return Ok("No hay tareas programadas.".to_string());
        }
        let now = scheduler::now();
        let mut out = String::new();
        for task in sched.tasks() {
            let next = if task.last_run == 0 {
                "ya".to_string()
            } else {
                scheduler::human_interval(
                    task.last_run
                        .saturating_add(task.every_secs)
                        .saturating_sub(now),
                )
            };
            out.push_str(&format!(
                "- {} [{}] cada {} · {} ejecuciones · próxima en {} · {}\n",
                task.id,
                if task.enabled { "activa" } else { "pausada" },
                scheduler::human_interval(task.every_secs),
                task.runs,
                next,
                task.goal
            ));
        }
        Ok(out)
    }
}

struct ScheduleRemoveTool {
    ws: Workspace,
}

impl Tool for ScheduleRemoveTool {
    fn name(&self) -> &str {
        "schedule_remove"
    }
    fn description(&self) -> &str {
        "Elimina una tarea programada por id."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]})
    }
    fn run(&self, args: &Value) -> Result<String> {
        let id = req_str(args, "id")?;
        let mut sched = Scheduler::open(scheduler::schedule_path(self.ws.root()))?;
        if sched.remove(&id)? {
            Ok(format!("tarea {id} eliminada"))
        } else {
            Ok(format!("no existe la tarea {id}"))
        }
    }
}

struct GoalPlanTool {
    goal: Arc<crate::goals::GoalState>,
}

impl Tool for GoalPlanTool {
    fn name(&self) -> &str { "goal_task" }
    fn description(&self) -> &str {
        "Gestiona el plan persistente del objetivo raíz. Usa action=add/update para definir tareas y dependencias; start para iniciar una tarea; complete para cerrarla con evidencia; list para consultar el plan. Solo el agente raíz puede usarlo."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "update", "start", "complete", "list"]},
                "id": {"type": "string"},
                "title": {"type": "string"},
                "depends_on": {"type": "array", "items": {"type": "string"}},
                "checks": {"type": "array", "items": {"type": "string"}, "description": "Comandos exactos de aceptación de la lista permitida; obligatorios para verificar la tarea."},
                "evidence": {"type": "string"}
            },
            "required": ["action"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        use crate::goals::TaskStatus;
        let action = req_str(args, "action")?;
        match action.as_str() {
            "add" | "update" => {
                let id = req_str(args, "id")?;
                let title = req_str(args, "title")?;
                let current = self.goal.tasks().into_iter().find(|task| task.id == id);
                let deps = if let Some(value) = args.get("depends_on") {
                    let items = value.as_array().ok_or_else(|| anyhow!("depends_on debe ser un array de strings"))?;
                    items.iter().map(|item| item.as_str().map(str::to_string)
                        .ok_or_else(|| anyhow!("cada dependencia debe ser un string")))
                        .collect::<Result<Vec<_>>>()?
                } else {
                    current.as_ref().map(|task| task.depends_on.clone()).unwrap_or_default()
                };
                let checks = if let Some(value) = args.get("checks") {
                    let items = value.as_array().ok_or_else(|| anyhow!("checks debe ser un array de strings"))?;
                    items.iter().map(|item| item.as_str().map(str::to_string)
                        .ok_or_else(|| anyhow!("cada check debe ser un string")))
                        .collect::<Result<Vec<_>>>()?
                } else {
                    current.map(|task| task.checks).unwrap_or_default()
                };
                self.goal.upsert_task(&id, &title, &deps, &checks)?;
                Ok(format!("Tarea {id} guardada en el plan persistente."))
            }
            "start" => {
                let id = req_str(args, "id")?;
                self.goal.set_task_status(&id, TaskStatus::InProgress, None)?;
                Ok(format!("Tarea {id} iniciada."))
            }
            "complete" => {
                let id = req_str(args, "id")?;
                let evidence = req_str(args, "evidence")?;
                self.goal.set_task_status(&id, TaskStatus::Completed, Some(&evidence))?;
                Ok(format!("Tarea {id} completada con evidencia registrada."))
            }
            "list" => {
                let tasks = self.goal.tasks();
                if tasks.is_empty() { return Ok("El plan todavía no contiene tareas.".into()); }
                let mut out = String::new();
                for task in tasks {
                    out.push_str(&format!("- {} [{:?}] {} · depende de: {} · evidencia: {}",
                        task.id, task.status, task.title,
                        if task.depends_on.is_empty() { "ninguna".to_string() } else { task.depends_on.join(", ") },
                        task.evidence.unwrap_or_else(|| "pendiente".into())
                    ));
                    out.push('\n');
                }
                Ok(out)
            }
            _ => bail!("acción desconocida para goal_task: {action}"),
        }
    }
}

struct GoalFailureTool {
    goal: Arc<crate::goals::GoalState>,
}

impl Tool for GoalFailureTool {
    fn name(&self) -> &str { "goal_failure" }
    fn description(&self) -> &str {
        "Registra un fallo, su síntoma y la estrategia intentada en el diario persistente. Debe llamarse antes de reintentar. Repetir el mismo fingerprint y estrategia se bloquea salvo que condition_changed=true y se explique el cambio."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "fingerprint": {"type": "string"},
                "category": {"type": "string"},
                "symptom": {"type": "string"},
                "strategy": {"type": "string"},
                "condition_changed": {"type": "boolean"}
            },
            "required": ["fingerprint", "category", "symptom", "strategy", "condition_changed"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let fingerprint = req_str(args, "fingerprint")?;
        let category = req_str(args, "category")?;
        let symptom = req_str(args, "symptom")?;
        let strategy = req_str(args, "strategy")?;
        let condition_changed = args.get("condition_changed").and_then(Value::as_bool)
            .ok_or_else(|| anyhow!("condition_changed debe ser boolean"))?;
        self.goal.record_failure(&fingerprint, &category, &symptom, &strategy, condition_changed)?;
        Ok(format!("Fallo registrado: {fingerprint}. La estrategia queda en el diario persistente."))
    }
}

struct GoalVerifyTool {
    goal: Arc<crate::goals::GoalState>,
    ws: Workspace,
}

impl Tool for GoalVerifyTool {
    fn name(&self) -> &str { "goal_verify" }
    fn description(&self) -> &str {
        "Evaluador determinista separado del agente ejecutor. Ejecuta los checks de aceptación predefinidos de cada tarea completada; solo marca verified si todos los comandos permitidos terminan con exit 0. No acepta evidencias textuales como sustituto de ejecutar checks."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"id": {"type": "string", "description": "ID de tarea; si se omite, evalúa todas las tareas completadas"}},
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let tasks = self.goal.tasks();
        if tasks.is_empty() { bail!("no hay tareas que evaluar"); }
        let selected = if let Some(id) = str_arg(args, "id") {
            tasks.into_iter().filter(|task| task.id == id).collect::<Vec<_>>()
        } else {
            tasks.into_iter().filter(|task| task.status == crate::goals::TaskStatus::Completed).collect::<Vec<_>>()
        };
        if selected.is_empty() { bail!("no hay tareas completadas que evaluar"); }
        let mut report = String::new();
        let mut failures = 0usize;
        for task in selected {
            if task.status != crate::goals::TaskStatus::Completed {
                self.goal.set_task_verification(&task.id, false, "la tarea no está completada")?;
                failures += 1;
                report.push_str(&format!("- {}: FAIL; tarea no completada\n", task.id));
                continue;
            }
            if task.checks.is_empty() {
                self.goal.set_task_verification(&task.id, false, "la tarea no define checks ejecutables")?;
                failures += 1;
                report.push_str(&format!("- {}: FAIL; faltan checks de aceptación\n", task.id));
                continue;
            }
            let mut output = String::new();
            let mut passed = true;
            for check in &task.checks {
                let Some(command) = verification_command(check) else {
                    passed = false;
                    output.push_str(&format!("check no permitido: {check}\n"));
                    break;
                };
                let result = run_command(command, self.ws.root(), Duration::from_secs(600))?;
                let failed = result.contains("[exit ") || result.contains("[timeout");
                output.push_str(&format!("$ {command}\n{result}\n"));
                if failed { passed = false; break; }
            }
            self.goal.set_task_verification(&task.id, passed, &output)?;
            if passed {
                report.push_str(&format!("- {}: PASS; checks ejecutados y exit 0\n", task.id));
            } else {
                failures += 1;
                report.push_str(&format!("- {}: FAIL; revisar salida y corregir antes de reintentar\n", task.id));
            }
        }
        if failures > 0 {
            report.push_str(&format!("Evaluación no superada: {failures} tarea(s) fallaron."));
        } else {
            report.push_str("Evaluación superada: todos los checks ejecutados terminaron correctamente.");
        }
        Ok(report)
    }
}

fn verification_command(check: &str) -> Option<&'static str> {
    match check.trim() {
        "cargo test" => Some("cargo test --locked"),
        "cargo clippy" => Some("cargo clippy --locked --all-targets -- -D warnings"),
        "cargo check" => Some("cargo check --locked"),
        "cargo fmt --check" => Some("cargo fmt --all -- --check"),
        "git diff --check" => Some("git diff --check"),
        "npm test" => Some("npm test"),
        "npm run test" => Some("npm run test"),
        "pytest" => Some("pytest"),
        "python -m pytest" => Some("python -m pytest"),
        "go test" => Some("go test ./..."),
        "make test" => Some("make test"),
        _ => None,
    }
}

struct GoalDoneTool {
    goal: Arc<crate::goals::GoalState>,
}

impl Tool for GoalDoneTool {
    fn name(&self) -> &str {
        "goal_done"
    }
    fn description(&self) -> &str {
        "Declara cumplida la meta activa. `evidence` es obligatorio: qué comprobaste y cómo."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"evidence": {"type": "string"}},
            "required": ["evidence"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        Ok(self.goal.ack(args["evidence"].as_str().unwrap_or("")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_no_deja_escapar_por_symlinks() {
        let ws = workspace_for_tests("resolve-symlink");
        let fuera = std::env::temp_dir().join(format!("agent-fuera-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&fuera);
        std::fs::create_dir_all(&fuera).unwrap();
        std::fs::write(fuera.join("secreto.txt"), "x").unwrap();

        let enlace = ws.root().join("enlace");
        let _ = std::fs::remove_file(&enlace);
        std::os::unix::fs::symlink(&fuera, &enlace).unwrap();

        assert!(
            ws.resolve("enlace/secreto.txt").is_err(),
            "el symlink escapó del workspace"
        );
        // Un fichero nuevo que aún no existe sigue permitido.
        assert!(ws.resolve("enlace-nuevo.txt").is_ok());
        let _ = std::fs::remove_dir_all(&fuera);
    }

    #[test]
    fn resolve_rechaza_escapes() {
        let ws = workspace_for_tests("resolve");
        assert!(ws.resolve("").is_ok());
        assert!(ws.resolve("src/main.rs").is_ok());
        assert!(ws.resolve("../fuera").is_err());
        assert!(ws.resolve("/etc/passwd").is_err());
        assert!(ws.resolve("a/../../fuera").is_err());
    }

    #[test]
    fn trocea_texto_con_solape() {
        let text: String = (0..100).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let chunks = chunk_text(&text, 40, 10);
        assert!(chunks.len() >= 3);
        assert!(chunks.iter().all(|c| c.chars().count() <= 40));
    }

    #[test]
    fn parametros_de_herramientas_son_json_schema() {
        let reg = ToolRegistry::empty();
        assert!(reg.defs().is_empty());
        let ws = workspace_for_tests("schema");
        let tool = FsReadTool { ws };
        assert_eq!(tool.name(), "fs_read");
        assert_eq!(tool.parameters()["type"], "object");
    }
}
