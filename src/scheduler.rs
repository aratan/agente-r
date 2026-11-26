//! Planificador de tareas periódicas, persistente entre sesiones.
//!
//! Las tareas se guardan en `.agent/schedule.json` y las ejecuta un subagente
//! (modo `--daemon`) o el propio agente mediante las herramientas `schedule_*`.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

fn default_role() -> String {
    "worker".to_string()
}

fn default_true() -> bool {
    true
}

fn default_interval() -> u64 {
    3600
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub goal: String,
    #[serde(default = "default_role")]
    pub role: String,
    #[serde(default = "default_interval")]
    pub every_secs: u64,
    #[serde(default)]
    pub runs: u64,
    #[serde(default)]
    pub last_run: u64,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Disk {
    #[serde(default)]
    tasks: Vec<Task>,
}

pub struct Scheduler {
    path: PathBuf,
    disk: Disk,
}

impl Scheduler {
    pub fn open(path: PathBuf) -> Result<Self> {
        let disk = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("agenda ilegible: {}", path.display()))?,
            Err(_) => Disk::default(),
        };
        Ok(Self { path, disk })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&self.disk)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn tasks(&self) -> &[Task] {
        &self.disk.tasks
    }

    pub fn add(&mut self, goal: &str, every_secs: u64, role: &str) -> Result<Task> {
        let goal = goal.trim();
        if goal.is_empty() {
            bail!("la tarea necesita un objetivo");
        }
        if every_secs < 10 {
            bail!("el intervalo mínimo es 10 segundos");
        }
        let id = short_id(goal, every_secs);
        if self.disk.tasks.iter().any(|t| t.id == id) {
            bail!("ya existe una tarea con ese objetivo e intervalo (id {id})");
        }
        let task = Task {
            id,
            goal: goal.to_string(),
            role: role.to_string(),
            every_secs,
            runs: 0,
            last_run: 0,
            enabled: true,
        };
        self.disk.tasks.push(task.clone());
        self.save()?;
        Ok(task)
    }

    pub fn remove(&mut self, id: &str) -> Result<bool> {
        let before = self.disk.tasks.len();
        self.disk.tasks.retain(|t| t.id != id);
        let removed = self.disk.tasks.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    #[allow(dead_code)]
    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<bool> {
        let Some(task) = self.disk.tasks.iter_mut().find(|t| t.id == id) else {
            return Ok(false);
        };
        task.enabled = enabled;
        self.save()?;
        Ok(true)
    }

    /// Tareas vencidas ahora mismo (nunca ejecutadas o intervalo cumplido).
    pub fn due(&self, now: u64) -> Vec<Task> {
        self.disk
            .tasks
            .iter()
            .filter(|t| t.enabled && now.saturating_sub(t.last_run) >= t.every_secs)
            .cloned()
            .collect()
    }

    pub fn mark_run(&mut self, id: &str, now: u64) -> Result<()> {
        if let Some(task) = self.disk.tasks.iter_mut().find(|t| t.id == id) {
            task.runs += 1;
            task.last_run = now;
            self.save()?;
        }
        Ok(())
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn schedule_path(root: &std::path::Path) -> PathBuf {
    root.join(".agent").join("schedule.json")
}

pub fn runs_dir(root: &std::path::Path) -> PathBuf {
    root.join(".agent").join("runs")
}

/// "30s" / "5m" / "2h" / "1d" / "90" → segundos.
pub fn parse_interval(text: &str) -> Option<u64> {
    let t = text.trim().to_lowercase();
    let (number, scale) = match t.chars().last()? {
        's' => (&t[..t.len() - 1], 1u64),
        'm' => (&t[..t.len() - 1], 60),
        'h' => (&t[..t.len() - 1], 3600),
        'd' => (&t[..t.len() - 1], 86_400),
        _ => (t.as_str(), 1),
    };
    let value: u64 = number.trim().parse().ok()?;
    value.checked_mul(scale)
}

pub fn human_interval(secs: u64) -> String {
    match secs {
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

fn short_id(goal: &str, every: u64) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in format!("{goal}\u{1f}{every}").bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", (hash ^ (hash >> 32)) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpreta_intervalos() {
        assert_eq!(parse_interval("30s"), Some(30));
        assert_eq!(parse_interval("5m"), Some(300));
        assert_eq!(parse_interval("2h"), Some(7200));
        assert_eq!(parse_interval("1d"), Some(86_400));
        assert_eq!(parse_interval("90"), Some(90));
        assert_eq!(parse_interval("nada"), None);
        assert_eq!(human_interval(7200), "2h");
        assert_eq!(human_interval(45), "45s");
    }

    #[test]
    fn agenda_persistente_y_vencimientos() {
        let dir = std::env::temp_dir().join(format!("agent-sched-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("schedule.json");
        let mut sched = Scheduler::open(path.clone()).unwrap();

        let task = sched.add("revisar el repo", 60, "worker").unwrap();
        assert!(sched.add("revisar el repo", 60, "worker").is_err());
        assert!(sched.add("x", 5, "worker").is_err(), "intervalo mínimo");

        // Nunca ejecutada: vence de inmediato.
        assert_eq!(sched.due(now()).len(), 1);
        sched.mark_run(&task.id, now()).unwrap();
        assert!(sched.due(now()).is_empty());

        let reopened = Scheduler::open(path).unwrap();
        assert_eq!(reopened.tasks().len(), 1);
        assert_eq!(reopened.tasks()[0].runs, 1);
        assert!(reopened.due(now() + 61).len() == 1);
    }

    #[test]
    fn borra_y_desactiva() {
        let dir = std::env::temp_dir().join(format!("agent-sched2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut sched = Scheduler::open(dir.join("s.json")).unwrap();
        let task = sched.add("limpiar logs", 30, "fast").unwrap();
        assert!(sched.set_enabled(&task.id, false).unwrap());
        assert!(sched.due(now()).is_empty());
        assert!(sched.remove(&task.id).unwrap());
        assert!(!sched.remove(&task.id).unwrap());
    }
}
