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
Hackerman (default), Cyberpunk, Claude, Dracula, Nord, Light, Amber, Tokyo Night

## Estado actual del proyecto
<!-- Actualizá esta sección manualmente a medida que avanzás -->
- [x] Paso 1: CLI + cargo new
- [x] Paso 2: Welcome screen con ratatui
- [x] Paso 3: Layout 3 paneles estático
- [x] Paso 4: Panel Git con git2
- [x] Paso 5: PTY con portable-pty
- [x] Paso 6: Métricas con file watcher
- [x] Paso 7: Persistencia de sesiones (crear/listar/resumir vía
      `claude --session-id` / `--resume`, captura de tokens al cerrar)
- [x] Paso 8: Gestión de sesiones — eliminar y duplicar (fork del
      transcript reescribiendo el `sessionId`), unificadas con renombrar
      en un picker único con barra de búsqueda
- [x] Paso 9: Sesiones por proyecto — la welcome solo lista las del
      directorio donde se abre Bruce (`load_for_project`)
- [x] Paso 10: Preferencias persistentes (tema + visibilidad de paneles)
      en `<config>/bruce/config.json`
- [x] Paso 11: Refresco en vivo del panel Git (poll throttleado a 1s)
- [x] Paso 12: `bruce` sin subcomando levanta la TUI (alias de `bruce tui`)
- [x] Paso 13: Chequeo de nueva versión al arrancar (curl best-effort,
      cacheado 1/día en config), badge sobre el ASCII + bloque "App" al
      lado de Options con "Check for updates" / "Update to latest"
- [x] Paso 14: Detección del método de instalación (por ruta de
      `current_exe`) + auto-update in-app para brew/cargo (tecla U), comando
      manual para AUR/curl/PS; feedback de "Check for updates"

## Versionado
- SemVer, pre-1.0: `feat:` → bump minor, `fix:` → bump patch.
- Bumpear `version` en Cargo.toml EN EL MISMO commit del cambio (la
  versión se muestra en la welcome screen). Versión actual: 0.11.0.

## Distribución
Repo: https://github.com/soydanielsierradev/bruce-tui (rama `main`).

Hecho:
- [x] GitHub Actions: `release.yml` cross-compila en cada tag `v*` y
      publica binarios (linux-gnu, macOS intel+arm, windows-msvc)
- [x] Release `v0.8.0` publicado con los 4 binarios
- [x] `install.sh` (curl|sh) para Linux/macOS
- [x] `cargo install --git` funcionando
- [x] README con guía de instalación por SO
- [x] LICENSE (MIT)
- [x] Recetas en `packaging/`: Homebrew (`homebrew/bruce.rb`) y
      AUR (`aur/PKGBUILD`) con SHA256 reales

- [x] Aviso al arrancar si `claude` no está en el PATH (chequeo en
      `main.rs` vía `pty::claude_missing`, respeta `BRUCE_CMD`)
- [x] Instalador para Windows: `install.ps1` (baja + extrae + agrega al
      PATH), vía `irm ... | iex`
- [x] Release `v0.9.0` publicado (Latest) con los 4 binarios
- [x] Tap Homebrew publicado: repo `soydanielsierradev/homebrew-bruce`
      (fórmula en `Formula/bruce.rb`) → `brew install soydanielsierradev/bruce/bruce`

Pendiente:
- [ ] Publicar el paquete AUR `bruce-bin` (repo AUR + cuenta/SSH)
- [ ] Actualizar `actions/checkout` y `action-gh-release` a Node 24
      (deprecación de Node 20, no bloqueante hasta ~sep 2026)

Para cada release nuevo: bump de versión en Cargo.toml → commit → push →
`git tag vX.Y.Z` → `git push origin vX.Y.Z`. El workflow compila los
binarios, publica el release Y **actualiza el tap Homebrew solo** (job
`update-tap`, vía el secret `TAP_GITHUB_TOKEN`). El AUR todavía es manual:
actualizar `pkgver` + `sha256sums` en `packaging/aur/PKGBUILD`.