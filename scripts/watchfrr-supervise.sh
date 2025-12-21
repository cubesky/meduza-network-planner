#!/bin/bash
set -euo pipefail

delay=1
cap=60

while true; do
  if ! pgrep -x watchfrr >/dev/null 2>&1; then
    echo "[supervise] watchfrr missing, restarting frr" >&2
    /usr/lib/frr/frrinit.sh start || true
    echo "[supervise] watchfrr restart backoff ${delay}s" >&2
    sleep "$delay"
    delay=$(( delay * 2 ))
    if (( delay > cap )); then delay=$cap; fi
  else
    delay=1
    sleep 5
  fi
done
