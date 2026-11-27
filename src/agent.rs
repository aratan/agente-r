//! Bucle del agente: recuperar memoria (RAG) → pensar → actuar → observar, y al
//! terminar consolidar aprendizajes (RSI acotada).

use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::goals::{self, GoalState};
use crate::memory::{self, Memory};
use crate::provider::{Embedder, LlmProvider, Msg};
use crate::tools::ToolRegistry;

const HISTORY_LIMIT: usize = 24;

/// RSI acotada: solo memoria, con evaluación y versiones reversibles.
const RSI_PROMPT: &str = "\
Eres el módulo de auto-mejora de un agente local. Recibes la traza de una tarea ya terminada.

Objetivo: convertir la experiencia en cambios PERSISTENTES y REUTILIZABLES, no en un resumen.

Reglas:
- Guarda como máximo 3 elementos, con memory_save.
- kind=skill: procedimiento generalizable (cuándo usarlo + pasos concretos + trampa a evitar).
- kind=lesson: corrección o aprendizaje (lo que falló, por qué, qué hacer en su lugar).
- kind=fact: dato estable del proyecto (rutas, comandos, versiones).
- Solo si el aprendizaje sirve para OTRA tarea futura. Si no, no llames a ninguna herramienta y responde 'nada'.
- Si mejoras un recuerdo existente que viste en la traza, pásalo en `replaces` con su id (sube versión y archiva el viejo).
- No inventes causas que la traza no demuestre.
- La auto-mejora de código no está permitida aquí: solo memoria. Nada de cambios en herramientas o fuentes.";

pub struct AgentOptions {
    pub memory: Option<Arc<Mutex<Memory>>>,
    pub embedder: Option<Embedder>,
    pub reflect: bool,
    /// Registro reducido (solo memoria) para la fase de auto-mejora.
    pub reflect_tools: Option<Arc<ToolRegistry>>,
    pub goal: Option<Arc<GoalState>>,
    pub depth: usize,
    pub max_steps: usize,
}

pub struct Agent {
    provider: Box<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    system: String,
    opts: AgentOptions,
    history: Vec<Msg>,
    last_transcript: String,
}

impl Agent {
    pub fn new(
        provider: Box<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
        system: String,
        opts: AgentOptions,
    ) -> Self {
        Self {
            provider,
            tools,
            system,
            opts,
            history: Vec::new(),
            last_transcript: String::new(),
        }
    }

    pub fn reset(&mut self) {
        self.history.clear();
        self.last_transcript.clear();
    }

    /// Cambia de modelo en caliente (comando `/model`).
    pub fn set_provider(&mut self, provider: Box<dyn LlmProvider>) {
        self.provider = provider;
    }

    pub fn tools(&self) -> Vec<String> {
        self.tools.names()
    }

    /// Un turno completo. Si hay meta activa, el cierre real lo decide `goal_done`.
    pub fn run(&mut self, input: &str) -> Result<String> {
        let system = self.system_with_recall(input);
        let mut msgs: Vec<Msg> = Vec::with_capacity(self.history.len() + 2);
        msgs.push(Msg::System(system));
        msgs.extend(self.history.iter().cloned());
        msgs.push(Msg::User(input.to_string()));

        let defs = self.tools.defs();
        let mut used_tools = false;
        let mut answer: Option<String> = None;
        // Desde qué índice un `skill_read` aún no se ha mostrado al modelo.
        let mut fresh_from = 0usize;

        for step in 0..self.opts.max_steps {
            prune_skill_context(&mut msgs, fresh_from);
            let turn = self.provider.chat(&msgs, &defs)?;

            if turn.calls.is_empty() {
                if !turn.text.trim().is_empty() {
                    eprintln!("{}", wrap(&turn.text, 200));
                }
                answer = Some(turn.text.clone());
                msgs.push(Msg::Assistant {
                    text: turn.text,
                    calls: Vec::new(),
                });
                break;
            }

            used_tools = true;
            if !turn.text.trim().is_empty() {
                eprintln!("  💭 {}", memory::truncate(&turn.text, 200));
            }
            for call in &turn.calls {
                eprintln!(
                    "  ⚙  {} {}",
                    call.name,
                    memory::truncate(&call.args.to_string(), 140)
                );
            }
            msgs.push(Msg::Assistant {
                text: turn.text.clone(),
                calls: turn.calls.clone(),
            });
            fresh_from = msgs.len();

            for call in &turn.calls {
                // Journal-before-effect: si el proceso cae tras la herramienta,
                // el reinicio encontrará la operación como resultado desconocido.
                let operation_id = match &self.opts.goal {
                    Some(goal) => goal.begin_operation(&call.name, &call.args)?,
                    None => None,
                };
                let out = match self.tools.run(&call.name, &call.args) {
                    Ok(text) => {
                        if let (Some(goal), Some(id)) = (&self.opts.goal, operation_id.as_deref()) {
                            goal.finish_operation(id, true, &text)?;
                        }
                        text
                    }
                    Err(err) => {
                        let message = format!("ERROR: {err:#}");
                        if let (Some(goal), Some(id)) = (&self.opts.goal, operation_id.as_deref()) {
                            goal.finish_operation(id, false, &message)?;
                        }
                        message
                    }
                };
                eprintln!("     ↳ {}", memory::truncate(&out, 180));
                msgs.push(Msg::Tool {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    content: out,
                });
            }

            if step + 1 == self.opts.max_steps {
                msgs.push(Msg::User(
                    "Se agotaron los pasos. Resume en pocas líneas qué lograste, qué falta y qué probaste."
                        .to_string(),
                ));
                let turn = self.provider.chat(&msgs, &[])?;
                answer = Some(turn.text.clone());
                msgs.push(Msg::Assistant {
                    text: turn.text,
                    calls: Vec::new(),
                });
            }
        }

        let answer = answer.unwrap_or_else(|| "sin respuesta del modelo".to_string());

        // Guarda el turno (sin el mensaje de sistema, que se reconstruye).
        let mut kept = msgs.split_off(1);
        if kept.len() > HISTORY_LIMIT {
            kept.drain(..kept.len() - HISTORY_LIMIT);
        }
        self.history = kept;
        self.last_transcript = transcript(&msgs);

        // RSI: solo en el agente raíz y solo tras usar herramientas.
        if used_tools && self.opts.reflect && self.opts.depth == 0 {
            let trace = self.last_transcript.clone();
            if let Err(err) = self.reflect(&trace) {
                eprintln!("aviso: la consolidación (RSI) falló: {err:#}");
            }
        }

        Ok(answer)
    }

    /// Ejecuta un presupuesto de ciclos y conserva el progreso entre procesos.
    /// Repetir el mismo objetivo reanuda el historial; uno distinto no desplaza
    /// silenciosamente una meta que siga activa.
    pub fn pursue(&mut self, goal: &str, max_attempts: usize) -> Result<String> {
        let max = max_attempts.max(1);
        let state = self.opts.goal.clone();
        if let Some(state) = &state {
            state.start(goal)?;
            if state.is_done() {
                return Ok(format!(
                    "El objetivo ya estaba completado. Evidencia: {}",
                    state.evidence().unwrap_or_default()
                ));
            }
        }

        let mut last = String::new();
        for cycle in 1..=max {
            if let Some(state) = &state {
                let max_per_hour = std::env::var("AGENT_GOAL_MAX_CYCLES_PER_HOUR")
                    .ok().and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(120).clamp(1, 500);
                let max_total = std::env::var("AGENT_GOAL_MAX_CYCLES_TOTAL")
                    .ok().and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(50_000).max(1);
                if let Some(reason) = state.budget_limit(max_per_hour, max_total) {
                    state.pause(&reason)?;
                    return Err(anyhow::anyhow!("objetivo pausado por presupuesto: {reason}"));
                }
            }
            let previous = state.as_ref().map(|g| g.previous_summaries(8)).unwrap_or_default();
            let attempt = state.as_ref().map(|g| g.attempts() as usize + 1).unwrap_or(cycle);
            eprintln!(
                "\n── objetivo · ciclo {attempt} · presupuesto local {cycle}/{max} · {}",
                memory::truncate(goal, 80)
            );
            let mut prompt = goals::attempt_prompt(goal, attempt, max, &previous);
            if let Some(state) = &state {
                prompt.push('\n');
                prompt.push_str(&state.plan_prompt());
                prompt.push_str(&state.failure_prompt(8));
            }
            if let Some(state) = &state {
                // El checkpoint existe antes de la llamada al modelo, incluso si
                // el proceso termina durante la inferencia.
                state.begin_cycle()?;
            }
            last = match self.run(&prompt) {
                Ok(answer) => answer,
                Err(err) => {
                    if let Some(state) = &state {
                        state.record_attempt(&format!("ERROR de ejecución: {err:#}"))?;
                        if state.repeated_attempts(3) {
                            state.pause("tres ciclos consecutivos con el mismo error de ejecución")?;
                        }
                    }
                    return Err(err);
                }
            };

            if state.as_ref().map(|g| g.is_done()).unwrap_or(false) {
                eprintln!(
                    "✓ objetivo completado · evidencia: {}",
                    state.as_ref().and_then(|g| g.evidence()).unwrap_or_default()
                );
                return Ok(last);
            }

            if let Some(state) = &state {
                state.record_attempt(&last)?;
                if state.repeated_attempts(3) {
                    state.pause("tres ciclos consecutivos sin progreso: respuesta idéntica")?;
                    return Err(anyhow::anyhow!("objetivo pausado por estancamiento; revisa el diario y reanuda explícitamente con el mismo --goal cuando cambies la estrategia"));
                }
            }
        }
        Ok(last)
    }

    /// Fuerza la consolidación de la última traza (comando `/improve`).
    pub fn improve(&mut self) -> Result<()> {
        if self.last_transcript.trim().is_empty() {
            eprintln!("no hay traza reciente que consolidar");
            return Ok(());
        }
        let trace = self.last_transcript.clone();
        self.reflect(&trace)
    }

    fn system_with_recall(&self, input: &str) -> String {
        let Some(memory) = &self.opts.memory else {
            return self.system.clone();
        };
        let qvec = self
            .opts
            .embedder
            .as_ref()
            .and_then(|embedder| embedder.embed(input).ok());
        let block = memory
            .lock()
            .ok()
            .and_then(|mem| mem.recall(input, qvec.as_deref(), 5));
        match block {
            Some(context) => format!("{}\n\n{context}", self.system),
            None => self.system.clone(),
        }
    }

    fn reflect(&mut self, trace: &str) -> Result<()> {
        let Some(tools) = self.opts.reflect_tools.clone() else {
            return Ok(());
        };
        let defs = tools.defs();
        let mut msgs = vec![
            Msg::System(RSI_PROMPT.to_string()),
            Msg::User(trace.to_string()),
        ];
        let mut saved = 0usize;

        for _ in 0..3 {
            let turn = self.provider.chat(&msgs, &defs)?;
            if turn.calls.is_empty() {
                break;
            }
            msgs.push(Msg::Assistant {
                text: turn.text.clone(),
                calls: turn.calls.clone(),
            });
            for call in &turn.calls {
                let out = if call.name.starts_with("memory_") {
                    match tools.run(&call.name, &call.args) {
                        Ok(text) => {
                            saved += 1;
                            text
                        }
                        Err(err) => format!("ERROR: {err:#}"),
                    }
                } else {
                    "En la fase de auto-mejora solo se permite memory_*".to_string()
                };
                eprintln!("  🧠 RSI · {}", memory::truncate(&out, 160));
                msgs.push(Msg::Tool {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    content: out,
                });
            }
        }

        if saved == 0 {
            eprintln!("  🧠 RSI · nada nuevo que consolidar");
        } else {
            eprintln!("  🧠 RSI · {saved} recuerdo(s) consolidados");
        }
        Ok(())
    }
}

/// Libera del contexto los skills ya usados.
///
/// `skill_read` devuelve el contenido completo, y ese contenido se queda en el
/// historial ocupando ventana y KV cache en todos los pasos siguientes. Se
/// conserva el último (el modelo aún no lo ha visto) y los anteriores se
/// sustituyen por un aviso: se releen con `skill_read` si vuelven a hacer falta.
fn prune_skill_context(msgs: &mut [Msg], keep_from: usize) {
    for (i, msg) in msgs.iter_mut().enumerate() {
        if i >= keep_from {
            break;
        }
        if let Msg::Tool { name, content, .. } = msg
            && name == "skill_read"
            && !content.starts_with("[liberado]")
        {
            *content = "[liberado] skill ya aplicado y descargado del contexto; \
                        reléelo con skill_read si lo necesitas"
                .to_string();
        }
    }
}

fn wrap(text: &str, max: usize) -> String {
    memory::truncate(text, max)
}

fn transcript(msgs: &[Msg]) -> String {
    let mut out = String::new();
    for msg in msgs {
        match msg {
            Msg::System(_) => {}
            Msg::User(text) => out.push_str(&format!("USER: {}\n", memory::truncate(text, 600))),
            Msg::Assistant { text, calls } => {
                out.push_str(&format!("ASSISTANT: {}\n", memory::truncate(text, 400)));
                for call in calls {
                    out.push_str(&format!(
                        "  CALL {} {}\n",
                        call.name,
                        memory::truncate(&call.args.to_string(), 200)
                    ));
                }
            }
            Msg::Tool { name, content, .. } => {
                out.push_str(&format!(
                    "TOOL({}): {}\n",
                    name,
                    memory::truncate(content, 400)
                ));
            }
        }
    }
    memory::truncate(&out, 6000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ToolCall, Turn};
    use crate::tools::{Tool, ToolRegistry};
    use serde_json::{Value, json};

    /// Proveedor guionizado: no necesita red para probar el bucle.
    struct Guionizado {
        turnos: Mutex<Vec<Turn>>,
    }

    impl LlmProvider for Guionizado {
        fn chat(&self, _msgs: &[Msg], _tools: &[Value]) -> Result<Turn> {
            let mut turnos = self.turnos.lock().unwrap();
            if turnos.is_empty() {
                Ok(Turn {
                    text: "fin".into(),
                    calls: Vec::new(),
                })
            } else {
                Ok(turnos.remove(0))
            }
        }
    }

    struct Contador {
        veces: Arc<Mutex<usize>>,
    }

    impl Tool for Contador {
        fn name(&self) -> &str {
            "contar"
        }
        fn description(&self) -> &str {
            "cuenta"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        fn run(&self, _args: &Value) -> Result<String> {
            *self.veces.lock().unwrap() += 1;
            Ok("contado".into())
        }
    }

    #[test]
    fn ejecuta_herramientas_y_devuelve_texto() {
        let veces = Arc::new(Mutex::new(0usize));
        let mut reg = ToolRegistry::empty();
        reg.add(Contador {
            veces: veces.clone(),
        });
        let provider = Guionizado {
            turnos: Mutex::new(vec![
                Turn {
                    text: String::new(),
                    calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "contar".into(),
                        args: json!({}),
                    }],
                },
                Turn {
                    text: "hecho".into(),
                    calls: Vec::new(),
                },
            ]),
        };
        let mut agent = Agent::new(
            Box::new(provider),
            Arc::new(reg),
            "sistema".into(),
            AgentOptions {
                memory: None,
                embedder: None,
                reflect: false,
                reflect_tools: None,
                goal: None,
                depth: 0,
                max_steps: 4,
            },
        );
        let answer = agent.run("hazlo").unwrap();
        assert_eq!(answer, "hecho");
        assert_eq!(*veces.lock().unwrap(), 1);
        assert!(!agent.history.is_empty());
    }

    #[test]
    fn los_skills_ya_usados_salen_del_contexto() {
        let mut msgs = vec![
            Msg::System("sistema".into()),
            Msg::Tool {
                id: "1".into(),
                name: "skill_read".into(),
                content: "PASOS LARGOS".into(),
            },
            Msg::Assistant {
                text: "ok".into(),
                calls: Vec::new(),
            },
        ];
        // Recién leído (índice >= keep_from): se conserva hasta que el modelo lo vea.
        prune_skill_context(&mut msgs, 1);
        assert!(
            matches!(&msgs[1], Msg::Tool { content, .. } if content == "PASOS LARGOS")
        );
        // Ya visto: se libera del contexto.
        prune_skill_context(&mut msgs, 3);
        assert!(
            matches!(&msgs[1], Msg::Tool { content, .. } if content.starts_with("[liberado]"))
        );
        // Las demás herramientas no se tocan.
        let mut otros = vec![Msg::Tool {
            id: "2".into(),
            name: "shell".into(),
            content: "salida".into(),
        }];
        prune_skill_context(&mut otros, 99);
        assert!(matches!(&otros[0], Msg::Tool { content, .. } if content == "salida"));
    }

    #[test]
    fn meta_se_cumple_con_evidencia() {
        let estado = Arc::new(GoalState::new());
        let mut reg = ToolRegistry::empty();
        struct Listo {
            goal: Arc<GoalState>,
        }
        impl Tool for Listo {
            fn name(&self) -> &str {
                "goal_done"
            }
            fn description(&self) -> &str {
                "marca"
            }
            fn parameters(&self) -> Value {
                json!({"type": "object", "properties": {}})
            }
            fn run(&self, _args: &Value) -> Result<String> {
                Ok(self.goal.ack("comprobado con cargo test"))
            }
        }
        reg.add(Listo {
            goal: estado.clone(),
        });

        let provider = Guionizado {
            turnos: Mutex::new(vec![Turn {
                text: String::new(),
                calls: vec![ToolCall {
                    id: "g1".into(),
                    name: "goal_done".into(),
                    args: json!({"evidence": "x"}),
                }],
            }]),
        };
        let mut agent = Agent::new(
            Box::new(provider),
            Arc::new(reg),
            "sistema".into(),
            AgentOptions {
                memory: None,
                embedder: None,
                reflect: false,
                reflect_tools: None,
                goal: Some(estado.clone()),
                depth: 0,
                max_steps: 2,
            },
        );
        agent.pursue("terminar algo", 3).unwrap();
        assert!(estado.is_done());
    }
}
