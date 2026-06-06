# Bruce — TUI workspace para Claude Code

## Qué es este proyecto
Aplicación TUI en Rust con 3 paneles: Git (izquierdo), Claude Code 
embebido en PTY (centro), Métricas de tokens (derecho). 
Pantalla de bienvenida con selección de sesiones y temas.

## Stack
- ratatui + crossterm — TUI
- clap — CLI (`bruce tui`)
- git2 — panel Git
- portable-pty — embeber proceso claude
- notify — file watcher para métricas
- serde_json — sesiones en ~/.config/bruce/sessions/
- tokio — async runtime
- anyhow — manejo de errores

## Reglas de código
- Nunca uses .unwrap() — siempre ? o manejo explícito de errores
- Compila en macOS y Linux, sin APIs exclusivas de un SO
- Corré `cargo check` después de cada cambio importante
- Estado compartido entre threads: Arc<Mutex<T>>
- Construí de afuera hacia adentro — cada paso debe compilar antes 
  del siguiente

## Estructura
src/main.rs — entry point + clap
src/app.rs — estado global y event loop
src/ui/ — layout, welcome screen, temas
src/session/ — struct Session, persistencia JSON
src/panels/ — git.rs, claude.rs, metrics.rs
src/pty/ — spawn y comunicación PTY

## Sesiones
JSON en ~/.config/bruce/sessions/<id>.json
Campos: id, name, project_path, branch, created_at, 
        last_used, tokens_used, scrollback (base64)

## Temas disponibles
Dark, Dracula, Nord, Light, Amber

## Estado actual del proyecto
<!-- Actualizá esta sección manualmente a medida que avanzás -->
- [x] Paso 1: CLI + cargo new
- [x] Paso 2: Welcome screen con ratatui
- [x] Paso 3: Layout 3 paneles estático
- [x] Paso 4: Panel Git con git2
- [x] Paso 5: PTY con portable-pty
- [x] Paso 6: Métricas con file watcher