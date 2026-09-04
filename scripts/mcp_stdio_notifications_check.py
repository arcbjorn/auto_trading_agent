#!/usr/bin/env python3
"""Checks that the stdio transport pushes `notifications/resources/updated` after engine events.

    python scripts/mcp_stdio_notifications_check.py target/debug/mcp-server

Spawns the server on stdio (the engine must be reachable at ENGINE_ADDR with a funded ACCOUNT_ID),
subscribes to the market summary, places and cancels a small order through the tools, and waits for
the notification the pump sends. No MCP client library is needed: the wire format is one JSON-RPC
message per line.
"""
import json
import os
import subprocess
import sys
import time


def main() -> None:
    server = subprocess.Popen(
        [sys.argv[1]], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, env=dict(os.environ)
    )
    next_id = 0

    def send(method, params=None, notify=False):
        nonlocal next_id
        msg = {"jsonrpc": "2.0", "method": method, "params": params or {}}
        if not notify:
            next_id += 1
            msg["id"] = next_id
        server.stdin.write(json.dumps(msg) + "\n")
        server.stdin.flush()
        return None if notify else next_id

    def read_until(pred, seconds):
        deadline = time.time() + seconds
        seen = []
        while time.time() < deadline:
            line = server.stdout.readline()
            if not line:
                break
            msg = json.loads(line)
            seen.append(msg)
            if pred(msg):
                return msg, seen
        return None, seen

    rid = send("initialize", {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "check", "version": "0"}})
    init, _ = read_until(lambda m: m.get("id") == rid, 5)
    assert init and init["result"]["capabilities"]["resources"]["subscribe"] is True, init
    send("notifications/initialized", notify=True)
    rid = send("resources/subscribe", {"uri": "market://ETH-USDC/summary"})
    ok, _ = read_until(lambda m: m.get("id") == rid, 5)
    assert ok and "result" in ok, ok
    rid = send("tools/call", {"name": "place_limit_order", "arguments": {"side": "buy", "price_usdc": "2500.00", "quantity_eth": "0.01"}})
    placed, seen = read_until(lambda m: m.get("id") == rid, 10)
    assert placed and not placed["result"]["isError"], placed
    order_id = placed["result"]["structuredContent"]["order_id"]
    updated, seen2 = read_until(lambda m: m.get("method") == "notifications/resources/updated", 5)
    assert updated, f"no notification after the placement; saw {seen + seen2}"
    assert updated["params"]["uri"] == "market://ETH-USDC/summary"
    assert "id" not in updated
    send("tools/call", {"name": "cancel_order", "arguments": {"order_id": order_id}})
    server.stdin.close()
    server.wait(timeout=5)
    print("STDIO NOTIFICATIONS OK")


main()
