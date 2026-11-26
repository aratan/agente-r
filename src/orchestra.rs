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
    /// Contexto asumido por inferencia: dimensio­na la KV cache y, con ella,
    /// cuántos subagentes caben a la vez.
    pub context_tokens: usize,
}

/// Tope de espera para conseguir hueco de ejecución.
const SLOT_WAIT: Duration = Duration::from_secs(900);

/// Permiso acotado para trabajar: cuántos agentes y cuántos pasos puede gastar
/// este subárbol.
///
/// Se reparte del padre al hijo, así una rama desbocada no se come el árbol
/// entero. El tope global (`Limits::max_agents`) sigue siendo el límite duro de
/// todo el proceso; el mandato añade aislamiento entre ramas hermanas.
#[derive(Debug, Clone, Copy)]
pub struct Mandate {
    pub agents: usize,
    pub steps: usize,
}

impl Mandate {
    /// Lo que recibe un hijo: la mitad de lo que tiene el padre, con suelo para
    /// que aún pueda pensar, actuar y responder.
    pub fn split(self) -> Self {
        self.share(1)
    }

    /// Reparte el mandato entre `parts` hijos: a cada uno la mitad de su parte
    /// justa, para que ninguno pueda quedarse con todo el subárbol.
    pub fn share(self, parts: usize) -> Self {
        let parts = parts.max(1);
        Self {
            agents: (self.agents / (parts * 2)).max(1),
            steps: (self.steps / 2).max(3),
        }
    }
}

/// Devuelve automáticamente el hueco de ejecución al salir del ámbito: también
/// cuando el subagente entra en pánico, porque `Drop` corre en el desenrollado.
/// Sin esto, un pánico dejaba el hueco ocupado para siempre y con un solo slot
/// la orquestación se colgaba 15 minutos en la siguiente delegación.
struct SlotGuard<'a> {
    orch: &'a Orchestra,
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        self.orch.running.fetch_sub(1, Ordering::SeqCst);
    }
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
    /// Tope estático de subagentes simultáneos según el hardware. El número
    /// real de huecos se recalcula en cada reparto con `slots_for`, porque
    /// depende del modelo y de la memoria libre del momento.
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

    /// Mandato del proceso raíz: todo lo que permiten los límites.
    pub fn root_mandate(&self) -> Mandate {
        Mandate {
            agents: self.limits.max_agents,
            steps: self.limits.steps,
        }
    }

    /// Huecos que caben **ahora** con este modelo y el contexto configurado.
    ///
    /// Se recalcula en cada reparto porque la memoria libre cambia mientras el
    /// agente trabaja (navegador, ficheros indexados, otros subagentes).
    pub fn slots_for(&self, model: &str) -> usize {
        match self.models.iter().find(|m| m.id == model) {
            Some(m) => models::concurrent_slots(m, &self.specs, self.limits.context_tokens),
            // Modelo desconocido (p. ej. llama.cpp sin tamaños): un hueco.
            None => 1,
        }
    }

    /// Reparte trabajo: crea un subagente, lo ejecuta y devuelve su respuesta.
    pub fn spawn(
        self: &Arc<Self>,
        depth: usize,
        role_name: &str,
        goal: &str,
        context: &str,
        mandate: Mandate,
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
        if mandate.agents == 0 {
            bail!(
                "el mandato de este subárbol no permite más subagentes; resuelve esta parte tú mismo"
            );
        }

        let role = Role::parse(role_name).unwrap_or(Role::Worker);
        let model = self.model_for(role);
        if model.is_empty() {
            bail!("no hay modelos instalados en el backend {}", self.backend.label());
        }
        let slots = self.slots_for(&model);

        let _slot = self.reserve(slots, SLOT_WAIT)?;
        let started = Instant::now();
        let result = self.run_agent(depth, role, &model, goal, context, mandate);
        match &result {
            Ok(_) => eprintln!(
                "  ✓ nivel {depth} ({role_name}) · {:.1}s",
                started.elapsed().as_secs_f64()
            ),
            Err(err) => eprintln!("  ✗ nivel {depth} ({role_name}): {err:#}"),
        }
        result
    }

    /// Reserva presupuesto global y hueco de ejecución a la vez.
    ///
    /// Si no hay hueco, la unidad de presupuesto se devuelve: esperar no debe
    /// gastar agentes. Antes se descontaba y se perdía al agotar la espera.
    fn reserve(&self, slots: usize, wait: Duration) -> Result<SlotGuard<'_>> {
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
        match self.acquire_slot(slots.max(1), wait) {
            Ok(()) => Ok(SlotGuard { orch: self }),
            Err(err) => {
                self.budget.fetch_add(1, Ordering::SeqCst);
                Err(err)
            }
        }
    }

    /// Espera a que haya hueco de los que caben ahora mismo.
    fn acquire_slot(&self, slots: usize, wait: Duration) -> Result<()> {
        let start = Instant::now();
        loop {
            let current = self.running.load(Ordering::SeqCst);
            if current < slots
                && self
                    .running
                    .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                return Ok(());
            }
            if start.elapsed() > wait {
                bail!(
                    "sin hueco de ejecución tras {}s (¿otro agente atascado?)",
                    wait.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn run_agent(
        self: &Arc<Self>,
        depth: usize,
        role: Role,
        model: &str,
        goal: &str,
        context: &str,
        mandate: Mandate,
    ) -> Result<String> {
        eprintln!(
            "  ↳ nivel {depth} · rol {} · modelo {model} · {} agente(s) libres",
            role.as_str(),
            self.agents_left()
        );

        let tools = Arc::new(ToolRegistry::for_role(self, depth, role, mandate));
        let mut agent = Agent::new(
            self.backend.provider(model),
            tools,
            self.system_prompt(depth, role, mandate),
            AgentOptions {
                memory: Some(self.memory.clone()),
                embedder: self.embedder.clone(),
                reflect: self.limits.reflect,
                reflect_tools: Some(Arc::new(ToolRegistry::memory_only(self))),
                goal: Some(self.goal.clone()),
                depth,
                max_steps: mandate.steps.min(self.limits.steps).max(1),
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
    pub fn system_prompt(&self, depth: usize, role: Role, mandate: Mandate) -> String {
        let mut out = format!(
            "{rol}\n\n\
             Equipo: {hw}\n\
             Backend: {backend} · modelo asignado: {modelo}\n\
             Nivel de anidamiento: {depth} de {max_depth} · subagentes restantes: {libres}\n\
             Presupuesto de este subárbol: {m_agents} agente(s) · {m_steps} paso(s) por turno\n\
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
            m_agents = mandate.agents,
            m_steps = mandate.steps,
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

    fn specs_flojo() -> Specs {
        Specs {
            cores: 4,
            ram_gb: 8.0,
            vram_gb: None,
            gpu: None,
        }
    }

    fn limites(max_agents: usize) -> Limits {
        Limits {
            max_depth: 1,
            max_agents,
            steps: 3,
            reflect: false,
            context_tokens: 64 * 1024,
        }
    }

    fn orchestra(tag: &str, limits: Limits) -> Arc<Orchestra> {
        let ws = workspace_for_tests(tag);
        let memory = Arc::new(Mutex::new(
            Memory::open(ws.root().join(".agent/memory.json"), 50).unwrap(),
        ));
        Arc::new(Orchestra::new(
            Backend::ollama("http://127.0.0.1:1"),
            specs_flojo(),
            vec![Model {
                id: "test:7b".into(),
                params_b: 7.0,
                size_gb: 4.0,
                family: "test".into(),
            }],
            ws,
            memory,
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(WebState::new())),
            None,
            Arc::new(GoalState::new()),
            limits,
        ))
    }

    #[test]
    fn profundidad_y_presupuesto_se_respetan() {
        let orch = orchestra("orch", limites(0));
        // El mandato permite trabajar: lo que se agota es el tope global.
        let mandate = Mandate {
            agents: 5,
            steps: 3,
        };
        // Sin presupuesto no se admite ningún subagente.
        let err = orch
            .spawn(1, "worker", "hola", "", mandate)
            .unwrap_err()
            .to_string();
        assert!(err.contains("presupuesto"), "{err}");
        // Profundidad excesiva se rechaza antes de intentar nada.
        let err = orch
            .spawn(9, "worker", "hola", "", mandate)
            .unwrap_err()
            .to_string();
        assert!(err.contains("profundidad"), "{err}");
        assert_eq!(orch.parallel, 1);
    }

    #[test]
    fn el_mandato_vacio_rechaza_sin_gastar_presupuesto() {
        let orch = orchestra("orch-mandato", limites(4));
        let vacio = Mandate {
            agents: 0,
            steps: 3,
        };
        let err = orch
            .spawn(1, "worker", "hola", "", vacio)
            .unwrap_err()
            .to_string();
        assert!(err.contains("mandato"), "{err}");
        assert_eq!(orch.agents_left(), 4, "un rechazo no debe gastar agentes");
    }

    #[test]
    fn el_mandato_se_reparte_del_padre_al_hijo() {
        let raiz = Mandate {
            agents: 12,
            steps: 8,
        };
        let hijo = raiz.split();
        assert_eq!(hijo.agents, 6);
        assert_eq!(hijo.steps, 4);
        // Suelo: un hijo siempre puede pensar, actuar y responder.
        let minimo = Mandate {
            agents: 1,
            steps: 1,
        };
        assert_eq!(minimo.split().agents, 1);
        assert_eq!(minimo.split().steps, 3);
        // Tres hermanos: cada uno la mitad de su parte justa.
        assert_eq!(raiz.share(3).agents, 2);
    }

    #[test]
    fn no_se_pierde_presupuesto_si_no_hay_hueco() {
        let orch = orchestra("orch-hueco", limites(4));
        let antes = orch.agents_left();
        // Ocupamos el único hueco a mano y pedimos otro con una espera corta.
        orch.acquire_slot(1, Duration::from_millis(10)).unwrap();
        let err = match orch.reserve(1, Duration::from_millis(50)) {
            Ok(_) => panic!("no debía haber hueco libre"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("hueco"), "{err}");
        assert_eq!(
            orch.agents_left(),
            antes,
            "el presupuesto se perdió al agotar la espera de hueco"
        );
        assert_eq!(orch.running(), 1, "el hueco sigue siendo nuestro");
    }

    #[test]
    fn el_hueco_se_libera_aunque_haya_panico() {
        let orch = orchestra("orch-panico", limites(4));
        let clon = orch.clone();
        let resultado = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            clon.acquire_slot(1, Duration::from_millis(10)).unwrap();
            let _slot = SlotGuard { orch: &clon };
            panic!("fallo simulado del subagente");
        }));
        assert!(resultado.is_err(), "el pánico debía propagarse");
        assert_eq!(orch.running(), 0, "el hueco quedó ocupado tras el pánico");
    }
}
