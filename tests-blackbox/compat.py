# -*- coding: utf-8 -*-
"""兼容性冒烟测试：逐个打通全部接口，并校验 OpenAI / Anthropic 响应结构。

用法：python tests/compat.py
- 起一个 mock 上游 + 一个网关实例（独立临时数据目录，不碰生产 data/db.json）
- 断言：所有接口不返回 5xx；/v1/* 的响应结构符合对应协议
"""

import json, os, shutil, subprocess, sys, time, pathlib
import httpx

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TMP = pathlib.Path(os.environ.get("TEMP", ".")) / "ngw-compat-rust"

# 测试实例的管理员凭据：通过 NGW_ADMIN_PASSWORD 注入，避免依赖首次运行随机生成的密码
ADMIN_USER = "admin"
ADMIN_PW = "ngw-test-pass"
shutil.rmtree(TMP, ignore_errors=True)
TMP.mkdir(parents=True)

MOCK_PORT, GW_PORT = 18712, 18713

mock_src = """
from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse, StreamingResponse
import json, asyncio
app = FastAPI()

@app.get("/v1/models")
async def models():
    return {"object":"list","data":[{"id":"mock-model","object":"model","created":0,"owned_by":"t"}]}

@app.post("/v1/embeddings")
async def emb(request: Request):
    b = await request.json()
    return {"object":"list","data":[{"object":"embedding","index":0,"embedding":[0.1,0.2,0.3]}],
            "model": b.get("model",""), "usage":{"prompt_tokens":3,"total_tokens":3}}

@app.post("/v1/completions")
async def cmpl(request: Request):
    b = await request.json()
    if b.get("stream"):
        async def g():
            yield "data: " + json.dumps({"id":"c","object":"text_completion","choices":[{"index":0,"text":"Hi","finish_reason":None}]}) + "\\n\\n"
            yield "data: [DONE]\\n\\n"
        return StreamingResponse(g(), media_type="text/event-stream")
    return {"id":"c","object":"text_completion","model":b.get("model",""),
            "choices":[{"index":0,"text":"Hi","finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}

@app.post("/v1/chat/completions")
async def chat(request: Request):
    b = await request.json()
    m = b.get("model","")
    if b.get("stream"):
        async def g():
            for t in ["Hel","lo"]:
                yield "data: " + json.dumps({"id":"c","object":"chat.completion.chunk","model":m,
                    "choices":[{"index":0,"delta":{"content":t},"finish_reason":None}]}) + "\\n\\n"
                await asyncio.sleep(0.05)
            yield "data: " + json.dumps({"id":"c","object":"chat.completion.chunk","model":m,
                "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}) + "\\n\\n"
            yield "data: [DONE]\\n\\n"
        return StreamingResponse(g(), media_type="text/event-stream")
    return {"id":"c","object":"chat.completion","model":m,
            "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}
"""
(TMP / "mock.py").write_text(mock_src, encoding="utf-8")

env = {**os.environ, "NGW_DATA_DIR": str(TMP), "NGW_ADMIN_PASSWORD": ADMIN_PW}
mk = subprocess.Popen(
    [sys.executable, "-m", "uvicorn", "mock:app", "--port", str(MOCK_PORT)],
    cwd=TMP,
    env=env,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)
GW_EXE = os.path.join(ROOT, "target", "release", "nim-gateway" + (".exe" if os.name == "nt" else ""))
gw = subprocess.Popen(
    [GW_EXE, "--port", str(GW_PORT)],
    cwd=ROOT,
    env=env,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)

results = []


def add(name, ok, detail=""):
    results.append((name, bool(ok), detail))
    print("%s %s%s" % ("PASS" if ok else "FAIL", name, (" | " + detail) if detail else ""))


try:
    for _ in range(100):
        try:
            if (
                httpx.get("http://127.0.0.1:%d/" % GW_PORT, timeout=2).status_code == 200
                and httpx.get("http://127.0.0.1:%d/v1/models" % MOCK_PORT, timeout=2).status_code == 200
            ):
                break
        except Exception:
            pass
        time.sleep(0.4)

    a = httpx.Client(base_url="http://127.0.0.1:%d" % GW_PORT, timeout=60)
    r = a.post("/api/login", json={"username": ADMIN_USER, "password": ADMIN_PW})
    add("admin 登录", r.status_code == 200, "st=%s" % r.status_code)
    a.headers["X-CSRF"] = r.json()["csrf"]
    a.post(
        "/api/settings",
        json={
            "config": {
                "rate_limit_per_minute": 100000,
                "warmup_seconds": 0,
                "queue_enabled": False,
                "max_retries": 0,
                "ban_step_seconds": 0,
                "ban_max_seconds": 0,
                "cool_429_seconds": 0,
                "cool_5xx_seconds": 0,
                "breaker_enabled": False,
                "account_cooldown_ms": 0,
                "daily_request_cap": 0,
                "daily_token_limit": 0,
                "hourly_request_limit": 0,
                "tpm_limit": 0,
                "ttfb_timeout": 30,
                "sse_idle_timeout": 30,
            }
        },
    )
    uid = a.post(
        "/api/upstreams", json={"name": "MOCK", "base": "http://127.0.0.1:%d/v1" % MOCK_PORT, "enabled": True}
    ).json()["upstream"]["id"]
    a.post(
        "/api/keys/import",
        json={
            "text": "\n".join("u%d@e.com,p,nvapi-tk1234567%d" % (i, i) for i in range(1, 4)),
            "upstream_id": uid,
        },
    )
    tok = a.get("/api/settings").json()["gateway_tokens"][0]["t"]
    c = httpx.Client(
        base_url="http://127.0.0.1:%d" % GW_PORT, timeout=60, headers={"Authorization": "Bearer " + tok}
    )

    # ---------- OpenAI 兼容：响应结构 ----------
    r = c.post(
        "/v1/chat/completions", json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]}
    )
    j = r.json() if r.status_code == 200 else {}
    add(
        "chat 非流式结构",
        r.status_code == 200
        and j.get("object") == "chat.completion"
        and (j.get("choices") or [{}])[0].get("message", {}).get("content") == "ok"
        and (j.get("usage") or {}).get("total_tokens") == 7
        and j.get("model") == "mock-model",
        "object=%s usage=%s" % (j.get("object"), j.get("usage")),
    )

    ct, lines, code = "", [], 0
    with c.stream(
        "POST",
        "/v1/chat/completions",
        json={"model": "mock-model", "stream": True, "messages": [{"role": "user", "content": "hi"}]},
    ) as r:
        ct = r.headers.get("content-type", "")
        code = r.status_code
        lines = [l for l in r.iter_lines() if l]
    chunks = [json.loads(l[6:]) for l in lines if l.startswith("data: ") and l != "data: [DONE]"]
    add(
        "chat 流式结构",
        code == 200
        and ct.startswith("text/event-stream")
        and lines[-1] == "data: [DONE]"
        and all(x.get("object") == "chat.completion.chunk" for x in chunks)
        and "".join((x.get("choices") or [{}])[0].get("delta", {}).get("content", "") for x in chunks)
        == "Hello",
        "ct=%s 块数=%d" % (ct, len(chunks)),
    )

    r = c.post("/v1/completions", json={"model": "mock-model", "prompt": "hi"})
    j = r.json() if r.status_code == 200 else {}
    add(
        "completions 结构",
        r.status_code == 200
        and j.get("object") == "text_completion"
        and (j.get("choices") or [{}])[0].get("text") == "Hi",
        "st=%s" % r.status_code,
    )

    r = c.post("/v1/embeddings", json={"model": "mock-model", "input": "hi"})
    j = r.json() if r.status_code == 200 else {}
    add(
        "embeddings 结构",
        r.status_code == 200
        and j.get("object") == "list"
        and isinstance((j.get("data") or [{}])[0].get("embedding"), list),
        "st=%s" % r.status_code,
    )

    r = c.get("/v1/models")
    j = r.json() if r.status_code == 200 else {}
    add(
        "models 结构",
        r.status_code == 200 and j.get("object") == "list" and isinstance(j.get("data"), list),
        "共 %d 个" % len(j.get("data") or []),
    )

    r = c.get("/v1/models/mock-model")
    add("models/{id} 结构", r.status_code == 200 and r.json().get("id") == "mock-model")

    r = c.post("/v1/responses", json={"model": "mock-model", "input": "hi"})
    j = r.json() if r.status_code == 200 else {}
    add(
        "responses 结构",
        r.status_code == 200 and "object" in j,
        "object=%s keys=%s" % (j.get("object"), sorted(j.keys())[:8]),
    )

    r = c.post(
        "/v1/messages",
        json={"model": "mock-model", "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]},
    )
    j = r.json() if r.status_code == 200 else {}
    add(
        "messages(Anthropic) 结构",
        r.status_code == 200 and j.get("type") == "message" and isinstance(j.get("content"), list),
        "type=%s" % j.get("type"),
    )

    # Responses / Anthropic 的流式转换（走 core/streams.py 状态机）
    with c.stream("POST", "/v1/responses", json={"model": "mock-model", "input": "hi", "stream": True}) as r:
        c1, ev1 = r.status_code, [l for l in r.iter_lines() if l]
    add(
        "responses 流式转换",
        c1 == 200 and any("response." in e for e in ev1),
        "st=%s %d 事件" % (c1, len(ev1)),
    )

    with c.stream(
        "POST",
        "/v1/messages",
        json={
            "model": "mock-model",
            "max_tokens": 16,
            "stream": True,
            "messages": [{"role": "user", "content": "hi"}],
        },
    ) as r:
        c2, ev2 = r.status_code, [l for l in r.iter_lines() if l]
    add(
        "messages 流式转换",
        c2 == 200 and any(("message_" in e or "content_block" in e) for e in ev2),
        "st=%s %d 事件" % (c2, len(ev2)),
    )

    r = c.request("OPTIONS", "/v1/chat/completions")
    add(
        "CORS 预检",
        r.status_code in (200, 204) and "access-control-allow-origin" in {k.lower() for k in r.headers},
        "st=%s" % r.status_code,
    )

    # 后台页面/脚本必须禁缓存：否则升级代码后浏览器仍用旧 admin.js，
    # 新加的界面（如模型测试页）会"打不开"。同时确认模型测试页结构在位。
    ra = a.get("/admin")
    rj = a.get("/assets/admin.js?v=1.7.0")
    add(
        "后台资源禁缓存",
        "no-cache" in (ra.headers.get("cache-control") or "")
        and "no-cache" in (rj.headers.get("cache-control") or ""),
        "/admin=%s js=%s" % (ra.headers.get("cache-control"), rj.headers.get("cache-control")),
    )
    add(
        "模型测试页在位",
        ra.status_code == 200
        and 'id="pane-test"' in ra.text
        and 'id="test-send"' in ra.text
        and "loadTestModels" in rj.text,
        "pane 与 JS 均已包含",
    )

    add(
        "无效令牌拒绝",
        c.post(
            "/v1/chat/completions",
            json={"model": "mock-model", "messages": []},
            headers={"Authorization": "Bearer bad"},
        ).status_code
        == 401,
    )

    # ---------- 全部管理端接口：不得 5xx ----------
    uid2 = a.post(
        "/api/upstreams", json={"name": "MOCK2", "base": "http://127.0.0.1:%d/v1" % MOCK_PORT}
    ).json()["upstream"]["id"]
    admin_calls = [
        ("GET", "/api/overview", None),
        ("GET", "/api/keys", None),
        ("GET", "/api/keydetail", {"id": a.get("/api/keys").json()["rows"][0]["id"]}),
        ("POST", "/api/keys/import", {"text": "z@e.com,p,nvapi-tk99999999", "upstream_id": uid}),
        ("GET", "/api/keys/export", None),
        ("GET", "/api/logs", None),
        ("GET", "/api/sessions", None),
        ("GET", "/api/upstreams", None),
        ("GET", "/api/settings", None),
        ("GET", "/api/queue", None),
        ("POST", "/api/queue", {"enabled": True, "max_wait": 20}),
        ("GET", "/api/presets", None),
        ("GET", "/api/queue/public", None),
        ("GET", "/api/config/export", None),
        ("POST", "/api/settings", {"config": {"max_retries": 2}}),
        ("POST", "/api/logs/clear", {}),
        ("POST", "/api/sessions/clear", {}),
        ("POST", "/api/stats/reset", {}),
        ("GET", "/", None),
        ("GET", "/admin", None),
        ("GET", "/queue", None),
        ("GET", "/assets/admin.js", None),
    ]
    bad = []
    for method, path, payload in admin_calls:
        try:
            r = a.request(method, path, json=payload) if payload is not None else a.request(method, path)
            if r.status_code >= 500:
                bad.append("%s %s -> %s %s" % (method, path, r.status_code, r.text[:120]))
        except Exception as e:
            bad.append("%s %s -> EXC %s" % (method, path, e))
    add("管理端接口无 5xx", not bad, "; ".join(bad) if bad else "%d 个接口全通" % len(admin_calls))

    # ---------- 破坏性/写操作接口：必须真正成功（200），不是仅仅不崩 ----------
    kids = [k["id"] for k in a.get("/api/keys").json()["rows"][:2]]
    cases = [
        ("keys/op enable", "/api/keys/op", {"op": "enable", "id": kids[0]}, 200),
        ("keys/op disable", "/api/keys/op", {"op": "disable", "id": kids[0]}, 200),
        ("keys/op test(真实探测)", "/api/keys/op", {"op": "test", "id": kids[0]}, 200),
        ("keys/batch enable", "/api/keys/batch", {"op": "enable", "ids": kids}, 200),
        ("keys/batch reset", "/api/keys/batch", {"op": "reset", "ids": kids}, 200),
        ("password 修改", "/api/password", {"old": ADMIN_PW, "new": ADMIN_PW}, 200),
        ("upstreams/delete 有账号时拒绝", "/api/upstreams/delete", {"id": uid}, 400),
        ("upstreams/delete 空渠道成功", "/api/upstreams/delete", {"id": uid2}, 200),
        ("keys/clear-all", "/api/keys/clear-all", {"confirm": "yes"}, 200),
        ("config/import", "/api/config/import", {"config": {}, "upstreams": [], "keys": []}, 200),
    ]
    bad2 = []
    for name, path, payload, want in cases:
        r = a.post(path, json=payload)
        if name == "password 修改" and r.status_code == 200:
            # 改密会轮换 session_secret(安全设计:吊销改密前的所有会话),
            # 需要重新登录(会话与 CSRF 都随之轮换)后才能继续验证后续写操作
            rl = a.post("/api/login", json={"username": ADMIN_USER, "password": ADMIN_PW})
            if rl.status_code == 200:
                a.headers["X-CSRF"] = rl.json()["csrf"]
        if r.status_code != want:
            bad2.append("%s: 期望%s 实际%s %s" % (name, want, r.status_code, r.text[:100]))
    add("写操作接口行为正确", not bad2, "; ".join(bad2) if bad2 else "%d 项全部符合预期" % len(cases))

    r = a.post("/api/login", json={"username": ADMIN_USER, "password": ADMIN_PW})
    add("重新登录", r.status_code == 200, "st=%s" % r.status_code)
    add("登出", a.post("/api/logout").status_code < 500)
finally:
    gw.terminate()
    mk.terminate()

fails = [x for x in results if not x[1]]
print("\n===== 兼容性结果 =====")
print("通过 %d / %d" % (len(results) - len(fails), len(results)))
if fails:
    print("FAILURES")
    sys.exit(1)
print("ALL PASS")
