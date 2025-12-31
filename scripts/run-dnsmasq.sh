#!/bin/bash
set -euo pipefail

exec dnsmasq --conf-file=/etc/dnsmasq.conf
