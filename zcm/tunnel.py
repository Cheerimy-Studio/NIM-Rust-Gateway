"""本地代理隧道：浏览器 → 本地脚本(WS) → 管理进程 → 账号出口（WARP/直连）。

WS 帧协议：
  客户端首帧（文本）：{"host": "...", "port": 12345}
  服务端回（文本）："ok"；出错时发 {"error": "..."} 后以 4502 关闭
  之后：二进制帧 = 原始 TCP 字节，双向对泵直到任一方断开
鉴权：查询串 ?token= 管理令牌，不符直接 4401 关闭。
"""
from __future__ import annotations

import asyncio
import json
import urllib.parse

from fastapi import WebSocket

from .composegen import warp_proxy


async def socks5_connect(proxy_host: str, proxy_port: int, host: str, port: int):
    """经 SOCKS5 代理连到 host:port；域名由代理端解析（gost 支持）。"""
    reader, writer = await asyncio.open_connection(proxy_host, proxy_port)
    writer.write(b"\x05\x01\x00")
    await writer.drain()
    greeting = await reader.readexactly(2)
    if greeting[0] != 5 or greeting[1] != 0:
        writer.close()
        raise RuntimeError("SOCKS5 握手失败")
    host_b = host.encode()
    writer.write(b"\x05\x01\x00\x03" + bytes([len(host_b)]) + host_b + int(port).to_bytes(2, "big"))
    await writer.drain()
    resp = await reader.readexactly(4)
    if resp[1] != 0:
        writer.close()
        raise RuntimeError(f"SOCKS5 CONNECT 失败（code {resp[1]}）")
    if resp[3] == 1:
        await reader.readexactly(4)
    elif resp[3] == 3:
        n = (await reader.readexactly(1))[0]
        await reader.readexactly(n)
    elif resp[3] == 4:
        await reader.readexactly(16)
    await reader.readexactly(2)
    return reader, writer


async def open_upstream(acc: dict, s, host: str, port: int, attempts: int = 3):
    """按账号出口建立到目标的连接：WARP 账号经其 gost，直连账号走本机。

    WARP 隧道对个别目标偶发瞬时不可达，快速重试可吸收；
    目标持续不可达时抛出最后一次错误。
    """
    last: Exception | None = None
    for i in range(attempts):
        try:
            proxy = warp_proxy(acc, s)
            if proxy:
                u = urllib.parse.urlparse(proxy)
                return await socks5_connect(
                    u.hostname or "127.0.0.1", u.port or 1080, host, port
                )
            return await asyncio.open_connection(host, port)
        except Exception as e:
            last = e
            if i < attempts - 1:
                await asyncio.sleep(0.25 * (i + 1))
    assert last is not None
    raise last


def register_tunnel(app, reg, s) -> None:
    @app.websocket("/api/tunnel/{acc_id}")
    async def tunnel(ws: WebSocket, acc_id: str, token: str = ""):
        if token != reg.admin_token:
            await ws.close(code=4401)
            return
        acc = reg.get(acc_id)
        if not acc:
            await ws.close(code=4404)
            return
        await ws.accept()
        try:
            info = json.loads(await ws.receive_text())
            host = str(info["host"])
            port = int(info["port"])
        except Exception:
            await ws.close(code=4400)
            return
        try:
            reader, writer = await open_upstream(acc, s, host, port)
        except Exception as e:
            try:
                await ws.send_text(
                    json.dumps({"error": f"出口连接失败：{e}"}, ensure_ascii=False)
                )
            finally:
                await ws.close(code=4502)
            return
        await ws.send_text("ok")

        async def ws_to_sock() -> None:
            while True:
                msg = await ws.receive()
                if msg["type"] == "websocket.disconnect":
                    break
                data = msg.get("bytes")
                if data is None:
                    data = (msg.get("text") or "").encode()
                writer.write(data)
                await writer.drain()

        async def sock_to_ws() -> None:
            while True:
                data = await reader.read(65536)
                if not data:
                    break
                await ws.send_bytes(data)

        tasks = {asyncio.create_task(ws_to_sock()), asyncio.create_task(sock_to_ws())}
        _done, pending = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        for t in pending:
            t.cancel()
        await asyncio.gather(*pending, return_exceptions=True)
        writer.close()
        try:
            await writer.wait_closed()
        except Exception:
            pass
        try:
            await ws.close()
        except Exception:
            pass
