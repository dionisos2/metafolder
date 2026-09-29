#!/usr/bin/env bash
# Tests of the documentation wiki's tooling (docs/wiki/tools/, behind
# scripts/doc): node's own test runner, no dependency.
set -euo pipefail
exec node --test "$(dirname "$0")/../docs/wiki/tools/"
