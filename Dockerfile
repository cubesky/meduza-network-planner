FROM library/debian:12-slim

ENV DEBIAN_FRONTEND=noninteractive

# Versions can be overridden at build time:
#   docker build --build-arg EASYTIER_VERSION=2.4.5 --build-arg MIHOMO_VERSION=1.19.17 --build-arg TINC_VERSION=1.1pre18 --build-arg MOSDNS_VERSION=5.3.3 .
ARG EASYTIER_VERSION=2.4.5
ARG MIHOMO_VERSION=1.19.17
ARG TINC_VERSION=1.1pre18
ARG MOSDNS_VERSION=5.3.3

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
    wireguard-tools \
    iproute2 iptables \
    iputils-ping dnsutils \
    mtr tcpdump nano \
    procps \
    curl jq git python3 python3-pip \
    ca-certificates \
    dnsmasq \
    avahi-daemon dbus \
    unzip gzip \
    build-essential autoconf automake libtool pkg-config meson ninja-build \
    libssl-dev zlib1g-dev liblzo2-dev libncurses5-dev \
 && rm -rf /var/lib/apt/lists/*

# --- s6-overlay ---
# Install s6-overlay for process supervision
ARG S6_OVERLAY_VERSION=v3.2.0.2
RUN set -eux; \
    PROXY="http://10.42.7.5:7890"; \
    CURL_PROXY=""; \
    if curl -fsSL --connect-timeout 2 --proxy "${PROXY}" https://github.com/ >/dev/null; then \
      CURL_PROXY="--proxy ${PROXY}"; \
    fi; \
    ARCH="amd64"; \
    URL="https://github.com/just-containers/s6-overlay/releases/download/${S6_OVERLAY_VERSION}/s6-overlay-noarch.tar.xz"; \
    curl -fL ${CURL_PROXY} "$URL" -o /tmp/s6-overlay-noarch.tar.xz; \
    URL="https://github.com/just-containers/s6-overlay/releases/download/${S6_OVERLAY_VERSION}/s6-overlay-x86_64.tar.xz"; \
    curl -fL ${CURL_PROXY} "$URL" -o /tmp/s6-overlay-x86_64.tar.xz; \
    tar -C / -Jxpf /tmp/s6-overlay-noarch.tar.xz; \
    tar -C / -Jxpf /tmp/s6-overlay-x86_64.tar.xz; \
    rm -f /tmp/s6-overlay-*.tar.xz

# --- EasyTier ---
# Release asset name: easytier-linux-x86_64-v<VER>.zip
# Zip structure: easytier-linux-x86_64/{easytier-core,easytier-cli,easytier-web,easytier-web-embed}
RUN set -eux; \
    PROXY="http://10.42.7.5:7890"; \
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
    PROXY="http://10.42.7.5:7890"; \
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

# --- MetaCubeXD (UI) ---
RUN set -eux; \
    PROXY="http://10.42.7.5:7890"; \
    CURL_PROXY=""; \
    if curl -fsSL --connect-timeout 2 --proxy "${PROXY}" https://github.com/ >/dev/null; then \
      CURL_PROXY="--proxy ${PROXY}"; \
    fi; \
    URL="https://github.com/MetaCubeX/metacubexd/archive/refs/heads/gh-pages.zip"; \
    curl -fL ${CURL_PROXY} "$URL" -o /tmp/metacubexd.zip; \
    unzip -q /tmp/metacubexd.zip -d /tmp/metacubexd; \
    rm -rf /etc/clash/ui; \
    mkdir -p /etc/clash; \
    cp -r /tmp/metacubexd/metacubexd-gh-pages /etc/clash/ui; \
    rm -rf /tmp/metacubexd /tmp/metacubexd.zip

# --- Tinc 1.1 ---
# Build from source (git branch 1.1)
RUN set -eux; \
    PROXY="http://10.42.7.5:7890"; \
    CURL_PROXY=""; \
    if curl -fsSL --connect-timeout 2 --proxy "${PROXY}" https://www.tinc-vpn.org/ >/dev/null; then \
      CURL_PROXY="--proxy ${PROXY}"; \
    fi; \
    if [ -n "${CURL_PROXY}" ]; then \
      git -c http.proxy="${PROXY}" clone --depth 1 -b 1.1 https://github.com/gsliepen/tinc.git /tmp/tinc; \
    else \
      git clone --depth 1 -b 1.1 https://github.com/gsliepen/tinc.git /tmp/tinc; \
    fi; \
    cd /tmp/tinc; \
    meson setup build; \
    ninja -C build; \
    ninja -C build install; \
    cd /; \
    rm -rf /tmp/tinc

# --- MosDNS ---
# Release asset name: mosdns-linux-amd64.zip (no nested folder)
RUN set -eux; \
    PROXY="http://10.42.7.5:7890"; \
    CURL_PROXY=""; \
    if curl -fsSL --connect-timeout 2 --proxy "${PROXY}" https://github.com/ >/dev/null; then \
      CURL_PROXY="--proxy ${PROXY}"; \
    fi; \
    ASSET="mosdns-linux-amd64.zip"; \
    URL="https://github.com/IrineSistiana/mosdns/releases/download/v${MOSDNS_VERSION}/${ASSET}"; \
    curl -fL ${CURL_PROXY} "$URL" -o /tmp/mosdns.zip; \
    unzip -q /tmp/mosdns.zip -d /tmp/mosdns; \
    install -m 0755 /tmp/mosdns/mosdns /usr/local/bin/mosdns; \
    install -d /usr/local/share/mosdns; \
    install -m 0644 /tmp/mosdns/config.yaml /usr/local/share/mosdns/config.yaml; \
    rm -rf /tmp/mosdns /tmp/mosdns.zip

RUN pip3 install --no-cache-dir --break-system-packages \
    "protobuf<=3.20.3" \
    etcd3 pyyaml requests toml

COPY entrypoint.sh /entrypoint.sh
COPY watcher.py /watcher.py
COPY generators/ /generators/
COPY s6-services/ /etc/s6-overlay/sv/
COPY scripts/watchfrr-supervise.sh /usr/local/bin/watchfrr-supervise.sh
COPY scripts/run-clash.sh /usr/local/bin/run-clash.sh
COPY scripts/preprocess-clash.py /usr/local/bin/preprocess-clash.py
COPY scripts/run-easytier.sh /usr/local/bin/run-easytier.sh
COPY scripts/run-tinc.sh /usr/local/bin/run-tinc.sh
COPY scripts/run-mosdns.sh /usr/local/bin/run-mosdns.sh
COPY scripts/run-dnsmasq.sh /usr/local/bin/run-dnsmasq.sh
COPY scripts/run-wireguard.sh /usr/local/bin/run-wireguard.sh
COPY scripts/run-dns-monitor.sh /usr/local/bin/run-dns-monitor.sh

COPY frr/ /etc/frr/
COPY clash/ /clash/
COPY avahi/avahi-daemon.conf /etc/avahi/avahi-daemon.conf
COPY scripts/tproxy.sh /usr/local/bin/tproxy.sh

RUN chmod +x /entrypoint.sh /usr/local/bin/tproxy.sh \
    /usr/local/bin/watchfrr-supervise.sh /usr/local/bin/run-clash.sh /usr/local/bin/run-easytier.sh \
    /usr/local/bin/run-tinc.sh /usr/local/bin/run-mosdns.sh /usr/local/bin/run-dnsmasq.sh \
    /usr/local/bin/run-wireguard.sh /usr/local/bin/run-dns-monitor.sh \
    /etc/s6-overlay/sv/*/run

ENTRYPOINT ["/entrypoint.sh"]
