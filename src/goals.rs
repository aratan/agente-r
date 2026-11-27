//! Estado persistente del objetivo raíz y registro de intentos.
//!
//! El objetivo sobrevive a reinicios. El plan puede evolucionar, pero no se
//! sustituye silenciosamente un objetivo activo por otro.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub number: u64,
    pub at: u64,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalTask {
    pub id: String,
    pub title: String,
    pub status: TaskStatus,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub evidence: Option<String>,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalRecord {
    pub id: String,
    pub goal: String,
    pub status: GoalStatus,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub attempts: Vec<AttemptRecord>,
    #[serde(default)]
    pub attempt_count: u64,
    #[serde(default)]
    pub evidence: Option<String>,
    #[serde(default)]
    pub tasks: Vec<GoalTask>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Disk {
    #[serde(default)]
    active: Option<GoalRecord>,
    #[serde(default)]
    completed: Vec<GoalRecord>,
}

pub struct GoalState {
    path: Option<PathBuf>,
    inner: Mutex<Disk>,
}

impl Default for GoalState {
    fn default() -> Self {
        Self::new()
    }
}

impl GoalState {
    /// Estado en memoria, útil para pruebas y ejecuciones efímeras.
    pub fn new() -> Self {
        Self { path: None, inner: Mutex::new(Disk::default()) }
    }

    /// Abre o crea el estado persistente del objetivo raíz.
    pub fn open(path: PathBuf) -> Result<Self> {
        let disk = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("estado de objetivo ilegible: {}", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Disk::default(),
            Err(err) => return Err(err).with_context(|| format!("no se pudo leer {}", path.display())),
        };
        Ok(Self { path: Some(path), inner: Mutex::new(disk) })
    }

    fn save(&self, disk: &Disk) -> Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(disk)?)?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("no se pudo persistir el objetivo en {}", path.display()))
    }

    /// Fija el objetivo único. Reanuda el mismo objetivo; rechaza reemplazar
    /// uno activo y conserva los objetivos ya completados.
    pub fn start(&self, goal: &str) -> Result<()> {
        let goal = goal.trim();
        if goal.is_empty() { bail!("el objetivo no puede estar vacío"); }
        let mut disk = self.inner.lock().map_err(|_| anyhow::anyhow!("estado de objetivo bloqueado"))?;
        match &disk.active {
            Some(current) if current.goal == goal => return Ok(()),
            Some(current) if current.status == GoalStatus::Active => {
                bail!("ya existe un objetivo activo ({}): {}; complétalo antes de iniciar otro", current.id, current.goal)
            }
            Some(_) => {
                if let Some(completed) = disk.active.take() {
                    disk.completed.push(completed);
                    if disk.completed.len() > 100 {
                        disk.completed.remove(0);
                    }
                }
            }
            None => {}
        }
        let now = now();
        disk.active = Some(GoalRecord {
            id: goal_id(goal, now),
            goal: goal.to_string(),
            status: GoalStatus::Active,
            created_at: now,
            updated_at: now,
            attempts: Vec::new(),
            attempt_count: 0,
            evidence: None,
            tasks: Vec::new(),
        });
        self.save(&disk)?;
        Ok(())
    }

    /// Añade el resultado de un ciclo. El resumen no se interpreta como éxito.
    pub fn record_attempt(&self, summary: &str) -> Result<u64> {
        let mut disk = self.inner.lock().map_err(|_| anyhow::anyhow!("estado de objetivo bloqueado"))?;
        let record = disk.active.as_mut().ok_or_else(|| anyhow::anyhow!("no hay objetivo activo"))?;
        if record.status == GoalStatus::Completed { bail!("el objetivo ya está completado"); }
        record.attempt_count += 1;
        let number = record.attempt_count;
        record.attempts.push(AttemptRecord {
            number,
            at: now(),
            summary: truncate(summary, 1200),
        });
        // Evita que el historial crezca sin límite; los logs completos pertenecen
        // a los artefactos de ejecución, no al pequeño estado de control.
        if record.attempts.len() > 500 {
            record.attempts.drain(..record.attempts.len() - 500);
        }
        record.updated_at = now();
        self.save(&disk)?;
        Ok(number)
    }

    /// Crea o actualiza una tarea del plan persistente. Las dependencias deben
    /// existir y el grafo debe permanecer acíclico.
    pub fn upsert_task(&self, id: &str, title: &str, depends_on: &[String]) -> Result<()> {
        let id = id.trim();
        let title = title.trim();
        if id.is_empty() || title.is_empty() { bail!("id y title de tarea son obligatorios"); }
        if depends_on.iter().any(|dep| dep == id) { bail!("una tarea no puede depender de sí misma"); }
        let mut disk = self.inner.lock().map_err(|_| anyhow::anyhow!("estado de objetivo bloqueado"))?;
        let goal = disk.active.as_mut().ok_or_else(|| anyhow::anyhow!("no hay objetivo activo"))?;
        for dep in depends_on {
            if !goal.tasks.iter().any(|task| task.id == *dep) {
                bail!("dependencia desconocida: {dep}");
            }
        }
        let existing = goal.tasks.iter().position(|task| task.id == id);
        let status = existing.map(|i| goal.tasks[i].status.clone()).unwrap_or(TaskStatus::Pending);
        let evidence = existing.and_then(|i| goal.tasks[i].evidence.clone());
        let task = GoalTask {
            id: id.to_string(),
            title: title.to_string(),
            status,
            depends_on: depends_on.to_vec(),
            evidence,
            updated_at: now(),
        };
        if let Some(index) = existing { goal.tasks[index] = task; } else { goal.tasks.push(task); }
        if has_dependency_cycle(&goal.tasks) { bail!("dependencias circulares: operación rechazada"); }
        goal.updated_at = now();
        self.save(&disk)?;
        Ok(())
    }

    /// Cambia estado de tarea. Una tarea solo puede empezar si sus dependencias
    /// están completadas; completar requiere evidencia no vacía.
    pub fn set_task_status(&self, id: &str, status: TaskStatus, evidence: Option<&str>) -> Result<()> {
        let mut disk = self.inner.lock().map_err(|_| anyhow::anyhow!("estado de objetivo bloqueado"))?;
        let goal = disk.active.as_mut().ok_or_else(|| anyhow::anyhow!("no hay objetivo activo"))?;
        let index = goal.tasks.iter().position(|task| task.id == id)
            .ok_or_else(|| anyhow::anyhow!("no existe la tarea {id}"))?;
        if status != TaskStatus::Pending {
            for dep in &goal.tasks[index].depends_on {
                let ready = goal.tasks.iter().any(|task| task.id == *dep && task.status == TaskStatus::Completed);
                if !ready { bail!("dependencia pendiente: {dep}"); }
            }
        }
        if status == TaskStatus::Completed && evidence.map(str::trim).unwrap_or("").is_empty() {
            bail!("evidence es obligatoria para completar una tarea");
        }
        let task = &mut goal.tasks[index];
        task.status = status;
        task.evidence = evidence.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        task.updated_at = now();
        goal.updated_at = now();
        self.save(&disk)?;
        Ok(())
    }

    pub fn tasks(&self) -> Vec<GoalTask> {
        self.inner.lock().ok()
            .and_then(|disk| disk.active.as_ref().map(|goal| goal.tasks.clone()))
            .unwrap_or_default()
    }

    pub fn plan_prompt(&self) -> String {
        let tasks = self.tasks();
        if tasks.is_empty() {
            return "PLAN: todavía no hay tareas persistentes. Descompón el objetivo en entregables verificables con goal_task antes de ejecutar cambios grandes.\\n".to_string();
        }
        let mut out = String::from("PLAN PERSISTENTE (respeta dependencias; trabaja primero en tareas listas):\\n");
        for task in tasks {
            out.push_str(&format!("- [{}] {} — {}{}\\n",
                match task.status { TaskStatus::Pending => "pendiente", TaskStatus::InProgress => "en curso", TaskStatus::Completed => "completada" },
                task.id, task.title,
                if task.depends_on.is_empty() { String::new() } else { format!(" (depende de: {})", task.depends_on.join(", ")) }
            ));
        }
        out
    }

    pub fn attempts(&self) -> u64 {
        self.inner.lock().ok()
            .and_then(|d| d.active.as_ref().map(|g| g.attempt_count))
            .unwrap_or(0)
    }

    pub fn previous_summaries(&self, limit: usize) -> Vec<String> {
        self.inner.lock().ok().and_then(|d| d.active.as_ref().map(|g| {
            g.attempts.iter().rev().take(limit).collect::<Vec<_>>()
                .into_iter().rev().map(|a| a.summary.clone()).collect()
        })).unwrap_or_default()
    }

    /// Marca la meta cumplida. La evidencia textual se conserva, pero no se
    /// considera verificación independiente: eso lo añadirá el Evaluator.
    pub fn ack(&self, evidence: &str) -> String {
        let evidence = evidence.trim();
        if evidence.is_empty() {
            return "Rechazado: `evidence` es obligatorio (qué comprobaste y cómo).".to_string();
        }
        let Ok(mut disk) = self.inner.lock() else {
            return "No se pudo registrar el estado de la meta".to_string();
        };
        {
            let Some(record) = disk.active.as_mut() else {
                return "Rechazado: no hay un objetivo activo".to_string();
            };
            if record.status == GoalStatus::Completed {
                return format!("El objetivo {} ya estaba completado.", record.id);
            }
            record.evidence = Some(evidence.to_string());
            record.status = GoalStatus::Completed;
            record.updated_at = now();
        }
        match self.save(&disk) {
            Ok(()) => format!("Meta marcada como cumplida con evidencia: {evidence}"),
            Err(err) => {
                if let Some(record) = disk.active.as_mut() {
                    record.status = GoalStatus::Active;
                    record.evidence = None;
                }
                format!("No se pudo persistir la finalización; la meta sigue activa: {err:#}")
            }
        }
    }

    pub fn is_done(&self) -> bool {
        self.inner.lock().ok().and_then(|d| d.active.as_ref().map(|g| g.status == GoalStatus::Completed)).unwrap_or(false)
    }

    pub fn evidence(&self) -> Option<String> {
        self.inner.lock().ok().and_then(|d| d.active.as_ref().and_then(|g| g.evidence.clone()))
    }

    pub fn active(&self) -> Option<GoalRecord> {
        self.inner.lock().ok().and_then(|d| d.active.clone())
    }

}

pub fn state_path(root: &Path) -> PathBuf {
    root.join(".agent").join("goal.json")
}

fn has_dependency_cycle(tasks: &[GoalTask]) -> bool {
    fn visit(id: &str, tasks: &[GoalTask], visiting: &mut Vec<String>, visited: &mut Vec<String>) -> bool {
        if visiting.iter().any(|item| item == id) { return true; }
        if visited.iter().any(|item| item == id) { return false; }
        visiting.push(id.to_string());
        if let Some(task) = tasks.iter().find(|task| task.id == id) {
            for dep in &task.depends_on {
                if visit(dep, tasks, visiting, visited) { return true; }
            }
        }
        visiting.pop();
        visited.push(id.to_string());
        false
    }
    let mut visiting = Vec::new();
    let mut visited = Vec::new();
    tasks.iter().any(|task| visit(&task.id, tasks, &mut visiting, &mut visited))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn goal_id(goal: &str, timestamp: u64) -> String {
    let seed = format!("{}:{timestamp}", goal.trim());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("g-{hash:016x}")
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max { return text.to_string(); }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// Instrucciones por ciclo. Los intentos previos se recuperan del disco.
pub fn attempt_prompt(goal: &str, attempt: usize, max: usize, previous: &[String]) -> String {
    let mut out = format!(
        "OBJETIVO PRINCIPAL (ciclo {attempt}; presupuesto local {max}).\n\
         El objetivo es persistente: no lo sustituyas ni declares éxito sin comprobarlo.\n\
         Objetivo: {goal}\n\
         Trabaja en la siguiente unidad de progreso verificable. Registra el resultado y sus evidencias.\n"
    );
    if !previous.is_empty() {
        out.push_str("\nHistorial de ciclos anteriores (no repitas estrategias fallidas):\n");
        for (i, summary) in previous.iter().enumerate() {
            out.push_str(&format!("- ciclo previo {}: {}\n", i + 1, summary));
        }
        out.push_str(
            "\nAntes de repetir una operación fallida, identifica la causa y cambia una condición relevante. \
             Si no puedes avanzar en una tarea, busca otra que contribuya al mismo objetivo.\n",
        );
    }
    out.push_str(
        "\nNo marques el objetivo como completado por haber terminado una tarea intermedia. \
         Solo usa goal_done cuando el resultado global esté realmente conseguido.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_persistente_valida_dependencias_y_evidencias() {
        let goal = GoalState::new();
        goal.start("objetivo con plan").unwrap();
        goal.upsert_task("a", "preparar base", &[]).unwrap();
        goal.upsert_task("b", "validar base", &["a".into()]).unwrap();
        assert!(goal.set_task_status("b", TaskStatus::InProgress, None).is_err());
        goal.set_task_status("a", TaskStatus::Completed, Some("tests verdes")).unwrap();
        goal.set_task_status("b", TaskStatus::InProgress, None).unwrap();
        assert!(goal.set_task_status("b", TaskStatus::Completed, None).is_err());
        goal.set_task_status("b", TaskStatus::Completed, Some("auditoría pasada")).unwrap();
        assert_eq!(goal.tasks().len(), 2);
    }

    #[test]
    fn plan_rechaza_ciclos_y_dependencias_inexistentes() {
        let goal = GoalState::new();
        goal.start("objetivo con grafo").unwrap();
        goal.upsert_task("a", "A", &[]).unwrap();
        assert!(goal.upsert_task("b", "B", &["missing".into()]).is_err());
        goal.upsert_task("b", "B", &["a".into()]).unwrap();
        assert!(goal.upsert_task("a", "A", &["b".into()]).is_err());
    }

    #[test]
    fn requiere_evidencia_y_persistencia() {
        let dir = std::env::temp_dir().join(format!("agent-goal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(".agent").join("goal.json");
        let goal = GoalState::open(path.clone()).unwrap();
        assert!(goal.start("dejar CI en verde").is_ok());
        assert!(!goal.is_done());
        assert!(goal.ack("   ").starts_with("Rechazado"));
        assert!(!goal.is_done());
        goal.record_attempt("cargo test falló por dependencia").unwrap();

        let reopened = GoalState::open(path.clone()).unwrap();
        assert_eq!(reopened.attempts(), 1);
        assert_eq!(reopened.active().unwrap().goal, "dejar CI en verde");
        assert!(reopened.start("otro objetivo").is_err());
        assert!(reopened.ack("cargo test: 38/38; CI verde").contains("cumplida"));

        let final_state = GoalState::open(path).unwrap();
        assert!(final_state.is_done());
        assert_eq!(final_state.evidence().as_deref(), Some("cargo test: 38/38; CI verde"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reanuda_mismo_objetivo_y_cambia_al_completar() {
        let goal = GoalState::new();
        goal.start("objetivo A").unwrap();
        goal.record_attempt("primer intento").unwrap();
        goal.start("objetivo A").unwrap();
        assert_eq!(goal.attempts(), 1);
        assert!(goal.start("objetivo B").is_err());
        goal.ack("verificado").to_string();
        assert!(goal.start("objetivo B").is_ok());
        assert_eq!(goal.active().unwrap().goal, "objetivo B");
    }

    #[test]
    fn el_prompt_incluye_historial_y_no_confunde_tarea_con_meta() {
        let prompt = attempt_prompt("completar proyecto", 4, 8, &["falló la compilación".into()]);
        assert!(prompt.contains("ciclo 4"));
        assert!(prompt.contains("falló la compilación"));
        assert!(prompt.contains("resultado global"));
    }
}
