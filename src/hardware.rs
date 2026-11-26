//! Detección del hardware para dimensionar el trabajo: cuántos subagentes
//! pueden correr a la vez y qué tamaño de modelo cabe.
//!
//! Esto no se asume: se mide en cada arranque, porque la máquina puede cambiar
//! (portátil, servidor, contenedor con menos RAM, GPU distinta...).

use std::fs;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct Specs {
    pub cores: usize,
    pub ram_gb: f64,
    pub vram_gb: Option<f64>,
    pub gpu: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Sin GPU o muy poca RAM: un agente a la vez y modelos pequeños.
    Small,
    Medium,
    Large,
}

impl Specs {
    pub fn detect() -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let ram_gb = read_ram_gb().unwrap_or(8.0);
        let (gpu, vram_gb) = read_gpu();
        Self {
            cores,
            ram_gb,
            vram_gb,
            gpu,
        }
    }

    /// Presupuesto realista: la RAM no se puede llenar y la VRAM se comparte.
    pub fn budget_gb(&self) -> f64 {
        let vram = self.vram_gb.unwrap_or(0.0) * 0.85;
        self.ram_gb * 0.5 + vram
    }

    pub fn tier(&self) -> Tier {
        match self.budget_gb() {
            gb if gb < 12.0 => Tier::Small,
            gb if gb < 40.0 => Tier::Medium,
            _ => Tier::Large,
        }
    }

    /// Subagentes simultáneos: ni más que huecos de memoria, ni más que núcleos.
    pub fn max_parallel(&self) -> usize {
        let by_memory = match self.tier() {
            Tier::Small => 1,
            Tier::Medium => 2,
            Tier::Large => 4,
        };
        let by_cores = (self.cores / 6).max(1);
        by_memory.min(by_cores).max(1)
    }

    /// Tamaño máximo razonable de modelo en miles de millones de parámetros (q4).
    pub fn max_params_b(&self) -> f64 {
        (self.budget_gb() / 0.7).clamp(1.0, 120.0)
    }

    pub fn summary(&self) -> String {
        let gpu = match (&self.gpu, self.vram_gb) {
            (Some(name), Some(vram)) => format!("{name} ({vram:.1} GB)"),
            _ => "sin GPU detectada".to_string(),
        };
        format!(
            "{} núcleos · {:.1} GB RAM · {} · presupuesto {:.1} GB · {:?} · {} subagente(s) en paralelo · modelos ≤ {:.0}B",
            self.cores,
            self.ram_gb,
            gpu,
            self.budget_gb(),
            self.tier(),
            self.max_parallel(),
            self.max_params_b()
        )
    }
}

fn read_ram_gb() -> Option<f64> {
    let text = fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: f64 = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())?;
            return Some(kb / 1024.0 / 1024.0);
        }
    }
    None
}

fn read_gpu() -> (Option<String>, Option<f64>) {
    let output = Command::new("nvidia-smi")
        .args(["--query-gpu=name,memory.total", "--format=csv,noheader,nounits"])
        .output();
    let Ok(output) = output else {
        return (None, None);
    };
    if !output.status.success() {
        return (None, None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.lines().next().unwrap_or("").trim().to_string();
    let mut parts = first.rsplitn(2, ',');
    let mib: Option<f64> = parts.next().and_then(|v| v.trim().parse().ok());
    let name = parts.next().map(|v| v.trim().to_string());
    (name, mib.map(|mb| mb / 1024.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detecta_un_equipo_coherente() {
        let specs = Specs::detect();
        assert!(specs.cores >= 1);
        assert!(specs.ram_gb > 0.5, "ram detectada: {}", specs.ram_gb);
        assert!(specs.max_parallel() >= 1);
        assert!(specs.max_params_b() >= 1.0);
        assert!(specs.summary().contains("núcleos"));
    }

    #[test]
    fn escalas_por_hardware() {
        let flojo = Specs {
            cores: 4,
            ram_gb: 8.0,
            vram_gb: None,
            gpu: None,
        };
        assert_eq!(flojo.tier(), Tier::Small);
        assert_eq!(flojo.max_parallel(), 1);

        let medio = Specs {
            cores: 8,
            ram_gb: 16.0,
            vram_gb: Some(8.0),
            gpu: Some("RTX".into()),
        };
        assert_eq!(medio.tier(), Tier::Medium);
        assert_eq!(medio.max_parallel(), 1, "8 núcleos / 6 = 1");

        let grande = Specs {
            cores: 32,
            ram_gb: 128.0,
            vram_gb: Some(24.0),
            gpu: Some("A100".into()),
        };
        assert_eq!(grande.tier(), Tier::Large);
        assert_eq!(grande.max_parallel(), 4);
        assert!(grande.max_params_b() > medio.max_params_b());
    }
}
