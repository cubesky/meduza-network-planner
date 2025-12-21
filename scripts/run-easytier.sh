#!/bin/bash
set -euo pipefail

ARGS_FILE="/run/easytier/args"

if [[ ! -f "$ARGS_FILE" ]]; then
  echo "missing $ARGS_FILE" >&2
  exit 1
fi

mapfile -t args < "$ARGS_FILE"
if [[ "${#args[@]}" -eq 0 ]]; then
  echo "empty args in $ARGS_FILE" >&2
  exit 1
fi

exec easytier-core "${args[@]}"
