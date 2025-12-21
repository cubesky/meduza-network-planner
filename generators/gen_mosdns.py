import json
from typing import Any, Dict, List

import yaml

from common import read_input, write_output

SOCKS_PORT = 7891


def _parse_json_map(raw: str) -> Dict[str, str]:
    if not raw:
        return {}
    obj = json.loads(raw)
    if not isinstance(obj, dict):
        raise ValueError("mosdns rule_files must be a JSON object")
    out: Dict[str, str] = {}
    for k, v in obj.items():
        if not isinstance(k, str) or not isinstance(v, str):
            raise ValueError("mosdns rule_files keys and values must be strings")
        out[k] = v
    return out


def _parse_plugins(raw: str) -> List[Dict[str, Any]]:
    if not raw:
        return []
    obj = yaml.safe_load(raw)
    if not isinstance(obj, list):
        raise ValueError("mosdns plugins must be a YAML list")
    for item in obj:
        if not isinstance(item, dict):
            raise ValueError("each mosdns plugin item must be a map")
    return obj


def _build_config_text(global_cfg: Dict[str, str]) -> str:
    plugins_raw = global_cfg.get("/global/mosdns/plugins", "")
    plugins = _parse_plugins(plugins_raw)
    if not plugins:
        base = open("/mosdns/config.yaml", encoding="utf-8").read()
        return base.replace("{{SOCKS_PORT}}", str(SOCKS_PORT))
    conf = {
        "log": {"level": "info"},
        "api": {"http": ":13688"},
        "plugins": plugins,
    }
    text = yaml.safe_dump(conf, sort_keys=False, allow_unicode=True)
    return text.replace("{{SOCKS_PORT}}", str(SOCKS_PORT))


def _refresh_minutes(node_id: str, node: Dict[str, str]) -> int:
    raw = node.get(f"/nodes/{node_id}/mosdns/refresh", "")
    try:
        val = int(raw)
    except Exception:
        val = 1440
    if val <= 0:
        return 1440
    return val


def main() -> None:
    payload = read_input()
    node_id = payload["node_id"]
    node = payload["node"]
    global_cfg = payload["global"]
    rules_raw = global_cfg.get("/global/mosdns/rule_files", "")
    rules = _parse_json_map(rules_raw) if rules_raw else {}
    out = {
        "config_text": _build_config_text(global_cfg),
        "rules": rules,
        "refresh_minutes": _refresh_minutes(node_id, node),
    }
    write_output(out)


if __name__ == "__main__":
    main()
