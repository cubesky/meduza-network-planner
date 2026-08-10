#!/usr/bin/python3
"""Small JSON query helper using only the Python standard library."""

import json
import sys


def load(path):
    with open(path, encoding="utf-8") as handle:
        value = json.load(handle)
    if not isinstance(value, dict):
        raise ValueError("Meduza JSON root must be an object")
    return value


def main():
    if len(sys.argv) < 4:
        raise SystemExit("usage: meduza-json.py get|children FILE KEY")
    operation, path, key = sys.argv[1:4]
    data = load(path)
    if operation == "get":
        value = data.get(key, "")
        if isinstance(value, bool):
            print("true" if value else "false")
        elif value is not None:
            print(value if isinstance(value, str) else json.dumps(value, separators=(",", ":")))
        return
    if operation == "children":
        children = set()
        for candidate in data:
            if candidate.startswith(key):
                child = candidate[len(key):].split("/", 1)[0]
                if child:
                    children.add(child)
        print("\n".join(sorted(children)))
        return
    raise SystemExit("unknown operation: " + operation)


if __name__ == "__main__":
    main()
