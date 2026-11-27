//! CLI del agente local: un turno, REPL, meta persistente, agenda y canales
//! (Telegram y voz).

mod agent;
mod browser;
mod daemon_lock;
mod ext;
mod goals;
mod hardware;
mod media;
mod memory;
mod models;
mod orchestra;
mod provider;
mod scheduler;
mod tools;
mod web;

#[cfg(test)]
pub use tools::workspace_for_tests;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::agent::{Agent, AgentOptions};
use crate::daemon_lock::DaemonLock;
use crate::browser::Browser;
use crate::goals::GoalState;
use crate::hardware::Specs;
use crate::memory::Memory;
use crate::models::{Downloads, Role, MAX_DOWNLOADS};
use crate::orchestra::{Limits, Orchestra};
use crate::provider::{Backend, Embedder, Kind};
use crate::scheduler::Scheduler;
use crate::tools::{ToolRegistry, Workspace};
use crate::web::WebState;

const USAGE: &str = "\
local-agent · agente LLM local (Ollama / llama.cpp) con RAG, web, memoria y subagentes

USO
  local-agent [opciones] [petición...]
  local-agent --goal \"objetivo\"        ejecuta un lote y persiste el progreso
  local-agent --goal \"objetivo\" --daemon trabaja por lotes hasta completar
  local-agent --daemon                 reanuda el objetivo activo y la agenda periódica
  local-agent --telegram               atiende Telegram (TELEGRAM_BOT_TOKEN)
  local-agent --voice                  conversación por micrófono y voz
  local-agent --info                   hardware, modelos y estado
  local-agent --pull <modelo>          descarga un modelo (máx. 3 por proyecto)

OPCIONES
  -p, --provider <ollama|llamacpp>   backend (defecto ollama)
  -m, --model <nombre>               modelo de chat (por defecto el mejor para el hardware)
  -w, --workspace <dir>              raíz de ficheros y shell
      --max-steps <n>                pasos por turno (defecto 15)
      --max-depth <n>                niveles de subagentes (defecto 3)
      --max-agents <n>               subagentes por sesión (defecto 12)
      --goal-attempts <n>            intentos de --goal (defecto 8)
      --no-reflect                   desactiva la consolidación (RSI)
      --every <30m> --task \"...\"      añade una tarea a la agenda y sale
      --daemon-once                  ejecuta vencimientos una vez y sale
  -h, --help                         esta ayuda

ENTORNO
  OLLAMA_URL, LLAMACPP_URL, AGENT_EMBED_MODEL, AGENT_BROWSER_PORT,
  AGENT_WEB_MIN_INTERVAL_MS, AGENT_MEMORY_MAX, TELEGRAM_BOT_TOKEN,
  AGENT_PIPER_MODEL, AGENT_WHISPER_MODEL

REPL: /help · /goal <texto> · /model <nombre> · /models · /info · /memory
      /improve · /new · /exit";

#[derive(Debug, Clone)]
struct Config {
    provider: String,
    model: Option<String>,
    workspace: PathBuf,
    max_steps: usize,
    max_depth: usize,
    max_agents: usize,
    goal_attempts: usize,
    reflect: bool,
    info: bool,
    pull: Option<String>,
    goal: Option<String>,
    daemon: bool,
    daemon_once: bool,
    telegram: bool,
    voice: bool,
    every: Option<String>,
    task: Option<String>,
    prompt: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: "ollama".into(),
            model: None,
            workspace: PathBuf::from("."),
            max_steps: 15,
            max_depth: 3,
            max_agents: 12,
            goal_attempts: 8,
            reflect: true,
            info: false,
            pull: None,
            goal: None,
            daemon: false,
            daemon_once: false,
            telegram: false,
            voice: false,
            every: None,
            task: None,
            prompt: Vec::new(),
        }
    }
}

fn parse_args(args: Vec<String>) -> Result<Config> {
    let mut cfg = Config::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let mut value = |name: &str| -> Result<String> {
            iter.next()
                .with_context(|| format!("{name} necesita un valor"))
        };
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "-p" | "--provider" => cfg.provider = value("--provider")?,
            "-m" | "--model" => cfg.model = Some(value("--model")?),
            "-w" | "--workspace" => cfg.workspace = PathBuf::from(value("--workspace")?),
            "--max-steps" => cfg.max_steps = value("--max-steps")?.parse().unwrap_or(15),
            "--max-depth" => cfg.max_depth = value("--max-depth")?.parse().unwrap_or(3),
            "--max-agents" => cfg.max_agents = value("--max-agents")?.parse().unwrap_or(12),
            "--goal-attempts" => cfg.goal_attempts = value("--goal-attempts")?.parse().unwrap_or(8),
            "--no-reflect" => cfg.reflect = false,
            "--info" => cfg.info = true,
            "--pull" => cfg.pull = Some(value("--pull")?),
            "--goal" => cfg.goal = Some(value("--goal")?),
            "--daemon" => cfg.daemon = true,
            "--daemon-once" => {
                cfg.daemon = true;
                cfg.daemon_once = true;
            }
            "--telegram" => cfg.telegram = true,
            "--voice" => cfg.voice = true,
            "--every" => cfg.every = Some(value("--every")?),
            "--task" => cfg.task = Some(value("--task")?),
            other if other.starts_with('-') => bail!("opción desconocida: {other} (usa --help)"),
            other => cfg.prompt.push(other.to_string()),
        }
    }
    Ok(cfg)
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn run() -> Result<()> {
    let cfg = parse_args(std::env::args().skip(1).collect())?;

    let workspace = Workspace::new(&cfg.workspace)?;
    let backend = match cfg.provider.to_lowercase().as_str() {
        "ollama" => Backend::ollama(env_or("OLLAMA_URL", "http://127.0.0.1:11434")),
        "llamacpp" | "llama.cpp" | "llama" => {
            Backend::llamacpp(env_or("LLAMACPP_URL", "http://127.0.0.1:8083"))
        }
        other => bail!("provider desconocido: {other} (usa ollama o llamacpp)"),
    };

    let specs = Specs::detect();
    let models = match backend.kind {
        Kind::Ollama => models::list_ollama(&backend.url),
        Kind::LlamaCpp => models::list_llamacpp(&backend.url),
    };
    if models.is_empty() {
        eprintln!(
            "aviso: no se ven modelos en {} ({}); ¿está el servidor arrancado?",
            backend.url,
            backend.label()
        );
    }

    // Descarga de modelos con el tope de 3 por proyecto.
    if let Some(name) = &cfg.pull {
        if backend.kind != Kind::Ollama {
            bail!("--pull solo funciona con Ollama; en llama.cpp coloca el GGUF a mano");
        }
        let mut ledger = Downloads::open(models::ledger_path(workspace.root()))?;
        if models.iter().any(|m| &m.id == name) {
            println!("'{name}' ya está instalado");
            return Ok(());
        }
        ledger.ensure_slot(name)?;
        println!("descargando {name}…");
        let message = models::pull_ollama(&backend.url, name)?;
        ledger.record(name)?;
        println!("{message} · descargas {}/{MAX_DOWNLOADS}", ledger.used());
        return Ok(());
    }

    let memory_max = std::env::var("AGENT_MEMORY_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500usize);
    let memory = Arc::new(Mutex::new(Memory::open(
        workspace.root().join(".agent").join("memory.json"),
        memory_max,
    )?));

    let embed_model = env_or("AGENT_EMBED_MODEL", "nomic-embed-text");
    let embedder = match backend.kind {
        Kind::Ollama => Embedder::probe(&backend.url, &embed_model),
        Kind::LlamaCpp => None,
    };

    // Rellena vectores pendientes (sesiones que guardaron sin embeddings).
    // Se acumulan y se guardan en un solo paso: un guardado por recuerdo
    // reescribía el fichero entero N veces.
    if let Some(embedder) = &embedder {
        let pending = memory
            .lock()
            .map(|mem| mem.pending_vectors(64))
            .unwrap_or_default();
        let mut filled: Vec<(String, Vec<f32>)> = Vec::new();
        for id in pending {
            let text = memory.lock().ok().and_then(|mem| mem.text_of(&id));
            let Some(text) = text else { continue };
            if let Ok(vector) = embedder.embed(&text) {
                filled.push((id, vector));
            }
        }
        if !filled.is_empty()
            && let Ok(mut mem) = memory.lock()
        {
            let _ = mem.set_vector_many(filled);
        }
    }

    // Contexto asumido por inferencia (AGENT_CONTEXT): entra en el cálculo de
    // slots, porque la KV cache se lleva tanta memoria como los pesos.
    let context_tokens = std::env::var("AGENT_CONTEXT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64 * 1024);

    let goal = Arc::new(GoalState::open(goals::state_path(workspace.root()))?);
    let orchestra = Arc::new(Orchestra::new(
        backend.clone(),
        specs.clone(),
        models.clone(),
        workspace.clone(),
        memory.clone(),
        Arc::new(Mutex::new(None::<Browser>)),
        Arc::new(Mutex::new(WebState::new())),
        embedder.clone(),
        goal.clone(),
        Limits {
            max_depth: cfg.max_depth,
            max_agents: cfg.max_agents,
            steps: cfg.max_steps,
            reflect: cfg.reflect,
            context_tokens,
        },
    ));

    if cfg.info {
        println!("{}", info_block(&orchestra));
        return Ok(());
    }

    // Modelo de chat: nunca un modelo de embeddings (bug de la v0.1).
    let model = match &cfg.model {
        Some(name) => {
            if let Some(found) = models.iter().find(|m| &m.id == name)
                && found.is_embedding()
            {
                bail!(
                    "'{name}' es un modelo de embeddings y no sirve para chat; usa `/models` para ver los válidos"
                );
            }
            name.clone()
        }
        None => {
            let picked = models::pick(&models, Role::Planner, &specs).unwrap_or_default();
            if picked.is_empty() {
                bail!("no hay ningún modelo de chat instalado en {}", backend.url);
            }
            picked
        }
    };

    if cfg.daemon {
        let scheduler = Scheduler::open(scheduler::schedule_path(workspace.root()))?;
        return daemon(
            &orchestra,
            scheduler,
            cfg.daemon_once,
            cfg.goal.as_deref(),
            cfg.goal_attempts,
        );
    }

    if let (Some(every), Some(task)) = (&cfg.every, &cfg.task) {
        let secs = scheduler::parse_interval(every)
            .ok_or_else(|| anyhow::anyhow!("intervalo no válido: {every}"))?;
        let mut sched = Scheduler::open(scheduler::schedule_path(workspace.root()))?;
        let created = sched.add(task, secs, "worker")?;
        println!(
            "tarea {} programada cada {} · ejecútala con --daemon",
            created.id,
            scheduler::human_interval(created.every_secs)
        );
        return Ok(());
    }

    let mut agent = build_agent(&orchestra, &model);
    println!(
        "{} · {} · {} · workspace {}\n{}",
        backend.label(),
        model,
        backend.url,
        workspace.root().display(),
        specs.summary()
    );
    if let Some(embedder) = &embedder {
        println!("RAG semántico: {}", embedder.label());
    }
    if cfg.reflect {
        println!("RSI acotada: activa (memoria versionada, sin tocar código)");
    }

    if cfg.telegram {
        return telegram_loop(&mut agent, workspace.root());
    }
    if cfg.voice {
        return voice_loop(&mut agent);
    }
    if let Some(goal_text) = &cfg.goal {
        let _goal_lock = DaemonLock::acquire(workspace.root())?;
        orchestra.goal.recover_interrupted()?;
        let answer = agent.pursue(goal_text, cfg.goal_attempts)?;
        println!("\n{answer}");
        return Ok(());
    }
    if !cfg.prompt.is_empty() {
        let answer = agent.run(&cfg.prompt.join(" "))?;
        println!("\n{answer}");
        return Ok(());
    }
    repl(&mut agent, &orchestra, &model, cfg.goal_attempts)
}

fn build_agent(orchestra: &Arc<Orchestra>, model: &str) -> Agent {
    let mandate = orchestra.root_mandate();
    let tools = Arc::new(ToolRegistry::for_role(
        orchestra,
        0,
        Role::Worker,
        mandate,
    ));
    Agent::new(
        orchestra.backend.provider(model),
        tools,
        orchestra.system_prompt(0, Role::Worker, mandate),
        AgentOptions {
            memory: Some(orchestra.memory.clone()),
            embedder: orchestra.embedder.clone(),
            reflect: orchestra.limits.reflect,
            reflect_tools: Some(Arc::new(ToolRegistry::memory_only(orchestra))),
            goal: Some(orchestra.goal.clone()),
            depth: 0,
            max_steps: orchestra.limits.steps,
        },
    )
}

fn info_block(orchestra: &Arc<Orchestra>) -> String {
    let mut out = String::new();
    out.push_str(&format!("Hardware: {}\n", orchestra.specs.summary()));
    if let Some(goal) = orchestra.goal.active() {
        out.push_str(&format!(
            "Objetivo: {} · {} · {} ciclos registrados\n",
            goal.id,
            match goal.status {
                goals::GoalStatus::Active => "activo",
                goals::GoalStatus::Paused => "pausado por estancamiento",
                goals::GoalStatus::Completed => "completado",
            },
            goal.attempt_count
        ));
        out.push_str(&format!("  {}\n", memory::truncate(&goal.goal, 180)));
        if let Some(reason) = &goal.pause_reason {
            out.push_str(&format!("  Motivo de pausa: {}\\n", memory::truncate(reason, 160)));
        }
        let pending = goal.tasks.iter().filter(|task| task.status == goals::TaskStatus::Pending).count();
        let in_progress = goal.tasks.iter().filter(|task| task.status == goals::TaskStatus::InProgress).count();
        let completed = goal.tasks.iter().filter(|task| task.status == goals::TaskStatus::Completed).count();
        let verified = goal.tasks.iter().filter(|task| task.verified).count();
        let unknown_ops = goal.operations.iter().filter(|op| op.status == goals::OperationStatus::Unknown).count();
        let started_ops = goal.operations.iter().filter(|op| op.status == goals::OperationStatus::Started).count();
        out.push_str(&format!("  Plan: {} tareas · {} pendientes · {} en curso · {} completadas · {} verificadas\\n",
            goal.tasks.len(), pending, in_progress, completed, verified));
        out.push_str(&format!("  Journal: {} operaciones · {} resultado desconocido · {} en ejecución\\n",
            goal.operations.len(), unknown_ops, started_ops));
    }
    out.push_str(&format!(
        "Backend: {} · {}\n",
        orchestra.backend.label(),
        orchestra.backend.url
    ));
    out.push_str("Modelos instalados:\n");
    for model in &orchestra.models {
        let kind = if model.is_embedding() { "embeddings" } else { "chat" };
        out.push_str(&format!(
            "  - {} ({:.1}B, {:.2} GB, {kind})\n",
            model.id, model.params_b, model.size_gb
        ));
    }
    for role in [Role::Planner, Role::Worker, Role::Coder, Role::Fast] {
        out.push_str(&format!("  {} → {}\n", role.as_str(), orchestra.model_for(role)));
    }
    let modelo_worker = orchestra.model_for(Role::Worker);
    out.push_str(&format!(
        "Subagentes: {} hueco(s) ahora para {modelo_worker} con {}k de contexto \
         (tope estático {}), {} por sesión, profundidad {}\n",
        orchestra.slots_for(&modelo_worker),
        orchestra.limits.context_tokens / 1024,
        orchestra.parallel,
        orchestra.limits.max_agents,
        orchestra.limits.max_depth
    ));
    if let Some(embedder) = &orchestra.embedder {
        out.push_str(&format!("Embeddings: {}\n", embedder.label()));
    }
    if let Ok(memory) = orchestra.memory.lock() {
        out.push_str(&format!(
            "Memoria: {} recuerdos · {} versiones archivadas · {}\n",
            memory.len(),
            memory.superseded_count(),
            memory.path().display()
        ));
    }
    if let Ok(sched) = Scheduler::open(scheduler::schedule_path(orchestra.workspace.root())) {
        out.push_str(&format!("Agenda: {} tarea(s)\n", sched.tasks().len()));
        for task in sched.tasks() {
            out.push_str(&format!(
                "  - {} cada {} ({} ejec.): {}\n",
                task.id,
                scheduler::human_interval(task.every_secs),
                task.runs,
                task.goal
            ));
        }
    }
    if let Ok(downloads) = Downloads::open(orchestra.downloads.clone()) {
        out.push_str(&format!(
            "Descargas de modelos: {}/{} (quedan {}) {:?}\n",
            downloads.used(),
            MAX_DOWNLOADS,
            downloads.remaining(),
            downloads.pulled()
        ));
    }
    out
}

fn daemon(
    orchestra: &Arc<Orchestra>,
    mut scheduler: Scheduler,
    once: bool,
    initial_goal: Option<&str>,
    goal_attempts: usize,
) -> Result<()> {
    // Impide que dos procesos compitan por el mismo objetivo persistente.
    let _daemon_lock = DaemonLock::acquire(orchestra.workspace.root())?;
    orchestra.goal.recover_interrupted()?;
    if let Some(goal) = initial_goal {
        orchestra.goal.start(goal)?;
    }
    let worker_model = orchestra.model_for(Role::Worker);
    let has_active_goal = initial_goal.is_some() || orchestra.goal.active().is_some();
    if has_active_goal && worker_model.is_empty() {
        bail!("no hay un modelo de chat instalado en {}; no se inicia el worker persistente", orchestra.backend.label());
    }
    let mut goal_agent = if has_active_goal {
        Some(build_agent(orchestra, &worker_model))
    } else {
        None
    };
    println!(
        "daemon · agenda {} · {} tarea(s) · objetivo persistente: {} · {} subagente(s) en paralelo",
        scheduler.path().display(),
        scheduler.tasks().len(),
        if goal_agent.is_some() { "activo/reanudable" } else { "sin objetivo activo" },
        orchestra.parallel
    );
    loop {
        let now = scheduler::now();

        // Si hay una misión raíz activa, el worker la continúa en lotes acotados.
        // Cada lote persiste sus ciclos; al reiniciar el daemon se reanuda desde disco.
        if let Some(active) = orchestra.goal.active()
            && active.status == goals::GoalStatus::Active
        {
            let agent = goal_agent.get_or_insert_with(|| build_agent(orchestra, &worker_model));
            let started = Instant::now();
            let result = agent.pursue(&active.goal, goal_attempts);
            let body = match result {
                Ok(text) => format!("OBJETIVO · {:.1}s\n{text}\n", started.elapsed().as_secs_f64()),
                Err(err) => format!("OBJETIVO ERROR · {:.1}s\n{err:#}\n", started.elapsed().as_secs_f64()),
            };
            let dir = scheduler::runs_dir(orchestra.workspace.root());
            std::fs::create_dir_all(&dir)?;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("goal.log"))?;
            writeln!(file, "--- {} ---\n{body}", scheduler::now())?;
            println!("{body}");
        }

        for task in scheduler.due(now) {
            println!(
                "▶ {} · {} · cada {}",
                task.id,
                memory::truncate(&task.goal, 90),
                scheduler::human_interval(task.every_secs)
            );
            let started = Instant::now();
            let result = orchestra.spawn(
                1,
                &task.role,
                &task.goal,
                "Tarea programada.",
                orchestra.root_mandate(),
            );
            let body = match result {
                Ok(text) => format!("OK ({:.1}s)\n{text}\n", started.elapsed().as_secs_f64()),
                Err(err) => format!("ERROR ({:.1}s)\n{err:#}\n", started.elapsed().as_secs_f64()),
            };
            let dir = scheduler::runs_dir(orchestra.workspace.root());
            std::fs::create_dir_all(&dir)?;
            let log = dir.join(format!("{}.log", task.id));
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)?;
            let stamp = now / 60;
            write!(file, "--- {stamp} ---\n{body}")?;
            scheduler.mark_run(&task.id, now)?;
            println!("{body}");
        }
        if once {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(20));
    }
}

fn telegram_loop(agent: &mut Agent, root: &Path) -> Result<()> {
    let token = media::telegram_token()?;
    let mut offset = media::telegram_offset(root);
    println!("Telegram: escuchando (offset {offset}). Ctrl-C para salir.");
    loop {
        match media::telegram_updates(&token, offset) {
            Ok(updates) => {
                for update in updates {
                    offset = update.update_id + 1;
                    media::telegram_save_offset(root, offset)?;
                    let (Some(chat), Some(text)) = (update.chat, update.text) else {
                        continue;
                    };
                    println!("📨 {chat}: {text}");
                    let reply = match agent.run(&text) {
                        Ok(reply) => reply,
                        Err(err) => format!("error: {err:#}"),
                    };
                    media::telegram_send(&token, chat, &reply)?;
                }
            }
            Err(err) => eprintln!("aviso Telegram: {err:#}"),
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn voice_loop(agent: &mut Agent) -> Result<()> {
    println!("Modo voz: habla cuando veas 🎤 (Ctrl-C para salir).");
    loop {
        print!("🎤 escuchando 6s… ");
        io::stdout().flush()?;
        let heard = match media::voice_listen(6) {
            Ok(text) => text,
            Err(err) => {
                eprintln!("\nvoz no disponible: {err:#}");
                return Ok(());
            }
        };
        if heard.trim().is_empty() {
            println!("(nada)");
            continue;
        }
        println!("\n🗣 {heard}");
        let reply = agent.run(&heard)?;
        println!("🤖 {reply}");
        if let Err(err) = media::voice_say(&reply) {
            eprintln!("(no se pudo hablar: {err:#})");
        }
    }
}

fn repl(
    agent: &mut Agent,
    orchestra: &Arc<Orchestra>,
    initial_model: &str,
    goal_attempts: usize,
) -> Result<()> {
    println!(
        "\n› escribe tu petición ({}) · /help para comandos · herramientas: {}",
        initial_model,
        agent.tools().join(", ")
    );
    loop {
        print!("› ");
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(goal) = line.strip_prefix("/goal ") {
            match DaemonLock::acquire(orchestra.workspace.root()) {
                Ok(_goal_lock) => {
                    if let Err(err) = orchestra.goal.recover_interrupted() {
                        eprintln!("error: {err:#}");
                        continue;
                    }
                    match agent.pursue(goal.trim(), goal_attempts) {
                        Ok(answer) => println!("\\n{answer}\\n"),
                        Err(err) => eprintln!("error: {err:#}"),
                    }
                }
                Err(err) => eprintln!("error: {err:#}"),
            }
            continue;
        }
        if let Some(name) = line.strip_prefix("/model ") {
            let name = name.trim();
            if let Some(found) = orchestra.models.iter().find(|m| m.id == name)
                && found.is_embedding()
            {
                eprintln!("'{name}' es de embeddings, no sirve para chat");
                continue;
            }
            agent.set_provider(orchestra.backend.provider(name));
            println!("modelo activo: {name}");
            continue;
        }
        match line {
            "/exit" | "/quit" | "/salir" => break,
            "/help" | "/ayuda" => println!("{USAGE}"),
            "/new" | "/nuevo" => {
                agent.reset();
                println!("historial limpio");
            }
            "/info" => println!("{}", info_block(orchestra)),
            "/models" | "/modelos" => {
                for m in &orchestra.models {
                    let kind = if m.is_embedding() { "embeddings" } else { "chat" };
                    println!("  {} ({:.1}B, {kind})", m.id, m.params_b);
                }
            }
            "/memory" | "/memoria" => match orchestra.memory.lock() {
                Ok(memory) => {
                    for entry in memory.list(None, 20) {
                        println!(
                            "  {} [{}·v{}] {}",
                            entry.id,
                            entry.kind,
                            entry.version,
                            memory::truncate(&entry.text, 90)
                        );
                    }
                }
                Err(_) => eprintln!("memoria no disponible"),
            },
            "/improve" | "/mejorar" => {
                if let Err(err) = agent.improve() {
                    eprintln!("error: {err:#}");
                }
            }
            other => match agent.run(other) {
                Ok(answer) => println!("\n{answer}\n"),
                Err(err) => eprintln!("error: {err:#}"),
            },
        }
    }
    Ok(())
}
