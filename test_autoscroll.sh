#!/usr/bin/env bash
# ==============================================================================
# Test & Run Script for Breeze Autoscroll (Rust)
# ==============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${SCRIPT_DIR}"

echo "=== Breeze Autoscroll Testlauf (Rust) ==="
echo "1) Führe Unit-Tests aus..."
cargo test --locked --verbose
echo "✓ Alle Unit-Tests bestanden!"
echo ""

echo "2) Erstelle Release-Binary..."
cargo build --release --locked
echo ""

echo "3) Starte Breeze Autoscroll im Vordergrund (Drücke Strg+C zum Beenden)..."
exec "${SCRIPT_DIR}/target/release/breeze-autoscroll" "$@"
