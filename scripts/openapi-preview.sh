#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SPEC_FILE="$ROOT_DIR/docs/openapi.yaml"

if [[ ! -f "$SPEC_FILE" ]]; then
  echo "OpenAPI spec not found at: $SPEC_FILE" >&2
  exit 1
fi

PORT="${1:-8080}"

echo "Serving OpenAPI UI on http://localhost:${PORT}"
echo "Using spec file: $SPEC_FILE"

docker run --rm \
  -p "${PORT}:8080" \
  -e SWAGGER_JSON=/spec/openapi.yaml \
  -v "$ROOT_DIR/docs:/spec:ro" \
  swaggerapi/swagger-ui
