# Puntos de mejora — nexus-ai-gateway

> Auditoría técnica del 2026-08-15 (6 dimensiones, análisis independiente sobre las
> ~18.168 líneas). Este archivo es la lista accionable de mejoras. Cada punto cita
> `archivo:línea` verificable. Todas las herramientas propuestas son **gratis / open-source**.
>
> **Veredicto global:** avanzado sólido con tramos profesionales. El techo del proyecto
> no está en el código — está en la **gobernanza automatizada** y en la **observabilidad**.

---

## Tablero por dimensión

| Dimensión | Nivel | Resumen |
|---|---|---|
| Arquitectura Rust | avanzado | Circuit breaker con CAS, cero `unsafe`; lastrada por una función SSE de 755 líneas y sin trait de proveedor. |
| Seguridad | avanzado | SSRF por capas + systemd endurecido; manchado por telemetría phone-home oculta por defecto. |
| DevOps / CI-CD | avanzado (DIY) | Supply-chain madura; auto-release sin gate y auto-sync frágil que contradice la política de ramas. |
| Testing | avanzado | 454 tests con disciplina real; los de integración son teatro (no ejercitan el pipeline). |
| Observabilidad | intermedio | Métricas Prometheus reales; falta trazas (3er pilar) y hay bombas de cardinalidad (DoS de memoria). |
| Release / Gobernanza | profesional | Versionado automático real; gobernanza de 1 persona algo teatral y docs desincronizadas. |

---

## 🔴 Crítico — atender primero

### 1. Telemetría opt-out, ofuscada y con token en repo público
- **Evidencia:** `src/config.rs:578-599`, `src/telemetry/beacon.rs:1-7`, `README.md:6`.
- **Qué pasa:** la telemetría está **encendida por defecto** (`telemetry_enabled = unwrap_or(true)`).
  La URL del beacon y un token de auth están **ofuscados con `obfstr`** («not visible in binary
  strings»). El token está commiteado en `config.rs` (repo público) → no autentica nada.
- **Por qué importa:** en open-source la confianza es la moneda. Ocultar el phone-home del binario
  es un antipatrón; quien clone y ejecute te envía datos sin poder descubrirlo.
- **Arreglo:** invertir el default a **opt-in** (apagada salvo activación explícita); quitar el
  token del binario; publicar el endpoint de forma transparente; documentar en README cómo
  desactivarla. Alternativa: eliminar la telemetría del todo.

### 2. Auto-release y auto-deploy sin aprobación humana
- **Evidencia:** `.github/workflows/auto-version.yml:144-193`, `.github/BRANCH_PROTECTION.md`.
- **Qué pasa:** cada push a `main` auto-bumpea versión, hace `git push origin main --tags` y publica
  un GitHub Release público, **sin gate**. Contradice tu propio `BRANCH_PROTECTION.md` («no direct
  pushes»): o la protección no está activa, o esos pushes fallarían.
- **Arreglo:** un **GitHub Environment `production` con required reviewers** detrás del step de
  release. Un humano aprueba la promoción. Esfuerzo: bajo (~10 min).

---

## 🟠 Alto

### 3. Auto-sync de 3 capas que compite consigo mismo
- **Evidencia:** `scripts/hooks/pre-push:93-174`, `scripts/nexus-git-sync.sh:130-175`.
- **Qué pasa:** el `pre-push` lanza un bucle de `git pull` en background *mientras* un daemon sondea
  cada 60s; ambos disparan `post-merge`, que reconstruye y **reinicia el servicio**. Builds y
  reinicios concurrentes con carrera en la instalación del binario. Un `git push` no debe desplegar.
- **Arreglo:** **merge queue nativo de GitHub** (gratis) serializa merges → borra el daemon entero.

### 4. Los tests de integración son teatro
- **Evidencia:** `tests/integration_test.rs:48-51`.
- **Qué pasa:** montan `wiremock` pero **nunca invocan `proxy_handler`** — el test lo admite. El
  corazón (retry, circuit breaker, streaming) no tiene cobertura end-to-end.
- **Arreglo:** `wiremock` ya está como dev-dependency — monta el Router de Axum real y ejercítalo con
  `tower::ServiceExt::oneshot`.

### 5. Sin trazas + bombas de cardinalidad (DoS de memoria)
- **Evidencia:** `src/proxy/mod.rs:245-249`, `src/proxy/edit_metrics.rs:61`, `src/main.rs:219`.
- **Qué pasa:** `tracing` se usa solo como logger (no hay OpenTelemetry, ni spans). Las métricas se
  etiquetan con `model` y con el nombre de fichero — **controlados por el cliente**: series
  ilimitadas → agota la RAM. Los módulos de resiliencia (retry, circuit breaker, rate limit,
  concurrency) no emiten ninguna métrica.
- **Arreglo:** validar `model` contra la lista configurada antes de etiquetar; logs JSON con
  `correlation-id`; emitir métricas en los módulos de resiliencia (`nexus_retries_total`,
  `nexus_circuit_breaker_state`); añadir TTFT (time-to-first-token) al streaming.

### 6. Función SSE de 755 líneas + sin trait de proveedor
- **Evidencia:** `src/proxy/streaming.rs:186-941`, ~25 sitios `UpstreamType` en transform/retry/discovery.
- **Qué pasa:** `create_sse_stream` es imposible de testear por unidades. Añadir un proveedor obliga a
  editar ~25 `match` dispersos en vez de implementar un trait.
- **Arreglo:** extraer una máquina de estados `SseTranslator` testeable; definir un
  `trait UpstreamProvider { build_request, parse_error, forward_headers, probe_limit }` con enum-dispatch.

---

## 🟡 Medio

- **Cero pinning de acciones por SHA** en los workflows (`@v7`, `@stable`, `@v2`…). Con un job que
  tiene `contents: write` y empuja a `main`, un tag repunteado es RCE. Irónico dado lo estricto que es
  el lado Rust. → `pinact` para pinnear + `zizmor` para auditar workflows. `.github/workflows/*.yml`.
- **`rust-toolchain.toml` fija 1.96 pero el CI instala `@stable`** → el CI no prueba lo que dice.
  Usa `@1.96`. `.github/workflows/ci.yml`.
- **nextest configurado pero sin usar:** existe `.nextest.toml` con perfil `ci` y junit, pero el CI
  corre `cargo test`. → `cargo nextest run --profile ci` + subir el junit.
- **Sin `--locked` en ningún build** → los builds "reproducibles" no lo son. Fix trivial.
- **Cero medición de cobertura en CI.** → `cargo-llvm-cov` con umbral (`--fail-under-lines`).
- **`install-service.sh` modifica el `~/.bashrc`** del usuario (envuelve la función `claude` con
  `--effort max`). Un instalador de servicio no debe alterar el shell. `scripts/install-service.sh:159-181`.
- **CORS mal formado panica en arranque** (`.parse().expect(...)`). → devolver error manejado.
  `src/main.rs:393`.
- **`SECURITY.md` desincronizado:** declara soportada la 0.13.x (versión real 0.28+), describe el SSRF
  sin el fix de DNS-rebinding, y pide «email the maintainer» sin dirección. → activar GitHub Private
  Vulnerability Reporting y actualizar la tabla.
- **`/metrics` y `/analytics` en el mismo puerto que el plano de datos**, sin auth (solo el allowlist
  de IP opcional). → puerto admin separado. `src/main.rs:420`.
- **`/health` devuelve OK aunque todos los upstreams estén caídos** (el health-check solo corre al
  arrancar y solo loguea). → `/readyz` con re-chequeo periódico. `src/health.rs`, `src/main.rs:720`.

---

## 🟢 Bajo

- Rigor bash inconsistente: los hooks `sh` no tienen `set` alguno; los scripts nuevos sí usan
  `set -euo pipefail`. (Reflejo de que el cascarón lo hizo un modelo y el resto otros.)
- 26 `allow(dead_code)` «reservado para PHASE 3.5» con variantes de error que nunca se construyen.
- Parsing de versión frágil (`grep '^version' Cargo.toml`), se rompe con `version.workspace = true`.
- Gobernanza teatro: CODEOWNERS + branch protection exigen 1 aprobación de code owner en un repo de 1
  persona (el único owner).
- `.hardening-epic-progress.md` quedó en la raíz (épica de v0.13.0, 2026-04-24).
- Falta `CODE_OF_CONDUCT.md` (health-check de comunidad de GitHub).

---

## Lo que hiciste MUY bien (conservar y replicar)

1. **Cero `unsafe`** en 18k líneas.
2. **Circuit breaker** con contador de generación + CAS (un solo probe, descarta obsoletos). `src/circuit_breaker.rs:72`.
3. **SSRF por capas** con resolución DNS que rechaza IPs privadas/metadata. `src/web_fetch.rs`.
4. **systemd unit endurecido** nivel pro (`NoNewPrivileges`, `ProtectSystem=strict`, anti crash-loop). `scripts/nexus-ai-gateway.service`.
5. **Supply chain madura:** `deny.toml` con política real + dependabot agrupado + `deps-monitor.yml` (cron que abre issue). 
6. **Métricas Prometheus reales** en `/metrics` + logrotate.
7. **454 funciones de test** con regresión ligada a issues + un property test genuino.
8. **Versionado automático** por conventional commits + changelog de fuente única. `scripts/generate-changelog-entries.sh`.
9. **Config lock-free** con `ArcSwap` + recarga en caliente por `SIGHUP`.
10. **`sync-from-release.sh`** verifica que el binario ejecuta en el glibc del host antes de instalar.

---

## Hoja de ruta a enterprise — sin gastar (resumen)

**Esta semana (bajo esfuerzo, alto impacto):** telemetría → opt-in · Environment con reviewer
· merge queue · SHA-pin (`pinact`) + `zizmor` · secret scanning (`gitleaks`) · `--locked` + `Swatinem/rust-cache`.

**Este mes (medio):** `release-please`/`cargo-dist` (reemplaza los 6 scripts bash y el auto-push)
· test e2e real (`tower::ServiceExt::oneshot`) · `cargo-llvm-cov` + `cargo-nextest` · logs JSON +
correlation-id · framework `pre-commit`.

**Cuando escale (mayor):** `trait UpstreamProvider` · descomponer la función SSE · OpenTelemetry
(→ Tempo/Jaeger) + TTFT · `cosign` + SLSA attest + SBOM (`syft`) + build matrix · `cargo-fuzz` + `cargo-mutants`.

> **Nota clave:** tu propio `CC_Actions/06_CI_CD_Replication_Blueprint.md` (en el repo `_docs`) ya
> identificó la mayoría de estas herramientas correctas (cargo-dist, cosign, SHA-pin, CodeQL, SLSA).
> El problema no fue de intención sino de **ejecución**: la implementación real derivó a scripts
> frágiles a mano. Retomar = cerrar la brecha entre ese blueprint y el código.
