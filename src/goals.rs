//! Estado persistente del objetivo raíz y registro de intentos.
//!
//! El objetivo sobrevive a reinicios. El plan puede evolucionar, pero no se
//! sustituye silenciosamente un objetivo activo por otro.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
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
    #[serde(default)]
    pub checks: Vec<String>,
    #[serde(default)]
    pub verified: bool,
    #[serde(default)]
    pub verification_output: Option<String>,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Started,
    Succeeded,
    Failed,
    Unknown,
    ResolvedApplied,
    ResolvedNotApplied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationRecord {
    pub id: String,
    pub cycle: u64,
    pub tool: String,
    pub args_fingerprint: String,
    pub status: OperationStatus,
    pub started_at: u64,
    #[serde(default)]
    pub finished_at: Option<u64>,
    #[serde(default)]
    pub result_summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleRecord {
    pub number: u64,
    pub started_at: u64,
    #[serde(default)]
    pub finished_at: Option<u64>,
    #[serde(default)]
    pub interrupted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureRecord {
    pub fingerprint: String,
    pub category: String,
    pub symptom: String,
    pub strategy: String,
    pub condition_changed: bool,
    pub at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalRecord {
    pub id: String,
    pub goal: String,
    pub status: GoalStatus,
    #[serde(default)]
    pub pause_reason: Option<String>,
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
    #[serde(default)]
    pub failures: Vec<FailureRecord>,
    #[serde(default)]
    pub cycles: Vec<CycleRecord>,
    #[serde(default)]
    pub operations: Vec<OperationRecord>,
    #[serde(default)]
    pub operation_sequence: u64,
    #[serde(default)]
    pub active_cycle: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Disk {
    #[serde(default)]
    active: Option<GoalRecord>,
    #[serde(default)]
    completed: Vec<GoalRecord>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaveFailpoint {
    BeforeTempWrite,
    AfterTempSyncBeforeRename,
    AfterRenameBeforeDirectorySync,
}

pub struct GoalState {
    path: Option<PathBuf>,
    inner: Mutex<Disk>,
    #[cfg(test)]
    failpoint: Mutex<Option<SaveFailpoint>>,
}

impl Default for GoalState {
    fn default() -> Self {
        Self::new()
    }
}

impl GoalState {
    /// Estado en memoria, útil para pruebas y ejecuciones efímeras.
    pub fn new() -> Self {
        Self {
            path: None,
            inner: Mutex::new(Disk::default()),
            #[cfg(test)]
            failpoint: Mutex::new(None),
        }
    }

    /// Abre o crea el estado persistente del objetivo raíz.
    pub fn open(path: PathBuf) -> Result<Self> {
        let disk = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("estado de objetivo ilegible: {}", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Disk::default(),
            Err(err) => {
                return Err(err).with_context(|| format!("no se pudo leer {}", path.display()));
            }
        };
        Ok(Self {
            path: Some(path),
            inner: Mutex::new(disk),
            #[cfg(test)]
            failpoint: Mutex::new(None),
        })
    }

    #[cfg(test)]
    fn fail_next_save_at(&self, point: SaveFailpoint) {
        *self.failpoint.lock().unwrap() = Some(point);
    }

    #[cfg(test)]
    fn check_save_failpoint(&self, point: SaveFailpoint) -> Result<()> {
        let mut failpoint = self
            .failpoint
            .lock()
            .map_err(|_| anyhow::anyhow!("failpoint de test bloqueado"))?;
        if *failpoint == Some(point) {
            *failpoint = None;
            bail!("fallo de persistencia inyectado en {point:?}");
        }
        Ok(())
    }

    fn save(&self, disk: &Disk) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let tmp = path.with_extension("json.tmp");
        #[cfg(test)]
        self.check_save_failpoint(SaveFailpoint::BeforeTempWrite)?;
        std::fs::write(&tmp, serde_json::to_vec_pretty(disk)?)?;
        std::fs::File::open(&tmp)?.sync_all()?;
        #[cfg(test)]
        self.check_save_failpoint(SaveFailpoint::AfterTempSyncBeforeRename)?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("no se pudo persistir el objetivo en {}", path.display()))?;
        #[cfg(test)]
        self.check_save_failpoint(SaveFailpoint::AfterRenameBeforeDirectorySync)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }

    /// Recupera ciclos interrumpidos. Debe llamarse solo después de adquirir
    /// el bloqueo exclusivo del workspace para evitar escrituras concurrentes.
    pub fn recover_interrupted(&self) -> Result<usize> {
        let mut disk = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("estado de objetivo bloqueado"))?;
        let Some(record) = disk.active.as_mut() else {
            return Ok(0);
        };
        let mut recovered = 0usize;
        for operation in &mut record.operations {
            if operation.status == OperationStatus::Started {
                operation.status = OperationStatus::Unknown;
                operation.finished_at = Some(now());
                operation.result_summary = Some("proceso interrumpido; resultado externo desconocido; no repetir automáticamente".into());
                recovered += 1;
            }
        }
        if let Some(cycle) = record.active_cycle.take().and_then(|cycle_number| {
            record
                .cycles
                .iter_mut()
                .find(|cycle| cycle.number == cycle_number)
        }) {
            cycle.interrupted = true;
            cycle.finished_at = Some(now());
            recovered += 1;
        }
        if recovered > 0 {
            record.updated_at = now();
            self.save(&disk)?;
        }
        Ok(recovered)
    }

    /// Fija el objetivo único. Reanuda el mismo objetivo; rechaza reemplazar
    /// uno activo y conserva los objetivos ya completados.
    pub fn start(&self, goal: &str) -> Result<()> {
        let goal = goal.trim();
        if goal.is_empty() {
            bail!("el objetivo no puede estar vacío");
        }
        let mut disk = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("estado de objetivo bloqueado"))?;
        let existing = disk
            .active
            .as_ref()
            .map(|current| (current.goal.clone(), current.status, current.id.clone()));
        if let Some((current_goal, current_status, current_id)) = existing {
            if current_goal == goal {
                match current_status {
                    GoalStatus::Active | GoalStatus::Completed => return Ok(()),
                    GoalStatus::Paused => {
                        let record = disk.active.as_mut().expect("objetivo presente");
                        record.status = GoalStatus::Active;
                        record.pause_reason = None;
                        record.updated_at = now();
                        self.save(&disk)?;
                        return Ok(());
                    }
                }
            }
            if current_status == GoalStatus::Active || current_status == GoalStatus::Paused {
                bail!(
                    "hay un objetivo activo o pausado ({current_id}): {current_goal}; reanúdalo o complétalo antes de iniciar otro"
                );
            }
        }
        if let Some(completed) = disk.active.take() {
            disk.completed.push(completed);
            if disk.completed.len() > 100 {
                disk.completed.remove(0);
            }
        }
        let now = now();
        disk.active = Some(GoalRecord {
            id: goal_id(goal, now),
            goal: goal.to_string(),
            status: GoalStatus::Active,
            pause_reason: None,
            created_at: now,
            updated_at: now,
            attempts: Vec::new(),
            attempt_count: 0,
            evidence: None,
            tasks: Vec::new(),
            failures: Vec::new(),
            cycles: Vec::new(),
            operations: Vec::new(),
            operation_sequence: 0,
            active_cycle: None,
        });
        self.save(&disk)?;
        Ok(())
    }

    /// Persiste el comienzo del ciclo antes de invocar el modelo.
    pub fn begin_cycle(&self) -> Result<u64> {
        let mut disk = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("estado de objetivo bloqueado"))?;
        let record = disk
            .active
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no hay objetivo activo"))?;
        if record.status != GoalStatus::Active {
            bail!("el objetivo no está activo");
        }
        if let Some(previous) = record.active_cycle.take() {
            if let Some(cycle) = record
                .cycles
                .iter_mut()
                .find(|cycle| cycle.number == previous)
            {
                cycle.interrupted = true;
                cycle.finished_at = Some(now());
            }
            for operation in record
                .operations
                .iter_mut()
                .filter(|op| op.cycle == previous && op.status == OperationStatus::Started)
            {
                operation.status = OperationStatus::Unknown;
                operation.finished_at = Some(now());
                operation.result_summary =
                    Some("ciclo anterior sin cierre; resultado desconocido".into());
            }
        }
        let number = record
            .cycles
            .iter()
            .map(|cycle| cycle.number)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        record.cycles.push(CycleRecord {
            number,
            started_at: now(),
            finished_at: None,
            interrupted: false,
        });
        if record.cycles.len() > 200 {
            record.cycles.drain(..record.cycles.len() - 200);
        }
        record.active_cycle = Some(number);
        record.updated_at = now();
        self.save(&disk)?;
        Ok(number)
    }

    /// Registra la intención antes de ejecutar una herramienta. Solo se conserva
    /// un fingerprint; los argumentos potencialmente sensibles nunca se guardan.
    pub fn begin_operation(&self, tool: &str, args: &serde_json::Value) -> Result<Option<String>> {
        let mut disk = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("estado de objetivo bloqueado"))?;
        let record = disk
            .active
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no hay objetivo activo"))?;
        let Some(cycle) = record.active_cycle else {
            return Ok(None);
        };
        let canonical = serde_json::to_vec(args)?;
        let fingerprint = stable_fingerprint(&canonical);
        if record.operations.iter().any(|operation| {
            operation.tool == tool
                && operation.args_fingerprint == fingerprint
                && matches!(
                    operation.status,
                    OperationStatus::Unknown | OperationStatus::ResolvedApplied
                )
        }) {
            bail!(
                "operación bloqueada: existe una ejecución anterior con el mismo fingerprint y resultado desconocido; reconcilia el efecto externo antes de intentar repetirla"
            );
        }
        // Nunca descartar operaciones ambiguas o aplicadas: hacerlo permitiría
        // repetir un efecto externo antiguo al superar el límite del journal.
        if record.operations.len() >= 1_000 {
            if let Some(index) = record.operations.iter().position(|operation| {
                matches!(
                    operation.status,
                    OperationStatus::Succeeded
                        | OperationStatus::Failed
                        | OperationStatus::ResolvedNotApplied
                )
            }) {
                record.operations.remove(index);
            } else {
                bail!(
                    "journal lleno: reconcilia operaciones desconocidas antes de ejecutar más herramientas"
                );
            }
        }
        record.operation_sequence = record
            .operation_sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("se agotó el contador de operaciones"))?;
        let id = format!("op-{}", record.operation_sequence);
        record.operations.push(OperationRecord {
            id: id.clone(),
            cycle,
            tool: truncate(tool, 100),
            args_fingerprint: fingerprint,
            status: OperationStatus::Started,
            started_at: now(),
            finished_at: None,
            result_summary: None,
        });
        record.updated_at = now();
        self.save(&disk)?;
        Ok(Some(id))
    }

    /// Guarda el resultado después de ejecutar la herramienta.
    pub fn finish_operation(&self, id: &str, succeeded: bool, result: &str) -> Result<()> {
        let mut disk = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("estado de objetivo bloqueado"))?;
        let record = disk
            .active
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no hay objetivo activo"))?;
        let operation = record
            .operations
            .iter_mut()
            .find(|op| op.id == id)
            .ok_or_else(|| anyhow::anyhow!("no existe la operación {id}"))?;
        if operation.status != OperationStatus::Started {
            bail!("la operación {id} ya no está en estado started");
        }
        operation.status = if succeeded {
            OperationStatus::Succeeded
        } else {
            OperationStatus::Failed
        };
        operation.finished_at = Some(now());
        operation.result_summary = Some(truncate(result, 800));
        record.updated_at = now();
        self.save(&disk)
    }

    /// Registra la reconciliación humana de una operación cuyo resultado era
    /// desconocido. "applied" impide repetir; "not applied" permite reintentar.
    pub fn resolve_operation(&self, id: &str, applied: bool, evidence: &str) -> Result<()> {
        let evidence = evidence.trim();
        if evidence.is_empty() {
            bail!("la evidencia de reconciliación es obligatoria");
        }
        let mut disk = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("estado de objetivo bloqueado"))?;
        let record = disk
            .active
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no hay objetivo activo"))?;
        let operation = record
            .operations
            .iter_mut()
            .find(|operation| operation.id == id)
            .ok_or_else(|| anyhow::anyhow!("no existe la operación {id}"))?;
        if operation.status != OperationStatus::Unknown {
            bail!("solo se pueden reconciliar operaciones con estado unknown");
        }
        operation.status = if applied {
            OperationStatus::ResolvedApplied
        } else {
            OperationStatus::ResolvedNotApplied
        };
        operation.finished_at = Some(now());
        operation.result_summary = Some(truncate(
            &format!("reconciliación humana: {}", evidence),
            800,
        ));
        record.updated_at = now();
        self.save(&disk)
    }

    /// Instrucciones explícitas para reanudar sin repetir efectos externos ambiguos.
    pub fn recovery_prompt(&self, limit: usize) -> String {
        let operations = self
            .inner
            .lock()
            .ok()
            .and_then(|disk| {
                disk.active.as_ref().map(|goal| {
                    goal.operations
                        .iter()
                        .rev()
                        .filter(|operation| operation.status == OperationStatus::Unknown)
                        .take(limit)
                        .cloned()
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default();
        if operations.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "RECUPERACIÓN DE OPERACIONES INTERRUMPIDAS: NO repitas ninguna operación UNKNOWN ni su efecto externo. Primero busca evidencia para reconciliar si se ejecutó. Si no puedes determinarlo, informa del bloqueo y pide intervención; una estrategia distinta debe producir una operación nueva y justificada.\n",
        );
        for operation in operations.iter().rev() {
            out.push_str(&format!(
                "- {} | herramienta: {} | fingerprint: {} | resultado: desconocido; no repetir\n",
                operation.id, operation.tool, operation.args_fingerprint
            ));
        }
        out
    }

    /// Añade el resultado de un ciclo. El resumen no se interpreta como éxito.
    pub fn record_attempt(&self, summary: &str) -> Result<u64> {
        let mut disk = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("estado de objetivo bloqueado"))?;
        let record = disk
            .active
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no hay objetivo activo"))?;
        if record.status != GoalStatus::Active {
            bail!("el objetivo no está activo");
        }
        record.attempt_count += 1;
        let number = record.attempt_count;
        if let Some(cycle) = record.active_cycle.take().and_then(|cycle_number| {
            record
                .cycles
                .iter_mut()
                .find(|cycle| cycle.number == cycle_number)
        }) {
            cycle.finished_at = Some(now());
        }
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
    pub fn upsert_task(
        &self,
        id: &str,
        title: &str,
        depends_on: &[String],
        checks: &[String],
    ) -> Result<()> {
        let id = id.trim();
        let title = title.trim();
        if id.is_empty() || title.is_empty() {
            bail!("id y title de tarea son obligatorios");
        }
        if depends_on.iter().any(|dep| dep == id) {
            bail!("una tarea no puede depender de sí misma");
        }
        let mut disk = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("estado de objetivo bloqueado"))?;
        let goal = disk
            .active
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no hay objetivo activo"))?;
        for dep in depends_on {
            if !goal.tasks.iter().any(|task| task.id == *dep) {
                bail!("dependencia desconocida: {dep}");
            }
        }
        let existing = goal.tasks.iter().position(|task| task.id == id);
        let status = existing
            .map(|i| goal.tasks[i].status.clone())
            .unwrap_or(TaskStatus::Pending);
        let evidence = existing.and_then(|i| goal.tasks[i].evidence.clone());
        let verified = existing.is_some_and(|i| {
            let previous = &goal.tasks[i];
            previous.verified
                && previous.title == title
                && previous.depends_on == depends_on
                && previous.checks == checks
        });
        let verification_output = if verified {
            existing.and_then(|i| goal.tasks[i].verification_output.clone())
        } else {
            None
        };
        if depends_on
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            != depends_on.len()
        {
            bail!("dependencias duplicadas");
        }
        if existing.is_some_and(|i| {
            let previous = &goal.tasks[i];
            previous.status == TaskStatus::Completed
                && (previous.title != title
                    || previous.depends_on != depends_on
                    || previous.checks != checks)
        }) {
            bail!(
                "no se puede modificar una tarea completada; reábrela primero si no tiene dependientes completados"
            );
        }
        if goal.tasks.iter().any(|task| {
            task.status == TaskStatus::Completed && task.depends_on.iter().any(|dep| dep == id)
        }) && existing.is_some_and(|i| goal.tasks[i].depends_on != depends_on)
        {
            bail!("no se pueden cambiar dependencias de una tarea con dependientes completados");
        }
        let task = GoalTask {
            id: id.to_string(),
            title: title.to_string(),
            status,
            depends_on: depends_on.to_vec(),
            evidence,
            checks: checks.to_vec(),
            verified,
            verification_output,
            updated_at: now(),
        };
        let mut proposed = goal.tasks.clone();
        if let Some(index) = existing {
            proposed[index] = task;
        } else {
            proposed.push(task);
        }
        if has_dependency_cycle(&proposed) {
            bail!("dependencias circulares: operación rechazada");
        }
        goal.tasks = proposed;
        goal.updated_at = now();
        self.save(&disk)?;
        Ok(())
    }

    /// Cambia estado de tarea. Una tarea solo puede empezar si sus dependencias
    /// están completadas; completar requiere evidencia no vacía.
    pub fn set_task_status(
        &self,
        id: &str,
        status: TaskStatus,
        evidence: Option<&str>,
    ) -> Result<()> {
        let mut disk = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("estado de objetivo bloqueado"))?;
        let goal = disk
            .active
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no hay objetivo activo"))?;
        let index = goal
            .tasks
            .iter()
            .position(|task| task.id == id)
            .ok_or_else(|| anyhow::anyhow!("no existe la tarea {id}"))?;
        if status != TaskStatus::Completed
            && goal.tasks.iter().any(|task| {
                task.status == TaskStatus::Completed && task.depends_on.iter().any(|dep| dep == id)
            })
        {
            bail!("no se puede reabrir una tarea con dependientes ya completados");
        }
        if status != TaskStatus::Pending {
            for dep in &goal.tasks[index].depends_on {
                let ready = goal
                    .tasks
                    .iter()
                    .any(|task| task.id == *dep && task.status == TaskStatus::Completed);
                if !ready {
                    bail!("dependencia pendiente: {dep}");
                }
            }
        }
        if status == TaskStatus::Completed && evidence.map(str::trim).unwrap_or("").is_empty() {
            bail!("evidence es obligatoria para completar una tarea");
        }
        let task = &mut goal.tasks[index];
        task.status = status;
        task.evidence = evidence
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        task.verified = false;
        task.verification_output = None;
        task.updated_at = now();
        goal.updated_at = now();
        self.save(&disk)?;
        Ok(())
    }

    /// Registra un fallo y la estrategia intentada. La misma estrategia para
    /// el mismo síntoma exige declarar un cambio real de condiciones antes de
    /// volver a intentarla.
    pub fn record_failure(
        &self,
        fingerprint: &str,
        category: &str,
        symptom: &str,
        strategy: &str,
        condition_changed: bool,
    ) -> Result<()> {
        let fingerprint = fingerprint.trim();
        let strategy = strategy.trim();
        if fingerprint.is_empty() || strategy.is_empty() {
            bail!("fingerprint y strategy son obligatorios");
        }
        let mut disk = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("estado de objetivo bloqueado"))?;
        let goal = disk
            .active
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no hay objetivo activo"))?;
        let repeated = goal
            .failures
            .iter()
            .any(|failure| failure.fingerprint == fingerprint && failure.strategy == strategy);
        if repeated && !condition_changed {
            bail!(
                "reintento bloqueado: esta estrategia ya falló para el mismo síntoma; cambia la estrategia o documenta un cambio de condiciones"
            );
        }
        goal.failures.push(FailureRecord {
            fingerprint: truncate(fingerprint, 160),
            category: truncate(category.trim(), 80),
            symptom: truncate(symptom.trim(), 800),
            strategy: truncate(strategy, 300),
            condition_changed,
            at: now(),
        });
        if goal.failures.len() > 200 {
            goal.failures.drain(..goal.failures.len() - 200);
        }
        goal.updated_at = now();
        self.save(&disk)?;
        Ok(())
    }

    pub fn failure_prompt(&self, limit: usize) -> String {
        let failures = self
            .inner
            .lock()
            .ok()
            .and_then(|disk| {
                disk.active.as_ref().map(|goal| {
                    goal.failures
                        .iter()
                        .rev()
                        .take(limit)
                        .cloned()
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default();
        if failures.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "DIARIO DE FALLOS (no repitas la misma estrategia sin cambiar condiciones):",
        );
        out.push('\n');
        for failure in failures.iter().rev() {
            out.push_str(&format!("- [{}] {} | síntoma: {} | estrategia fallida: {} | cambio de condiciones declarado: {}",
                failure.category, failure.fingerprint, failure.symptom, failure.strategy,
                if failure.condition_changed { "sí" } else { "no" }
            ));
            out.push('\n');
        }
        out
    }

    /// Registra el resultado del evaluador determinista de comandos de aceptación.
    pub fn set_task_verification(&self, id: &str, passed: bool, output: &str) -> Result<()> {
        let mut disk = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("estado de objetivo bloqueado"))?;
        let goal = disk
            .active
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no hay objetivo activo"))?;
        let task = goal
            .tasks
            .iter_mut()
            .find(|task| task.id == id)
            .ok_or_else(|| anyhow::anyhow!("no existe la tarea {id}"))?;
        if task.status != TaskStatus::Completed {
            bail!("la tarea debe estar completada antes de evaluarla");
        }
        task.verified = passed;
        task.verification_output = Some(truncate(output, 4000));
        task.updated_at = now();
        goal.updated_at = now();
        self.save(&disk)?;
        Ok(())
    }

    pub fn tasks(&self) -> Vec<GoalTask> {
        self.inner
            .lock()
            .ok()
            .and_then(|disk| disk.active.as_ref().map(|goal| goal.tasks.clone()))
            .unwrap_or_default()
    }

    pub fn plan_prompt(&self) -> String {
        let tasks = self.tasks();
        if tasks.is_empty() {
            return "PLAN: todavía no hay tareas persistentes. Descompón el objetivo en entregables verificables con goal_task antes de ejecutar cambios grandes.\n".to_string();
        }
        let mut out = String::from(
            "PLAN PERSISTENTE (respeta dependencias; trabaja primero en tareas listas):\n",
        );
        for task in tasks {
            out.push_str(&format!(
                "- [{}{}] {} — {}{} | checks: {}\n",
                match task.status {
                    TaskStatus::Pending => "pendiente",
                    TaskStatus::InProgress => "en curso",
                    TaskStatus::Completed => "completada",
                },
                if task.verified { " · verificada" } else { "" },
                task.id,
                task.title,
                if task.depends_on.is_empty() {
                    String::new()
                } else {
                    format!(" (depende de: {})", task.depends_on.join(", "))
                },
                if task.checks.is_empty() {
                    "sin definir".to_string()
                } else {
                    task.checks.join(", ")
                }
            ));
        }
        out
    }

    pub fn pause(&self, reason: &str) -> Result<()> {
        let mut disk = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("estado de objetivo bloqueado"))?;
        let record = disk
            .active
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no hay objetivo activo"))?;
        if record.status != GoalStatus::Active {
            bail!("solo se puede pausar un objetivo activo");
        }
        record.status = GoalStatus::Paused;
        record.pause_reason = Some(truncate(reason.trim(), 800));
        record.updated_at = now();
        self.save(&disk)
    }

    /// Presupuesto persistente de ciclos. El contador total nunca se reinicia al reanudar.
    pub fn budget_limit(&self, max_per_hour: usize, max_total: u64) -> Option<String> {
        self.inner.lock().ok().and_then(|disk| {
            disk.active.as_ref().and_then(|goal| {
                if goal.attempt_count >= max_total {
                    return Some(format!("límite total de ciclos alcanzado ({max_total})"));
                }
                let cutoff = now().saturating_sub(3600);
                let recent = goal
                    .attempts
                    .iter()
                    .filter(|attempt| attempt.at >= cutoff)
                    .count();
                if recent >= max_per_hour {
                    return Some(format!(
                        "límite horario alcanzado ({max_per_hour} ciclos/hora)"
                    ));
                }
                None
            })
        })
    }

    /// Detecta respuestas idénticas consecutivas, señal determinista de estancamiento.
    pub fn repeated_attempts(&self, limit: usize) -> bool {
        if limit < 2 {
            return false;
        }
        self.inner
            .lock()
            .ok()
            .and_then(|disk| {
                disk.active.as_ref().map(|goal| {
                    if goal.attempts.len() < limit {
                        return false;
                    }
                    let tail = &goal.attempts[goal.attempts.len() - limit..];
                    let first = tail[0].summary.trim();
                    !first.is_empty() && tail.iter().all(|attempt| attempt.summary.trim() == first)
                })
            })
            .unwrap_or(false)
    }

    pub fn attempts(&self) -> u64 {
        self.inner
            .lock()
            .ok()
            .and_then(|d| d.active.as_ref().map(|g| g.attempt_count))
            .unwrap_or(0)
    }

    pub fn previous_summaries(&self, limit: usize) -> Vec<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|d| {
                d.active.as_ref().map(|g| {
                    g.attempts
                        .iter()
                        .rev()
                        .take(limit)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .map(|a| a.summary.clone())
                        .collect()
                })
            })
            .unwrap_or_default()
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
            if record.status == GoalStatus::Paused {
                return "Rechazado: el objetivo está pausado por el governor; reanúdalo explícitamente antes de cerrarlo.".to_string();
            }
            let unfinished = record
                .tasks
                .iter()
                .filter(|task| task.status != TaskStatus::Completed || !task.verified)
                .map(|task| task.id.clone())
                .collect::<Vec<_>>();
            if !unfinished.is_empty() {
                return format!(
                    "Rechazado: quedan tareas sin completar: {}",
                    unfinished.join(", ")
                );
            }
            record.evidence = Some(evidence.to_string());
            record.status = GoalStatus::Completed;
            if let Some(cycle) = record.active_cycle.take().and_then(|cycle_number| {
                record
                    .cycles
                    .iter_mut()
                    .find(|cycle| cycle.number == cycle_number)
            }) {
                cycle.finished_at = Some(now());
            }
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
        self.inner
            .lock()
            .ok()
            .and_then(|d| d.active.as_ref().map(|g| g.status == GoalStatus::Completed))
            .unwrap_or(false)
    }

    pub fn evidence(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|d| d.active.as_ref().and_then(|g| g.evidence.clone()))
    }

    pub fn active(&self) -> Option<GoalRecord> {
        self.inner.lock().ok().and_then(|d| d.active.clone())
    }
}

pub fn state_path(root: &Path) -> PathBuf {
    root.join(".agent").join("goal.json")
}

fn has_dependency_cycle(tasks: &[GoalTask]) -> bool {
    fn visit(
        id: &str,
        tasks: &[GoalTask],
        visiting: &mut Vec<String>,
        visited: &mut Vec<String>,
    ) -> bool {
        if visiting.iter().any(|item| item == id) {
            return true;
        }
        if visited.iter().any(|item| item == id) {
            return false;
        }
        visiting.push(id.to_string());
        if let Some(task) = tasks.iter().find(|task| task.id == id) {
            for dep in &task.depends_on {
                if visit(dep, tasks, visiting, visited) {
                    return true;
                }
            }
        }
        visiting.pop();
        visited.push(id.to_string());
        false
    }
    let mut visiting = Vec::new();
    let mut visited = Vec::new();
    tasks
        .iter()
        .any(|task| visit(&task.id, tasks, &mut visiting, &mut visited))
}

fn stable_fingerprint(bytes: &[u8]) -> String {
    // FNV-1a 64-bit: fingerprint determinista sin persistir argumentos.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a64:{hash:016x}")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
    if text.chars().count() <= max {
        return text.to_string();
    }
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
    fn historial_de_ciclos_se_acota_sin_perder_contador_total() {
        let goal = GoalState::new();
        goal.start("stress").unwrap();
        for cycle in 0..1_200 {
            goal.record_attempt(&format!("resultado {cycle}")).unwrap();
        }
        assert_eq!(goal.attempts(), 1_200);
        let history = goal.previous_summaries(600);
        assert_eq!(history.len(), 500);
        assert_eq!(history.first().map(String::as_str), Some("resultado 700"));
        assert_eq!(history.last().map(String::as_str), Some("resultado 1199"));
    }

    #[test]
    fn governor_limita_ciclos_por_hora_y_presupuesto_total() {
        let goal = GoalState::new();
        goal.start("presupuesto").unwrap();
        goal.record_attempt("ciclo 1").unwrap();
        assert!(goal.budget_limit(1, 100).is_some());
        assert!(goal.budget_limit(10, 1).is_some());
        assert!(goal.budget_limit(10, 100).is_none());
    }

    #[test]
    fn pausa_por_estancamiento_y_reanudacion_explicita() {
        let goal = GoalState::new();
        goal.start("misión persistente").unwrap();
        for _ in 0..3 {
            goal.record_attempt("sin cambios; no se pudo avanzar")
                .unwrap();
        }
        assert!(goal.repeated_attempts(3));
        goal.pause("tres ciclos idénticos").unwrap();
        assert_eq!(goal.active().unwrap().status, GoalStatus::Paused);
        assert!(goal.record_attempt("otro ciclo").is_err());
        assert!(goal.start("otra misión").is_err());
        goal.start("misión persistente").unwrap();
        assert_eq!(goal.active().unwrap().status, GoalStatus::Active);
        assert!(goal.active().unwrap().pause_reason.is_none());
    }

    #[test]
    fn diario_causal_bloquea_repeticion_sin_cambio_de_condiciones() {
        let goal = GoalState::new();
        goal.start("corregir fallo recurrente").unwrap();
        goal.record_failure(
            "cargo-test-17",
            "test",
            "assertion failed",
            "cambiar el timeout",
            false,
        )
        .unwrap();
        assert!(
            goal.record_failure(
                "cargo-test-17",
                "test",
                "assertion failed",
                "cambiar el timeout",
                false
            )
            .is_err()
        );
        assert!(
            goal.record_failure(
                "cargo-test-17",
                "test",
                "assertion failed",
                "aislar la carrera",
                false
            )
            .is_ok()
        );
        assert!(
            goal.record_failure(
                "cargo-test-17",
                "test",
                "assertion failed",
                "cambiar el timeout",
                true
            )
            .is_ok()
        );
        assert!(goal.failure_prompt(8).contains("aislar la carrera"));
    }

    #[test]
    fn cambiar_checks_invalida_verificacion_anterior() {
        let goal = GoalState::new();
        goal.start("checks versionados").unwrap();
        goal.upsert_task("test", "tests", &[], &["cargo test".into()])
            .unwrap();
        goal.set_task_status("test", TaskStatus::Completed, Some("tests ejecutados"))
            .unwrap();
        goal.set_task_verification("test", true, "cargo test: ok")
            .unwrap();
        assert!(
            goal.upsert_task("test", "tests", &[], &["cargo check".into()])
                .is_err()
        );
        goal.set_task_status("test", TaskStatus::Pending, None)
            .unwrap();
        goal.upsert_task("test", "tests", &[], &["cargo check".into()])
            .unwrap();
        assert!(!goal.tasks()[0].verified);
    }

    #[test]
    fn plan_persistente_valida_dependencias_y_evidencias() {
        let goal = GoalState::new();
        goal.start("objetivo con plan").unwrap();
        goal.upsert_task("a", "preparar base", &[], &["cargo test".into()])
            .unwrap();
        goal.upsert_task("b", "validar base", &["a".into()], &["cargo clippy".into()])
            .unwrap();
        assert!(
            goal.set_task_status("b", TaskStatus::InProgress, None)
                .is_err()
        );
        goal.set_task_status("a", TaskStatus::Completed, Some("tests verdes"))
            .unwrap();
        goal.set_task_status("b", TaskStatus::InProgress, None)
            .unwrap();
        assert!(
            goal.set_task_status("b", TaskStatus::Completed, None)
                .is_err()
        );
        goal.set_task_status("b", TaskStatus::Completed, Some("auditoría pasada"))
            .unwrap();
        goal.set_task_verification("a", true, "cargo test: ok")
            .unwrap();
        goal.set_task_verification("b", true, "cargo clippy: ok")
            .unwrap();
        assert_eq!(goal.tasks().len(), 2);
    }

    #[test]
    fn plan_y_estado_de_tareas_sobreviven_a_reapertura() {
        let dir = std::env::temp_dir().join(format!("agent-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("goal.json");
        let goal = GoalState::open(path.clone()).unwrap();
        goal.start("objetivo durable").unwrap();
        goal.upsert_task("docs", "documentar", &[], &["git diff --check".into()])
            .unwrap();
        goal.set_task_status("docs", TaskStatus::InProgress, None)
            .unwrap();
        drop(goal);
        let reopened = GoalState::open(path).unwrap();
        assert_eq!(reopened.tasks()[0].id, "docs");
        assert_eq!(reopened.tasks()[0].status, TaskStatus::InProgress);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn plan_rechaza_ciclos_y_dependencias_inexistentes() {
        let goal = GoalState::new();
        goal.start("objetivo con grafo").unwrap();
        goal.upsert_task("a", "A", &[], &["cargo test".into()])
            .unwrap();
        assert!(
            goal.upsert_task("b", "B", &["missing".into()], &[])
                .is_err()
        );
        goal.upsert_task("b", "B", &["a".into()], &[]).unwrap();
        assert!(goal.upsert_task("a", "A", &["b".into()], &[]).is_err());
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
        goal.record_attempt("cargo test falló por dependencia")
            .unwrap();

        let reopened = GoalState::open(path.clone()).unwrap();
        assert_eq!(reopened.attempts(), 1);
        assert_eq!(reopened.active().unwrap().goal, "dejar CI en verde");
        assert!(reopened.start("otro objetivo").is_err());
        assert!(
            reopened
                .ack("cargo test: 38/38; CI verde")
                .contains("cumplida")
        );

        let final_state = GoalState::open(path).unwrap();
        assert!(final_state.is_done());
        assert_eq!(
            final_state.evidence().as_deref(),
            Some("cargo test: 38/38; CI verde")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_permite_reabrir_dependencia_de_tarea_completada() {
        let goal = GoalState::new();
        goal.start("dependencias estables").unwrap();
        goal.upsert_task("a", "base", &[], &["cargo test".into()])
            .unwrap();
        goal.upsert_task("b", "integración", &["a".into()], &["cargo clippy".into()])
            .unwrap();
        goal.set_task_status("a", TaskStatus::Completed, Some("base verificada"))
            .unwrap();
        goal.set_task_verification("a", true, "cargo test: ok")
            .unwrap();
        goal.set_task_status("b", TaskStatus::Completed, Some("integración verificada"))
            .unwrap();
        goal.set_task_verification("b", true, "cargo clippy: ok")
            .unwrap();
        assert!(
            goal.set_task_status("a", TaskStatus::Pending, None)
                .is_err()
        );
        assert!(goal.upsert_task("a", "base", &["b".into()], &[]).is_err());
    }

    #[test]
    fn no_permite_cerrar_objetivo_con_tareas_pendientes() {
        let goal = GoalState::new();
        goal.start("objetivo dividido en tareas").unwrap();
        goal.upsert_task("compile", "compilar", &[], &["cargo test".into()])
            .unwrap();
        assert!(
            goal.ack("el agente afirma que terminó")
                .starts_with("Rechazado")
        );
        assert!(!goal.is_done());
        goal.set_task_status("compile", TaskStatus::Completed, Some("cargo test: ok"))
            .unwrap();
        assert!(
            goal.ack("el agente afirma que pasó")
                .starts_with("Rechazado")
        );
        goal.set_task_verification("compile", true, "cargo test: ok")
            .unwrap();
        assert!(
            goal.ack("tests ejecutados y resultado comprobado")
                .contains("cumplida")
        );
        assert!(goal.is_done());
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
    fn goal_json_legacy_migra_sin_campos_de_journal() {
        let dir = std::env::temp_dir().join(format!("agent-goal-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("goal.json");
        std::fs::write(
            &path,
            r#"{
            "active": {
                "id": "legacy",
                "goal": "objetivo anterior",
                "status": "active",
                "created_at": 1,
                "updated_at": 1,
                "attempts": [],
                "attempt_count": 0,
                "tasks": [],
                "failures": []
            },
            "completed": []
        }"#,
        )
        .unwrap();
        let goal = GoalState::open(path).unwrap();
        let record = goal.active().unwrap();
        assert!(record.cycles.is_empty());
        assert!(record.operations.is_empty());
        assert!(record.active_cycle.is_none());
        assert_eq!(record.goal, "objetivo anterior");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn operacion_interrumpida_se_marca_unknown_y_no_se_repite() {
        let dir = std::env::temp_dir().join(format!("agent-op-recovery-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(".agent").join("goal.json");
        let goal = GoalState::open(path.clone()).unwrap();
        goal.start("recuperar operación").unwrap();
        let first_cycle = goal.begin_cycle().unwrap();
        let operation = goal
            .begin_operation(
                "send_email",
                &serde_json::json!({
                    "to": "private@example.test",
                    "body": "contenido sensible"
                }),
            )
            .unwrap()
            .unwrap();
        drop(goal);

        let recovered = GoalState::open(path.clone()).unwrap();
        assert_eq!(recovered.recover_interrupted().unwrap(), 2);
        let record = recovered.active().unwrap();
        assert_eq!(record.operations[0].status, OperationStatus::Unknown);
        assert!(
            !record.operations[0]
                .result_summary
                .as_deref()
                .unwrap()
                .contains("private@example.test")
        );
        assert!(
            record
                .cycles
                .iter()
                .any(|cycle| cycle.number == first_cycle && cycle.interrupted)
        );
        assert!(record.active_cycle.is_none());

        let second_cycle = recovered.begin_cycle().unwrap();
        assert!(
            second_cycle > first_cycle,
            "los ciclos no deben reutilizar IDs tras un crash"
        );
        assert!(
            recovered
                .begin_operation(
                    "send_email",
                    &serde_json::json!({
                        "to": "private@example.test",
                        "body": "contenido sensible"
                    })
                )
                .is_err(),
            "un efecto externo de resultado desconocido no debe repetirse con los mismos argumentos"
        );
        let next = recovered
            .begin_operation("send_email", &serde_json::json!({"to":"other"}))
            .unwrap()
            .unwrap();
        assert_ne!(next, operation);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reconciliacion_humana_distingue_efecto_aplicado_y_no_aplicado() {
        let dir = std::env::temp_dir().join(format!("agent-op-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("goal.json");
        let goal = GoalState::open(path.clone()).unwrap();
        goal.start("reconciliar operaciones").unwrap();
        goal.begin_cycle().unwrap();
        let applied = goal
            .begin_operation("send", &serde_json::json!({"id":1}))
            .unwrap()
            .unwrap();
        let not_applied = goal
            .begin_operation("send", &serde_json::json!({"id":2}))
            .unwrap()
            .unwrap();
        drop(goal);

        let recovered = GoalState::open(path).unwrap();
        recovered.recover_interrupted().unwrap();
        recovered
            .resolve_operation(&applied, true, "el proveedor confirma el envío")
            .unwrap();
        recovered
            .resolve_operation(
                &not_applied,
                false,
                "el proveedor confirma que no recibió la petición",
            )
            .unwrap();
        recovered.begin_cycle().unwrap();
        assert!(
            recovered
                .begin_operation("send", &serde_json::json!({"id":1}))
                .is_err()
        );
        assert!(
            recovered
                .begin_operation("send", &serde_json::json!({"id":2}))
                .is_ok()
        );
        assert!(
            recovered
                .resolve_operation(&applied, true, "duplicado")
                .is_err()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn persistencia_soporta_1200_reaperturas_con_historial_acotado() {
        let dir = std::env::temp_dir().join(format!("agent-goal-soak-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("goal.json");

        for index in 0..1_200u64 {
            let goal = GoalState::open(path.clone()).unwrap();
            goal.recover_interrupted().unwrap();
            goal.start("soak persistente").unwrap();
            goal.begin_cycle().unwrap();
            let id = goal
                .begin_operation("external", &serde_json::json!({"sequence": index}))
                .unwrap()
                .unwrap();

            // Cada 100 ciclos simulamos una caída tras registrar la intención,
            // antes de guardar el resultado de la operación.
            if index % 100 == 0 {
                drop(goal);
                continue;
            }

            goal.finish_operation(&id, true, "ok").unwrap();
            goal.record_attempt(&format!("ciclo {index} correcto"))
                .unwrap();
        }

        let recovered = GoalState::open(path.clone()).unwrap();
        recovered.recover_interrupted().unwrap();
        let record = recovered.active().unwrap();
        assert_eq!(record.attempt_count, 1_188);
        assert!(record.attempts.len() <= 500);
        assert!(record.cycles.len() <= 200);
        assert!(record.operations.len() <= 1_000);
        assert_eq!(record.operation_sequence, 1_200);
        assert_eq!(
            record
                .operations
                .iter()
                .filter(|op| op.status == OperationStatus::Unknown)
                .count(),
            12
        );
        let bytes = std::fs::metadata(&path).unwrap().len();
        assert!(
            bytes < 2 * 1024 * 1024,
            "goal.json creció demasiado: {bytes} bytes"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn journal_lleno_conserva_unknown_y_no_reutiliza_ids() {
        let goal = GoalState::new();
        goal.start("journal bajo carga").unwrap();
        goal.begin_cycle().unwrap();

        let mut ids = std::collections::HashSet::new();
        for index in 0..999 {
            let id = goal
                .begin_operation("write", &serde_json::json!({"index": index}))
                .unwrap()
                .unwrap();
            assert!(ids.insert(id.clone()), "ID duplicado: {id}");
            goal.finish_operation(&id, true, "ok").unwrap();
        }

        let unknown_args = serde_json::json!({"payment": "ambiguous"});
        let unknown = goal
            .begin_operation("payment", &unknown_args)
            .unwrap()
            .unwrap();
        assert!(ids.insert(unknown.clone()));

        // Al superar el límite solo se expulsa un registro terminal.
        let next = goal
            .begin_operation("write", &serde_json::json!({"index": 1000}))
            .unwrap()
            .unwrap();
        assert!(ids.insert(next.clone()));
        let record = goal.active().unwrap();
        assert_eq!(record.operations.len(), 1_000);
        assert!(
            record
                .operations
                .iter()
                .any(|op| { op.id == unknown && op.status == OperationStatus::Started })
        );

        // Llenamos el resto con operaciones aún abiertas. La poda solo puede
        // retirar resultados terminales; al no quedar ninguno, debe fallar cerrado.
        for index in 0..998 {
            let id = goal
                .begin_operation("external", &serde_json::json!({"index": index}))
                .unwrap()
                .unwrap();
            assert!(ids.insert(id), "ID duplicado al reciclar el journal");
        }
        assert!(
            goal.begin_operation("external", &serde_json::json!({"index": 9999}))
                .is_err()
        );

        let record = goal.active().unwrap();
        assert_eq!(record.operations.len(), 1_000);
        assert!(record.operations.iter().any(|op| op.id == unknown));
        assert!(
            record
                .operations
                .iter()
                .all(|op| op.status == OperationStatus::Started)
        );
        assert_eq!(record.operation_sequence, 1_999);
        let retained_ids: std::collections::HashSet<_> =
            record.operations.iter().map(|op| op.id.as_str()).collect();
        assert_eq!(retained_ids.len(), record.operations.len());

        goal.recover_interrupted().unwrap();
        assert!(goal.begin_cycle().is_ok());
        assert!(goal.begin_operation("payment", &unknown_args).is_err());
        let recovered = goal.active().unwrap();
        let all_ids: Vec<_> = recovered
            .operations
            .iter()
            .map(|op| op.id.clone())
            .collect();
        assert_eq!(
            all_ids
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            all_ids.len()
        );
    }

    #[test]
    fn operacion_completada_se_persiste_con_fingerprint_y_resultado() {
        let dir = std::env::temp_dir().join(format!("agent-op-success-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("goal.json");
        let goal = GoalState::open(path.clone()).unwrap();
        goal.start("ejecutar herramienta").unwrap();
        goal.begin_cycle().unwrap();
        let id = goal
            .begin_operation(
                "fs_write",
                &serde_json::json!({"path":"x","content":"secret"}),
            )
            .unwrap()
            .unwrap();
        goal.finish_operation(&id, true, "archivo escrito").unwrap();
        drop(goal);
        let reopened = GoalState::open(path).unwrap();
        let recovered_record = reopened.active().unwrap();
        let operation = &recovered_record.operations[0];
        assert_eq!(operation.status, OperationStatus::Succeeded);
        assert_eq!(operation.result_summary.as_deref(), Some("archivo escrito"));
        assert!(operation.args_fingerprint.starts_with("fnv1a64:"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fallo_antes_de_rename_no_publica_un_checkpoint_incompleto() {
        let dir =
            std::env::temp_dir().join(format!("agent-save-fail-before-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("goal.json");
        let goal = GoalState::open(path.clone()).unwrap();
        goal.start("checkpoint seguro").unwrap();

        goal.fail_next_save_at(SaveFailpoint::AfterTempSyncBeforeRename);
        assert!(goal.begin_cycle().is_err());
        drop(goal);

        let reopened = GoalState::open(path.clone()).unwrap();
        assert!(reopened.active().unwrap().active_cycle.is_none());
        assert_eq!(reopened.begin_cycle().unwrap(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fallo_al_guardar_resultado_de_efecto_externo_se_recupera_como_unknown() {
        let dir =
            std::env::temp_dir().join(format!("agent-save-fail-result-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("goal.json");
        let goal = GoalState::open(path.clone()).unwrap();
        goal.start("resultado externo ambiguo").unwrap();
        goal.begin_cycle().unwrap();
        let id = goal
            .begin_operation("send", &serde_json::json!({"id": 7}))
            .unwrap()
            .unwrap();

        // Simula que el efecto externo ocurrió, pero el checkpoint final no llegó al rename.
        goal.fail_next_save_at(SaveFailpoint::AfterTempSyncBeforeRename);
        assert!(
            goal.finish_operation(&id, true, "efecto ejecutado")
                .is_err()
        );
        drop(goal);

        let recovered = GoalState::open(path).unwrap();
        assert_eq!(recovered.recover_interrupted().unwrap(), 2);
        assert_eq!(
            recovered.active().unwrap().operations[0].status,
            OperationStatus::Unknown
        );
        assert!(recovered.begin_cycle().unwrap() > 1);
        assert!(
            recovered
                .begin_operation("send", &serde_json::json!({"id": 7}))
                .is_err()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rename_completado_con_error_antes_de_sync_del_directorio_conserva_resultado() {
        let dir = std::env::temp_dir().join(format!(
            "agent-save-fail-after-rename-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("goal.json");
        let goal = GoalState::open(path.clone()).unwrap();
        goal.start("resultado tras rename").unwrap();
        goal.begin_cycle().unwrap();
        let id = goal
            .begin_operation("write", &serde_json::json!({"path": "x"}))
            .unwrap()
            .unwrap();

        goal.fail_next_save_at(SaveFailpoint::AfterRenameBeforeDirectorySync);
        assert!(goal.finish_operation(&id, true, "escrito").is_err());
        drop(goal);

        let reopened = GoalState::open(path).unwrap();
        assert_eq!(
            reopened.active().unwrap().operations[0].status,
            OperationStatus::Succeeded
        );
        assert_eq!(
            reopened.active().unwrap().operations[0]
                .result_summary
                .as_deref(),
            Some("escrito")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn el_prompt_incluye_historial_y_no_confunde_tarea_con_meta() {
        let prompt = attempt_prompt("completar proyecto", 4, 8, &["falló la compilación".into()]);
        assert!(prompt.contains("ciclo 4"));
        assert!(prompt.contains("falló la compilación"));
        assert!(prompt.contains("resultado global"));
    }

    /// Ensayo prolongado manual. Por defecto dura 48 horas.
    /// Ejecutar con bash scripts/goal-soak.sh; solo usa operaciones sintéticas locales.
    #[test]
    #[ignore = "soak prolongado manual; ejecutar con scripts/goal-soak.sh"]
    fn soak_durable_goal_state_for_configured_duration() {
        use std::time::{Duration, Instant};

        let seconds = std::env::var("AGENT_GOAL_SOAK_SECONDS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(172_800);
        let interval_ms = std::env::var("AGENT_GOAL_SOAK_INTERVAL_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(1_000);
        let path = std::env::var_os("AGENT_GOAL_SOAK_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::temp_dir().join(format!("agente-r-goal-soak-{}.json", std::process::id()))
            });
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        assert!(
            !path.exists(),
            "el archivo de soak ya existe; conserva los datos y configura otra ruta"
        );

        let goal = GoalState::open(path.clone()).unwrap();
        goal.start("soak sintético de persistencia").unwrap();
        drop(goal);

        let started = Instant::now();
        let duration = Duration::from_secs(seconds);
        let interval = Duration::from_millis(interval_ms);
        let mut iterations = 0u64;
        let mut recoveries = 0u64;
        let mut peak_file_bytes = 0u64;
        let mut peak_rss_kib = 0u64;
        let mut next_report = Duration::ZERO;

        while started.elapsed() < duration {
            let goal = GoalState::open(path.clone()).unwrap();
            goal.begin_cycle().unwrap();
            let operation_id = goal
                .begin_operation(
                    "soak.synthetic",
                    &serde_json::json!({"iteration": iterations}),
                )
                .unwrap()
                .expect("el ciclo activo debe aceptar operaciones");

            if iterations.is_multiple_of(17) {
                // Simula caída entre el registro de intención y el resultado.
                drop(goal);
                let recovered = GoalState::open(path.clone()).unwrap();
                assert!(recovered.recover_interrupted().unwrap() >= 1);
                recovered
                    .resolve_operation(
                        &operation_id,
                        true,
                        "efecto sintético confirmado por el propio harness",
                    )
                    .unwrap();
                recovered
                    .record_attempt("interrupción sintética recuperada")
                    .unwrap();
                recoveries += 1;
            } else {
                goal.finish_operation(&operation_id, true, "resultado sintético correcto")
                    .unwrap();
                goal.record_attempt("ciclo sintético completado").unwrap();
            }
            iterations += 1;

            let metadata = std::fs::metadata(&path).unwrap();
            peak_file_bytes = peak_file_bytes.max(metadata.len());
            if let Ok(status) = std::fs::read_to_string("/proc/self/status")
                && let Some(rss) = status.lines().find(|line| line.starts_with("VmRSS:"))
                && let Some(kib) = rss
                    .split_whitespace()
                    .nth(1)
                    .and_then(|value| value.parse().ok())
            {
                peak_rss_kib = peak_rss_kib.max(kib);
            }

            if started.elapsed() >= next_report {
                eprintln!(
                    "goal-soak elapsed={}s iterations={} recoveries={} file_bytes={} peak_file_bytes={} peak_rss_kib={}",
                    started.elapsed().as_secs(),
                    iterations,
                    recoveries,
                    metadata.len(),
                    peak_file_bytes,
                    peak_rss_kib
                );
                next_report += Duration::from_secs(60);
            }
            std::thread::sleep(interval);
        }

        let final_state = GoalState::open(path.clone()).unwrap();
        let record = final_state.active().unwrap();
        assert_eq!(record.status, GoalStatus::Active);
        assert!(record.cycles.len() <= 200);
        assert!(record.attempts.len() <= 500);
        assert!(record.operations.len() <= 1_000);
        assert!(std::fs::metadata(&path).unwrap().len() < 2 * 1024 * 1024);
        assert!(recoveries > 0 || iterations == 0);
        eprintln!(
            "goal-soak complete: iterations={iterations} recoveries={recoveries} peak_file_bytes={peak_file_bytes} peak_rss_kib={peak_rss_kib} state={}",
            path.display()
        );
    }
}
