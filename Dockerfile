FROM debian:12-slim

ENV DEBIAN_FRONTEND=noninteractive

# Versions can be overridden at build time:
#   docker build --build-arg EASYTIER_VERSION=2.4.5 --build-arg MIHOMO_VERSION=1.19.17 .
ARG EASYTIER_VERSION=2.4.5
ARG MIHOMO_VERSION=1.19.17

RUN set -eux; \
    if [ -f /etc/apt/sources.list ]; then \
      sed -i 's|http://deb.debian.org|https://mirrors.tuna.tsinghua.edu.cn|g; s|http://security.debian.org|https://mirrors.tuna.tsinghua.edu.cn/debian-security|g' /etc/apt/sources.list; \
    elif [ -f /etc/apt/sources.list.d/debian.sources ]; then \
      sed -i 's|http://deb.debian.org|https://mirrors.tuna.tsinghua.edu.cn|g; s|http://security.debian.org|https://mirrors.tuna.tsinghua.edu.cn/debian-security|g' /etc/apt/sources.list.d/debian.sources; \
    else \
      echo "No apt sources file found" >&2; \
      exit 1; \
    fi

RUN apt-get update && apt-get install -y \
    frr frr-pythontools \
    openvpn \
    iproute2 iptables \
    curl jq python3 python3-pip \
    ca-certificates \
    unzip gzip \
 && rm -rf /var/lib/apt/lists/*

# --- EasyTier ---
# Release asset name: easytier-linux-x86_64-v<VER>.zip
# Zip structure: easytier-linux-x86_64/{easytier-core,easytier-cli,easytier-web,easytier-web-embed}
RUN set -eux;         ASSET="easytier-linux-x86_64-v${EASYTIER_VERSION}.zip";         URL="https://github.com/EasyTier/EasyTier/releases/download/v${EASYTIER_VERSION}/${ASSET}";         curl -fL "$URL" -o /tmp/easytier.zip;         unzip -q /tmp/easytier.zip -d /tmp/easytier;         install -m 0755 /tmp/easytier/easytier-linux-x86_64/easytier-core /usr/local/bin/easytier-core;         install -m 0755 /tmp/easytier/easytier-linux-x86_64/easytier-cli /usr/local/bin/easytier-cli || true;         rm -rf /tmp/easytier /tmp/easytier.zip

# --- Clash Meta (mihomo) ---
# Release asset name: mihomo-linux-amd64-v2-v<VER>.gz
# Gzip contains binary: mihomo-linux-amd64-v2
RUN set -eux; \
    ASSET="mihomo-linux-amd64-v2-v${MIHOMO_VERSION}.gz"; \
    URL="https://github.com/MetaCubeX/mihomo/releases/download/v${MIHOMO_VERSION}/${ASSET}"; \
    curl -fL "$URL" -o /tmp/mihomo.gz; \
    gunzip -c /tmp/mihomo.gz > /usr/local/bin/mihomo; \
    chmod +x /usr/local/bin/mihomo; \
    rm -f /tmp/mihomo.gz

RUN pip3 install --no-cache-dir etcd3 pyyaml requests

COPY entrypoint.sh /entrypoint.sh
COPY watcher.py /watcher.py

COPY frr/ /etc/frr/
COPY clash/ /clash/
COPY scripts/tproxy.sh /usr/local/bin/tproxy.sh

RUN chmod +x /entrypoint.sh /usr/local/bin/tproxy.sh

ENTRYPOINT ["/entrypoint.sh"]
