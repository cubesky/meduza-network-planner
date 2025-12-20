FROM debian:12-slim

ENV DEBIAN_FRONTEND=noninteractive

# Versions can be overridden at build time:
#   docker build --build-arg EASYTIER_VERSION=2.4.5 --build-arg MIHOMO_VERSION=1.19.17 --build-arg TINC_VERSION=1.1pre18 .
ARG EASYTIER_VERSION=2.4.5
ARG MIHOMO_VERSION=1.19.17
ARG TINC_VERSION=1.1pre18

ENV PIP_INDEX_URL=https://pypi.tuna.tsinghua.edu.cn/simple

RUN set -eux; \
    if [ -f /etc/apt/sources.list ]; then \
      sed -i 's|http://deb.debian.org|http://mirrors.tuna.tsinghua.edu.cn|g; s|http://security.debian.org|http://mirrors.tuna.tsinghua.edu.cn/debian-security|g' /etc/apt/sources.list; \
    elif [ -f /etc/apt/sources.list.d/debian.sources ]; then \
      sed -i 's|http://deb.debian.org|http://mirrors.tuna.tsinghua.edu.cn|g; s|http://security.debian.org|http://mirrors.tuna.tsinghua.edu.cn/debian-security|g' /etc/apt/sources.list.d/debian.sources; \
    else \
      echo "No apt sources file found" >&2; \
      exit 1; \
    fi

RUN apt-get update && apt-get install -y \
    frr frr-pythontools \
    openvpn \
    iproute2 iptables \
    procps \
    curl jq python3 python3-pip \
    ca-certificates \
    unzip gzip \
    build-essential autoconf automake libtool pkg-config \
    libssl-dev zlib1g-dev liblzo2-dev libncurses5-dev \
 && rm -rf /var/lib/apt/lists/*

# --- EasyTier ---
# Release asset name: easytier-linux-x86_64-v<VER>.zip
# Zip structure: easytier-linux-x86_64/{easytier-core,easytier-cli,easytier-web,easytier-web-embed}
RUN set -eux; \
    PROXY="http://10.42.1.2:7890"; \
    CURL_PROXY=""; \
    if curl -fsSL --connect-timeout 2 --proxy "${PROXY}" https://github.com/ >/dev/null; then \
      CURL_PROXY="--proxy ${PROXY}"; \
    fi; \
    ASSET="easytier-linux-x86_64-v${EASYTIER_VERSION}.zip"; \
    URL="https://github.com/EasyTier/EasyTier/releases/download/v${EASYTIER_VERSION}/${ASSET}"; \
    curl -fL ${CURL_PROXY} "$URL" -o /tmp/easytier.zip; \
    unzip -q /tmp/easytier.zip -d /tmp/easytier; \
    install -m 0755 /tmp/easytier/easytier-linux-x86_64/easytier-core /usr/local/bin/easytier-core; \
    install -m 0755 /tmp/easytier/easytier-linux-x86_64/easytier-cli /usr/local/bin/easytier-cli || true; \
    rm -rf /tmp/easytier /tmp/easytier.zip

# --- Clash Meta (mihomo) ---
# Release asset name: mihomo-linux-amd64-v2-v<VER>.gz
# Gzip contains binary: mihomo-linux-amd64-v2
RUN set -eux; \
    PROXY="http://10.42.1.2:7890"; \
    CURL_PROXY=""; \
    if curl -fsSL --connect-timeout 2 --proxy "${PROXY}" https://github.com/ >/dev/null; then \
      CURL_PROXY="--proxy ${PROXY}"; \
    fi; \
    ASSET="mihomo-linux-amd64-v2-v${MIHOMO_VERSION}.gz"; \
    URL="https://github.com/MetaCubeX/mihomo/releases/download/v${MIHOMO_VERSION}/${ASSET}"; \
    curl -fL ${CURL_PROXY} "$URL" -o /tmp/mihomo.gz; \
    gunzip -c /tmp/mihomo.gz > /usr/local/bin/mihomo; \
    chmod +x /usr/local/bin/mihomo; \
    rm -f /tmp/mihomo.gz

# --- Tinc 1.1 ---
# Release asset name: tinc-<VER>.tar.gz
RUN set -eux; \
    PROXY="http://10.42.1.2:7890"; \
    CURL_PROXY=""; \
    if curl -fsSL --connect-timeout 2 --proxy "${PROXY}" https://www.tinc-vpn.org/ >/dev/null; then \
      CURL_PROXY="--proxy ${PROXY}"; \
    fi; \
    ASSET="tinc-${TINC_VERSION}.tar.gz"; \
    URL="https://www.tinc-vpn.org/packages/${ASSET}"; \
    curl -fL ${CURL_PROXY} "$URL" -o /tmp/tinc.tar.gz; \
    tar -xzf /tmp/tinc.tar.gz -C /tmp; \
    cd "/tmp/tinc-${TINC_VERSION}"; \
    ./configure; \
    make -j"$(nproc)"; \
    make install; \
    cd /; \
    rm -rf "/tmp/tinc-${TINC_VERSION}" /tmp/tinc.tar.gz

RUN pip3 install --no-cache-dir --break-system-packages \
    "protobuf<=3.20.3" \
    etcd3 pyyaml requests

COPY entrypoint.sh /entrypoint.sh
COPY watcher.py /watcher.py

COPY frr/ /etc/frr/
COPY clash/ /clash/
COPY scripts/tproxy.sh /usr/local/bin/tproxy.sh

RUN chmod +x /entrypoint.sh /usr/local/bin/tproxy.sh

ENTRYPOINT ["/entrypoint.sh"]
