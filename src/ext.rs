//! Autoextensión: el agente crea sus propias **tools** (scripts Python),
//! **skills** (procedimientos en Markdown) y **servidores MCP** (Python, JSON-RPC
//! por stdio), y los usa sin recompilar.
//!
//! Todo vive en `.agent/` y es texto legible: se puede revisar, versionar y
//! borrar a mano. Las tools Python reciben los argumentos como JSON por stdin y
//! devuelven el resultado por stdout.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::tools::{Tool, Workspace};

pub fn tools_dir(root: &Path) -> PathBuf {
    root.join(".agent").join("tools")
}

pub fn skills_dir(root: &Path) -> PathBuf {
    root.join(".agent").join("skills")
}

pub fn mcp_dir(root: &Path) -> PathBuf {
    root.join(".agent").join("mcp")
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '_')
            .unwrap_or(false)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn default_schema() -> Value {
    json!({"type": "object", "properties": {}})
}

fn python() -> &'static str {
    if Path::new("/usr/bin/python3").exists() {
        "python3"
    } else {
        "python"
    }
}

// ---------------------------------------------------------------------------
// Tools en Python
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PyToolMeta {
    pub name: String,
    pub description: String,
    #[serde(default = "default_schema")]
    pub parameters: Value,
}

pub fn load_python_tools(root: &Path) -> Vec<PyToolMeta> {
    let dir = tools_dir(root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "json").unwrap_or(false) {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(meta) = serde_json::from_str::<PyToolMeta>(&text) else {
                continue;
            };
            if valid_name(&meta.name) && dir.join(format!("{}.py", meta.name)).is_file() {
                found.push(meta);
            }
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

pub struct PythonTool {
    meta: PyToolMeta,
    script: PathBuf,
}

impl PythonTool {
    pub fn new(meta: PyToolMeta, root: &Path) -> Self {
        let script = tools_dir(root).join(format!("{}.py", meta.name));
        Self { meta, script }
    }
}

impl Tool for PythonTool {
    fn name(&self) -> &str {
        &self.meta.name
    }
    fn description(&self) -> &str {
        &self.meta.description
    }
    fn parameters(&self) -> Value {
        self.meta.parameters.clone()
    }
    fn run(&self, args: &Value) -> Result<String> {
        run_python_tool(&self.script, args, 60)
    }
}

fn run_python_tool(script: &Path, args: &Value, timeout_secs: u64) -> Result<String> {
    let mut child = Command::new("timeout")
        .arg(timeout_secs.to_string())
        .arg(python())
        .arg(script)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("no se pudo ejecutar {}", script.display()))?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(args.to_string().as_bytes())?;
    }
    drop(child.stdin.take());
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        bail!(
            "la herramienta falló ({})\n{}{}",
            output.status,
            stdout,
            if stderr.is_empty() {
                String::new()
            } else {
                format!("\n[stderr]\n{stderr}")
            }
        );
    }
    Ok(if stdout.is_empty() {
        "(sin salida)".to_string()
    } else {
        stdout
    })
}

pub struct ToolCreateTool {
    pub ws: Workspace,
}

impl Tool for ToolCreateTool {
    fn name(&self) -> &str {
        "tool_create"
    }
    fn description(&self) -> &str {
        "Crea una herramienta nueva en Python (.agent/tools). El script lee los argumentos JSON por stdin y escribe el resultado por stdout. Se prueba antes de aceptarla."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "snake_case, p. ej. contar_lineas"},
                "description": {"type": "string", "description": "Cuándo debe usarla el agente"},
                "code": {"type": "string", "description": "Código Python completo"},
                "parameters": {"type": "object", "description": "JSON Schema de los argumentos"}
            },
            "required": ["name", "description", "code"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let name = args["name"].as_str().unwrap_or("").trim().to_string();
        if !valid_name(&name) {
            bail!("nombre inválido: usa snake_case alfanumérico (máx 40)");
        }
        let description = args["description"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();
        if description.is_empty() {
            bail!("la descripción es obligatoria: explica cuándo usarla");
        }
        let code = args["code"].as_str().unwrap_or("");
        if !code.contains("import") && !code.contains("print") {
            bail!("el código parece vacío");
        }

        let dir = tools_dir(self.ws.root());
        std::fs::create_dir_all(&dir)?;
        let script = dir.join(format!("{name}.py"));
        let meta_path = dir.join(format!("{name}.json"));
        if script.exists() {
            bail!("ya existe la herramienta '{name}'; bórrala con tool_remove antes de recrearla");
        }

        std::fs::write(&script, code)?;
        // Autoprueba: si no arranca, no se registra.
        let probe = run_python_tool(&script, &json!({}), 30);
        match probe {
            Ok(_) => {}
            Err(err) => {
                let _ = std::fs::remove_file(&script);
                return Err(err.context("la herramienta no pasó la autoprueba"));
            }
        }

        let parameters = match &args["parameters"] {
            Value::Object(_) => args["parameters"].clone(),
            Value::String(text) => serde_json::from_str(text).unwrap_or_else(|_| default_schema()),
            _ => default_schema(),
        };
        let meta = PyToolMeta {
            name: name.clone(),
            description,
            parameters,
        };
        std::fs::write(&meta_path, serde_json::to_vec_pretty(&meta)?)?;
        Ok(format!(
            "herramienta '{name}' creada y probada en {}. Disponible en el siguiente turno o subagente.",
            script.display()
        ))
    }
}

pub struct ToolListTool {
    pub ws: Workspace,
}

impl Tool for ToolListTool {
    fn name(&self) -> &str {
        "tool_list"
    }
    fn description(&self) -> &str {
        "Lista las herramientas Python creadas por el agente."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn run(&self, _args: &Value) -> Result<String> {
        let tools = load_python_tools(self.ws.root());
        if tools.is_empty() {
            return Ok("No hay herramientas propias todavía (puedes crear con tool_create).".into());
        }
        let mut out = String::new();
        for tool in tools {
            out.push_str(&format!("- {}: {}\n", tool.name, tool.description));
        }
        Ok(out)
    }
}

pub struct ToolRemoveTool {
    pub ws: Workspace,
}

impl Tool for ToolRemoveTool {
    fn name(&self) -> &str {
        "tool_remove"
    }
    fn description(&self) -> &str {
        "Borra una herramienta Python creada (script y metadatos)."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]})
    }
    fn run(&self, args: &Value) -> Result<String> {
        let name = args["name"].as_str().unwrap_or("").trim();
        if !valid_name(name) {
            bail!("nombre inválido: {name}");
        }
        let dir = tools_dir(self.ws.root());
        let mut removed = false;
        for path in [dir.join(format!("{name}.py")), dir.join(format!("{name}.json"))] {
            if path.exists() {
                std::fs::remove_file(&path)?;
                removed = true;
            }
        }
        if removed {
            Ok(format!("herramienta '{name}' borrada"))
        } else {
            Ok(format!("no existe la herramienta '{name}'"))
        }
    }
}

// ---------------------------------------------------------------------------
// Skills en Markdown
// ---------------------------------------------------------------------------

pub struct SkillCreateTool {
    pub ws: Workspace,
}

impl Tool for SkillCreateTool {
    fn name(&self) -> &str {
        "skill_create"
    }
    fn description(&self) -> &str {
        "Crea/actualiza una skill (procedimiento reutilizable en Markdown) con su condición de uso."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "identificador snake_case"},
                "when": {"type": "string", "description": "cuándo debe cargarse esta skill"},
                "body": {"type": "string", "description": "pasos concretos, comandos, trampas"}
            },
            "required": ["name", "when", "body"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let name = args["name"].as_str().unwrap_or("").trim().to_string();
        if !valid_name(&name) {
            bail!("nombre inválido: usa snake_case alfanumérico");
        }
        let when = args["when"].as_str().unwrap_or("").trim();
        let body = args["body"].as_str().unwrap_or("").trim();
        if when.is_empty() || body.is_empty() {
            bail!("`when` y `body` son obligatorios");
        }
        let dir = skills_dir(self.ws.root());
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{name}.md"));
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let content = format!("---\nname: {name}\nwhen: {when}\nupdated: {stamp}\n---\n\n{body}\n");
        std::fs::write(&path, content)?;
        Ok(format!(
            "skill '{name}' guardada en {}. Aparece en el prompt de los próximos turnos.",
            path.display()
        ))
    }
}

pub struct SkillListTool {
    pub ws: Workspace,
}

impl Tool for SkillListTool {
    fn name(&self) -> &str {
        "skill_list"
    }
    fn description(&self) -> &str {
        "Lista las skills disponibles con su condición de uso."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn run(&self, _args: &Value) -> Result<String> {
        let skills = list_skills(self.ws.root());
        if skills.is_empty() {
            return Ok("No hay skills guardadas.".into());
        }
        let mut out = String::new();
        for (name, when) in skills {
            out.push_str(&format!("- {name}: {when}\n"));
        }
        Ok(out)
    }
}

pub struct SkillReadTool {
    pub ws: Workspace,
}

impl Tool for SkillReadTool {
    fn name(&self) -> &str {
        "skill_read"
    }
    fn description(&self) -> &str {
        "Lee el contenido completo de una skill por nombre."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]})
    }
    fn run(&self, args: &Value) -> Result<String> {
        let name = args["name"].as_str().unwrap_or("").trim();
        if !valid_name(name) {
            bail!("nombre inválido: {name}");
        }
        let path = skills_dir(self.ws.root()).join(format!("{name}.md"));
        std::fs::read_to_string(&path)
            .with_context(|| format!("no existe la skill '{name}' (usa skill_list)"))
    }
}

fn list_skills(root: &Path) -> Vec<(String, String)> {
    let dir = skills_dir(root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut skills = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "md").unwrap_or(false) {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let when = text
                .lines()
                .find_map(|line| line.strip_prefix("when:").map(|v| v.trim().to_string()))
                .unwrap_or_else(|| "(sin condición)".into());
            skills.push((name, when));
        }
    }
    skills.sort();
    skills
}

/// Índice de skills para el prompt de sistema.
pub fn skills_index(root: &Path) -> String {
    let skills = list_skills(root);
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nSkills propias (cárgalas con skill_read cuando encajen):\n");
    for (name, when) in skills {
        out.push_str(&format!("- {name}: {when}\n"));
    }
    out
}

// ---------------------------------------------------------------------------
// Servidores MCP (Python, JSON-RPC por stdio)
// ---------------------------------------------------------------------------

const MCP_CLIENT: &str = r#"#!/usr/bin/env python3
"""Cliente MCP mínimo: initialize + una llamada, y termina."""
import json, subprocess, sys

server, method = sys.argv[1], sys.argv[2]
params = json.loads(sys.argv[3]) if len(sys.argv) > 3 else {}

proc = subprocess.Popen([sys.executable, server], stdin=subprocess.PIPE,
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

def send(obj):
    proc.stdin.write(json.dumps(obj) + "\n")
    proc.stdin.flush()

def read():
    while True:
        line = proc.stdout.readline()
        if not line:
            err = proc.stderr.read()[:500]
            raise SystemExit("servidor MCP cerró la conexión: " + err)
        line = line.strip()
        if not line:
            continue
        try:
            return json.loads(line)
        except json.JSONDecodeError:
            continue

try:
    send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
          "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                     "clientInfo": {"name": "local-agent", "version": "0.2"}}})
    read()
    send({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})
    send({"jsonrpc": "2.0", "id": 2, "method": method, "params": params})
    while True:
        msg = read()
        if msg.get("id") == 2:
            print(json.dumps(msg, ensure_ascii=False))
            break
finally:
    proc.terminate()
"#;

const MCP_SCAFFOLD: &str = r#"#!/usr/bin/env python3
"""Servidor MCP mínimo (stdio). Edita TOOLS para añadir capacidades.

Protocolo: JSON-RPC 2.0 por líneas. Métodos: initialize, tools/list, tools/call.
"""
import json, sys

TOOLS = [
    {
        "name": "echo",
        "description": "Devuelve el texto recibido (ejemplo).",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    }
]

def call(name, args):
    if name == "echo":
        return {"content": [{"type": "text", "text": str(args.get("text", ""))}]}
    raise ValueError("herramienta desconocida: " + name)

def handle(msg):
    method = msg.get("method")
    if method == "initialize":
        return {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                "serverInfo": {"name": "custom", "version": "0.1"}}
    if method == "tools/list":
        return {"tools": TOOLS}
    if method == "tools/call":
        params = msg.get("params") or {}
        return call(params.get("name"), params.get("arguments") or {})
    raise ValueError("método no soportado: " + str(method))

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except json.JSONDecodeError:
        continue
    if "id" not in msg:
        continue
    try:
        result = handle(msg)
        reply = {"jsonrpc": "2.0", "id": msg["id"], "result": result}
    except Exception as exc:  # noqa: BLE001
        reply = {"jsonrpc": "2.0", "id": msg["id"],
                 "error": {"code": -32000, "message": str(exc)}}
    print(json.dumps(reply, ensure_ascii=False), flush=True)
"#;

fn write_client(root: &Path) -> Result<PathBuf> {
    let dir = mcp_dir(root);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("client.py");
    if !path.exists() {
        std::fs::write(&path, MCP_CLIENT)?;
    }
    Ok(path)
}

fn mcp_request(root: &Path, server: &str, method: &str, params: Value) -> Result<Value> {
    let client = write_client(root)?;
    let server_path = mcp_dir(root).join(if server.ends_with(".py") {
        server.to_string()
    } else {
        format!("{server}.py")
    });
    if !server_path.is_file() {
        bail!("no existe el servidor MCP '{}'", server_path.display());
    }
    let output = Command::new("timeout")
        .args(["25", python()])
        .arg(&client)
        .arg(&server_path)
        .arg(method)
        .arg(params.to_string())
        .output()
        .context("no se pudo ejecutar el cliente MCP")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        bail!(
            "el servidor MCP falló: {}",
            if stderr.trim().is_empty() {
                stdout.trim().to_string()
            } else {
                stderr.trim().to_string()
            }
        );
    }
    let value: Value = serde_json::from_str(stdout.trim())
        .with_context(|| format!("respuesta MCP no-JSON: {}", stdout.trim()))?;
    if !value["error"].is_null() {
        bail!("MCP: {}", value["error"]);
    }
    Ok(value["result"].clone())
}

pub struct McpCreateTool {
    pub ws: Workspace,
}

impl Tool for McpCreateTool {
    fn name(&self) -> &str {
        "mcp_create"
    }
    fn description(&self) -> &str {
        "Crea un servidor MCP en Python (.agent/mcp). Sin `code` genera una plantilla con la tool 'echo'."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "code": {"type": "string", "description": "Código del servidor (opcional)"}
            },
            "required": ["name"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let name = args["name"].as_str().unwrap_or("").trim();
        if !valid_name(name) {
            bail!("nombre inválido: usa snake_case");
        }
        let dir = mcp_dir(self.ws.root());
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{name}.py"));
        if path.exists() {
            bail!("ya existe el servidor MCP '{name}'");
        }
        let code = args["code"]
            .as_str()
            .filter(|c| !c.trim().is_empty())
            .unwrap_or(MCP_SCAFFOLD);
        std::fs::write(&path, code)?;
        // Prueba real: handshake + tools/list.
        match mcp_request(self.ws.root(), name, "tools/list", json!({})) {
            Ok(value) => {
                let count = value["tools"].as_array().map(|a| a.len()).unwrap_or(0);
                Ok(format!(
                    "servidor MCP '{name}' creado y verificado ({count} tool(s)) en {}",
                    path.display()
                ))
            }
            Err(err) => {
                let _ = std::fs::remove_file(&path);
                Err(err.context("el servidor MCP no pasó el handshake"))
            }
        }
    }
}

pub struct McpServersTool {
    pub ws: Workspace,
}

impl Tool for McpServersTool {
    fn name(&self) -> &str {
        "mcp_servers"
    }
    fn description(&self) -> &str {
        "Lista los servidores MCP disponibles en .agent/mcp."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn run(&self, _args: &Value) -> Result<String> {
        let dir = mcp_dir(self.ws.root());
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok("No hay servidores MCP (crea uno con mcp_create).".into());
        };
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().map(|e| e == "py").unwrap_or(false)
                    && p.file_name().map(|n| n != "client.py").unwrap_or(false)
            })
            .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
            .collect();
        names.sort();
        if names.is_empty() {
            return Ok("No hay servidores MCP (crea uno con mcp_create).".into());
        }
        Ok(names.join("\n"))
    }
}

pub struct McpToolsTool {
    pub ws: Workspace,
}

impl Tool for McpToolsTool {
    fn name(&self) -> &str {
        "mcp_tools"
    }
    fn description(&self) -> &str {
        "Lista las herramientas que expone un servidor MCP."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"server": {"type": "string"}}, "required": ["server"]})
    }
    fn run(&self, args: &Value) -> Result<String> {
        let server = args["server"].as_str().unwrap_or("").trim();
        let value = mcp_request(self.ws.root(), server, "tools/list", json!({}))?;
        let Some(tools) = value["tools"].as_array() else {
            return Ok(format!("{server}: sin herramientas"));
        };
        let mut out = String::new();
        for tool in tools {
            out.push_str(&format!(
                "- {}: {}\n",
                tool["name"].as_str().unwrap_or("?"),
                tool["description"].as_str().unwrap_or("")
            ));
        }
        Ok(out)
    }
}

pub struct McpCallTool {
    pub ws: Workspace,
}

impl Tool for McpCallTool {
    fn name(&self) -> &str {
        "mcp_call"
    }
    fn description(&self) -> &str {
        "Llama a una herramienta de un servidor MCP propio."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": {"type": "string"},
                "tool": {"type": "string"},
                "arguments": {"type": "object"}
            },
            "required": ["server", "tool"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let server = args["server"].as_str().unwrap_or("").trim();
        let tool = args["tool"].as_str().unwrap_or("").trim();
        if tool.is_empty() {
            bail!("falta `tool`");
        }
        let arguments = match &args["arguments"] {
            Value::Object(_) => args["arguments"].clone(),
            _ => json!({}),
        };
        let value = mcp_request(
            self.ws.root(),
            server,
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
        )?;
        Ok(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valida_nombres() {
        assert!(valid_name("contar_lineas"));
        assert!(!valid_name("2contar"));
        assert!(!valid_name("con-espacios"));
        assert!(!valid_name(""));
    }

    #[test]
    fn crea_y_ejecuta_una_tool_python() {
        if Command::new(python()).arg("--version").output().is_err() {
            return; // sin Python no hay nada que comprobar
        }
        let ws = crate::workspace_for_tests("ext-tool");
        let create = ToolCreateTool { ws: ws.clone() };
        let code = "import json,sys\nargs=json.load(sys.stdin)\nprint(args.get('a',0)+args.get('b',0))";
        create
            .run(&json!({
                "name": "sumar",
                "description": "suma dos números",
                "code": code,
                "parameters": {"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}}}
            }))
            .unwrap();
        let tools = load_python_tools(ws.root());
        assert_eq!(tools.len(), 1);
        let tool = PythonTool::new(tools[0].clone(), ws.root());
        assert_eq!(tool.run(&json!({"a": 2, "b": 40})).unwrap(), "42");
    }

    #[test]
    fn skill_se_guarda_y_aparece_en_el_indice() {
        let ws = crate::workspace_for_tests("ext-skill");
        let create = SkillCreateTool { ws: ws.clone() };
        create
            .run(&json!({
                "name": "revisar_pr",
                "when": "cuando haya que revisar un pull request",
                "body": "1. cargo test\n2. leer el diff"
            }))
            .unwrap();
        let index = skills_index(ws.root());
        assert!(index.contains("revisar_pr"));
        assert!(index.contains("pull request"));
    }

    #[test]
    fn mcp_plantilla_responde_al_handshake() {
        if Command::new(python()).arg("--version").output().is_err() {
            return;
        }
        let ws = crate::workspace_for_tests("ext-mcp");
        let create = McpCreateTool { ws: ws.clone() };
        let message = create.run(&json!({"name": "demo"})).unwrap();
        assert!(message.contains("1 tool"), "{message}");
        let call = McpCallTool { ws: ws.clone() };
        let out = call
            .run(&json!({"server": "demo", "tool": "echo", "arguments": {"text": "hola"}}))
            .unwrap();
        assert!(out.contains("hola"), "{out}");
    }
}
