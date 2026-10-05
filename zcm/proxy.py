"""数据面：Key 到容器的反代，只做认证与字节级转发，不解析业务体、不跨容器重试。

认 `Authorization: Bearer` 与 `x-api-key` 两种头；请求体 ≤8MB 缓冲转发，
更大分块流式；响应逐块透传，空闲看门狗断开僵死上游。
"""
from __future__ import annotations

import asyncio

import httpx
from starlette.requests import Request
from starlette.responses import StreamingResponse

HOP_HEADERS = {
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
}
REQ_STRIP = HOP_HEADERS | {"host", "content-length", "authorization", "x-api-key"}
RESP_STRIP = HOP_HEADERS | {"content-length"}
MAX_BUFFER = 8 * 1024 * 1024


class UpstreamError(Exception):
    """容器不可达/请求未发出——调用方转 502 并标记账号不健康。"""


async def _build_content(request: Request):
    cl = request.headers.get("content-length")
    if cl:
        try:
            if int(cl) > MAX_BUFFER:
                return request.stream()
        except ValueError:
            pass
    return await request.body()


async def forward(
    request: Request,
    base: str,
    upstream_key: str,
    client: httpx.AsyncClient,
    idle_timeout: float,
) -> StreamingResponse:
    path = request.url.path
    query = request.url.query
    url = base.rstrip("/") + path + (f"?{query}" if query else "")

    headers = {k: v for k, v in request.headers.items() if k.lower() not in REQ_STRIP}
    headers["authorization"] = f"Bearer {upstream_key}"
    headers["x-api-key"] = upstream_key

    content = await _build_content(request)
    req = client.build_request(request.method, url, headers=headers, content=content)
    try:
        resp = await client.send(req, stream=True)
    except (httpx.ConnectError, httpx.ConnectTimeout) as e:
        raise UpstreamError(f"容器不可达：{type(e).__name__}") from e
    except httpx.HTTPError as e:
        raise UpstreamError(f"上游请求失败：{type(e).__name__}") from e

    resp_headers = {k: v for k, v in resp.headers.items() if k.lower() not in RESP_STRIP}
    resp_headers.setdefault("access-control-allow-origin", "*")
    is_sse = "text/event-stream" in resp_headers.get("content-type", "")

    async def stream():
        try:
            # 预加载型响应没有流可迭代
            if resp.is_stream_consumed:
                yield resp.content
                return
            it = resp.aiter_raw()
            while True:
                try:
                    # 兼容 3.9：不用内置 anext()
                    chunk = await asyncio.wait_for(it.__anext__(), timeout=idle_timeout)
                except asyncio.TimeoutError:
                    # SSE 先发注释帧再断
                    if is_sse:
                        yield b": upstream-idle-timeout\n\n"
                    break
                except StopAsyncIteration:
                    break
                yield chunk
        finally:
            await resp.aclose()

    return StreamingResponse(stream(), status_code=resp.status_code, headers=resp_headers)
