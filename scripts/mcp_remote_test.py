#!/usr/bin/env python3
"""yunying-mcp remote functional test.

Per skill rules: session_name="yunying", attach-before-create,
min pane_id, no session cleanup. Full depth: session/exec/state/
wait/file transfer/tunnel/batch.
"""
import json
import os
import select
import socket
import subprocess
import sys

BIN = "target/debug/yunying-mcp"
CA = "certs/ca.crt"
HOSTS_FILE = "config/hosts.yaml"
HOSTS = ["k8s-m1", "dns-backup", "tf001"]
SESSION = "yunying"
MARK = f"SMK{os.getpid()}"

results = []


class Mcp:
    def __init__(self):
        self.p = subprocess.Popen(
            [BIN, "--ca-cert", CA, "--hosts-file", HOSTS_FILE],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
        )
        self._id = 0

    def rpc(self, method, params=None, timeout=120):
        self._id += 1
        req = {"jsonrpc": "2.0", "id": self._id, "method": method}
        if params is not None:
            req["params"] = params
        self.p.stdin.write(json.dumps(req) + "\n")
        self.p.stdin.flush()
        fd = self.p.stdout.fileno()
        r, _, _ = select.select([fd], [], [], timeout)
        if not r:
            raise TimeoutError(f"{method} no response in {timeout}s")
        line = self.p.stdout.readline()
        if not line:
            raise RuntimeError("server closed stdout")
        return json.loads(line)

    def tool(self, name, args, timeout=120):
        try:
            resp = self.rpc("tools/call", {"name": name, "arguments": args}, timeout)
        except (TimeoutError, RuntimeError) as e:
            return {"ok": False, "_rpc_error": str(e)}
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
    results.append((name, bool(cond)))
    mark = "PASS" if cond else "FAIL"
    suffix = f"  -- {detail[:400]}" if detail and not cond else ""
    print(f"[{mark}] {name}{suffix}", flush=True)


def ok(r):
    return isinstance(r, dict) and r.get("ok") is True


def host_flow(m, host, tunnel_port):
    pfx = f"{host}"
    # 1. attach, create if missing
    r = m.tool("session_attach", {"host": host, "session_name": SESSION}, timeout=30)
    if not ok(r):
        r = m.tool("session_create", {"host": host, "session_name": SESSION}, timeout=30)
        check(f"{pfx}: session_create", ok(r), str(r))
    else:
        check(f"{pfx}: session_attach", True)

    # 2. min pane_id
    r = m.tool("list_window_panes", {"host": host, "session_name": SESSION, "window_index": 0})
    panes = r.get("panes", [])
    check(f"{pfx}: list_window_panes", len(panes) > 0, str(r))
    if not panes:
        return
    pane = min((p["pane_id"] for p in panes), key=lambda s: int(s.lstrip("%")))
    base = {"host": host, "session_name": SESSION, "pane_id": pane}

    # 3. pane_info + capture_pane (terminal_state fields)
    r = m.tool("pane_info", base)
    check(f"{pfx}: pane_info", ok(r) and "terminal_state" in r, str(r))
    r = m.tool("capture_pane", base)
    check(f"{pfx}: capture_pane + terminal_state", ok(r) and "terminal_state" in r and "cursor" in r, str(r))

    # 4. exec harmless read-only command
    r = m.tool("exec", {**base, "command": "hostname && uptime", "timeout_ms": 15000})
    check(f"{pfx}: exec hostname/uptime", ok(r), str(r))
    state = r.get("terminal_state", "?")
    check(f"{pfx}: exec returns terminal_state", state in ("ready", "running", "unknown"), str(state))
    check(f"{pfx}: exec returns cursor", isinstance(r.get("cursor"), dict), str(r.get("cursor")))

    # 5. exec safety refusal: occupy terminal with sleep 30
    m.tool("send_keys", {**base, "keys": "sleep 30\n"})
    m.tool("wait_stable", {**base, "stable_ms": 400, "timeout_ms": 5000})
    r = m.tool("exec", {**base, "command": "echo SHOULD_NOT_RUN", "timeout_ms": 8000})
    refused = r.get("refused") is True or r.get("ok") is False
    check(f"{pfx}: exec refused while running", refused, str(r))
    m.tool("send_keys", {**base, "keys": "\x03"})
    m.tool("wait_stable", {**base, "stable_ms": 400, "timeout_ms": 5000})
    r = m.tool("exec", {**base, "command": "echo back_to_ready", "timeout_ms": 8000})
    check(f"{pfx}: exec works after Ctrl-C", ok(r), str(r))

    # 6. wait_for_text / find_pane_text with unique marker
    m.tool("send_keys", {**base, "keys": f"echo {MARK}\n"})
    r = m.tool("wait_for_text", {**base, "text": MARK, "timeout_ms": 10000})
    check(f"{pfx}: wait_for_text marker", ok(r), str(r))
    r = m.tool("find_pane_text", {**base, "pattern": MARK})
    found = ok(r) and (r.get("found") is True or r.get("matches"))
    check(f"{pfx}: find_pane_text marker", bool(found), str(r))

    # 7. file upload/download round-trip
    local_up = f"/tmp/mcp_smoke_up_{os.getpid()}.txt"
    local_dn = f"/tmp/mcp_smoke_dn_{os.getpid()}_{host}.txt"
    remote = f"/tmp/mcp_smoke_{os.getpid()}_{host}.txt"
    payload = f"yunying smoke {host} {MARK}\n"
    with open(local_up, "w") as f:
        f.write(payload)
    r = m.tool("file_upload", {"host": host, "local_path": local_up, "remote_path": remote})
    check(f"{pfx}: file_upload", ok(r), str(r))
    r = m.tool("file_download", {"host": host, "remote_path": remote, "local_path": local_dn})
    check(f"{pfx}: file_download", ok(r), str(r))
    got = open(local_dn).read() if os.path.exists(local_dn) else ""
    check(f"{pfx}: download content matches", got == payload, repr(got))
    m.tool("exec", {**base, "command": f"rm -f {remote}", "timeout_ms": 8000})
    os.unlink(local_dn) if os.path.exists(local_dn) else None
    os.unlink(local_up) if host == HOSTS[-1] else None

    # 8. tunnel create -> SSH banner through tunnel -> close
    r = m.tool("tunnel_create", {"host": host, "local_port": tunnel_port,
                                 "remote_host": "127.0.0.1", "remote_port": 22}, timeout=30)
    tid = r.get("tunnel_id")
    check(f"{pfx}: tunnel_create", ok(r) and tid, str(r))
    if tid:
        banner = b""
        try:
            with socket.create_connection(("127.0.0.1", tunnel_port), timeout=8) as s:
                s.settimeout(8)
                banner = s.recv(64)
        except OSError as e:
            banner = str(e).encode()
        check(f"{pfx}: tunnel carries SSH banner", banner.startswith(b"SSH-"), repr(banner))
        r = m.tool("tunnel_list", {})
        listed = tid in json.dumps(r)
        check(f"{pfx}: tunnel_list contains id", listed, str(r))
        r = m.tool("tunnel_close", {"tunnel_id": tid})
        check(f"{pfx}: tunnel_close", ok(r), str(r))

    # 9. capabilities
    r = m.tool("host_capabilities", {"host": host})
    check(f"{pfx}: host_capabilities", ok(r), str(r))

    # 10. error path: bad pane
    r = m.tool("exec", {**base, "pane_id": "%99", "command": "true", "timeout_ms": 8000})
    check(f"{pfx}: exec bad pane -> error", not ok(r), str(r))
    check(f"{pfx}: bad pane -> PANE_NOT_FOUND envelope",
          r.get("error_code") == "PANE_NOT_FOUND" and bool(r.get("recovery_hint"))
          and r.get("_is_error") is True, str(r))


def main():
    m = Mcp()
    m.rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                         "clientInfo": {"name": "mcp-remote-test", "version": "0"}})

    for i, host in enumerate(HOSTS):
        print(f"--- {host} ---", flush=True)
        try:
            host_flow(m, host, 19171 + i)
        except Exception as e:  # keep testing remaining hosts
            check(f"{host}: flow crashed", False, repr(e))

    print("--- batch ---", flush=True)
    r = m.tool("batch_exec", {"hosts": HOSTS, "command": "hostname"}, timeout=90)
    text = json.dumps(r)
    all_hosts = all(h in text for h in HOSTS)
    check("batch_exec all hosts", (ok(r) or "results" in r) and all_hosts, str(r)[:400])

    m.close()
    fails = [n for n, c in results if not c]
    print(f"\n{len(results) - len(fails)}/{len(results)} passed")
    if fails:
        print("FAILED:", *fails, sep="\n  ")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
