#!/bin/bash
# Verify s6-overlay v3 configuration
# Usage: verify-s6-config.sh

echo "=== s6-overlay v3 Configuration Verification ===" >&2
echo "" >&2

cd "$(dirname "$0")/.."

# 1. Check bundle structure
echo "[1. Bundle Structure]" >&2
if [[ -f "s6-services/default/type" ]]; then
    type=$(cat "s6-services/default/type")
    echo "  ✅ default bundle: type=$type" >&2
    if [[ "$type" != "bundle" ]]; then
        echo "  ❌ ERROR: default/type should be 'bundle', not '$type'" >&2
        exit 1
    fi
else
    echo "  ❌ default bundle missing type file" >&2
    exit 1
fi

if [[ -f "s6-services/user/type" ]]; then
    type=$(cat "s6-services/user/type")
    echo "  ✅ user bundle: type=$type" >&2
    if [[ "$type" != "bundle" ]]; then
        echo "  ❌ ERROR: user/type should be 'bundle', not '$type'" >&2
        exit 1
    fi
else
    echo "  ❌ user bundle missing type file" >&2
    exit 1
fi
echo "" >&2

# 2. Check bundle contents
echo "[2. Bundle Contents]" >&2
if [[ -d "s6-services/default/contents.d" ]]; then
    echo "  ✅ default/contents.d exists" >&2
    echo "  Contents: $(ls -1 s6-services/default/contents.d/ | tr '\n' ' ')" >&2
else
    echo "  ❌ default/contents.d missing" >&2
    exit 1
fi

if [[ -d "s6-services/user/contents.d" ]]; then
    echo "  ✅ user/contents.d exists" >&2
    echo "  Contents: $(ls -1 s6-services/user/contents.d/ | tr '\n' ' ')" >&2
else
    echo "  ❌ user/contents.d missing" >&2
    exit 1
fi
echo "" >&2

# 3. Check service types
echo "[3. Service Types]" >&2
for svc in dbus avahi watchfrr watcher mihomo easytier tinc mosdns dnsmasq dns-monitor; do
    if [[ -f "s6-services/$svc/type" ]]; then
        type=$(cat "s6-services/$svc/type")
        echo "  ✅ $svc: type=$type" >&2
    else
        echo "  ❌ $svc: missing type file" >&2
    fi
done
echo "" >&2

# 4. Check dependencies
echo "[4. Service Dependencies]" >&2
for svc in dbus avahi watcher watchfrr; do
    if [[ -d "s6-services/$svc/dependencies.d" ]]; then
        deps=$(ls -1 s6-services/$svc/dependencies.d/ | tr '\n' ' ')
        echo "  ✅ $svc depends on: $deps" >&2
        # Verify files are empty
        for dep_file in s6-services/$svc/dependencies.d/*; do
            if [[ -s "$dep_file" ]]; then
                echo "  ❌ ERROR: $(basename $dep_file) should be empty!" >&2
                exit 1
            fi
        done
    fi
done
echo "" >&2

# 5. Check pipelines
echo "[5. Pipeline Configurations]" >&2
for svc in mihomo watcher tinc mosdns easytier dnsmasq dns-monitor; do
    if [[ -f "s6-services/$svc/producer-for" ]]; then
        producer=$(cat "s6-services/$svc/producer-for")
        consumer=$(cat "s6-services/$svc/log/consumer-for" 2>/dev/null || echo "missing")
        pipeline=$(cat "s6-services/$svc/log/pipeline-name" 2>/dev/null || echo "missing")
        echo "  ✅ $svc → $producer (pipeline: $pipeline)" >&2
    fi
done
echo "" >&2

# 6. Check log scripts
echo "[6. Log Script Syntax]" >&2
bad_logs=0
for log_run in s6-services/*/log/run; do
    if [[ -f "$log_run" ]]; then
        if grep -q "s6-svlogd" "$log_run"; then
            echo "  ❌ $(dirname $log_run) uses incorrect s6-svlogd syntax" >&2
            bad_logs=$((bad_logs + 1))
        elif grep -q "logutil-service" "$log_run"; then
            echo "  ✅ $(dirname $log_run) uses logutil-service (correct)" >&2
        fi
    fi
done
if [[ $bad_logs -gt 0 ]]; then
    echo "  ❌ Found $bad_logs services with incorrect log syntax" >&2
    exit 1
fi
echo "" >&2

echo "=== Verification Complete ===" >&2
echo "" >&2
echo "✅ Configuration is correct for s6-overlay v3!" >&2
echo "" >&2
echo "You can now rebuild the container:" >&2
echo "  docker compose down && docker compose build --no-cache && docker compose up -d" >&2
