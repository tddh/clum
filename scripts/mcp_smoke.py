#!/usr/bin/env python3
"""clum-mcp stdio smoke driver.

Spawns the MCP server, speaks newline-delimited JSON-RPC, runs a test
plan, prints PASS/FAIL per step. Read-only against remote hosts.
"""
import json
import re
import subprocess
import sys

BIN = "target/debug/clum-mcp"
CA = "certs/ca.crt"
HOSTS = "config/hosts.yaml"

results = []


class Mcp:
    def __init__(self):
        self.p = subprocess.Popen(
            [BIN, "--ca-cert", CA, "--hosts-file", HOSTS],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
        )
        self._id = 0

    def rpc(self, method, params=None):
        self._id += 1
        req = {"jsonrpc": "2.0", "id": self._id, "method": method}
        if params is not None:
            req["params"] = params
        self.p.stdin.write(json.dumps(req) + "\n")
        self.p.stdin.flush()
        line = self.p.stdout.readline()
        if not line:
            raise RuntimeError("server closed stdout")
        return json.loads(line)

    def tool(self, name, args):
        resp = self.rpc("tools/call", {"name": name, "arguments": args})
        if "error" in resp:
            return {"ok": False, "_rpc_error": resp["error"]["message"],
                    "_rpc_code": resp["error"]["code"]}
        result = resp["result"]
        text = result["content"][0]["text"]
        try:
            out = json.loads(text)
        except json.JSONDecodeError:
            out = {"ok": None, "_raw": text}
        if result.get("isError"):
            out["_is_error"] = True
        return out

    def close(self):
        self.p.stdin.close()
        self.p.wait(timeout=10)


def check(name, cond, detail=""):
    results.append((name, bool(cond), detail))
    mark = "PASS" if cond else "FAIL"
    print(f"[{mark}] {name}" + (f"  -- {detail}" if detail and not cond else ""))


def main():
    m = Mcp()

    # ── 1. protocol layer (no host interaction) ──
    r = m.rpc("initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "mcp-smoke", "version": "0"},
    })
    info = r.get("result", {}).get("serverInfo", {})
    check("initialize: serverInfo", info.get("name") == "clum-mcp", str(r))
    check("initialize: semver version", bool(re.match(r"^\d+\.\d+\.\d+", info.get("version", ""))), str(info))

    r = m.rpc("tools/list")
    tools = r.get("result", {}).get("tools", [])
    names = [t["name"] for t in tools]
    check("tools/list: returns tools", len(tools) > 0)
    check("tools/list: 67 tools", len(names) == 67, f"got {len(names)}")
    for expect in ["exec", "capture_pane", "session_attach", "file_upload",
                   "tunnel_create", "batch_exec", "reload_config"]:
        check(f"tools/list: has {expect}", expect in names)

    r = m.rpc("no/such.method")
    check("unknown method -> -32601", r.get("error", {}).get("code") == -32601, str(r))

    r = m.tool("no_such_tool", {})
    check("unknown tool -> -32602", r.get("_rpc_code") == -32602, str(r))

    r = m.tool("host_list", {})
    check("host_list ok", "hosts" in r, str(r)[:300])
    hosts = [h["name"] for h in r.get("hosts", [])]
    check("host_list: returns list", isinstance(hosts, list), str(hosts))

    r = m.tool("exec", {"host": "nonexistent-host", "session_name": "clum",
                        "pane_id": "%0", "command": "true"})
    check("exec bad host -> structured error", r.get("ok") is False and "_rpc_error" not in r, str(r)[:300])
    check("exec bad host -> HOST_NOT_FOUND", r.get("error_code") == "HOST_NOT_FOUND", str(r)[:300])
    check("exec bad host -> hint+retryable+isError",
          bool(r.get("recovery_hint")) and r.get("retryable") is False and r.get("_is_error") is True,
          str(r)[:300])

    m.close()

    fails = [n for n, ok, _ in results if not ok]
    print(f"\n{len(results) - len(fails)}/{len(results)} passed")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
