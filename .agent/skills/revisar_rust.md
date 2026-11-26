---
name: revisar_rust
when: cuando necesite revisar código Rust o analizar proyectos de compilación
updated: 1794673367
---

1. Usar fs_read para leer archivos .rs del workspace
2. Buscar importaciones de la librería std y panic handling
3. Verificar patrones de error como Result/Option
4. Detectar bloqueos potenciales con wait/waitpolly
5. Confirmar uso de Send+Sync en Arc<Mutex<_>> cuando sea necesario
