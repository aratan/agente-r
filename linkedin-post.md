Hice un agente de IA que funciona 100% en mi ordenador. Sin nube, sin API keys, sin suscripciones.

Se llama local-agent y está escrito en Rust, sobre Ollama o llama.cpp. Y no es un chatbot: es un agente que piensa, actúa y observa, con herramientas de verdad:

🔧 Shell, ficheros, navegador (vía CDP)
📧 Correo, Telegram, voz (TTS/STT locales)
🧠 Memoria persistente entre sesiones con RAG (BM25 + embeddings)
🤖 Subagentes recursivos que se reparten tareas, cada uno con su modelo según el rol
📅 Agenda de tareas que corre en daemon
⚙️ Auto-extensión: crea sus propias herramientas en Python, sus skills y sus servidores MCP

Las decisiones de diseño que más me gustaron:

1. Auto-mejora ACOTADA. El agente extrae lecciones y skills reutilizables de sus propios turnos y las versiona, con historial y feedback (lo que sirvió, pesa más; lo que no, baja). Lo que NO hace: reescribirse a sí mismo ni auto-entrenarse. Esa parte de la "RSI abierta" no tiene evidencia pública; la que sí es práctica de ingeniería sólida, y es la que está implementada.

2. El hardware manda. En cada arranque mide RAM, núcleos y VRAM, y de ahí salen el presupuesto, cuántos subagentes corren en paralelo y el tamaño máximo de modelo. Mi RTX 4060 de 8 GB limita a ~19B y 2 subagentes simultáneos. Sin magia, sin prometer un agente que luego no corre.

3. Sin async, sin estado global, 4 dependencias. El conocimiento vive en ficheros de texto revisables: memoria en JSON, skills en Markdown, herramientas en Python. Si se rompe algo, se puede leer y arreglar con un editor.

4. Metas con evidencia obligatoria: el agente solo puede cerrar un objetivo si demuestra con pruebas qué hizo. Sin evidencia, se rechaza.

Todo el código es local, el estado es auditable y los límites son reales (rutas confinadas, máximo 3 descargas de modelos por proyecto, profundidad acotada).

Si te interesa construir agentes locales serios —sin la capa de humo—, dime y te muestro el repo.

#Rust #IA #LLM #Ollama #AgentesDeIA #IAlocal #OpenSource #SoftwareLocal
