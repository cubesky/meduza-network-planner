#!/usr/bin/env python3
import json
import os
import ssl
import sys
from string import Template
from urllib.parse import urlparse

from ldap3 import ALL, Connection, Server, Tls
from ldap3.utils.conv import escape_filter_chars


CONFIG_PATH = "/etc/openvpn/generated/access-ldap.json"


def _load_config():
    with open(CONFIG_PATH, "r", encoding="utf-8") as f:
        return json.load(f)


def _render_filter(template: str, username: str, user_dn: str = "") -> str:
    return Template(template.replace("{username}", "$username").replace("{user_dn}", "$user_dn")).safe_substitute(
        username=escape_filter_chars(username),
        user_dn=escape_filter_chars(user_dn),
    )


def _build_server(cfg):
    parsed = urlparse(cfg["uri"])
    if not parsed.scheme or not parsed.hostname:
        raise RuntimeError("invalid LDAP URI")
    use_ssl = parsed.scheme.lower() == "ldaps"
    tls = None
    if use_ssl or cfg.get("start_tls"):
        validate = ssl.CERT_NONE if cfg.get("insecure") else ssl.CERT_REQUIRED
        tls = Tls(
            validate=validate,
            ca_certs_file=cfg.get("ca_cert_path") or None,
            version=ssl.PROTOCOL_TLS_CLIENT,
        )
    port = parsed.port or (636 if use_ssl else 389)
    return Server(parsed.hostname, port=port, use_ssl=use_ssl, tls=tls, get_info=ALL)


def _new_connection(server, user=None, password=None, start_tls=False):
    conn = Connection(server, user=user, password=password)
    if not conn.open():
        raise RuntimeError("failed to open LDAP connection")
    if start_tls:
        if not conn.start_tls():
            raise RuntimeError("failed to start LDAP TLS")
    if not conn.bind():
        raise RuntimeError("failed to bind LDAP connection")
    return conn


def main() -> int:
    username = os.environ.get("username", "")
    password = os.environ.get("password", "")
    if not username or not password:
        return 1

    cfg = _load_config()
    server = _build_server(cfg)

    bind_user = cfg.get("bind_dn") or None
    bind_password = cfg.get("bind_password") or None
    conn = _new_connection(server, user=bind_user, password=bind_password, start_tls=cfg.get("start_tls"))

    user_filter = _render_filter(cfg["user_filter"], username)
    if not conn.search(cfg["base_dn"], user_filter, attributes=["dn"]):
        conn.unbind()
        return 1
    if len(conn.entries) != 1:
        conn.unbind()
        return 1

    user_dn = conn.entries[0].entry_dn
    group_filter = _render_filter(cfg["group_filter"], username, user_dn)
    if not conn.search(cfg["group_base_dn"], group_filter, attributes=["dn"]):
        conn.unbind()
        return 1
    if len(conn.entries) < 1:
        conn.unbind()
        return 1
    conn.unbind()

    user_conn = _new_connection(server, user=user_dn, password=password, start_tls=cfg.get("start_tls"))
    user_conn.unbind()
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception:
        sys.exit(1)
