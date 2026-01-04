#!/bin/bash
set -euo pipefail

exec dnsmasq --keep-in-foreground --conf-file=/etc/dnsmasq.conf
