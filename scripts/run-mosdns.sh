#!/bin/bash
set -euo pipefail

exec mosdns start --config /etc/mosdns/config.yaml -d /etc/mosdns/
