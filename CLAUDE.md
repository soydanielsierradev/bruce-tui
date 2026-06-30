# Bruce — TUI workspace para Claude Code

## Qué es este proyecto
Aplicación TUI en Rust con 4 paneles: Git (izquierdo), Claude Code
embebido en PTY (centro), File Manager (derecho) y Terminal (full-width
abajo, segundo PTY). El conteo de tokens vive embebido en la welcome (se
lee del transcript JSONL de Claude, sin watcher). Pantalla de bienvenida
con 4 bloques en grilla 2×2 (Options, Settings, Documentation, Skills):
abrir/crear/renombrar/duplicar/eliminar sesiones vía picker con búsqueda,
preferencias de look en Settings (tema, bordes, layout, file icons),
repo + atajos en Documentation y manage/install de skills en Skills.
Transición de loading al abrir una sesión.

## Stack
- ratatui + crossterm — TUI
- vt100 — emulador de terminal (parsea ANSI del PTY a una grilla propia)
- clap — CLI (`bruce tui`)
- git2 — panel Git (sin features default, evita openssl-sys en Windows)
- portable-pty — embeber proceso `claude` y la terminal inferior
- serde / serde_json — sesiones en `~/.config/bruce/sessions/` + config
- uuid (v4) — id de sesión que se le pasa a `claude --session-id`
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
src/ui/ — layout (workspace.rs), welcome screen, temas
src/session/ — struct Session, persistencia JSON
src/panels/ — git.rs, files.rs (file manager), metrics.rs
              (metrics ya no es panel — quedó como helper para leer
              tokens del transcript JSONL de Claude)
src/pty/ — spawn y comunicación PTY (Claude + terminal inferior)
src/skills/ — install/manage de skills (ledger en skills.json)
src/update/ — chequeo de versión y auto-update por método de instalación
src/config/ — preferencias persistentes en `config.json`

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
- [x] Paso 15 (v0.13.0): overhaul de UX. Bloque Settings (tema vía modal,
      sync de colores OSC, estilo de borde, ancho de paneles, title/footer
      bars; persiste al toggle). Welcome rediseñada: 3 bloques (Options,
      Settings, Documentation), picker "Open session", tagline con link al
      autor (mouse capture solo en welcome), foco por color, dim de fondo
      tras los dialogs. Bloque App eliminado (Check/Update pasaron a Options).
      Loading full-screen + overlay "waking Claude" en el panel. Panel Claude
      sin bordes (Git/Metrics enmarcados), cursor oculto mientras streamea,
      MCP activos en métricas. Navegación Ctrl+1/2/3 (+Tab), scroll
      Shift+PageUp/Down. Tokyo Night: `modified` en cyan.
- [x] Paso 16 (v0.14.0): gestor de skills. Bloque Skills en la welcome
      (Manage + Install). Install corre el comando en un **PTY interactivo**
      (reusa el pipeline del panel Claude: responde prompts tipo "which
      agent?", output en vivo, navegable con flechas); al terminar
      **auto-desactiva** la skill (rename `SKILL.md` → `SKILL.md.disabled`) y la
      registra en un ledger (`<config>/bruce/skills.json`); puente
      `~/.agents/skills` → `~/.claude/skills` (donde Claude lee). Detección por
      carpeta nueva O `SKILL.md` modificado durante el install (mtime). Manage:
      lista solo lo instalado por Bruce, preview del `SKILL.md` con header
      (name+description) y word-wrap navegable, enable/disable/delete, comandos
      dentro del modal. Fix: paste multilínea en el panel Claude entra como un
      solo mensaje (bracketed paste en Unix; coalescing de teclas en Windows,
      donde crossterm no emite `Event::Paste`). Labels de métricas en inglés.
- [x] Paso 17 (v0.15.0): rediseño de workspace. **Métricas como panel
      eliminadas** (también la dep `notify` y el watcher); el módulo
      `metrics.rs` sobrevive como helper que lee el transcript JSONL y
      alimenta el contador de tokens en la welcome. Nuevo panel **File
      Manager** a la derecha que navega por carpeta, abre archivos en el
      editor (vía shell en Windows para soportar `code`, etc.), e iconos
      de archivo configurables (emoji por default, Nerd Font opt-in en
      Settings). Nuevo panel **Terminal** full-width abajo, backed por un
      segundo PTY, toggle con `Ctrl+T` y dim del fondo cuando hay overlay.
      Overlay `Ctrl+F`: **fuzzy file-search** sobre todo el proyecto
      (subsequence match, cap 200 resultados). Panel enum pasa a 4
      variantes: `Git`, `Claude`, `FileManager`, `Terminal`.
- [x] Paso 18 (v0.15.1): pulido. Border style **"none"** (borderless)
      sumado a los existentes y refresh de la doc de keybindings. Fix:
      los Ctrl+digit de cambio de panel se reportan vía el **keyboard
      protocol** (no llegaban con terminales que negocian kitty/CSI-u).
      Fix: el file walker ahora **incluye archivos dentro de carpetas
      hidden** (antes los saltaba enteros y se perdían cosas legítimas
      como `.github/workflows/*`).
- [x] Paso 19 (v0.16.0): UX en tiempo real + skills per-project.
      **File manager live refresh**: `FileManager::tick()` ahora re-lee el
      directorio visible cada 1.5s (cadencia separada del walk completo
      del proyecto, que sigue corriendo cada 30s para alimentar `Ctrl+F`).
      `reload_dir` mantiene el reset-al-top para navegación; `refresh_dir`
      es nuevo y preserva la selección por nombre — los archivos
      creados/borrados/renombrados aparecen sin tener que salir y reabrir.
      **Skills activation per-project**: la library en
      `~/.claude/skills/<folder>/` queda **siempre disabled**
      (`SKILL.md.disabled`); el toggle `E` en Manage copia la library a
      `<project>/.claude/skills/<folder>/` y habilita `SKILL.md` ahí
      (donde Claude descubre skills project-scoped). Deactivate borra
      solo la copia del proyecto. Estado en Manage = "¿existe
      `<project>/.claude/skills/<folder>/SKILL.md`?". `enable_skill`
      eliminado (la library nunca se re-habilita). Funciones nuevas en
      `src/skills/mod.rs`: `activate_in_project`, `deactivate_in_project`,
      `is_active_in_project`, `project_skills_dir` (+ helper privado
      `copy_dir_recursive`). Copia en vez de symlink — portable en Windows
      y el equipo puede commitear `.claude/skills/` si quiere compartirlo.
- [x] Paso 20 (v0.16.1): higiene + robustness pasada post-release.
      `.gitattributes` con `* text=auto eol=lf` para cortar el drama
      CRLF↔LF al editar desde Windows + Linux. `fetch_latest()` ahora
      cappea `curl` con `--connect-timeout 5 --max-time 10` para que el
      worker del update check no se cuelgue si GitHub no responde.
      `FileManager::start_walk()` gatea con `JoinHandle::is_finished()`
      para no spawnear walks paralelos sobre el mismo `Arc<Mutex<…>>`.
      `respond_to_queries()` ahora persiste un tail buffer entre
      lecturas del PTY, así las secuencias ESC de device-attributes que
      cruzan el borde entre dos chunks (chunk N termina en `\x1b[`,
      chunk N+1 arranca con `c`) ya no se pierden silenciosamente —
      pura `detect_queries(chunk, prev_tail, cursor)` testeada con 10
      casos. `Session::claude_args(resume)` extraído de
      `WorkspaceState::new` para fijar por test que `--session-id` y
      `--resume` no se invierten. `#![deny(unsafe_code)]` crate-wide.
      Tests nuevos para `encode_key()` cubriendo Ctrl letters, DECCKM
      cursor mode y tilde nav keys. README documenta `BRUCE_CMD` y
      `BRUCE_EDITOR`. Total: 44 → 66 tests.

## Commits
- Conventional commits SIEMPRE: `feat:`, `fix:`, `docs:`, `chore:`,
  `refactor:`, `test:`. Nunca atribución de IA (`Co-Authored-By`, etc.).
- Un commit por unidad lógica. Si varios concerns caen en el mismo archivo
  y no se pueden separar por hunks, agrupá por el concern dominante y
  describí el resto en el cuerpo.
- Asunto en imperativo y en inglés. Cuerpo: explicá el PORQUÉ (qué motivó
  el cambio, qué rompía), no solo el qué.

## Versionado
- SemVer, pre-1.0: `feat:` → bump minor, `fix:` → bump patch. Si un release
  junta varios cambios, manda el de mayor impacto (un `feat:` entre `fix:`
  hace minor).
- Bumpear `version` en `Cargo.toml` **y** `Cargo.lock` EN EL MISMO commit
  del cambio (la versión se muestra en la welcome screen).
  Versión actual: 0.16.1.

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
- [x] Arch cubierto por `install.sh` (baja el binario glibc) y
      `cargo install --git`.

Pendiente:
- [ ] Actualizar `actions/checkout` y `action-gh-release` a Node 24
      (deprecación de Node 20, no bloqueante hasta ~sep 2026)

### Cómo publicar un release (EN ORDEN)
1. Bump de `version` en `Cargo.toml` + `Cargo.lock` → commit.
2. `git push origin main`.
3. `git tag vX.Y.Z` → `git push origin vX.Y.Z`. **El tag es OBLIGATORIO y es
   lo que dispara el workflow** — nunca lo saltees, ni aunque el usuario vaya
   a compilar local.
4. El workflow (`release.yml`) compila los binarios (linux-gnu, macOS
   intel+arm, windows-msvc), publica el release **con el cuerpo VACÍO** y
   actualiza el tap Homebrew solo (job `update-tap`, secret
   `TAP_GITHUB_TOKEN`). El runner de Windows es el más lento: el `.zip`
   windows-msvc puede aparecer minutos después que el resto.
5. **Escribir las notas a mano** — el workflow NO las genera. Seguí la
   plantilla de abajo y aplicalas con
   `gh release edit vX.Y.Z --notes-file <archivo>`. Un patch chico sin
   novedades visibles igual lleva notas breves: no te saltees este paso (le pasó a v0.11.1 y quedó sin cuerpo hasta que se corrigió a mano).

### Plantilla de notas de release
Van en **inglés** (como todos los releases anteriores). Bullets con lead-in
en negrita + descripción orientada al USUARIO, no al código. Referencia:
`gh release view v0.11.0`.

```markdown
**Bruce** is a terminal workspace for [Claude Code](https://docs.claude.com/claude-code).

### What's new in vX.Y.Z

- **<Short title>**: <one sentence, user-facing, what changed and why it helps>.
- **<Short title>**: <...>.

### Install

- **macOS:** `brew install soydanielsierradev/bruce/bruce`
- **macOS / Linux:** `curl -fsSL https://raw.githubusercontent.com/soydanielsierradev/bruce-tui/main/install.sh | sh`
- **Windows (PowerShell):** `irm https://raw.githubusercontent.com/soydanielsierradev/bruce-tui/main/install.ps1 | iex`
- **Any platform (Rust):** `cargo install --git https://github.com/soydanielsierradev/bruce-tui`

> Requires the Claude Code CLI (`claude`) on your PATH.
```