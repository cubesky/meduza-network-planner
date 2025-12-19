#!/bin/bash
set -euo pipefail
ZIP_NAME="easytier-frr-gateway.zip"
rm -f "$ZIP_NAME"
zip -r "$ZIP_NAME" .
echo "Built $ZIP_NAME"
