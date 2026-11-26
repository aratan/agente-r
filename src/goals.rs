//! Metas persistentes: `/goal` insiste hasta cumplirla o agotar intentos.

use std::sync::Mutex;

/// Estado de la meta activa. El agente la marca cumplida con `goal_done` y solo
/// cuenta si aporta evidencia verificable (texto no vacío).
#[derive(Default)]
pub struct GoalState {
    inner: Mutex<Option<String>>,
}

impl GoalState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Marca la meta como cumplida. Devuelve el mensaje para el modelo.
    pub fn ack(&self, evidence: &str) -> String {
        let evidence = evidence.trim();
        if evidence.is_empty() {
            return "Rechazado: `evidence` es obligatorio (qué comprobaste y cómo).".to_string();
        }
        match self.inner.lock() {
            Ok(mut slot) => {
                *slot = Some(evidence.to_string());
                format!("Meta marcada como cumplida con evidencia: {evidence}")
            }
            Err(_) => "No se pudo registrar el estado de la meta".to_string(),
        }
    }

    pub fn is_done(&self) -> bool {
        self.inner
            .lock()
            .map(|slot| slot.is_some())
            .unwrap_or(false)
    }

    pub fn evidence(&self) -> Option<String> {
        self.inner.lock().ok().and_then(|slot| slot.clone())
    }

    pub fn reset(&self) {
        if let Ok(mut slot) = self.inner.lock() {
            *slot = None;
        }
    }
}

/// Instrucciones por intento: cada vuelta cambia el enfoque, no repite lo mismo.
pub fn attempt_prompt(goal: &str, attempt: usize, max: usize, previous: &[String]) -> String {
    let mut out = format!(
        "META ACTIVA (intento {attempt}/{max}). No está cumplida todavía.\n\
         Objetivo: {goal}\n\
         Trabaja hasta cumplirla y llama a `goal_done` con la evidencia cuando lo esté de verdad.\n"
    );
    if attempt > 1 {
        out.push_str("\nIntentos anteriores (no los repitas tal cual):\n");
        for (i, summary) in previous.iter().enumerate() {
            out.push_str(&format!("- intento {}: {}\n", i + 1, summary));
        }
        out.push_str(
            "\nCambia de estrategia: si una herramienta falló, usa otra; si faltaba información, \
             búscala; divide el trabajo con `delegate` si es demasiado grande.\n",
        );
    }
    if attempt == max {
        out.push_str(
            "\nÚltimo intento: si no puedes cumplirla, explica con precisión qué falta y qué probaste.\n",
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requiere_evidencia() {
        let goal = GoalState::new();
        assert!(!goal.is_done());
        assert!(goal.ack("   ").starts_with("Rechazado"));
        assert!(!goal.is_done());
        assert!(goal.ack("cargo test pasa 4/4").contains("cumplida"));
        assert!(goal.is_done());
        assert_eq!(goal.evidence().as_deref(), Some("cargo test pasa 4/4"));
        goal.reset();
        assert!(!goal.is_done());
    }

    #[test]
    fn el_prompt_escala_con_los_intentos() {
        let first = attempt_prompt("arregla el build", 1, 3, &[]);
        assert!(first.contains("intento 1/3"));
        assert!(!first.contains("Intentos anteriores"));
        let third = attempt_prompt("arregla el build", 3, 3, &["falló X".to_string()]);
        assert!(third.contains("Último intento"));
        assert!(third.contains("falló X"));
    }
}
