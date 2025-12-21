#!/bin/bash
set -euo pipefail

exec mosdns run -c /etc/mosdns/config.yaml
