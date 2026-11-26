//! Memoria persistente entre sesiones + recuperación (RAG).
//!
//! Un único fichero JSON legible y versionable por git. La recuperación combina
//! BM25 léxico (siempre disponible, sin dependencias) con similitud coseno de
//! embeddings cuando hay un modelo de embeddings accesible.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Tipos de recuerdo admitidos.
pub const KINDS: [&str; 4] = ["fact", "lesson", "skill", "doc"];

fn uno() -> u32 {
    1
}

fn ahora() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub created: u64,
    #[serde(default = "uno")]
    pub version: u32,
    #[serde(default)]
    pub hits: u32,
    #[serde(default)]
    pub useful: i32,
    #[serde(default)]
    pub vector: Vec<f32>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Disk {
    #[serde(default)]
    entries: Vec<Entry>,
    /// Versiones sustituidas: permiten auditar y revertir una mejora (RSI).
    #[serde(default)]
    superseded: Vec<Entry>,
}

pub struct Memory {
    path: PathBuf,
    disk: Disk,
    max: usize,
}

impl Memory {
    pub fn open(path: PathBuf, max: usize) -> Result<Self> {
        let disk = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(disk) => disk,
                Err(err) => {
                    // No se pierde nada: el fichero corrupto se aparta.
                    let backup = path.with_extension(format!("json.corrupto-{}", ahora()));
                    let _ = std::fs::rename(&path, &backup);
                    eprintln!("aviso: memoria ilegible ({err}); copia en {}", backup.display());
                    Disk::default()
                }
            },
            Err(_) => Disk::default(),
        };
        Ok(Self {
            path,
            disk,
            max: max.max(20),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> usize {
        self.disk.entries.len()
    }

    fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(&self.disk)?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("no se pudo guardar {}", self.path.display()))?;
        Ok(())
    }

    /// Guarda un recuerdo. Idempotente: el mismo texto en el mismo tipo no se
    /// duplica, solo suma usos. Con `replaces` sube la versión y archiva la anterior.
    pub fn remember(
        &mut self,
        kind: &str,
        text: &str,
        tags: Vec<String>,
        source: &str,
        replaces: Option<&str>,
        vector: Vec<f32>,
    ) -> Result<String> {
        let kind = normalize_kind(kind)?;
        let text = text.trim();
        if text.is_empty() {
            bail!("no se puede recordar un texto vacío");
        }
        let id = entry_id(&kind, text);

        if let Some(pos) = self.disk.entries.iter().position(|e| e.id == id) {
            let entry = &mut self.disk.entries[pos];
            entry.hits += 1;
            entry.source = source.to_string();
            for tag in tags {
                if !entry.tags.contains(&tag) {
                    entry.tags.push(tag);
                }
            }
            if entry.vector.is_empty() && !vector.is_empty() {
                entry.vector = vector;
            }
            let hits = entry.hits;
            self.save()?;
            return Ok(format!("ya existía [{kind}] id={id} (usos={hits})"));
        }

        let mut version = 1;
        if let Some(old_id) = replaces {
            if let Some(pos) = self.disk.entries.iter().position(|e| e.id == old_id) {
                let old = self.disk.entries.remove(pos);
                version = old.version + 1;
                self.disk.superseded.push(old);
            }
        }

        self.disk.entries.push(Entry {
            id: id.clone(),
            kind: kind.clone(),
            text: text.to_string(),
            tags,
            source: source.to_string(),
            created: ahora(),
            version,
            hits: 1,
            useful: 0,
            vector,
        });
        self.trim();
        self.save()?;
        Ok(format!(
            "guardado [{kind}] id={id} v{version} ({} recuerdos)",
            self.disk.entries.len()
        ))
    }

    /// Si se supera el máximo, se descartan los recuerdos menos útiles y viejos.
    fn trim(&mut self) {
        while self.disk.entries.len() > self.max {
            let victim = self
                .disk
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| (e.useful, e.hits, e.created))
                .map(|(i, _)| i);
            match victim {
                Some(i) => {
                    let old = self.disk.entries.remove(i);
                    self.disk.superseded.push(old);
                }
                None => break,
            }
        }
    }

    pub fn forget(&mut self, id: &str) -> Result<bool> {
        let Some(pos) = self.disk.entries.iter().position(|e| e.id == id) else {
            return Ok(false);
        };
        let old = self.disk.entries.remove(pos);
        self.disk.superseded.push(old);
        self.save()?;
        Ok(true)
    }

    pub fn feedback(&mut self, id: &str, useful: bool) -> Result<bool> {
        let Some(entry) = self.disk.entries.iter_mut().find(|e| e.id == id) else {
            return Ok(false);
        };
        entry.useful += if useful { 1 } else { -1 };
        self.save()?;
        Ok(true)
    }

    pub fn list(&self, kind: Option<&str>, limit: usize) -> Vec<&Entry> {
        let mut entries: Vec<&Entry> = self
            .disk
            .entries
            .iter()
            .filter(|e| kind.is_none_or(|k| e.kind == k))
            .collect();
        entries.sort_by(|a, b| b.created.cmp(&a.created));
        entries.truncate(limit);
        entries
    }

    /// Recuperación híbrida: BM25 + coseno si hay vectores.
    pub fn search(
        &self,
        query: &str,
        kind: Option<&str>,
        limit: usize,
        qvec: Option<&[f32]>,
    ) -> Vec<(f64, &Entry)> {
        if limit == 0 {
            return Vec::new();
        }
        let query_tokens = tokens(query);
        let docs: Vec<&Entry> = self
            .disk
            .entries
            .iter()
            .filter(|e| kind.is_none_or(|k| e.kind == k))
            .collect();
        if docs.is_empty() {
            return Vec::new();
        }

        let doc_tokens: Vec<Vec<String>> = docs.iter().map(|e| entry_tokens(e)).collect();
        let n = docs.len() as f64;
        let avg = doc_tokens.iter().map(|t| t.len()).sum::<usize>() as f64 / n;

        // Frecuencia de documento de cada término de la consulta.
        let mut df: HashMap<&str, f64> = HashMap::new();
        for term in &query_tokens {
            let count = doc_tokens.iter().filter(|t| t.contains(term)).count() as f64;
            df.insert(term.as_str(), count);
        }

        let mut scored: Vec<(f64, &Entry)> = Vec::new();
        for (entry, doc) in docs.iter().zip(&doc_tokens) {
            let mut lex = 0.0;
            let mut seen: HashSet<&str> = HashSet::new();
            for term in &query_tokens {
                if !seen.insert(term.as_str()) {
                    continue;
                }
                let tf = doc.iter().filter(|t| *t == term).count() as f64;
                if tf == 0.0 {
                    continue;
                }
                let d = df.get(term.as_str()).copied().unwrap_or(0.0);
                let idf = ((n - d + 0.5) / (d + 0.5) + 1.0).ln();
                let norm = 1.0 - 0.75 + 0.75 * (doc.len() as f64 / avg.max(1.0));
                lex += idf * (tf * 2.2) / (tf + 1.2 * norm);
            }

            let score = match qvec {
                Some(qv) if !entry.vector.is_empty() => {
                    let semantic = cosine(qv, &entry.vector).max(0.0) as f64;
                    0.5 * (lex / (1.0 + lex)) + 0.5 * semantic
                }
                _ => lex,
            };
            if score <= 0.0 {
                continue;
            }
            // La utilidad confirmada sube el ranking (memoria que se mejora sola).
            let boost = 1.0 + (entry.useful.max(0) as f64 * 0.05).min(0.5);
            scored.push((score * boost, entry));
        }

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        scored
    }

    /// Bloque de contexto para el prompt: el RAG propiamente dicho.
    pub fn recall(&self, query: &str, qvec: Option<&[f32]>, k: usize) -> Option<String> {
        let hits = self.search(query, None, k, qvec);
        if hits.is_empty() {
            return None;
        }
        let mut out = String::from("Memoria recuperada (RAG, de sesiones anteriores):\n");
        for (score, entry) in hits {
            out.push_str(&format!(
                "- [{}·v{}] {} (id {}, relevancia {:.2})\n",
                entry.kind,
                entry.version,
                truncate(&entry.text, 300),
                entry.id,
                score
            ));
        }
        out.push_str("Úsalo solo si encaja con la petición; si contradice lo observado, gana lo observado.\n");
        Some(out)
    }

    /// Recuerdos sin vector, para rellenarlos al arrancar si hay embeddings.
    pub fn pending_vectors(&self, limit: usize) -> Vec<String> {
        self.disk
            .entries
            .iter()
            .filter(|e| e.vector.is_empty())
            .take(limit)
            .map(|e| e.id.clone())
            .collect()
    }

    pub fn set_vector(&mut self, id: &str, vector: Vec<f32>) -> Result<()> {
        if let Some(entry) = self.disk.entries.iter_mut().find(|e| e.id == id) {
            entry.vector = vector;
            self.save()?;
        }
        Ok(())
    }

    pub fn text_of(&self, id: &str) -> Option<String> {
        self.disk
            .entries
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.text.clone())
    }

    pub fn superseded_count(&self) -> usize {
        self.disk.superseded.len()
    }
}

fn normalize_kind(kind: &str) -> Result<String> {
    let k = kind.trim().to_lowercase();
    let mapped = match k.as_str() {
        "" | "fact" | "hecho" | "dato" => "fact",
        "lesson" | "leccion" | "lección" | "aprendizaje" => "lesson",
        "skill" | "habilidad" | "procedimiento" => "skill",
        "doc" | "documento" | "fragmento" => "doc",
        other => bail!("tipo desconocido '{other}'; usa uno de {KINDS:?}"),
    };
    Ok(mapped.to_string())
}

/// Identidad estable: mismo texto y tipo ⇒ mismo id (deduplicación gratis).
pub fn entry_id(kind: &str, text: &str) -> String {
    let seed = format!("{kind}\u{1f}{}", text.trim().to_lowercase());
    format!("{:016x}", fnv1a(seed.as_bytes()))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Minúsculas y sin acentos: "Canción" y "cancion" se recuperan igual.
fn fold(text: &str) -> String {
    text.chars()
        .map(|c| match c.to_lowercase().next().unwrap_or(c) {
            'á' | 'à' | 'â' | 'ä' | 'ã' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ñ' => 'n',
            'ç' => 'c',
            other => other,
        })
        .collect()
}

pub fn tokens(text: &str) -> Vec<String> {
    fold(text)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 2)
        .map(|t| t.to_string())
        .collect()
}

fn entry_tokens(entry: &Entry) -> Vec<String> {
    let mut all = tokens(&entry.text);
    all.extend(entry.tags.iter().flat_map(|t| tokens(t)));
    all
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

pub fn truncate(text: &str, max: usize) -> String {
    let flat = text.replace('\n', " ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_memory() -> (Memory, PathBuf) {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agent-mem-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("memory.json");
        (Memory::open(path.clone(), 100).unwrap(), path)
    }

    #[test]
    fn guarda_recupera_y_deduplica() {
        let (mut mem, path) = tmp_memory();
        mem.remember("lesson", "Los tests CDP necesitan Chrome headless", vec![], "test", None, vec![])
            .unwrap();
        let again = mem
            .remember("lesson", "Los tests CDP necesitan Chrome headless", vec![], "test", None, vec![])
            .unwrap();
        assert!(again.starts_with("ya existía"), "{again}");
        assert_eq!(mem.len(), 1);

        let hits = mem.search("chrome tests", None, 5, None);
        assert_eq!(hits.len(), 1);

        // Persistencia real: se reabre desde disco.
        let reopened = Memory::open(path, 100).unwrap();
        assert_eq!(reopened.len(), 1);
        assert!(reopened.search("cdp", None, 5, None).len() == 1);
    }

    #[test]
    fn acentos_y_tags_se_recuperan() {
        let (mut mem, _) = tmp_memory();
        mem.remember("fact", "La canción del despliegue es opcional", vec!["musica".into()], "x", None, vec![])
            .unwrap();
        assert_eq!(mem.search("cancion", None, 5, None).len(), 1);
        assert_eq!(mem.search("musica", None, 5, None).len(), 1);
    }

    #[test]
    fn versiona_al_reemplazar() {
        let (mut mem, _) = tmp_memory();
        mem.remember("skill", "v1: usar curl para buscar", vec![], "x", None, vec![])
            .unwrap();
        let old_id = mem.list(None, 1)[0].id.clone();
        mem.remember("skill", "v2: usar web_search", vec![], "x", Some(&old_id), vec![])
            .unwrap();
        assert_eq!(mem.len(), 1);
        assert_eq!(mem.list(None, 1)[0].version, 2);
        assert_eq!(mem.superseded_count(), 1);
    }

    #[test]
    fn olvida_por_id() {
        let (mut mem, _) = tmp_memory();
        mem.remember("fact", "dato temporal", vec![], "x", None, vec![])
            .unwrap();
        let id = mem.list(None, 1)[0].id.clone();
        assert!(mem.forget(&id).unwrap());
        assert!(!mem.forget(&id).unwrap());
        assert_eq!(mem.len(), 0);
    }

    #[test]
    fn coseno_basico() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[1.0], &[]), 0.0);
    }

    #[test]
    fn tipo_invalido_falla() {
        let (mut mem, _) = tmp_memory();
        assert!(mem.remember("chorrada", "x", vec![], "s", None, vec![]).is_err());
    }
}
