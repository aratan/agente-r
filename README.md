# local-agent

[![CI](https://github.com/aratan/agente-r/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/aratan/agente-r/actions/workflows/ci.yml)

Agente LLM **local** en Rust sobre **Ollama (:11434)** o **llama.cpp `llama-server` (:8083)**,
con RAG, buscador web gratuito, memoria persistente, auto-mejora acotada, subagentes
recursivos, agenda, y control de shell, ficheros, navegador, multimedia, escritorio,
correo, Telegram y voz.

Sin async, sin estado global, 4 dependencias (`anyhow`, `serde`, `serde_json`, `ureq`,
`tungstenite`) y todo el conocimiento en ficheros de texto revisables.

## Arranque rápido

```bash
cargo run -- --info                                  # hardware, modelos y modelo por rol
cargo run -- "lista los ficheros y dime qué hace main.rs"
cargo run -- --goal "deja cargo test en verde en src" # persigue la meta
cargo run -- --daemon                                # ejecuta la agenda periódica
cargo run -- --telegram                              # TELEGRAM_BOT_TOKEN=...
cargo run -- --voice                                 # conversación hablada
cargo run -- --pull qwen2.5-coder:7b                 # descarga (máx. 3 por proyecto)
```

## Comandos del REPL

`/goal <texto>` · `/model <nombre>` · `/models` · `/models` · `/info` · `/memory`
`/improve` · `/new` · `/help` · `/exit`

## Qué hace

### RAG (recuperación aumentada)
- Memoria en `.agent/memory.json` con **BM25** léxico (acentos normalizados) **+ embeddings**
  (`nomic-embed-text` vía Ollama) con similitud coseno. Sin embeddings configurados, sigue
  funcionando en modo léxico.
- `rag_index` indexa ficheros de texto del workspace en fragmentos solapados (`kind=doc`).
- En cada turno se recuperan los 5 recuerdos más relevantes y se inyectan en el prompt.
- Herramientas: `memory_search`, `memory_save`, `memory_list`, `memory_forget`,
  `memory_feedback` (sube/baja el peso de lo que sirve), `rag_index`.

### Buscador web gratuito y sin clave
- `web_search` usa **DuckDuckGo → Brave → Bing** a través del Chrome ya controlado por CDP
  (sin API key y sin TLS propio: el registro de crates disponible no resuelve `rustls`).
- **Cortesía con el servicio**: pausa mínima de 2 s entre búsquedas
  (`AGENT_WEB_MIN_INTERVAL_MS`), caché de 10 minutos y máximo 10 resultados.
- `web_fetch` abre una URL y devuelve su texto visible.

### Memoria persistente entre sesiones
- JSON versionado en `.agent/`, con `id` estable (mismo texto+tipo ⇒ no se duplica),
  contador de usos, utilidad e historial de versiones sustituidas.
- Tipos: `fact`, `lesson`, `skill`, `doc`. Límite configurable (`AGENT_MEMORY_MAX`, 500).
- Si el fichero se corrompe, se aparta como `memory.json.corrupto-*` en vez de perderse.

### Auto-mejora recursiva (RSI), acotada y auditable
El estado del arte a noviembre de 2026 es claro: la auto-mejora **acotada** es práctica de
ingeniería (memoria, skills, harness, evaluadores), mientras que la RSI **abierta** no está
demostrada y sigue limitada por evaluación, colapso y cómputo (survey RSI 2026; METR 2026;
IEEE Spectrum 2026; IBM 2026). Este proyecto implementa la parte sólida:

1. Tras un turno con herramientas, el agente hace una llamada extra a **sí mismo** con un
   registro reducido a `memory_*`: extrae 0-3 `lesson`/`skill`/`fact` reutilizables.
2. Cada mejora puede sustituir una anterior (`replaces`), subiendo versión y **archivando**
   la vieja en `superseded` (reversible con git o `memory_forget`).
3. `memory_feedback` ajusta el peso de lo que demostró ser útil.
4. Las `skill` se inyectan como índice en el prompt de los siguientes turnos.

Lo que **no** hace y por qué: no reescribe su propio código ni sus pesos, no cambia su
evaluador ni se auto-entrena. Eso es exactamente la parte de la RSI sin evidencia pública,
y hacerlo sin supervisión sería irresponsable. Las mejoras son datos, no código, y se revisan.

### Subagentes en varios niveles
- `delegate` (uno) y `delegate_many` (varios en paralelo, con `thread::scope`).
- Cada subagente tiene su **propio modelo según rol** (`planner`, `worker`, `coder`, `fast`)
  y su propio juego de herramientas (el `fast` no puede escribir ni borrar).
- Límites: `--max-depth` (3), `--max-agents` (12), y paralelismo real derivado del hardware.
- El hardware se **mide en cada arranque** (`/proc/meminfo`, núcleos, `nvidia-smi`):
  presupuesto = RAM·0.5 + VRAM·0.85; de ahí salen el nivel (Small/Medium/Large),
  los subagentes simultáneos y el tamaño máximo de modelo. En este equipo (24 núcleos,
  32 GB, RTX 4060 8 GB) salen 2 en paralelo y modelos ≤ ~19B.
- **Máximo 3 descargas de modelos por proyecto**, con registro persistente en
  `.agent/models.json` y validación antes de descargar (`--pull`, `model_pull`).

### Objetivo persistente y ejecución prolongada (`/goal`, `--goal`, `--daemon`)
- El objetivo raíz y el historial de ciclos se guardan en `.agent/goal.json`; repetir el
  mismo `--goal` reanuda el estado, no lo reinicia. Un objetivo activo impide sustituirlo
  silenciosamente por otro.
- `--goal-attempts` define el tamaño de cada lote de trabajo, no el límite total de la misión.
- `--goal "objetivo" --daemon` inicia y persigue el objetivo en lotes sucesivos; el daemon
  también reanuda un objetivo activo al reiniciarse. Los resultados se anexan a
  `.agent/runs/goal.log`.
- `--daemon-once` ejecuta un lote y sale; `--daemon` mantiene el proceso vivo.
- Supervisor de usuario systemd disponible en `contrib/systemd/local-agent-goal.service`.
  Ajusta la ruta del binario y workspace; después instala con:
  `mkdir -p ~/.config/systemd/user && cp contrib/systemd/local-agent-goal.service ~/.config/systemd/user/`
  `systemctl --user daemon-reload && systemctl --user enable --now local-agent-goal.service`.
  Para mantenerlo activo tras cerrar sesión y arrancarlo al iniciar el equipo:
  `loginctl enable-linger "$USER"` (puede requerir autorización del sistema).
  Consulta el estado con `systemctl --user status local-agent-goal.service` y logs con
  `journalctl --user -u local-agent-goal.service -f`.
- El daemon adquiere un bloqueo exclusivo del workspace (`.agent/daemon.lock`) mediante `flock`; una segunda instancia falla sin tocar el objetivo. El kernel libera el bloqueo al morir el proceso, por lo que no quedan locks obsoletos tras un crash.
- Los resúmenes de los últimos 500 ciclos se conservan para continuidad; el contador total
  es acumulativo. Se archivan hasta 100 objetivos completados.
- Solo el agente raíz puede cerrar el objetivo global mediante `goal_done`. Los subagentes
  devuelven resultados al agente raíz.
- El agente raíz dispone de `goal_task` para crear/actualizar tareas, dependencias y checks
  de aceptación, iniciar tareas, aportar evidencia y consultar el plan. El grafo se guarda en
  `.agent/goal.json`; rechaza dependencias inexistentes/cíclicas y no permite saltárselas.
- `goal_verify` es un evaluador determinista separado: ejecuta checks exactos de una lista
  permitida (cargo test/clippy/check/fmt, git diff --check, pytest, npm test, go test y make test).
  Una tarea no queda verificada por una declaración textual; los checks deben terminar con exit 0.
  Cambiar el título, las dependencias o los checks invalida la verificación anterior.
- `goal_done` no permite cerrar el objetivo mientras haya tareas pendientes, en curso o no verificadas.
- Cada ciclo recibe el plan y el diario de fallos para reanudar el trabajo tras una interrupción.
- El governor pausa el objetivo tras tres resultados consecutivos idénticos (incluidos errores
  del proveedor), registra el motivo y evita quemar ciclos sin progreso. Para reanudar, vuelve
  a pasar el mismo objetivo con `--goal`; un objetivo pausado no se sustituye por otro.
- Presupuesto por defecto: 120 ciclos por hora y 50.000 ciclos acumulados por objetivo.
  Ajustable con `AGENT_GOAL_MAX_CYCLES_PER_HOUR` y `AGENT_GOAL_MAX_CYCLES_TOTAL`; al
  agotarse, el objetivo se pausa y solo se reanuda explícitamente tras revisar el consumo.
- ### Ensayo prolongado de persistencia (manual)
- Ejecuta un harness sintético de larga duración que reabre `.agent/goal.json`, completa ciclos y simula una interrupción cada 17 iteraciones entre la intención y el resultado de una herramienta.
- El harness reconcilia únicamente operaciones sintéticas; no llama a proveedores, red ni herramientas externas.
- Por defecto dura 48 horas y registra cada minuto duración, iteraciones, recuperaciones, tamaño del estado y pico de RSS en Linux.
- Ejecutar: `scripts/goal-soak.sh`. Ajustar con `AGENT_GOAL_SOAK_SECONDS` y `AGENT_GOAL_SOAK_INTERVAL_MS`.
- El test es ignorado por CI de forma deliberada. Una ejecución corta valida el harness, pero **no** satisface el criterio de soak de varios días. Registrar duración y resultado real antes de cerrar la issue.

El diario causal conserva hasta 200 fallos con fingerprint, categoría, síntoma y estrategia
  probada. `goal_failure` bloquea repetir el mismo fingerprint+estrategia salvo que el agente
  declare y describa un cambio de condiciones; los últimos fallos se inyectan en los ciclos
  siguientes para reducir bucles de ensayo idéntico.
- **Límite pendiente:** el evaluador solo ejecuta checks de la lista permitida; todavía no valida
  de forma general objetivos no ejecutables ni interpreta criterios arbitrarios.

### Agenda de tareas
- `schedule_add` / `schedule_list` / `schedule_remove` y `--every 30m --task "..."`.
- `--daemon` ejecuta lo vencido cada 20 s con un subagente, guarda log en
  `.agent/runs/<id>.log` y actualiza la próxima ejecución. `--daemon-once` ejecuta
  los vencimientos una vez y sale.
- Intervalos: `30s`, `5m`, `2h`, `1d`; mínimo 10 s.

### Autoextensión en Python
- **Tools**: `tool_create` escribe `.agent/tools/<name>.py` (+ `.json` con el esquema).
  El script lee los argumentos JSON por **stdin** y escribe el resultado por **stdout**.
  Se **autoprueba** antes de registrarse; si falla, se borra y se devuelve el error.
  `tool_list`, `tool_remove`.
- **Skills**: `skill_create` guarda procedimientos en `.agent/skills/<name>.md` con su
  condición de uso; `skill_list`, `skill_read`. Aparecen en el prompt automáticamente.
- **MCP**: `mcp_create` genera un servidor MCP en Python (JSON-RPC por stdio) o instala el
  código dado, lo arranca y verifica el `initialize` + `tools/list`. `mcp_servers`,
  `mcp_tools`, `mcp_call`.

### Sistema
| Área | Herramienta | Requisitos |
|---|---|---|
| Shell / ficheros | `shell`, `fs_read`, `fs_write`, `fs_list`, `fs_mkdir`, `fs_delete` | — |
| Navegador | `browser` (open/text/click/type/eval/screenshot) | Chrome/Chromium |
| Música y cine | `media` (status/play/pause/next/volume/open) | `playerctl` + `mpv`/`vlc` |
| Escritorio | `gui` (windows/focus/click/type/key/scroll/screenshot) | `xdotool` (+`wmctrl`), `ffmpeg` |
| Correo | `mail` (folders/list/read/send) | `himalaya` configurado |
| Telegram | `telegram` (send/poll) y `--telegram` | `curl` + `TELEGRAM_BOT_TOKEN` |
| Voz | `voice` (say/listen) y `--voice` | `piper` (voz femenina) o `espeak-ng`; `whisper-cli` + `arecord` |
| Agenda | `schedule_*` | — |
| Modelos | `model_pull`, `system_info` | Ollama |

Voz: TTS prioriza **Piper** (`AGENT_PIPER_MODEL=/ruta/voz.onnx`, p. ej. `es_ES-davefx-medium`
o una voz femenina `es_ES-*`); si no está, usa `espeak-ng -v es+f3`. STT con **whisper.cpp**
(`AGENT_WHISPER_MODEL=/ruta/ggml-base.bin`) grabando con `arecord`.

## Diseño (patrones + KISS)

```
main.rs        CLI, REPL, daemon, canales     Inyección de dependencias
orchestra.rs   subagentes, presupuesto        Mediator + límites por hardware
agent.rs       pensar→actuar→observar→RSI     Template Method
provider.rs    Ollama | llama.cpp             Strategy + Adapter (+ Embedder)
tools.rs       Tool + ToolRegistry            Command + Registry (por rol)
memory.rs      BM25 + coseno                  Repository
web.rs         DDG/Brave/Bing por CDP         Strategy + caché/throttle
models.rs      elección por rol y hardware    Policy + registro de descargas
ext.rs         tools Python, skills, MCP      Plugin/Pipeline (stdin→stdout)
scheduler.rs   tareas periódicas              Scheduler persistente
goals.rs       metas con evidencia            State + retry con cambio de estrategia
```

## Seguridad y límites reales

- Rutas confinadas al workspace (se rechazan `..` y absolutas externas); **no** resuelve
  symlinks y `sh` puede escribir fuera si el comando lo pide: no es un sandbox de seguridad.
- El navegador solo acepta `http(s)://` y `file://`; comparte una pestaña entre subagentes.
- Las tools Python se ejecutan con los permisos del usuario: revísalas antes de confiar.
- Telegram/voz requieren credenciales y binarios externos; sin ellos las herramientas
  responden con un error explicativo, no se cae el agente.

## Verificación

```bash
cargo test                     # 38 tests: memoria, RAG, modelos, hardware, agenda,
                               # orquestación, agente (proveedor guionizado), Python y MCP
cargo test -- --ignored        # CDP real: abre file://, lee y hace click
```

Estado comprobado en este equipo: `cargo build` y `cargo test` en verde; elección de modelo
excluyendo embeddings; tools Python y servidor MCP creados y ejecutados de verdad en tests.
