#!/usr/bin/env bash
# ==============================================================================
# Test Script for Breeze Autoscroll
# ==============================================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export PYTHONPATH="${SCRIPT_DIR}/src:${PYTHONPATH}"

echo "=== Breeze Autoscroll Testlauf ==="
echo "1) Syntaxprüfung aller Python-Module..."
python3 -m py_compile "${SCRIPT_DIR}/src"/*.py
echo "✓ Alle Python-Module kompilieren fehlerfrei!"
echo ""

echo "2) Starte Breeze Autoscroll im Vordergrund (Drücke Strg+C zum Beenden)..."
python3 "${SCRIPT_DIR}/src/main.py" "$@"
