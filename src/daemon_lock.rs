//! Exclusión entre procesos para impedir dos daemons sobre el mismo workspace.
use std::fs::{File, OpenOptions};
use std::path::Path;

use anyhow::{Context, Result, bail};

pub struct DaemonLock {
    // Mantener abierto el descriptor mantiene activo el flock.
    _file: File,
}

impl DaemonLock {
    pub fn acquire(workspace: &Path) -> Result<Self> {
        let dir = workspace.join(".agent");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("no se pudo crear {}", dir.display()))?;
        let path = dir.join("daemon.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("no se pudo abrir {}", path.display()))?;
        lock_file(&file)?;
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
fn lock_file(file: &File) -> Result<()> {
    use std::os::fd::AsRawFd;

    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;

    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }

    // flock es liberado automáticamente por el kernel al cerrar el descriptor,
    // incluso si el proceso termina abruptamente o la máquina se reinicia.
    let result = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(11) | Some(35)) {
        bail!("ya hay otro daemon activo en este workspace (.agent/daemon.lock)");
    }
    Err(error).context("no se pudo adquirir el bloqueo exclusivo del daemon")
}

#[cfg(not(unix))]
fn lock_file(_file: &File) -> Result<()> {
    bail!("el bloqueo de daemon solo está implementado para sistemas Unix")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn el_bloqueo_se_mantiene_mientras_vive_el_guardian() {
        let root = std::env::temp_dir().join(format!("agent-daemon-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let first = DaemonLock::acquire(&root).unwrap();
        assert!(DaemonLock::acquire(&root).is_err());
        drop(first);
        assert!(DaemonLock::acquire(&root).is_ok());
        let _ = std::fs::remove_dir_all(root);
    }
}
