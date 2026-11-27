//! Integraciones locales: reproducción de audio/vídeo, control del escritorio,
//! correo con `himalaya`, Telegram y voz (STT/TTS local).
//!
//! Todo se apoya en programas del sistema (KISS): si falta uno, la herramienta
//! responde con un mensaje claro de qué instalar en vez de fallar en silencio.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::tools::Tool;

// ---------------------------------------------------------------------------
// Utilidades
// ---------------------------------------------------------------------------

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| candidate.is_file())
}

fn need(bin: &str, hint: &str) -> Result<String> {
    which(bin)
        .map(|path| path.display().to_string())
        .ok_or_else(|| anyhow!("falta `{bin}`: {hint}"))
}

/// Ejecuta un binario y devuelve stdout+stderr recortado.
fn run_bin(bin: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("no se pudo ejecutar {bin}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let mut text = stdout;
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&stderr);
    }
    if !output.status.success() && text.is_empty() {
        bail!("{bin} falló con {}\n{text}", output.status);
    }
    if text.is_empty() {
        text = format!("{bin}: sin salida ({})", output.status);
    }
    Ok(text)
}

/// Lanza algo en segundo plano sin bloquear el turno (música, vídeo).
fn spawn_detached(bin: &str, args: &[String]) -> Result<()> {
    Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("no se pudo lanzar {bin}"))?;
    Ok(())
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>() + "…"
}

// ---------------------------------------------------------------------------
// Música y cine (MPRIS + mpv)
// ---------------------------------------------------------------------------

pub struct MediaTool;

impl Tool for MediaTool {
    fn name(&self) -> &str {
        "media"
    }
    fn description(&self) -> &str {
        "Controla la reproducción: status|play|pause|toggle|next|previous|stop|volume|open (abre audio/vídeo con mpv)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["status","play","pause","toggle","next","previous","stop","volume","open"]},
                "path": {"type": "string", "description": "Fichero o URL para action=open"},
                "volume": {"type": "number", "description": "0.0 - 1.0 para action=volume"}
            },
            "required": ["action"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let action = args["action"].as_str().unwrap_or("status");
        match action {
            "open" => {
                let target = args["path"]
                    .as_str()
                    .filter(|p| !p.trim().is_empty())
                    .ok_or_else(|| anyhow!("action=open necesita `path` (fichero o URL)"))?;
                if which("mpv").is_some() {
                    spawn_detached(
                        "mpv",
                        &[
                            "--no-terminal".to_string(),
                            "--force-window=yes".to_string(),
                            target.to_string(),
                        ],
                    )?;
                    Ok(format!("reproduciendo con mpv: {target}"))
                } else if which("vlc").is_some() {
                    spawn_detached(
                        "vlc",
                        &[
                            "--play-and-exit".to_string(),
                            "--no-video-title-show".to_string(),
                            target.to_string(),
                        ],
                    )?;
                    Ok(format!("reproduciendo con vlc: {target}"))
                } else {
                    bail!("instala mpv o vlc para reproducir audio/vídeo")
                }
            }
            "status" => {
                let playerctl = need("playerctl", "sudo apt install playerctl")?;
                let _ = playerctl;
                let status = run_bin("playerctl", &["status"]).unwrap_or_else(|_| "Stopped".into());
                let meta = run_bin(
                    "playerctl",
                    &["metadata", "--format", "{{artist}} — {{title}} ({{album}})"],
                )
                .unwrap_or_default();
                Ok(format!("{status}\n{meta}"))
            }
            "volume" => {
                let volume = args["volume"].as_f64().unwrap_or(0.5).clamp(0.0, 1.0);
                need("playerctl", "sudo apt install playerctl")?;
                run_bin("playerctl", &["volume", &format!("{volume:.2}")])
            }
            other => {
                need("playerctl", "sudo apt install playerctl")?;
                let cmd = match other {
                    "play" => "play",
                    "pause" => "pause",
                    "toggle" => "play-pause",
                    "next" => "next",
                    "previous" => "previous",
                    "stop" => "stop",
                    _ => bail!("acción desconocida: {other}"),
                };
                run_bin("playerctl", &[cmd])
            }
        }
    }
}

// ---------------------------------------------------------------------------
// GUI del escritorio (xdotool / wmctrl)
// ---------------------------------------------------------------------------

pub struct GuiTool;

impl Tool for GuiTool {
    fn name(&self) -> &str {
        "gui"
    }
    fn description(&self) -> &str {
        "Controla el escritorio X11: windows|focus|click|move|type|key|scroll|screenshot."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["windows","focus","click","move","type","key","scroll","screenshot"]},
                "x": {"type": "integer"},
                "y": {"type": "integer"},
                "text": {"type": "string"},
                "title": {"type": "string", "description": "Título/ventana para focus"},
                "keys": {"type": "string", "description": "p. ej. 'ctrl+s' o 'Return'"},
                "direction": {"type": "string", "enum": ["up","down"]},
                "amount": {"type": "integer"}
            },
            "required": ["action"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let action = args["action"].as_str().unwrap_or("windows");
        let x = args["x"].as_i64().unwrap_or(0).to_string();
        let y = args["y"].as_i64().unwrap_or(0).to_string();
        match action {
            "windows" => {
                if which("wmctrl").is_some() {
                    run_bin("wmctrl", &["-l"])
                } else {
                    need("xdotool", "sudo apt install xdotool")?;
                    run_bin(
                        "xdotool",
                        &[
                            "search",
                            "--onlyvisible",
                            "--name",
                            "",
                            "getwindowname",
                            "%@",
                        ],
                    )
                }
            }
            "focus" => {
                let title = args["title"]
                    .as_str()
                    .ok_or_else(|| anyhow!("action=focus necesita `title`"))?;
                if which("wmctrl").is_some() {
                    run_bin("wmctrl", &["-a", title])
                } else {
                    need("xdotool", "sudo apt install xdotool")?;
                    run_bin(
                        "xdotool",
                        &["search", "--name", title, "windowactivate", "%@"],
                    )
                }
            }
            "move" => {
                need("xdotool", "sudo apt install xdotool")?;
                run_bin("xdotool", &["mousemove", &x, &y])
            }
            "click" => {
                need("xdotool", "sudo apt install xdotool")?;
                if args["x"].is_i64() || args["y"].is_i64() {
                    run_bin("xdotool", &["mousemove", &x, &y])?;
                }
                run_bin("xdotool", &["click", "1"])
            }
            "type" => {
                let text = args["text"]
                    .as_str()
                    .ok_or_else(|| anyhow!("action=type necesita `text`"))?;
                need("xdotool", "sudo apt install xdotool")?;
                run_bin(
                    "xdotool",
                    &["type", "--clearmodifiers", "--delay", "30", text],
                )
            }
            "key" => {
                let keys = args["keys"]
                    .as_str()
                    .ok_or_else(|| anyhow!("action=key necesita `keys`"))?;
                need("xdotool", "sudo apt install xdotool")?;
                run_bin("xdotool", &["key", "--clearmodifiers", keys])
            }
            "scroll" => {
                let button = match args["direction"].as_str().unwrap_or("down") {
                    "up" => "4",
                    _ => "5",
                };
                let amount = args["amount"].as_u64().unwrap_or(3).clamp(1, 30);
                need("xdotool", "sudo apt install xdotool")?;
                for _ in 0..amount {
                    run_bin("xdotool", &["click", button])?;
                }
                Ok(format!("scroll {button} × {amount}"))
            }
            "screenshot" => {
                let dir = std::env::temp_dir();
                let path = dir.join(format!("agent-gui-{}.png", std::process::id()));
                let path_text = path.display().to_string();
                if which("scrot").is_some() {
                    run_bin("scrot", &["-o", &path_text])?;
                } else if which("gnome-screenshot").is_some() {
                    run_bin("gnome-screenshot", &["-f", &path_text])?;
                } else if which("ffmpeg").is_some() {
                    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
                    run_bin(
                        "ffmpeg",
                        &[
                            "-loglevel",
                            "error",
                            "-y",
                            "-f",
                            "x11grab",
                            "-i",
                            &format!("{display}.0"),
                            "-frames:v",
                            "1",
                            &path_text,
                        ],
                    )?;
                } else {
                    bail!("instala scrot o ffmpeg para capturas");
                }
                Ok(format!("captura en {path_text}"))
            }
            other => bail!("acción desconocida: {other}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Correo (himalaya)
// ---------------------------------------------------------------------------

pub struct MailTool;

impl Tool for MailTool {
    fn name(&self) -> &str {
        "mail"
    }
    fn description(&self) -> &str {
        "Correo con himalaya: folders|list|read|send."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["folders","list","read","send"]},
                "id": {"type": "string", "description": "Id del mensaje para read"},
                "to": {"type": "string"},
                "subject": {"type": "string"},
                "body": {"type": "string"},
                "folder": {"type": "string", "description": "Carpeta (defecto INBOX)"}
            },
            "required": ["action"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let action = args["action"].as_str().unwrap_or("list");
        need(
            "himalaya",
            "instala himalaya (`cargo install himalaya`) y configura ~/.config/himalaya/config.toml",
        )?;
        match action {
            "folders" => run_bin("himalaya", &["folder", "list"]),
            "list" => {
                let folder = args["folder"].as_str().unwrap_or("INBOX");
                run_bin("himalaya", &["envelope", "list", "-f", folder, "-s", "10"])
            }
            "read" => {
                let id = args["id"]
                    .as_str()
                    .ok_or_else(|| anyhow!("action=read necesita `id`"))?;
                run_bin("himalaya", &["message", "read", id])
            }
            "send" => {
                let to = args["to"]
                    .as_str()
                    .ok_or_else(|| anyhow!("action=send necesita `to`"))?;
                let subject = args["subject"].as_str().unwrap_or("(sin asunto)");
                let body = args["body"].as_str().unwrap_or("");
                let message = format!("To: {to}\nSubject: {subject}\n\n{body}\n");
                let mut child = Command::new("himalaya")
                    .args(["message", "send"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .context("no se pudo lanzar himalaya")?;
                if let Some(stdin) = child.stdin.as_mut() {
                    stdin.write_all(message.as_bytes())?;
                }
                let output = child.wait_with_output()?;
                let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !output.status.success() {
                    bail!(
                        "himalaya no pudo enviar: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
                Ok(if text.is_empty() {
                    format!("correo enviado a {to}")
                } else {
                    text
                })
            }
            other => bail!("acción desconocida: {other}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Telegram (Bot API vía curl: sin TLS propio)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub update_id: i64,
    pub chat: Option<i64>,
    pub text: Option<String>,
}

pub fn telegram_token() -> Result<String> {
    std::env::var("TELEGRAM_BOT_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            anyhow!("define TELEGRAM_BOT_TOKEN (créalo con @BotFather) para usar Telegram")
        })
}

fn telegram_call(token: &str, method: &str, params: &[(&str, String)]) -> Result<Value> {
    need("curl", "instala curl para hablar con la API de Telegram")?;
    let url = format!("https://api.telegram.org/bot{token}/{method}");
    let mut args: Vec<String> = vec![
        "-s".into(),
        "-m".into(),
        "40".into(),
        "-X".into(),
        "POST".into(),
        url,
    ];
    for (key, value) in params {
        args.push("--data-urlencode".into());
        args.push(format!("{key}={value}"));
    }
    let output = Command::new("curl")
        .args(&args)
        .output()
        .context("no se pudo ejecutar curl")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("respuesta no-JSON de Telegram: {}", clip(&text, 200)))?;
    if value["ok"] != json!(true) {
        bail!(
            "Telegram: {}",
            value["description"].as_str().unwrap_or("error desconocido")
        );
    }
    Ok(value)
}

pub fn telegram_send(token: &str, chat: i64, text: &str) -> Result<()> {
    telegram_call(
        token,
        "sendMessage",
        &[("chat_id", chat.to_string()), ("text", clip(text, 4000))],
    )?;
    Ok(())
}

pub fn telegram_updates(token: &str, offset: i64) -> Result<Vec<Update>> {
    let value = telegram_call(
        token,
        "getUpdates",
        &[
            ("offset", offset.to_string()),
            ("timeout", "0".to_string()),
            ("allowed_updates", "[\"message\"]".to_string()),
        ],
    )?;
    Ok(parse_updates(&value))
}

pub fn parse_updates(value: &Value) -> Vec<Update> {
    let Some(items) = value["result"].as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let update_id = item["update_id"].as_i64()?;
            let message = &item["message"];
            let chat = message["chat"]["id"].as_i64();
            let text = message["text"].as_str().map(|s| s.to_string());
            Some(Update {
                update_id,
                chat,
                text,
            })
        })
        .collect()
}

pub fn telegram_offset_path(root: &Path) -> PathBuf {
    root.join(".agent").join("telegram.json")
}

pub fn telegram_offset(root: &Path) -> i64 {
    let path = telegram_offset_path(root);
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value["offset"].as_i64())
        .unwrap_or(0)
}

pub fn telegram_save_offset(root: &Path, offset: i64) -> Result<()> {
    let path = telegram_offset_path(root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(&json!({"offset": offset}))?)?;
    Ok(())
}

pub struct TelegramTool;

impl Tool for TelegramTool {
    fn name(&self) -> &str {
        "telegram"
    }
    fn description(&self) -> &str {
        "Envía un mensaje de Telegram (action=send) o lee los últimos mensajes recibidos (action=poll)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["send", "poll"]},
                "chat_id": {"type": "integer"},
                "text": {"type": "string"}
            },
            "required": ["action"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        let token = telegram_token()?;
        match args["action"].as_str().unwrap_or("poll") {
            "send" => {
                let chat = args["chat_id"]
                    .as_i64()
                    .ok_or_else(|| anyhow!("action=send necesita `chat_id`"))?;
                let text = args["text"]
                    .as_str()
                    .ok_or_else(|| anyhow!("action=send necesita `text`"))?;
                telegram_send(&token, chat, text)?;
                Ok(format!("mensaje enviado a {chat}"))
            }
            "poll" => {
                let updates = telegram_updates(&token, 0)?;
                if updates.is_empty() {
                    return Ok("sin mensajes nuevos".to_string());
                }
                Ok(updates
                    .iter()
                    .map(|u| {
                        format!(
                            "#{} chat={:?}: {}",
                            u.update_id,
                            u.chat,
                            u.text.clone().unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            other => bail!("acción desconocida: {other}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Voz: STT con whisper.cpp y TTS femenina con Piper
// ---------------------------------------------------------------------------

fn find_model(env_key: &str, dirs: &[&str], ext: &str) -> Option<PathBuf> {
    if let Ok(path) = std::env::var(env_key) {
        let path = PathBuf::from(path.trim());
        if path.is_file() {
            return Some(path);
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    for dir in dirs {
        let dir = PathBuf::from(dir);
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut found: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().map(|e| e == ext).unwrap_or(false))
                .collect();
            found.sort();
            if let Some(first) = found.into_iter().next() {
                return Some(first);
            }
        }
        if let Some(home) = &home {
            let alt = home.join(dir);
            if let Ok(entries) = std::fs::read_dir(alt) {
                let mut found: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().map(|e| e == ext).unwrap_or(false))
                    .collect();
                found.sort();
                if let Some(first) = found.into_iter().next() {
                    return Some(first);
                }
            }
        }
    }
    None
}

/// Texto → voz. Prioriza Piper (voces neuronales femeninas), luego espeak-ng.
pub fn voice_say(text: &str) -> Result<()> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(());
    }
    if which("piper").is_some() {
        let model = find_model(
            "AGENT_PIPER_MODEL",
            &[
                "models/piper",
                ".local/share/piper",
                "/usr/share/piper",
                "piper",
            ],
            "onnx",
        );
        if let Some(model) = model {
            let wav = std::env::temp_dir().join(format!("agent-tts-{}.wav", std::process::id()));
            let mut child = Command::new("piper")
                .arg("--model")
                .arg(&model)
                .arg("--output_file")
                .arg(&wav)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("no se pudo lanzar piper")?;
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(text.as_bytes())?;
            }
            let status = child.wait()?;
            if status.success() {
                let player = if which("paplay").is_some() {
                    "paplay"
                } else if which("aplay").is_some() {
                    "aplay"
                } else {
                    bail!("instala paplay (pulseaudio) o aplay (alsa) para reproducir la voz");
                };
                run_bin(player, &[&wav.display().to_string()])?;
                let _ = std::fs::remove_file(&wav);
                return Ok(());
            }
        }
    }
    if which("espeak-ng").is_some() {
        // Voz femenina en español (variante 3 es la más natural de espeak).
        return run_bin("espeak-ng", &["-v", "es+f3", "-s", "165", "-p", "60", text]).map(|_| ());
    }
    bail!(
        "no hay TTS instalado: instala piper (`pip install piper-tts`) con una voz es_ES femenina \
         (define AGENT_PIPER_MODEL=/ruta/voz.onnx) o `espeak-ng`"
    )
}

/// Micrófono → texto con whisper.cpp (o OpenAI whisper si está instalado).
pub fn voice_listen(seconds: u64) -> Result<String> {
    need(
        "arecord",
        "instala alsa-utils (`sudo apt install alsa-utils`)",
    )?;
    let wav = std::env::temp_dir().join(format!("agent-stt-{}.wav", std::process::id()));
    let wav_text = wav.display().to_string();
    let secs = seconds.clamp(1, 60).to_string();
    run_bin(
        "arecord",
        &[
            "-q", "-f", "S16_LE", "-r", "16000", "-c", "1", "-d", &secs, "-t", "wav", &wav_text,
        ],
    )?;

    let model = find_model(
        "AGENT_WHISPER_MODEL",
        &[
            "models/whisper",
            ".local/share/whisper",
            "/usr/share/whisper",
        ],
        "bin",
    );

    let text = if which("whisper-cli").is_some() {
        let model = model.ok_or_else(|| {
            anyhow!("define AGENT_WHISPER_MODEL=/ruta/ggml-base.bin para whisper-cli")
        })?;
        run_bin(
            "whisper-cli",
            &[
                "-m",
                &model.display().to_string(),
                "-f",
                &wav_text,
                "-l",
                "es",
                "-nt",
            ],
        )?
    } else if which("whisper").is_some() {
        run_bin(
            "whisper",
            &[
                &wav_text,
                "--model",
                "base",
                "--language",
                "es",
                "--output_format",
                "txt",
            ],
        )?
    } else {
        bail!(
            "no hay STT instalado: usa whisper.cpp (`whisper-cli` + ggml-base.bin, \
             define AGENT_WHISPER_MODEL) o `pip install openai-whisper`"
        );
    };
    let _ = std::fs::remove_file(&wav);
    Ok(text
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty() && !line.starts_with('['))
        .collect::<Vec<_>>()
        .join(" "))
}

pub struct VoiceTool;

impl Tool for VoiceTool {
    fn name(&self) -> &str {
        "voice"
    }
    fn description(&self) -> &str {
        "Voz local: action=say (texto→voz femenina) o action=listen (micrófono→texto)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["say", "listen"]},
                "text": {"type": "string"},
                "seconds": {"type": "integer", "description": "Duración de escucha (defecto 6)"}
            },
            "required": ["action"]
        })
    }
    fn run(&self, args: &Value) -> Result<String> {
        match args["action"].as_str().unwrap_or("say") {
            "say" => {
                let text = args["text"]
                    .as_str()
                    .ok_or_else(|| anyhow!("action=say necesita `text`"))?;
                voice_say(text)?;
                Ok("dicho en voz alta".to_string())
            }
            "listen" => {
                let seconds = args["seconds"].as_u64().unwrap_or(6);
                let text = voice_listen(seconds)?;
                if text.trim().is_empty() {
                    Ok("(no se entendió nada)".to_string())
                } else {
                    Ok(text)
                }
            }
            other => bail!("acción desconocida: {other}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsea_updates_de_telegram() {
        let value = json!({
            "ok": true,
            "result": [
                {"update_id": 10, "message": {"chat": {"id": 42}, "text": "hola"}},
                {"update_id": 11, "edited_message": {"chat": {"id": 42}}}
            ]
        });
        let updates = parse_updates(&value);
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].update_id, 10);
        assert_eq!(updates[0].chat, Some(42));
        assert_eq!(updates[0].text.as_deref(), Some("hola"));
        assert!(updates[1].text.is_none());
    }

    #[test]
    fn recorta_mensajes_largos() {
        assert_eq!(clip("abc", 10), "abc");
        assert!(clip(&"x".repeat(50), 10).ends_with('…'));
    }

    #[test]
    fn binarios_ausentes_dan_error_claro() {
        let err = need("binario-que-no-existe-xyz", "instálalo").unwrap_err();
        assert!(err.to_string().contains("instálalo"));
    }
}
