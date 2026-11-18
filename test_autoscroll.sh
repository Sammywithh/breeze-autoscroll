#!/usr/bin/env bash
# ==============================================================================
# Test & Run Script for Breeze Autoscroll (Rust)
# ==============================================================================

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${SCRIPT_DIR}"

echo "=== Breeze Autoscroll Testlauf (Rust) ==="
echo "1) Führe Unit-Tests aus..."
cargo test --verbose
echo "✓ Alle Unit-Tests bestanden!"
echo ""

echo "2) Erstelle Release-Binary..."
cargo build --release
echo ""

echo "3) Starte Breeze Autoscroll im Vordergrund (Drücke Strg+C zum Beenden)..."
exec "${SCRIPT_DIR}/target/release/breeze-autoscroll" "$@"
