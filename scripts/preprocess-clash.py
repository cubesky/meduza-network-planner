#!/usr/bin/env python3
"""
Clash 配置预处理脚本

功能:
1. 下载 proxy-provider 中的远程 URL 到本地
2. 提取所有代理服务器的 IP 地址
3. 创建 ipset 包含这些 IP
4. 修改配置以使用本地文件

使用方法:
    python3 preprocess-clash.py /etc/clash/config.yaml /etc/clash/providers/
"""

import base64
import ipaddress
import os
import re
import sys
import yaml
import json
import subprocess
import tempfile
import shutil
from pathlib import Path
from urllib.parse import urlparse, unquote
from typing import Set, List, Dict, Any, Optional


def curl_download(url: str, timeout: int = 10) -> str:
    """使用 curl 下载 URL 内容"""
    try:
        result = subprocess.run(
            ["curl", "-fL", "--retry", "2", "--connect-timeout", str(timeout), "-s", url],
            capture_output=True,
            text=True,
            timeout=timeout + 5
        )
        if result.returncode == 0:
            return result.stdout
        return None
    except Exception as e:
        print(f"[!] 下载失败 {url}: {e}", flush=True)
        return None


def extract_ips_from_proxies(proxies: List[Dict[str, Any]]) -> Set[str]:
    """从代理配置中提取所有 IP 地址"""
    ips = set()

    for proxy in proxies:
        if not isinstance(proxy, dict):
            continue

        server = proxy.get("server", "")
        if not server:
            continue

        # 如果是 IP 地址,直接添加
        if server.replace(".", "").replace(":", "").isdigit() or "::" in server:
            ips.add(server)
        # 如果是域名,尝试解析
        else:
            resolved_ips = resolve_hostname(server)
            ips.update(resolved_ips)

    return ips


def resolve_hostname(hostname: str) -> Set[str]:
    """解析域名到 IP 地址"""
    ips = set()
    try:
        # 使用 getent 命令解析 (容器中通常可用)
        result = subprocess.run(
            ["getent", "hosts", hostname],
            capture_output=True,
            text=True,
            timeout=5
        )
        if result.returncode == 0:
            for line in result.stdout.strip().split("\n"):
                parts = line.split()
                if len(parts) >= 1:
                    ip = parts[0]
                    if ip and ":" not in ip:  # 跳过 IPv6
                        ips.add(ip)
    except Exception:
        pass

    return ips


def _maybe_decode_base64(text: str) -> Optional[str]:
    stripped = "".join(text.split())
    if not stripped:
        return None
    if not re.fullmatch(r"[A-Za-z0-9+/=_-]+", stripped):
        return None
    padding = (-len(stripped)) % 4
    if padding:
        stripped += "=" * padding
    try:
        decoded = base64.urlsafe_b64decode(stripped)
    except Exception:
        return None
    try:
        return decoded.decode("utf-8")
    except UnicodeDecodeError:
        return None


def _is_ip(host: str) -> bool:
    try:
        ipaddress.ip_address(host)
        return True
    except ValueError:
        return False


def _host_from_ss(link: str) -> Optional[str]:
    rest = link[len("ss://"):]
    rest = rest.split("#", 1)[0]
    rest = rest.split("?", 1)[0]
    rest = unquote(rest)
    if "@" in rest:
        hostport = rest.rsplit("@", 1)[1]
        return hostport.split(":", 1)[0] if hostport else None
    decoded = _maybe_decode_base64(rest)
    if not decoded or "@" not in decoded:
        return None
    hostport = decoded.rsplit("@", 1)[1]
    return hostport.split(":", 1)[0] if hostport else None


def _host_from_link(link: str) -> Optional[str]:
    if link.startswith("ss://"):
        return _host_from_ss(link)
    if "://" not in link:
        return None
    parsed = urlparse(link)
    return parsed.hostname


def extract_ips_from_subscription_text(text: str) -> Set[str]:
    ips: Set[str] = set()
    for raw in text.splitlines():
        line = raw.strip()
        if not line:
            continue
        host = _host_from_link(line)
        if not host:
            continue
        if _is_ip(host):
            ips.add(host)
        else:
            ips.update(resolve_hostname(host))
    return ips


def download_provider(url: str, output_dir: str) -> str:
    """下载 provider 配置到本地"""
    print(f"[*] 下载 provider: {url}", flush=True)

    content = curl_download(url)
    if not content:
        print(f"[!] 无法下载 {url}", flush=True)
        return None

    # 解析内容以获取文件名
    parsed = urlparse(url)
    basename = os.path.basename(parsed.path) or "provider.yml"
    local_path = os.path.join(output_dir, basename)

    # 写入文件
    os.makedirs(output_dir, exist_ok=True)
    with open(local_path, "w", encoding="utf-8") as f:
        f.write(content)

    print(f"[✓] 已下载到 {local_path}", flush=True)
    return local_path


def process_providers(config: Dict[str, Any], provider_dir: str) -> Set[str]:
    """处理所有 proxy-provider"""
    proxy_providers = config.get("proxy-providers", {})
    if not proxy_providers:
        return set()

    all_ips = set()

    for provider_name, provider_config in proxy_providers.items():
        if not isinstance(provider_config, dict):
            continue

        url = provider_config.get("url", "")
        if not url:
            continue

        # 下载到本地
        local_path = download_provider(url, provider_dir)
        if not local_path:
            continue

        # 读取下载的配置
        try:
            with open(local_path, encoding="utf-8") as f:
                provider_data = yaml.safe_load(f)
        except Exception as e:
            print(f"[!] 无法解析 {local_path}: {e}", flush=True)
            continue

        if isinstance(provider_data, str):
            decoded = _maybe_decode_base64(provider_data)
            candidate = decoded or provider_data
            link_ips = extract_ips_from_subscription_text(candidate)
            if link_ips:
                all_ips.update(link_ips)
                print(f"[✓] from {provider_name} extracted {len(link_ips)} IP (links)", flush=True)
            if decoded:
                try:
                    provider_data = yaml.safe_load(decoded)
                    if isinstance(provider_data, dict):
                        print(f"[*] provider base64 decoded yaml: {local_path}", flush=True)
                    else:
                        print(f"[!] provider config invalid ({type(provider_data).__name__}): {local_path}", flush=True)
                        continue
                except Exception as e:
                    print(f"[!] provider base64 parse failed: {local_path}: {e}", flush=True)
                    continue
            else:
                if link_ips:
                    continue
                print(f"[!] provider config invalid (str): {local_path}", flush=True)
                continue

        # 提取代理
        if not isinstance(provider_data, dict):
            print(f"[!] provider config invalid ({type(provider_data).__name__}): {local_path}", flush=True)
            continue

        proxies = provider_data.get("proxies", [])
        if not isinstance(proxies, list):
            print(f"[!] provider proxies invalid: {local_path}", flush=True)
            proxies = []
        if proxies:
            ips = extract_ips_from_proxies(proxies)
            all_ips.update(ips)
            print(f"[✓] 从 {provider_name} 提取了 {len(ips)} 个 IP", flush=True)

        # 修改配置使用本地文件
        provider_config["url"] = f"file://{local_path}"
        if "path" in provider_config:
            provider_config["path"] = local_path

    return all_ips


def extract_proxies_from_config(config: Dict[str, Any]) -> Set[str]:
    """直接从配置的 proxies 中提取 IP"""
    proxies = config.get("proxies", [])
    return extract_ips_from_proxies(proxies)


def save_ipset(ips: Set[str], output_file: str):
    """保存 IP 列表到文件"""
    os.makedirs(os.path.dirname(output_file), exist_ok=True)
    with open(output_file, "w") as f:
        for ip in sorted(ips):
            f.write(f"{ip}\n")
    print(f"[✓] 已保存 {len(ips)} 个 IP 到 {output_file}", flush=True)


def main():
    if len(sys.argv) < 3:
        print(f"用法: {sys.argv[0]} <config.yaml> <providers_dir>", file=sys.stderr)
        sys.exit(1)

    config_file = sys.argv[1]
    providers_dir = sys.argv[2]

    # 读取配置
    print(f"[*] 读取配置: {config_file}", flush=True)
    try:
        with open(config_file, encoding="utf-8") as f:
            config = yaml.safe_load(f) or {}
    except Exception as e:
        print(f"[!] 无法读取配置文件: {e}", flush=True)
        sys.exit(1)

    # 处理 proxy-providers
    all_ips = process_providers(config, providers_dir)

    # 从本地 proxies 中提取 IP
    local_ips = extract_proxies_from_config(config)
    all_ips.update(local_ips)

    # 保存 IP 列表
    if all_ips:
        ipset_file = os.path.join(providers_dir, "proxy_servers.txt")
        save_ipset(all_ips, ipset_file)

        # 同时保存为 JSON 方便 iptables 使用
        json_file = os.path.join(providers_dir, "proxy_servers.json")
        with open(json_file, "w") as f:
            json.dump({"ips": sorted(all_ips)}, f, indent=2)
        print(f"[✓] 已保存 JSON 到 {json_file}", flush=True)
    else:
        print("[!] 没有找到任何代理服务器 IP", flush=True)

    # 写回修改后的配置
    with open(config_file, "w", encoding="utf-8") as f:
        yaml.safe_dump(config, f, allow_unicode=True, sort_keys=False)

    print(f"[✓] 预处理完成", flush=True)


if __name__ == "__main__":
    main()
