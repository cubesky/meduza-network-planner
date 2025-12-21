#!/bin/sh
set -e

RULES_DIR="${MOSDNS_RULES_DIR:-/etc/mosdns}"
RULES_JSON="${MOSDNS_RULES_JSON:-}"

if [ -z "${HTTP_PROXY:-}" ]; then
    proxy="${MOSDNS_HTTP_PROXY:-http://127.0.0.1:7890}"
    export HTTP_PROXY="${proxy}"
    export HTTPS_PROXY="${proxy}"
fi

safe_curl() {
    url=$1
    output=$2
    temp=$(mktemp)
    if curl -fsL --speed-limit 8192 --speed-time 10 "$url" -o "$temp"; then
        mkdir -p "$(dirname "$output")"
        cp "$temp" "$output"
        chmod 0644 "$output"
    else
        rm -f "$temp"
        echo "Failed to download $url" >&2
        exit 1
    fi
    rm -f "$temp"
}

mkdir -p "${RULES_DIR}"

safe_path() {
    p="$1"
    p="${p#/}"
    case "$p" in
        *".."*) echo "Invalid rule file path: $1" >&2; exit 1 ;;
    esac
    echo "$p"
}

if [ -n "$RULES_JSON" ] && [ -s "$RULES_JSON" ]; then
    jq -r 'to_entries[] | "\(.key)\t\(.value)"' "$RULES_JSON" | while IFS=$'\t' read -r rel url; do
        if [ -z "$rel" ] || [ -z "$url" ]; then
            continue
        fi
        safe_rel="$(safe_path "$rel")"
        out="${RULES_DIR}/${safe_rel}"
        echo "update rule ${safe_rel}..."
        safe_curl "$url" "$out"
    done
else
    mkdir -p "${RULES_DIR}/ad" "${RULES_DIR}/geoip" "${RULES_DIR}/geosite" "${RULES_DIR}/tld"

    echo update ddns domain list...
    safe_curl https://profile.kookxiang.com/rules/mosdns/ddns.txt "${RULES_DIR}/ddns.txt"

    echo update block list...
    safe_curl https://profile.kookxiang.com/rules/mosdns/block.txt "${RULES_DIR}/block.txt"

    echo update forced local resolver list...
    safe_curl https://profile.kookxiang.com/rules/mosdns/local.txt "${RULES_DIR}/local.txt"

    echo update hosts map...
    safe_curl https://profile.kookxiang.com/rules/mosdns/hosts.txt "${RULES_DIR}/hosts.txt"

    echo update tld list from IANA...
    safe_curl https://data.iana.org/TLD/tlds-alpha-by-domain.txt "${RULES_DIR}/tld/tlds-alpha-by-domain.txt"

    echo update geoip cn from metowolf/iplist...
    safe_curl https://metowolf.github.io/iplist/data/special/china.txt "${RULES_DIR}/geoip/cn.txt"

    echo update oisd block list...
    safe_curl https://big.oisd.nl/domainswild2 "${RULES_DIR}/ad/oisd.big.txt"

    echo update HaGeZi's Light DNS Blocklist...
    safe_curl https://cdn.jsdelivr.net/gh/hagezi/dns-blocklists@latest/domains/light.txt "${RULES_DIR}/ad/hagezi.light.txt"

    for geosite in 'alibaba' 'apple@cn' 'baidu' 'bytedance' 'category-ads' 'category-httpdns-cn' 'cloudflare-cn' 'cn' 'geolocation-!cn' 'geolocation-cn' 'microsoft@cn' 'private' 'qihoo360' 'steam@cn' 'tencent'; do
        echo "update geosite:$geosite..."
        safe_curl "https://profile.kookxiang.com/geosite/domains/${geosite}" "${RULES_DIR}/geosite/${geosite}.txt"
    done
fi
