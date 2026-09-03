#!/usr/bin/env python3
"""Drive the hand-rolled MCP server with the official Python MCP client (`pip install mcp`).

    python scripts/mcp_interop_check.py --stdio target/debug/mcp-server     # spawns the server
    python scripts/mcp_interop_check.py --http http://127.0.0.1:8000/mcp   # against a running --http server

The engine must be reachable at ENGINE_ADDR (default http://127.0.0.1:50051). The client validates
structured results against the advertised output schemas, so a passing run also checks the schemas.
"""
import asyncio
import os
import sys

from mcp import Client, StdioServerParameters


async def main() -> None:
    mode, target = sys.argv[1], sys.argv[2]
    server = StdioServerParameters(command=target, env=dict(os.environ)) if mode == "--stdio" else target
    async with Client(server) as c:
        tools = (await c.list_tools()).tools
        names = [t.name for t in tools]
        print("tools:", names)
        assert len(names) == 10, names
        r = await c.call_tool("get_market_summary", {})
        assert not r.is_error, r
        print("summary:", r.structured_content)
        r = await c.call_tool("get_order_book", {"depth": 3})
        print("book:", r.structured_content)
        r = await c.call_tool("place_limit_order", {"side": "buy", "price_usdc": "3000.123", "quantity_eth": "0.5"})
        assert r.is_error, r
        print("bad tick ->", r.content[0].text)
        r = await c.call_tool("place_limit_order", {"side": "buy", "price_usdc": "2990.00", "quantity_eth": "0.5"})
        assert not r.is_error, r
        print("place:", r.structured_content)
        order_id = r.structured_content["order_id"]
        r = await c.call_tool("get_balances", {})
        assert not r.is_error, r
        print("balances:", r.structured_content)
        r = await c.call_tool("get_order", {"order_id": order_id})
        assert not r.is_error and r.structured_content["status"] == "open", r
        print("get_order:", r.structured_content["status"])
        r = await c.call_tool("get_quote", {"side": "sell", "quantity_eth": "0.25"})
        print("quote:", r.structured_content)
        r = await c.call_tool("list_orders", {})
        print("open orders:", r.structured_content["count"])
        r = await c.call_tool("cancel_order", {"order_id": order_id})
        print("cancel:", r.structured_content)
        r = await c.call_tool("cancel_all_orders", {})
        assert not r.is_error, r
        print("cancel_all:", r.structured_content["cancelled"])
        r = await c.call_tool("list_trades", {"limit": 5})
        print("trades:", r.structured_content["count"])
        res = await c.list_resources()
        print("resources:", [x.uri for x in res.resources])
        rr = await c.read_resource("market://ETH-USDC/summary")
        print("resource text:", rr.contents[0].text[:90])
        p = await c.get_prompt("trading_assistant")
        print("prompt messages:", len(p.messages))
        print("INTEROP OK")


asyncio.run(main())
