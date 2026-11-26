//! Orquestación de subagentes en varios niveles.
//!
//! Cada subagente es un `Agent` completo con su proveedor, su modelo (elegido
//! por rol y por el hardware medido) y un juego de herramientas adecuado a su
//! papel. La recursión es real: un subagente puede delegar a su vez, hasta
//! `max_depth`, con un presupuesto total de agentes y un límite de ejecución
//! simultánea calculado a partir de la máquina.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::agent::{Agent, AgentOptions};
use crate::browser::Browser;
use crate::goals::GoalState;
use crate::hardware::Specs;
use crate::memory::Memory;
use crate::models::{self, Model, Role};
use crate::provider::{Backend, Embedder};
use crate::tools::{ToolRegistry, Workspace};
use crate::web::WebState;

/// Límites de la orquestación, fijados en el arranque.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_depth: usize,
    pub max_agents: usize,
    pub steps: usize,
    pub reflect: bool,
}

pub struct Orchestra {
    pub backend: Backend,
    pub specs: Specs,
    pub models: Vec<Model>,
    pub workspace: Workspace,
    pub memory: Arc<Mutex<Memory>>,
    pub browser: Arc<Mutex<Option<Browser>>>,
    pub web: Arc<Mutex<WebState>>,
    pub embedder: Option<Embedder>,
    pub goal: Arc<GoalState>,
    pub downloads: std::path::PathBuf,
    pub limits: Limits,
    /// Subagentes simultáneos permitidos por este hardware.
    pub parallel: usize,
    budget: AtomicUsize,
    running: AtomicUsize,
}

impl Orchestra {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backend: Backend,
        specs: Specs,
        models: Vec<Model>,
        workspace: Workspace,
        memory: Arc<Mutex<Memory>>,
        browser: Arc<Mutex<Option<Browser>>>,
        web: Arc<Mutex<WebState>>,
        embedder: Option<Embedder>,
        goal: Arc<GoalState>,
        limits: Limits,
    ) -> Self {
        let parallel = specs.max_parallel();
        let downloads = models::ledger_path(workspace.root());
        Self {
            backend,
            specs,
            models,
            workspace,
            memory,
            browser,
            web,
            embedder,
            goal,
            downloads,
            limits,
            parallel,
            budget: AtomicUsize::new(limits.max_agents),
            running: AtomicUsize::new(0),
        }
    }

    pub fn agents_left(&self) -> usize {
        self.budget.load(Ordering::SeqCst)
    }

    pub fn running(&self) -> usize {
        self.running.load(Ordering::SeqCst)
    }

    /// Modelo elegido para un rol según hardware y catálogo.
    pub fn model_for(&self, role: Role) -> String {
        models::pick(&self.models, role, &self.specs)
            .or_else(|| {
                self.models
                    .iter()
                    .find(|m| !m.is_embedding())
                    .map(|m| m.id.clone())
            })
            .unwrap_or_default()
    }

    /// Reparte trabajo: crea un subagente, lo ejecuta y devuelve su respuesta.
    pub fn spawn(
        self: &Arc<Self>,
        depth: usize,
        role_name: &str,
        goal: &str,
        context: &str,
    ) -> Result<String> {
        if goal.trim().is_empty() {
            bail!("el objetivo del subagente está vacío");
        }
        if depth > self.limits.max_depth {
            bail!(
                "profundidad máxima {} alcanzada; resuelve esta parte tú mismo",
                self.limits.max_depth
            );
        }
        if self
            .budget
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1))
            .is_err()
        {
            bail!(
                "presupuesto de subagentes agotado (máx {}); termina con lo que tengas",
                self.limits.max_agents
            );
        }

        self.acquire_slot()?;
        let started = Instant::now();
        let result = self.run_agent(depth, role_name, goal, context);
        self.running.fetch_sub(1, Ordering::SeqCst);
        match &result {
            Ok(_) => eprintln!(
                "  ✓ nivel {depth} ({role_name}) · {:.1}s",
                started.elapsed().as_secs_f64()
            ),
            Err(err) => eprintln!("  ✗ nivel {depth} ({role_name}): {err:#}"),
        }
        result
    }

    /// Espera a que haya hueco según la memoria disponible del equipo.
    fn acquire_slot(&self) -> Result<()> {
        let start = Instant::now();
        loop {
            let current = self.running.load(Ordering::SeqCst);
            if current < self.parallel
                && self
                    .running
                    .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                return Ok(());
            }
            if start.elapsed() > Duration::from_secs(900) {
                bail!("sin hueco de ejecución tras 15 minutos (¿otro agente atascado?)");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn run_agent(
        self: &Arc<Self>,
        depth: usize,
        role_name: &str,
        goal: &str,
        context: &str,
    ) -> Result<String> {
        let role = Role::parse(role_name).unwrap_or(Role::Worker);
        let model = self.model_for(role);
        if model.is_empty() {
            bail!("no hay modelos instalados en el backend {}", self.backend.label());
        }
        eprintln!(
            "  ↳ nivel {depth} · rol {} · modelo {model} · {} agente(s) libres",
            role.as_str(),
            self.agents_left()
        );

        let tools = Arc::new(ToolRegistry::for_role(self, depth, role));
        let mut agent = Agent::new(
            self.backend.provider(&model),
            tools,
            self.system_prompt(depth, role),
            AgentOptions {
                memory: Some(self.memory.clone()),
                embedder: self.embedder.clone(),
                reflect: self.limits.reflect,
                reflect_tools: Some(Arc::new(ToolRegistry::memory_only(self))),
                goal: Some(self.goal.clone()),
                depth,
                max_steps: self.limits.steps,
            },
        );

        let input = if context.trim().is_empty() {
            goal.to_string()
        } else {
            format!("Contexto del padre:\n{}\n\nObjetivo:\n{goal}", context.trim())
        };
        agent.run(&input)
    }

    /// Prompt de sistema: hardware real, rol, límites y reglas de honestidad.
    pub fn system_prompt(&self, depth: usize, role: Role) -> String {
        let mut out = format!(
            "{rol}\n\n\
             Equipo: {hw}\n\
             Backend: {backend} · modelo asignado: {modelo}\n\
             Nivel de anidamiento: {depth} de {max_depth} · subagentes restantes: {libres}\n\
             Workspace: {ws}\n\n\
             Reglas:\n\
             - Usa herramientas en vez de suponer; si no sabes algo, compruébalo (shell, fs_*, web_search).\n\
             - Para dividir trabajo usa `delegate` (uno) o `delegate_many` (varios en paralelo, solo si de verdad son independientes).\n\
             - Recupera y guarda conocimiento con `memory_search` / `memory_save` (kind=fact|lesson|skill).\n\
             - Busca en internet con moderación: hay pausa mínima entre peticiones y caché; no repitas la misma consulta.\n\
             - Devuelve resultados concretos y verificables; di lo que no hayas podido comprobar.\n\
             - Cuando tengas una meta activa, no la des por cumplida sin pruebas: usa `goal_done` solo con evidencia.",
            rol = role.guidance(),
            hw = self.specs.summary(),
            backend = self.backend.label(),
            modelo = self.model_for(role),
            depth = depth,
            max_depth = self.limits.max_depth,
            libres = self.agents_left(),
            ws = self.workspace.root().display(),
        );
        out.push_str(&crate::ext::skills_index(self.workspace.root()));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_for_tests;

    #[test]
    fn profundidad_y_presupuesto_se_respetan() {
        let limits = Limits {
            max_depth: 1,
            max_agents: 0,
            steps: 3,
            reflect: false,
        };
        let specs = Specs {
            cores: 4,
            ram_gb: 8.0,
            vram_gb: None,
            gpu: None,
        };
        let ws = workspace_for_tests("orch");
        let memory = Arc::new(Mutex::new(
            Memory::open(ws.root().join(".agent/memory.json"), 50).unwrap(),
        ));
        let orch = Arc::new(Orchestra::new(
            Backend::ollama("http://127.0.0.1:1"),
            specs,
            Vec::new(),
            ws,
            memory,
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(WebState::new())),
            None,
            Arc::new(GoalState::new()),
            limits,
        ));
        // Sin presupuesto no se admite ningún subagente.
        let err = orch.spawn(1, "worker", "hola", "").unwrap_err().to_string();
        assert!(err.contains("presupuesto"), "{err}");
        // Profundidad excesiva se rechaza antes de intentar nada.
        let err = orch.spawn(9, "worker", "hola", "").unwrap_err().to_string();
        assert!(err.contains("profundidad"), "{err}");
        assert_eq!(orch.parallel, 1);
    }
}
