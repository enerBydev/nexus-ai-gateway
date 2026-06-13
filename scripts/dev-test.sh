#!/usr/bin/env bash
# ============================================================
# dev-test.sh — Arranca una instancia AISLADA de NEXUS para pruebas / mejora continua.
#
# Garantías de seguridad:
#   - NUNCA toca producción (:8315) ni el binario instalado (~/.cargo/bin).
#   - Usa CARGO_TARGET_DIR=target-test → fuera de target/release/, que es lo único
#     que vigila el watcher systemd → el watcher NUNCA dispara → prod jamás se reinicia.
#   - Guardarraíl: aborta si el puerto resuelve a 8315.
#
# Uso:
#   ./scripts/dev-test.sh            # build debug (rápido) y corre en :8316
#   ./scripts/dev-test.sh release    # build release (a target-test, sigue siendo watcher-safe)
#
# Nota: para iterar tests/lint usa el target normal (rápido y ya watcher-safe):
#   cargo test     cargo clippy -- -D warnings     cargo fmt
# El target-test aislado es para CORRER el binario servidor de pruebas.
# ============================================================
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target-test}"

if [ ! -f .env.test ]; then
  echo "❌ Falta .env.test (config de pruebas). Cópialo de .env.example y ajusta UPSTREAM_*."
  exit 1
fi

# Cargar .env.test al entorno (dotenvy no sobreescribe variables ya presentes)
set -a
# shellcheck disable=SC1091
. ./.env.test
set +a
export PORT="${PORT:-8316}"

# Guardarraíl: jamás el puerto de producción
if [ "$PORT" = "8315" ]; then
  echo "🚨 PORT=8315 es PRODUCCIÓN. Aborto. Usa 8316 u otro en .env.test."
  exit 1
fi

if [ -z "${UPSTREAM_BASE_URL:-}" ]; then
  echo "⚠️  UPSTREAM_BASE_URL vacío en .env.test — rellénalo antes de probar contra un upstream real."
fi

MODE="${1:-debug}"
echo "🧪 NEXUS TEST  ·  target=$CARGO_TARGET_DIR  ·  port=$PORT  ·  mode=$MODE   (producción :8315 intacta)"

if [ "$MODE" = "release" ]; then
  cargo build --release
  BIN="$CARGO_TARGET_DIR/release/nexus-ai-gateway"
else
  cargo build
  BIN="$CARGO_TARGET_DIR/debug/nexus-ai-gateway"
fi

echo "▶  $BIN  (Ctrl-C para detener)"
exec "$BIN"
