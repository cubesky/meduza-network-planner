import json
import sys
from typing import Any, Dict, List


def read_input() -> Dict[str, Any]:
    return json.load(sys.stdin)


def write_output(obj: Dict[str, Any]) -> None:
    json.dump(obj, sys.stdout, ensure_ascii=True)


def split_ml(val: str) -> List[str]:
    if not val:
        return []
    return [x.strip() for x in val.replace("\r\n", "\n").replace("\r", "\n").split("\n") if x.strip()]


def node_lans(node: Dict[str, str], node_id: str) -> List[str]:
    base = f"/nodes/{node_id}/lan"
    raw = node.get(base, "")
    return sorted(set(split_ml(raw)))
