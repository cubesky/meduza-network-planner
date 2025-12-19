#!/bin/bash
set -euo pipefail
echo "Populate /nodes/<NODE_ID>/... keys and bump /commit."
echo "Schema note: sites removed; LANs live under /nodes/<NODE_ID>/lan/*"
