# -*- coding: utf-8 -*-
import json, os, shutil, subprocess, sys, threading, time
import httpx

# 输出强制 UTF-8:用例详情里会出现上游原文/替换字符(\ufffd)等任意文本,
# 而 Windows 下重定向到文件时默认是 GBK —— 打印结果那一步会直接抛 UnicodeEncodeError,
# 让「所有用例都跑完了」变成「一行结果都看不到」(还可能卡在收尾 wait 上)。
try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")
except Exception:
    pass

# 允许从任意目录运行：定位到项目根
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TMP = os.path.join(os.environ.get("TEMP", "."), "ngw-reg4-%d" % os.getpid())

# 测试实例的管理员凭据：通过 NGW_ADMIN_PASSWORD 注入，避免依赖首次运行随机生成的密码
ADMIN_USER = "admin"
ADMIN_PW = "ngw-test-pass"
shutil.rmtree(TMP, ignore_errors=True)
# 每轮用独立目录(带 pid):上一轮进程没退干净时旧目录删不掉,而「复用旧目录」会让
# 整套用例读到脏 db（曾经因此崩在 437 行）。删不掉就换一个目录,而不是硬崩。
os.makedirs(TMP, exist_ok=True)

mock = """
from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse, Response, StreamingResponse
import io, json, asyncio, tarfile
app = FastAPI()
_c = {"rl": 0, "flaky": 0}

@app.get("/v1/models")
async def models():
    return {"object":"list","data":[{"id":"mock-model","object":"model","created":0,"owned_by":"t"}]}

@app.post("/fail/v1/chat/completions")
async def chat_fail(request: Request):
    # 只按「渠道」失败的路径：用于验证按模型的可靠性路由
    await request.json()
    return JSONResponse({"error":{"message":"this channel is broken for the model"}}, status_code=500)

@app.get("/update-test.tar.gz")
async def upd_tar():
    # 演练用的「更新包」:结构等同 GitHub tarball(顶层目录 + 源码 + data/tests)
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tf:
        for name, body in [
            ("pkg-main/nim-gateway-x86_64-pc-windows-msvc.exe", "MZ-RUST-UPDATE-PAYLOAD"),
            ("pkg-main/server.py", "x = 1"),
            ("pkg-main/core/util.py", "y = 2"),
            ("pkg-main/web/upd.html", "<b>upd</b>"),
            ("pkg-main/notes.txt", "hello"),
            ("pkg-main/data/db.json", '{"REAL":1}'),
            ("pkg-main/tests/t.py", "z = 3"),
        ]:
            b = body.encode("utf-8")
            ti = tarfile.TarInfo(name)
            ti.size = len(b)
            tf.addfile(ti, io.BytesIO(b))
    return Response(content=buf.getvalue(), media_type="application/gzip")

@app.post("/v1/chat/completions")
async def chat(request: Request):
    b = await request.json()
    m = b.get("model","")
    eff = b.get("thinking_effort") or b.get("reasoning_effort") or ""
    st = b.get("stream")
    if m == "kimi" and eff and eff != "low":
        return JSONResponse({"error":{"message":"Unsupported thinking_effort"}}, status_code=400)
    if m == "empty":
        return JSONResponse({}, status_code=200)
    if m == "flaky":
        _c["flaky"] += 1
        if _c["flaky"] == 1: return JSONResponse({"error":{"message":"t"}}, status_code=502)
    if m == "rl":
        _c["rl"] += 1
        if _c["rl"] <= 2: return JSONResponse({"error":{"message":"rate limited"}}, status_code=429)
    if m == "authfail":
        # 401:网关按鉴权失败硬封禁该账号(hard_fail_ban_seconds)
        return JSONResponse({"error":{"message":"invalid api key"}}, status_code=401)
    if b.get("tool_choice") is not None and not b.get("tools"):
        # 模拟严格校验的上游(FastAPI/vLLM 系):tool_choice 无 tools 直接拒绝 ——
        # 网关必须在转发前清理这种无意义组合
        return JSONResponse(
            {"detail": [{"type": "value_error", "loc": ["body"],
                         "msg": "Value error, When using `tool_choice`, `tools` must be set."}]},
            status_code=400,
        )
    _mt = b.get("max_tokens") or b.get("max_completion_tokens")
    if isinstance(_mt, int) and _mt <= 0:
        # 模拟严格上游:max_tokens 非法值(客户端按上下文窗口算出超大负数)直接拒绝
        return JSONResponse(
            {"error": {"message": "max_tokens must be at least 1, got %s. (parameter=max_tokens)" % _mt}},
            status_code=400,
        )
    if m == "chdown":
        return JSONResponse({"error":{"message":"No available channel"}}, status_code=500)
    if m == "nomodel":
        return JSONResponse({"error":{"message":"model '%s' not found" % m}}, status_code=404)
    if m == "path404":
        # 上游对「不存在的资源/错误路径」的 404:泛化短语不含模型,不能算模型不存在
        return JSONResponse({"error":{"message":"Endpoint does not exist"}}, status_code=404)
    if m == "nousage":
        # 上游不返回 usage —— 严格客户端(New-API)会因此判渠道测试失败
        return {"id":"c1","object":"chat.completion","model":m,
                "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}
    if m == "ssejson":
        # 上游用 application/json 声明 SSE 体 —— 原样转发会让客户端按 JSON 解析而失败
        async def g3():
            yield "data: " + json.dumps({"model":m,"choices":[{"delta":{"content":"Hi"}}]}) + "\\n\\n"
            yield "data: [DONE]\\n\\n"
        return StreamingResponse(g3(), media_type="application/json")
    if m == "slowfirst":
        # 模拟推理模型首字节很慢：期间网关必须先发心跳占住连接
        async def g4():
            await asyncio.sleep(9)
            yield "data: " + json.dumps({"model":m,"choices":[{"delta":{"content":"Slow"}}]}) + "\\n\\n"
            yield "data: [DONE]\\n\\n"
        return StreamingResponse(g4(), media_type="text/event-stream")
    if m == "slowhdr":
        # 先等待再构造响应 —— 让上游连「响应头」都迟迟不返回（推理模型排队时的真实表现）
        await asyncio.sleep(20)
        async def g5():
            yield "data: " + json.dumps({"model":m,"choices":[{"delta":{"content":"Hdr"}}]}) + "\\n\\n"
            yield "data: [DONE]\\n\\n"
        return StreamingResponse(g5(), media_type="text/event-stream")
    if m == "brokenstream":
        # 上游下发一部分后突然断开（无 [DONE]）——网关必须显式报错，不能当正常结束
        async def g6():
            yield "data: " + json.dumps({"model": m, "choices": [{"delta": {"content": "part"}}]}) + "\\n\\n"
            raise RuntimeError("upstream died mid-stream")
        return StreamingResponse(g6(), media_type="text/event-stream")
    if m == "hold4":
        await asyncio.sleep(4)   # 占住并发，用于逼出排队
        return {"id":"c1","object":"chat.completion","model":m,
                "choices":[{"index":0,"message":{"role":"assistant","content":"held"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}
    if m == "emptystream":
        # 流式响应但没有任何内容：会走「空流保护」，属于未交接给透传的失败路径
        async def g7():
            if False:
                yield b""
        return StreamingResponse(g7(), media_type="text/event-stream")
    if m == "nodone":
        # 正常下发内容后直接结束连接（不带 [DONE]）——转换流必须补发终端事件,
        # 否则 Anthropic/Responses 客户端会一直等 message_stop/response.completed 而挂起
        async def g8():
            yield "data: " + json.dumps({"model":m,"choices":[{"delta":{"content":"nodone"}}]}) + "\\n\\n"
            await asyncio.sleep(0.1)
        return StreamingResponse(g8(), media_type="text/event-stream")
    if m == "thinkjunk":
        # 思考退化场景:先输出合法思考,再退化成成片感叹号,另有分隔线(不该被误伤)
        if st:
            async def g10():
                yield "data: " + json.dumps({"model": m, "choices": [{"delta": {"reasoning_content": "让我思考一下。"}}]}, ensure_ascii=False) + "\\n\\n"
                yield "data: " + json.dumps({"model": m, "choices": [{"delta": {"reasoning_content": "----------\\n"}}]}, ensure_ascii=False) + "\\n\\n"
                yield "data: " + json.dumps({"model": m, "choices": [{"delta": {"reasoning_content": "!!!!!!!!!!!!!!!!!!!!!!!!"}}]}, ensure_ascii=False) + "\\n\\n"
                yield "data: " + json.dumps({"model": m, "choices": [{"delta": {"content": "答案"}}]}, ensure_ascii=False) + "\\n\\n"
                yield "data: [DONE]\\n\\n"
            return StreamingResponse(g10(), media_type="text/event-stream")
        return {"id": "c1", "object": "chat.completion", "model": m,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "答案",
                             "reasoning_content": "!!!!!!!!!!!!!!!!!!!!!!!!!!"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}}
    if m == "thinker":
        # 干净的思考内容:Anthropic 侧必须转成带 signature 的 thinking 块
        return {"id": "c1", "object": "chat.completion", "model": m,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "答案",
                             "reasoning_content": "先想一下"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}}
    if m == "dupfield":
        _hist_rc = any(isinstance(mm, dict) and ("reasoning_content" in mm or "reasoning" in mm)
                       for mm in b.get("messages", []))
        if _hist_rc:
            msg = "Failed to deserialize the JSON body into the target type: duplicate field `reasoning_content` at line 1 column 143731"
            if st:
                async def g11():
                    yield "data: " + json.dumps({"error": {"message": msg}}) + "\\n\\n"
                return StreamingResponse(g11(), media_type="text/event-stream")
            return JSONResponse({"error": {"message": msg}}, status_code=400)
        return {"id": "c1", "object": "chat.completion", "model": m,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}}
    if m == "enablethink" and "enable_thinking" in b:
        msg = "Validation: Unsupported parameter(s): `enable_thinking`"
        if st:
            async def g12():
                yield "data: " + json.dumps({"error": {"message": msg}}) + "\\n\\n"
            return StreamingResponse(g12(), media_type="text/event-stream")
        return JSONResponse({"error": {"message": msg}}, status_code=400)
    if st:
        async def g():
            for t in ["Hello"," world"]:
                yield "data: " + json.dumps({"model":m,"choices":[{"delta":{"content":t}}]}) + "\\n\\n"
                await asyncio.sleep(0.1)
            yield "data: [DONE]\\n\\n"
        return StreamingResponse(g(), media_type="text/event-stream")
    return {"id":"c1","object":"chat.completion","model":m,
            "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}
"""
open(os.path.join(TMP, "mock.py"), "w", encoding="utf-8").write(mock)

env = {
    **os.environ,
    "NGW_DATA_DIR": TMP,
    "NGW_ADMIN_PASSWORD": ADMIN_PW,
    # 更新/回滚的备份目录也隔离到临时目录：否则 target/release 里残留的 backup/
    # 会让「有令牌但无备份 → 500」这条用例真的执行回滚并重启网关
    "NGW_BASE_DIR": TMP,
    # 更新管线回归:源指向本地 mock 的假更新包,演练模式只下载/解包/自检,
    # 不覆盖任何文件也不重启(真实 GitHub 源与非演练路径不在此测试)
    "NGW_UPDATE_DRYRUN": "1",
    "NGW_UPDATE_URL": "http://127.0.0.1:18212/update-test.tar.gz",
    # 避免本机残留代理设置把 127.0.0.1 的假更新源绕到代理上
    "http_proxy": "",
    "https_proxy": "",
    "HTTP_PROXY": "",
    "HTTPS_PROXY": "",
    "no_proxy": "*",
    "NO_PROXY": "*",
}
mk = subprocess.Popen(
    [sys.executable, "-m", "uvicorn", "mock:app", "--port", "18212"],
    cwd=TMP,
    env=env,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)
GW_EXE = os.path.join(ROOT, "target", "release", "nim-gateway" + (".exe" if os.name == "nt" else ""))
gw = subprocess.Popen(
    [GW_EXE, "--port", "18213"],
    cwd=ROOT,
    env=env,
    stdout=subprocess.DEVNULL,
    stderr=open(os.path.join(TMP, "gateway-stderr.log"), "w", encoding="utf-8"),
)
try:
    for _ in range(100):
        try:
            if (
                httpx.get("http://127.0.0.1:18213/", timeout=2).status_code == 200
                and httpx.post(
                    "http://127.0.0.1:18212/v1/chat/completions", json={"model": "x"}, timeout=2
                ).status_code
                == 200
            ):
                break
        except Exception:
            pass
        time.sleep(0.4)
    a = httpx.Client(base_url="http://127.0.0.1:18213", timeout=60)
    r = a.post("/api/login", json={"username": ADMIN_USER, "password": ADMIN_PW})
    a.headers["X-CSRF"] = r.json()["csrf"]
    a.post(
        "/api/settings",
        json={
            "config": {
                "rate_limit_per_minute": 100000,
                "account_cooldown_ms": -1,
                "warmup_seconds": 0,
                "queue_enabled": True,
                "queue_max_wait": 8,
                "queue_poll_ms": 150,
                "max_retries": 2,
                "retry_backoff_base_ms": 10,
                "retry_backoff_max_ms": 20,
                "retry_min_wait_ms": 0,
                "ban_step_seconds": 0,
                "ban_max_seconds": 0,
                "daily_request_cap": 0,
                "daily_token_limit": 0,
                "hourly_request_limit": 0,
                "cool_429_seconds": 1,
                "cool_5xx_seconds": 0,
                "breaker_enabled": False,
                "ttfb_timeout": 30,
                "sse_idle_timeout": 30,
            }
        },
    )
    r = a.post(
        "/api/upstreams",
        json={
            "name": "T",
            "base": "http://127.0.0.1:18212/v1",
            "enabled": True,
            "thinking_defaults": "kimi=low",
        },
    )
    uid = r.json()["upstream"]["id"]
    a.post(
        "/api/keys/import",
        json={
            "text": "u@e.com,p,nvapi-tk12345678\nu2@e.com,p,nvapi-tk22345678\nu3@e.com,p,nvapi-tk32345678",
            "upstream_id": uid,
        },
    )
    toks = a.get("/api/settings").json()["gateway_tokens"]
    c = httpx.Client(
        base_url="http://127.0.0.1:18213", timeout=60, headers={"Authorization": "Bearer " + toks[0]["t"]}
    )

    results = []

    def add(n, ok, d=""):
        results.append((n, bool(ok), d))

    # RPM 时间戳只能记给「真正被使用」的账号。曾经给所有候选账号都打点，导致每个账号的
    # 60 秒窗口被无谓塞满、整池一起撞上单账号上限 → 号池假性枯竭（吞吐被压到约等于单账号
    # RPM，与账号数量无关）。此处号池刚建、窗口为空，正好验证：3 账号 × rpm=4 ⇒ 12 次全过。
    a.post("/api/settings", json={"config": {"rate_limit_per_minute": 4}})
    ok_rpm = 0
    for _ in range(12):
        r = c.post(
            "/v1/chat/completions",
            json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
        )
        if r.status_code == 200:
            ok_rpm += 1
    add("RPM 按账号计而非全池", ok_rpm == 12, "rpm=4 × 3 账号，12 次全部成功（%d/12）" % ok_rpm)
    a.post("/api/settings", json={"config": {"rate_limit_per_minute": 100000}})

    r = c.post(
        "/v1/chat/completions", json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]}
    )
    add(
        "chat 非流式",
        r.status_code == 200 and r.json().get("object") == "chat.completion",
        "st=%s" % r.status_code,
    )

    t0 = time.time()
    first = None
    parts = []
    code = 0
    with c.stream(
        "POST",
        "/v1/chat/completions",
        json={"model": "mock-model", "stream": True, "messages": [{"role": "user", "content": "hi"}]},
    ) as r:
        for line in r.iter_lines():
            if line and first is None:
                first = time.time() - t0
            parts.append(line)
        code = r.status_code
    body = "|".join(parts)
    total = time.time() - t0
    add(
        "chat 流式(真流式)",
        code == 200 and "Hello" in body and "[DONE]" in body and first is not None and first < total,
        "首字=%.2fs 总=%.2fs" % (first or -1, total),
    )

    r = c.post(
        "/v1/chat/completions",
        json={"model": "kimi", "thinking_effort": "medium", "messages": [{"role": "user", "content": "hi"}]},
    )
    add("thinking_effort 降级", r.status_code == 200, "st=%s" % r.status_code)
    r = c.post(
        "/v1/chat/completions", json={"model": "empty", "messages": [{"role": "user", "content": "hi"}]}
    )
    add("空响应保护", r.status_code >= 400, "st=%s" % r.status_code)
    r = c.post(
        "/v1/chat/completions", json={"model": "flaky", "messages": [{"role": "user", "content": "hi"}]}
    )
    add("同号重试", r.status_code == 200, "st=%s" % r.status_code)
    r = c.post("/v1/chat/completions", json={"model": "rl", "messages": [{"role": "user", "content": "hi"}]})
    add("429 吸收", r.status_code == 200, "st=%s" % r.status_code)
    r = c.post(
        "/v1/chat/completions", json={"model": "chdown", "messages": [{"role": "user", "content": "hi"}]}
    )
    add("渠道级快速失败", r.status_code >= 400, "st=%s" % r.status_code)
    # /v1/models 语义：渠道未配置模型白名单时网关是透传的，任意模型名都放行
    add("models 列表", c.get("/v1/models").status_code == 200)
    add("models/{已知}", c.get("/v1/models/mock-model").status_code == 200)
    add(
        "models/{未知}放行(未配置白名单)",
        c.get("/v1/models/nope").status_code == 200,
        "st=%s" % c.get("/v1/models/nope").status_code,
    )

    # 配置白名单后：/v1/models 只返回网关支持的模型（绝不能泄漏上游真实模型清单），
    # 且未知模型必须 404。上游模型列表刻意不访问，所以这里也验证不发生上游请求。
    allow = "mock-model,kimi,empty,flaky,rl,chdown,nousage,ssejson,slowfirst,slowhdr"
    a.post(
        "/api/upstreams",
        json={"id": uid, "name": "T", "base": "http://127.0.0.1:18212/v1", "enabled": True, "models": allow},
    )
    ids = [m["id"] for m in c.get("/v1/models").json()["data"]]
    add(
        "models 只返回网关配置的模型",
        sorted(ids) == sorted(allow.split(",")),
        "共 %d 个：%s" % (len(ids), ids[:4]),
    )
    add(
        "models/{未知}404(配置白名单后)",
        c.get("/v1/models/nope").status_code == 404,
        "st=%s" % c.get("/v1/models/nope").status_code,
    )
    a.post(
        "/api/upstreams",
        json={
            "id": uid,
            "name": "T",
            "base": "http://127.0.0.1:18212/v1",
            "enabled": True,
            "models": "mock-model,shadow-model",
            "model_map": "my-alias=shadow-model",
            "hide_mapped": 1,
        },
    )
    ids = [m["id"] for m in c.get("/v1/models").json()["data"]]
    add(
        "禁用原名时清单剔除原名",
        "shadow-model" not in ids and "my-alias" in ids and "mock-model" in ids,
        "清单=%s（上游原名 shadow-model 已剔除）" % ids,
    )
    a.post(
        "/api/upstreams",
        json={
            "id": uid,
            "name": "T",
            "base": "http://127.0.0.1:18212/v1",
            "enabled": True,
            "models": "",
            "model_map": "",
            "hide_mapped": 0,
        },
    )

    # 模型清单解析容错：把从 Python/JSON 复制来的 ['a','b'] 粘进输入框，不能整段当成一个
    # 模型名存下来（否则白名单里挂着一个永远匹配不上的名字，该渠道等于废掉）。
    # 同时模型名自带 [free] 之类后缀必须原样保留。
    for raw, want in (
        ("['mock-model']", ["mock-model"]),
        ('["mock-model"]', ["mock-model"]),
        ("mock-model,shadow-model", ["mock-model", "shadow-model"]),
        ("Suffix-Model[free]", ["Suffix-Model[free]"]),
    ):
        a.post(
            "/api/upstreams",
            json={
                "id": uid,
                "name": "T",
                "base": "http://127.0.0.1:18212/v1",
                "enabled": True,
                "models": raw,
            },
        )
        got = next(x for x in (a.get("/api/upstreams").json().get("rows") or []) if x["id"] == uid)["models"]
        add("模型清单解析容错", got == want, "%r -> %s" % (raw, got))
    a.post(
        "/api/upstreams",
        json={"id": uid, "name": "T", "base": "http://127.0.0.1:18212/v1", "enabled": True, "models": ""},
    )

    # 回归线上真实故障：列表页的「启用/禁用」按钮曾把整行对象回传（models 是数组、
    # model_map 是字典），后端按文本解析就把白名单写成了 ["['a']"] —— 该渠道从此对
    # 任何请求都判「渠道模型不匹配」，等于废掉。停用→启用一轮后配置必须原样完好。
    def _up_row():
        return next(x for x in (a.get("/api/upstreams").json().get("rows") or []) if x["id"] == uid)

    a.post(
        "/api/upstreams",
        json={
            "id": uid,
            "name": "T",
            "base": "http://127.0.0.1:18212/v1",
            "enabled": True,
            "models": ["mock-model"],
            "model_map": {"ali": "upstream-x"},
        },
    )
    row = _up_row()
    a.post("/api/upstreams", json={**row, "enabled": False})  # 模拟点击「停用」
    off = _up_row()
    a.post("/api/upstreams", json={**off, "enabled": True})  # 模拟点击「启用」
    on = _up_row()
    add(
        "停用/启用不改写渠道配置",
        off["enabled"] is False
        and on["enabled"] is True
        and off["models"] == ["mock-model"]
        and on["models"] == ["mock-model"]
        and off["model_map"] == {"ali": "upstream-x"}
        and on["model_map"] == {"ali": "upstream-x"},
        "models=%s model_map=%s" % (on["models"], on["model_map"]),
    )
    a.post(
        "/api/upstreams",
        json={
            "id": uid,
            "name": "T",
            "base": "http://127.0.0.1:18212/v1",
            "enabled": True,
            "models": "",
            "model_map": "",
        },
    )
    add("responses", c.post("/v1/responses", json={"model": "mock-model", "input": "hi"}).status_code == 200)
    # reasoning 被客户端简写成字符串（"high"）时不能 500：它按 Responses API 是对象，
    # 但简写很常见；以前 (req.get("reasoning") or {}).get("effort") 会因 str 没有 .get
    # 抛 AttributeError 变成 500。
    r = c.post("/v1/responses", json={"model": "mock-model", "input": "hi", "reasoning": "high"})
    add("reasoning 传字符串不 500", r.status_code == 200, "st=%s" % r.status_code)
    r = c.post("/v1/responses", json={"model": "mock-model", "input": "hi", "reasoning": {"effort": "high"}})
    add("reasoning 传对象正常", r.status_code == 200, "st=%s" % r.status_code)
    # 渠道「思考强度默认值」的取值集合必须覆盖上游实际支持的值（Kimi 文档里就有 max），
    # 否则配了也会被静默丢弃、退化成「删掉思考参数」。
    sys.path.insert(0, ROOT)
    from core.convert import parse_thinking_defaults as _ptd

    got_d = _ptd("\n".join(["kimi=max", "qwen3=low", "glm=nonsense"]))
    add("思考强度配置解析", got_d == {"kimi": "max", "qwen3": "low"}, "%s（非法值应被忽略）" % got_d)

    # 参数覆写：值里带逗号的数组不能被切断（以前按逗号盲拆，stop=["a","b"] 只会剩 ["a"），
    # 且数组/对象要解析成 JSON 原生类型 —— 否则上游按字符串收到数组会直接拒绝。
    from core.upstreams import _parse_param_pairs as _ppp

    pp = _ppp('stop=["a","b"]')
    pp2 = _ppp("top_p=0.9,seed=42")
    add(
        "参数覆写解析",
        pp == {"stop": ["a", "b"]} and pp2 == {"top_p": 0.9, "seed": 42},
        "数组=%s 多组=%s" % (pp, pp2),
    )
    add(
        "messages",
        c.post(
            "/v1/messages",
            json={"model": "mock-model", "max_tokens": 10, "messages": [{"role": "user", "content": "hi"}]},
        ).status_code
        == 200,
    )
    add("queue API", a.get("/api/queue").status_code == 200)

    # 上游漏字段时的响应补全：严格客户端(New-API 渠道测试/计费)靠这些字段判定成败
    r = c.post(
        "/v1/chat/completions", json={"model": "nousage", "messages": [{"role": "user", "content": "hi"}]}
    )
    u = (r.json() or {}).get("usage") if r.status_code == 200 else None
    add(
        "缺 usage 自动补齐",
        r.status_code == 200
        and isinstance(u, dict)
        and isinstance(u.get("total_tokens"), int)
        and u["total_tokens"] == u.get("prompt_tokens", 0) + u.get("completion_tokens", 0),
        "usage=%s" % (u,),
    )
    ct = ""
    with c.stream(
        "POST",
        "/v1/chat/completions",
        json={"model": "ssejson", "stream": True, "messages": [{"role": "user", "content": "hi"}]},
    ) as r:
        ct = r.headers.get("content-type", "")
        sbody = "|".join(l for l in r.iter_lines() if l)
    add("流式强制 text/event-stream", ct.startswith("text/event-stream") and "[DONE]" in sbody, "ct=%s" % ct)

    # 上游首字节很慢时：必须靠保活帧占住连接，否则中间代理空闲超时会先把连接掐断，
    # 客户端只看到「请求失败」而网关日志却记 200（New-API 渠道测试失败的典型成因）。
    # 保活帧必须是合法 data: 空 chunk —— 中转网关(New-API)只转发 data: 行，
    # 注释行会被丢弃，下游那段仍会静默超时并报 client_gone。
    def _is_keepalive(line):
        if not line.startswith("data:"):
            return False
        p = line[5:].strip()
        if not p or p == "[DONE]":
            return False
        try:
            j = json.loads(p)
        except Exception:
            return False
        ch = (j.get("choices") or [{}])[0]
        if "delta" in ch:
            d = ch.get("delta") or {}
            return not (d.get("content") or d.get("reasoning_content"))
        return not ch.get("text")

    t0 = time.time()
    ping_at = data_at = None
    parts = []
    comments = 0
    with c.stream(
        "POST",
        "/v1/chat/completions",
        json={"model": "slowfirst", "stream": True, "messages": [{"role": "user", "content": "hi"}]},
    ) as r:
        code = r.status_code
        for line in r.iter_lines():
            if not line:
                continue
            if line.startswith(":"):
                comments += 1
            if _is_keepalive(line) and ping_at is None:
                ping_at = time.time() - t0
            if line.startswith("data:") and not _is_keepalive(line) and data_at is None:
                data_at = time.time() - t0
            parts.append(line)
    sbody2 = "|".join(parts)
    add(
        "慢首字节保活(可穿透中转网关)",
        code == 200
        and ping_at is not None
        and data_at is not None
        and ping_at < data_at
        and "Slow" in sbody2
        and "[DONE]" in sbody2
        and comments == 0,
        "保活帧=%.1fs 首数据=%.1fs 注释行=%d 总=%.1fs"
        % (ping_at or -1, data_at or -1, comments, time.time() - t0),
    )

    # 上游连响应头都迟迟不返回（实测 NVIDIA kimi-k3 要 128s）：网关卡在 client.send() 里
    # 发不出任何东西，必须靠兜底保活流占住连接，否则中间 nginx 60s 空闲超时会先掐断
    t0 = time.time()
    hb = []
    dd = []
    parts3 = []
    with c.stream(
        "POST",
        "/v1/chat/completions",
        json={"model": "slowhdr", "stream": True, "messages": [{"role": "user", "content": "hi"}]},
    ) as r:
        code3 = r.status_code
        for line in r.iter_lines():
            el = time.time() - t0
            if _is_keepalive(line):
                hb.append(round(el, 1))
            elif line.startswith("data:"):
                dd.append(round(el, 1))
                parts3.append(line)
    body3 = "|".join(parts3)
    add(
        "响应头延迟时保活",
        code3 == 200 and hb and dd and hb[0] < dd[0] and "Hdr" in body3 and "[DONE]" in body3,
        "心跳=%s 首数据=%s" % (hb[:3], dd[:1]),
    )

    # 等前面的 429 冷却结束；Rust 网关跑前面的用例更快，固定 sleep(2) 不足以让
    # 指数退避的冷却过期，这里轮询到所有账号冷却/封禁清零（语义与原 sleep 一致）
    for _ in range(100):
        _st_rows = a.get("/api/keys").json()["rows"]
        _now_ts = int(time.time())
        if all(
            int(k.get("cooldown_until") or 0) < _now_ts and int(k.get("banned_until") or 0) < _now_ts
            for k in _st_rows
            if k.get("enabled")
        ):
            break
        time.sleep(0.2)
    before = [k.get("total_requests", 0) for k in a.get("/api/keys").json()["rows"]]
    for _ in range(12):
        c.post(
            "/v1/chat/completions",
            json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
        )
    now = [k.get("total_requests", 0) for k in a.get("/api/keys").json()["rows"]]
    delta = sorted([n - b for n, b in zip(now, before)])
    add(
        "账号轮换分散",
        sum(delta) == 12 and delta[-1] - delta[0] <= 2,
        "增量=%s 累计=%s" % (delta, sorted(now)),
    )

    # 断连回收：客户端在「上游还没返回响应头」时放弃，网关必须检测到并立刻释放账号。
    # 注意：必须用原始 socket 真实关闭 TCP —— httpx 的 close() 只是把连接还回连接池，
    # 连接并未断开，服务端不会收到 disconnect，用它测断连会得出错误结论。
    ids = [k["id"] for k in a.get("/api/keys").json()["rows"]]
    for kid in ids[1:]:
        a.post("/api/keys/op", json={"op": "disable", "id": kid})
    a.post("/api/settings", json={"config": {"acct_concurrency": 1}})

    import socket

    def _slow_disconnect():
        CRLF = "\r\n"
        body = json.dumps(
            {"model": "slowhdr", "stream": True, "messages": [{"role": "user", "content": "hi"}]}
        ).encode()
        head = (
            "POST /v1/chat/completions HTTP/1.1"
            + CRLF
            + "Host: 127.0.0.1"
            + CRLF
            + "Authorization: Bearer "
            + toks[0]["t"]
            + CRLF
            + "Content-Type: application/json"
            + CRLF
            + "Content-Length: "
            + str(len(body))
            + CRLF
            + CRLF
        ).encode()
        sk = socket.create_connection(("127.0.0.1", 18213), timeout=10)
        try:
            sk.sendall(head + body)
            time.sleep(3.0)  # 上游 20s 才返回响应头，此刻仍在等待/保活阶段
        finally:
            sk.close()  # 真实关闭 TCP，服务端应收到 disconnect

    th = threading.Thread(target=_slow_disconnect, daemon=True)
    th.start()
    th.join(10)
    t0 = time.time()
    freed = False
    while time.time() - t0 < 20:
        r = c.post(
            "/v1/chat/completions",
            json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
        )
        if r.status_code == 200:
            freed = True
            break
        time.sleep(1.0)
    add("断连后账号立即回收", freed, "断开后 %.1fs 内账号可复用" % (time.time() - t0))
    # 断连释放必须恰好一次:等响应头期间断连的路径以前 release 后没置空 hold,
    # finally 兜底会再释放一次 → 一条真 499 + 一条假 500「兜底释放」。
    # 「账号可复用」测不出这个(提前释放也"可复用"),必须查日志。
    time.sleep(2)  # 等日志落盘
    _rows = a.get("/api/logs?n=50").json().get("rows") or []
    _since = [x for x in _rows if isinstance(x, list) and len(x) > 6 and "客户端已断开" in str(x[6])]
    _dup = [x for x in _since if "兜底释放" in str(x[6])]
    add(
        "断连释放恰好一次",
        bool(_since) and not _dup,
        "499 断连日志 %d 条,兜底释放 %d 条" % (len(_since), len(_dup)),
    )
    # 恢复
    a.post("/api/settings", json={"config": {"acct_concurrency": 0}})
    for kid in ids[1:]:
        a.post("/api/keys/op", json={"op": "enable", "id": kid})

    # 账户锁定兜底：httpx.InvalidURL 不是 httpx.HTTPError 的子类，非法 base URL
    # （例如 http://[bad，仍能通过 http(s):// 前缀校验）会穿透原有捕获，把账号永久
    # 锁在该请求上。现在 _proxy/_convert 的整个重试循环外层有 try/finally 兜底释放。
    a.post("/api/settings", json={"config": {"acct_concurrency": 1}})
    # 用会真正抛 httpx.InvalidURL 的地址（IPv6 端口写错）：InvalidURL 不是 HTTPError
    # 的子类，会穿透原有 except，只有兜底 finally 能救回账号。
    a.post("/api/upstreams", json={"id": uid, "name": "T", "base": "http://[::1:99999]/v1", "enabled": True})
    r = c.post(
        "/v1/chat/completions", json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]}
    )
    bad_st = r.status_code
    # 兜底日志必须带上真实异常，否则只有一句固定文案、无从定位
    errs = [x[6] for x in a.get("/api/logs").json()["rows"] if x[2] == "mock-model"]
    has_reason = any("InvalidURL" in (e or "") for e in errs)
    a.post(
        "/api/upstreams", json={"id": uid, "name": "T", "base": "http://127.0.0.1:18212/v1", "enabled": True}
    )
    t0 = time.time()
    ok = False
    while time.time() - t0 < 10:
        rr = c.post(
            "/v1/chat/completions",
            json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
        )
        if rr.status_code == 200:
            ok = True
            break
        time.sleep(0.5)
    add(
        "异常路径不锁定账号",
        ok and bad_st >= 400 and has_reason,
        "非法 base 时 st=%s；兜底日志含异常=%s；恢复后 %.1fs 内账号可用"
        % (bad_st, has_reason, time.time() - t0),
    )
    a.post("/api/settings", json={"config": {"acct_concurrency": 0}})

    # 上游中途断流（无 [DONE]）必须显式报错：静默截断比报错危险得多 ——
    # 下游会把半截内容当成完整回复。网关应补发 error 事件 + [DONE] 收口。
    t0 = time.time()
    lines = []
    with c.stream(
        "POST",
        "/v1/chat/completions",
        json={"model": "brokenstream", "stream": True, "messages": [{"role": "user", "content": "hi"}]},
    ) as r:
        code = r.status_code
        for line in r.iter_lines():
            if line:
                lines.append(line)
    body = "|".join(lines)
    add(
        "上游断流显式报错(不静默截断)",
        code == 200 and "上游流中断" in body and "[DONE]" in body,
        "%d 行，%.1fs" % (len(lines), time.time() - t0),
    )

    # 连接泄漏：流式失败路径（空流）曾经不关闭上游响应，连接被一直占住；反复失败会耗尽
    # 共享连接池（max_connections=100），之后所有上游请求都卡在「等连接」直到超时 ——
    # 生产日志就表现为清一色「上游返回 HTTP 0」。这里连打 120 次（超过池上限），
    # 随后普通请求必须仍然可用。
    n_fail = 0
    for _ in range(120):
        rr = c.post(
            "/v1/chat/completions",
            json={"model": "emptystream", "stream": True, "messages": [{"role": "user", "content": "hi"}]},
        )
        if rr.status_code >= 400:
            n_fail += 1
    t0 = time.time()
    rr = c.post(
        "/v1/chat/completions",
        json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
    )
    # 排队：曾经有人反馈「进了队列就不见动静」。实测排队中的请求会持续重试取号，
    # 账号/并发一释放就能拿到。这里用 total_concurrency=1 + 一个占 4 秒的请求逼出排队。
    a.post("/api/settings", json={"config": {"total_concurrency": 1, "queue_max_wait": 15}})
    _qres = {}

    def _qhold():
        cc = httpx.Client(
            base_url="http://127.0.0.1:18213", timeout=60, headers={"Authorization": "Bearer " + toks[0]["t"]}
        )
        cc.post(
            "/v1/chat/completions", json={"model": "hold4", "messages": [{"role": "user", "content": "hold"}]}
        )

    def _qwait(i):
        cc = httpx.Client(
            base_url="http://127.0.0.1:18213", timeout=60, headers={"Authorization": "Bearer " + toks[0]["t"]}
        )
        t = time.time()
        rr = cc.post(
            "/v1/chat/completions",
            json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
        )
        _qres[i] = (rr.status_code, round(time.time() - t, 1))

    th0 = threading.Thread(target=_qhold, daemon=True)
    th0.start()
    time.sleep(0.6)  # 等它确实占住并发
    ths = [threading.Thread(target=_qwait, args=(i,), daemon=True) for i in range(2)]
    for t in ths:
        t.start()
    time.sleep(0.8)
    qlen = a.get("/api/queue").json().get("length")
    for t in ths:
        t.join(30)
    add(
        "排队中的请求会持续取号",
        all(v[0] == 200 for v in _qres.values()) and qlen >= 1,
        "排队长度=%s 结果=%s" % (qlen, sorted(_qres.values())),
    )
    a.post("/api/settings", json={"config": {"total_concurrency": 0}})

    add(
        "流式失败不泄漏连接池",
        rr.status_code == 200,
        "120 次空流失败（%d）后普通请求 st=%s（%.1fs）" % (n_fail, rr.status_code, time.time() - t0),
    )

    # 可靠性按「渠道+模型」定：同一模型在一个渠道上一直失败、在另一个渠道正常时，
    # 路由必须学会跳过坏渠道，而不是每次都先撞一次 500 再重试。
    a.post(
        "/api/upstreams",
        json={
            "id": uid,
            "name": "T",
            "base": "http://127.0.0.1:18212/v1",
            "enabled": True,
            "models": "mock-model",
        },
    )
    bad = a.post(
        "/api/upstreams",
        json={"name": "BAD", "base": "http://127.0.0.1:18212/fail/v1", "enabled": True, "models": "mx"},
    ).json()["upstream"]["id"]
    good = a.post(
        "/api/upstreams",
        json={"name": "GOOD", "base": "http://127.0.0.1:18212/v1", "enabled": True, "models": "mx"},
    ).json()["upstream"]["id"]
    a.post(
        "/api/keys/import",
        json={"text": "bad@e.com,p,nvapi-tkbd123456\n" "good@e.com,p,nvapi-tkgd123456", "upstream_id": bad},
    )
    a.post("/api/keys/import", json={"text": "good2@e.com,p,nvapi-tkgd234567", "upstream_id": good})
    a.post("/api/settings", json={"config": {"max_retries": 2, "retry_min_wait_ms": 0}})

    ok_n = 0
    for _ in range(6):
        r = c.post(
            "/v1/chat/completions", json={"model": "mx", "messages": [{"role": "user", "content": "hi"}]}
        )
        if r.status_code == 200:
            ok_n += 1
    # 日志行格式：[t, ep, model, key, status, ms, err, ip, ...] → model 在 idx 2，status 在 idx 4
    fails = sum(1 for x in a.get("/api/logs").json()["rows"] if x[2] == "mx" and x[4] == 500)
    add(
        "按模型学习渠道可靠性",
        ok_n == 6 and 0 < fails <= 3,
        "6/6 成功，坏渠道只被撞 %d 次（撞过即学会跳过）" % fails,
    )

    # 清理：禁用而非删除 —— 渠道下还有账号时删除接口会拒绝，账号会残留在号池里
    a.post(
        "/api/upstreams",
        json={
            "id": bad,
            "name": "BAD",
            "base": "http://127.0.0.1:18212/fail/v1",
            "enabled": False,
            "models": "mx",
        },
    )
    a.post(
        "/api/upstreams",
        json={
            "id": good,
            "name": "GOOD",
            "base": "http://127.0.0.1:18212/v1",
            "enabled": False,
            "models": "mx",
        },
    )
    a.post(
        "/api/upstreams",
        json={"id": uid, "name": "T", "base": "http://127.0.0.1:18212/v1", "enabled": True, "models": ""},
    )

    # 错误分级契约：402 必须按硬失败处理（余额耗尽的账号重试无意义，会被反复选中反复失败），
    # 401/403 归鉴权、429 归限流、0/超时归连接、5xx 归上游故障。
    sys.path.insert(0, ROOT)
    from core import pool as _pool

    # 导入解析契约:严格模式按 CSV 规则拆(空密码列 "email,,apikey" 必须保住 —— 导出的
    # 就是这形态,老写法 re.split(r",+") 会把 ",," 折叠成一个分隔符,整行判无效,
    # 等于「导出备份再导入」丢掉密码为空的账号);引号字段(导出的转义形式)必须还原,
    # 宽松模式(上传文件)也一样。
    _csv_strict, _csv_strict_bad = _pool.parse_accounts("rt1@e.com,,nvapi-rt000001")
    _csv_q, _csv_q_bad = _pool.parse_accounts('rt2@e.com,"p,w",nvapi-rt000002')
    _csv_loose, _ = _pool.parse_accounts_loose('rt2@e.com,"p,w",nvapi-rt000002')
    _csv_ok = (
        len(_csv_strict) == 1 and _csv_strict[0]["password"] == "" and _csv_strict_bad == 0
        and len(_csv_q) == 1 and _csv_q[0]["password"] == "p,w" and _csv_q_bad == 0
        and len(_csv_loose) == 1 and _csv_loose[0]["apikey"] == "nvapi-rt000002"
        and _csv_loose[0]["email"] == "rt2@e.com"
    )
    add("导入解析:空密码列不丢 + 引号字段还原(严格/宽松)",
        _csv_ok,
        "空密码=%s 引号严格=%s 引号宽松=%s"
        % (_csv_strict[0]["password"] if _csv_strict else None,
           _csv_q[0]["password"] if _csv_q else None,
           _csv_loose[0]["password"] if _csv_loose else None))

    # 端到端:导入 → 导出 CSV → 删掉 → 用导出的 CSV 重新导入,两条账号必须原样回来
    # (老写法第二条会因为密码含逗号被引号包起来而在宽松解析里被劈成两半)
    a.post("/api/keys/import", json={"text": 'rt1@e.com,,nvapi-rt000001\nrt2@e.com,"p,w",nvapi-rt000002',
                                     "upstream_id": uid})
    _exp = a.get("/api/keys/export").text
    _exp_lines = [ln for ln in _exp.splitlines() if "nvapi-rt" in ln]
    _rt_ids = [r["id"] for r in a.get("/api/keys?page=1&size=200").json()["rows"]
               if r["email"] in ("rt1@e.com", "rt2@e.com")]
    a.post("/api/keys/batch", json={"op": "delete", "ids": _rt_ids})
    _gone = [r for r in a.get("/api/keys?page=1&size=200").json()["rows"]
             if r["email"] in ("rt1@e.com", "rt2@e.com")]
    _rg = a.post("/api/keys/import", json={"text": "\n".join(_exp_lines), "upstream_id": uid}).json()
    _back = {r["email"]: r for r in a.get("/api/keys?page=1&size=200").json()["rows"]
             if r["email"] in ("rt1@e.com", "rt2@e.com")}
    add("密钥导出→重新导入往返无损(含空密码/含逗号密码)",
        not _gone and len(_rt_ids) == 2 and len(_back) == 2 and _rg.get("added") == 2
        and _back.get("rt1@e.com", {}).get("password") == ""
        and _back.get("rt2@e.com", {}).get("password") == "p,w"
        and _back.get("rt2@e.com", {}).get("apikey") == "nvapi-rt000002",
        "删除后=%d 重新导入 added=%s 空密码=%r 逗号密码=%r"
        % (len(_gone), _rg.get("added"), _back.get("rt1@e.com", {}).get("password"),
           _back.get("rt2@e.com", {}).get("password")))

    want = {
        (402, "payment"),
        (401, "auth"),
        (403, "auth"),
        (429, "429"),
        (500, "5xx"),
        (503, "5xx"),
        (0, "conn"),
    }
    got = {st: _pool._classify(st, 0, "") for st, _ in want}
    wrong = ["%s->%s(期望%s)" % (st, got[st], c) for st, c in want if got[st] != c]
    # 连接池耗尽必须单列(全局容量问题,不惩罚账号),且不能被 "timeout" 字样误判
    if _pool._classify(0, 0, "PoolTimeout: 连接池耗尽") != "pool_exhausted":
        wrong.append("pooltimeout->%s(期望pool_exhausted)" % _pool._classify(0, 0, "PoolTimeout: 连接池耗尽"))
    if _pool._classify(0, 0, "httpx.PoolTimeout occurred") != "pool_exhausted":
        wrong.append("pooltimeout类名->%s(期望pool_exhausted)" % _pool._classify(0, 0, "httpx.PoolTimeout occurred"))
    # 连接池配置可保存/读取
    a.post("/api/settings", json={"config": {"pool_max_connections": 400}})
    if int(a.get("/api/settings").json().get("pool_max_connections") or 0) != 400:
        wrong.append("pool_max_connections 设置不生效")
    add("错误分级契约", not wrong, "；".join(wrong) if wrong else "402/401/403/429/5xx/conn/pool_exhausted 归类正确")

    # 批量解封:401 触发硬封禁 → 批量 enable 解封 → 账号必须立即可调度
    a.post("/api/settings", json={"config": {"hard_fail_ban_seconds": 600, "hard_fail_disable_count": 99}})
    r = c.post(
        "/v1/chat/completions",
        json={"model": "authfail", "messages": [{"role": "user", "content": "hi"}]},
    )
    now_i = int(time.time())
    rows_all = a.get("/api/keys").json()["rows"]
    banned_ids = [k["id"] for k in rows_all if (k.get("banned_until") or 0) > now_i]
    # 复现断点一:401 是否触发了封禁
    add("401 触发账号封禁", r.status_code == 401 and bool(banned_ids),
        "HTTP %s,封禁账号 %d 个" % (r.status_code, len(banned_ids)))
    if banned_ids:
        rb = a.post("/api/keys/batch", json={"op": "enable", "ids": banned_ids})
        rows2 = a.get("/api/keys").json()["rows"]
        still = [k["id"] for k in rows2 if (k.get("banned_until") or 0) > now_i]
        st_bad = [k["id"] for k in rows2 if k["id"] in banned_ids and k.get("status") != "active"]
        r_ok = c.post(
            "/v1/chat/completions",
            json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
        )
        add("批量解封生效(enable)", rb.status_code == 200 and not still and not st_bad and r_ok.status_code == 200,
            "batch=%s 仍封禁=%s status异常=%s 解封后请求=%s" % (rb.status_code, still, st_bad, r_ok.status_code))

    # ---- 训练资料收集 ----
    # 非流式 chat:成功请求必须收集「完整消息 + 回复」(测试先关质量过滤)
    a.post("/api/settings", json={"config": {"training_log_max": 500, "training_min_chars": 0}})
    a.post("/api/training/clear", json={})
    c.post(
        "/v1/chat/completions",
        json={
            "model": "mock-model",
            "messages": [
                {"role": "system", "content": "你是测试"},
                {"role": "user", "content": "你好训练"},
            ],
        },
    )
    tr = a.get("/api/training?n=10").json()
    tr_rows = tr.get("rows") or []
    ent = next((e for e in tr_rows if e.get("model") == "mock-model"), None)
    add(
        "训练资料:非流式 chat 收集",
        ent is not None
        and len(ent.get("messages") or []) == 2
        and ent["messages"][0]["role"] == "system"
        and ent["messages"][1]["content"] == "你好训练"
        and ent.get("response") == "ok",
        "entry=%s" % ("有" if ent else "无"),
    )

    # 流式 chat:SSE 全文提取("Hello world")
    c.post(
        "/v1/chat/completions",
        json={"model": "mock-model", "stream": True, "messages": [{"role": "user", "content": "hi"}]},
    )
    tr2 = a.get("/api/training?n=10").json()
    ent2 = next((e for e in (tr2.get("rows") or []) if e.get("response") == "Hello world"), None)
    add("训练资料:流式全文提取", ent2 is not None, "response=%r" % (ent2 or {}).get("response"))

    # /v1/messages 流式(协议转换流):text_acc 全文
    c.post(
        "/v1/messages",
        json={"model": "mock-model", "max_tokens": 32, "stream": True,
              "messages": [{"role": "user", "content": "你好"}]},
    )
    tr3 = a.get("/api/training?n=10").json()
    ent3 = next((e for e in (tr3.get("rows") or []) if (e.get("ep") or "")[:3] == "msg"), None)
    add("训练资料:Messages 流式收集", ent3 is not None and ent3.get("response") == "Hello world",
        "ep=%s resp=%r" % ((ent3 or {}).get("ep"), (ent3 or {}).get("response")))

    # 关闭收集(training_log_max=0)
    a.post("/api/settings", json={"config": {"training_log_max": 0}})
    c.post(
        "/v1/chat/completions",
        json={"model": "mock-model", "messages": [{"role": "user", "content": "不应收集"}]},
    )
    time.sleep(0.6)
    tr4 = a.get("/api/training?n=50").json()
    off_ok = all((e.get("messages") or [{}])[-1].get("content") != "不应收集" for e in (tr4.get("rows") or []))
    add("训练资料:关闭后不收集", off_ok, "total=%s" % tr4.get("total"))
    a.post("/api/settings", json={"config": {"training_log_max": 500}})

    # 导出 JSONL:每行含 assistant 尾条
    ex = a.get("/api/training/export")
    ex_lines = [ln for ln in ex.text.split("\n") if ln.strip()]
    ex_ok = False
    if ex_lines:
        j0 = json.loads(ex_lines[0])
        ex_ok = isinstance(j0.get("messages"), list) and j0["messages"][-1].get("role") == "assistant"
    add("训练资料:JSONL 导出格式", ex_ok, "%d 行,首行尾角色=%s" % (len(ex_lines), json.loads(ex_lines[0])["messages"][-1]["role"] if ex_lines else "-"))

    # 清空
    a.post("/api/training/clear", json={})
    tr5 = a.get("/api/training?n=10").json()
    add("训练资料:清空", (tr5.get("rows") or []) == [] and tr5.get("total") == 0, "total=%s" % tr5.get("total"))

    # 质量过滤:短回复/超短输入的垃圾语料(冒烟测试、模型测试页)不收
    a.post("/api/settings", json={"config": {"training_min_chars": 20}})
    c.post(
        "/v1/chat/completions",
        json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
    )  # 回复 "ok"(2 字符) → 不收
    c.post(
        "/v1/chat/completions",
        json={"model": "nousage", "messages": [{"role": "user", "content": "???"}]},
    )  # 回复 "ok",用户输入 3 字符 → 不收
    time.sleep(0.5)
    trq = a.get("/api/training?n=20").json()
    junk = [e for e in (trq.get("rows") or []) if len(str(e.get("response") or "").strip()) < 20]
    add("训练资料:短/垃圾语料被过滤", (trq.get("total") or 0) == 0 and not junk, "total=%s 垃圾=%d" % (trq.get("total"), len(junk)))
    a.post("/api/settings", json={"config": {"training_min_chars": 0}})

    # 小时限语义:hourly 是「日 token 超标后的限速阀」,不是独立小时硬限 ——
    # 健康账号本小时请求数超 hourly 也必须可调度(被拆成独立闸门时,
    # 线上出现「小时限 137/共 201」大面积误伤)。用独立账号精确验证。
    kids_all = [k["id"] for k in a.get("/api/keys").json()["rows"]]
    a.post("/api/keys/import", json={
        "text": "fresh@t.com,p,nvapi-fresh12345678", "upstream_id": uid,
    })
    kids_fresh = [k["id"] for k in a.get("/api/keys").json()["rows"] if k["email"] == "fresh@t.com"]
    a.post("/api/keys/batch", json={"op": "disable", "ids": kids_all})
    a.post("/api/settings", json={"config": {
        "hourly_request_limit": 5, "daily_token_limit": 0, "daily_request_cap": 0,
        "rate_limit_per_minute": 100000, "acct_concurrency": 0, "warmup_seconds": 0,
    }})
    h_codes = []
    for _i in range(7):  # 7 次 > hourly=5,日 token 未超 → 必须全部放行
        h_codes.append(c.post(
            "/v1/chat/completions",
            json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
        ).status_code)
    add("小时限不独立拦截健康账号", h_codes == [200] * 7,
        "7 连发=%s(hourly=5 且已超,仍全部放行)" % (sorted(set(h_codes)),))
    # 日 token 超标 + 小时达限 → 才触发限速(daily_token_limit=1 使账号立即超标)
    a.post("/api/settings", json={"config": {"daily_token_limit": 1}})
    r_th = c.post(
        "/v1/chat/completions",
        json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
    )
    add("日token超标后按小时限速", r_th.status_code == 429, "st=%s(应为限速 429)" % r_th.status_code)
    # 恢复环境:删除 fresh 账号 + 启用全部 + 清累计统计(后续测试回到干净基线)
    a.post("/api/settings", json={"config": {
        "daily_token_limit": 0, "daily_request_cap": 100, "hourly_request_limit": 5,
    }})
    for kid in kids_fresh:
        a.post("/api/keys/op", json={"op": "delete", "id": kid})
    a.post("/api/keys/batch", json={"op": "enable", "ids": kids_all})
    a.post("/api/keys/batch", json={"op": "unban", "ids": kids_all})
    a.post("/api/keys/batch", json={"op": "reset", "ids": kids_all})

    # tool_choice 兼容:客户端/转换器带 tool_choice 而 tools 为空时,严格上游会 400
    # (线上实测:"When using `tool_choice`, `tools` must be set.")。网关须转发前清理。
    r_tc = c.post(
        "/v1/chat/completions",
        json={"model": "mock-model", "tool_choice": "auto",
              "messages": [{"role": "user", "content": "hi"}]},
    )
    add("tool_choice 无 tools 时被清理(chat)", r_tc.status_code == 200, "st=%s" % r_tc.status_code)
    r_tc2 = c.post(
        "/v1/messages",
        json={"model": "mock-model", "max_tokens": 32, "tool_choice": {"type": "auto"},
              "messages": [{"role": "user", "content": "hi"}]},
    )
    add("tool_choice 无 tools 时被清理(Anthropic)", r_tc2.status_code == 200, "st=%s" % r_tc2.status_code)
    r_tc3 = c.post(
        "/v1/responses",
        json={"model": "mock-model", "tool_choice": "auto", "input": "hi"},
    )
    add("tool_choice 无 tools 时被清理(Responses)", r_tc3.status_code == 200, "st=%s" % r_tc3.status_code)
    # Responses 的 usage 字段必须齐全:openai SDK 的 pydantic 模型把
    # input_tokens_details 的 cached_tokens 与 cache_write_tokens 都当必填,少一个整个
    # 响应就 ValidationError(openai 3.23 实测)。流式 response.completed 里那一份同样要齐
    # (以前 convert.py 与 streams.py 各写一份,字段一漂移就两边不一致)。
    _ru = (r_tc3.json().get("usage") or {}) if r_tc3.status_code == 200 else {}
    _rui = _ru.get("input_tokens_details") or {}
    _ruo = _ru.get("output_tokens_details") or {}
    with c.stream("POST", "/v1/responses",
                  json={"model": "mock-model", "input": "hi", "stream": True}) as _rs:
        _raw_rs = _rs.read().decode("utf-8", "replace")
    _rs_done = next((ln for ln in _raw_rs.splitlines()
                     if ln.startswith("data:") and "response.completed" in ln), "")
    add("Responses usage 字段齐全(SDK 必填 cache_write_tokens)",
        bool(_rui) and "cached_tokens" in _rui and "cache_write_tokens" in _rui
        and "reasoning_tokens" in _ruo and '"cache_write_tokens"' in _rs_done,
        "非流式 usage=%s;流式 completed 含 cache_write_tokens=%s"
        % (json.dumps(_ru, ensure_ascii=False)[:88], '"cache_write_tokens"' in _rs_done))
    # 有 tools 时 tool_choice 必须保留(不能误删)
    from core import convert as _cv
    _keep = _cv.sanitize_request(
        {"tool_choice": "auto", "tools": [{"type": "function", "function": {"name": "f", "parameters": {}}}]}
    )
    _drop = _cv.sanitize_request({"tool_choice": "auto"})
    _drop2 = _cv.sanitize_request({"tool_choice": {"type": "function", "function": {"name": "x"}}, "tools": []})
    add(
        "tool_choice 清理不误伤",
        _keep.get("tool_choice") == "auto" and "tool_choice" not in _drop and "tool_choice" not in _drop2,
        "keep=%s drop=%s drop2=%s" % ("tool_choice" in _keep, "tool_choice" in _drop, "tool_choice" in _drop2),
    )

    # 访问令牌管理(CRUD):从设置迁移出的独立板块
    r_tok = a.post("/api/tokens", json={})
    new_tok = r_tok.json().get("token") or ""
    add("令牌:自动生成并添加", r_tok.status_code == 200 and new_tok.startswith("sk-gw-"), new_tok[:18])
    r_dup = a.post("/api/tokens", json={"t": new_tok})
    add("令牌:重复添加被拒", r_dup.status_code == 400, "st=%s" % r_dup.status_code)
    r_lim_full = c.get("/v1/models").json()
    full_ids = [x["id"] for x in r_lim_full.get("data") or []]
    add("令牌:清单可读", bool(full_ids), "当前 %d 个模型" % len(full_ids))
    target_model = full_ids[0] if full_ids else "mock-model"
    r_lim = a.post("/api/tokens", json={"t": "sk-gw-testlimit01", "m": target_model})
    with httpx.Client(base_url="http://127.0.0.1:18213", timeout=30,
                      headers={"Authorization": "Bearer sk-gw-testlimit01"}) as c_lim:
        r_models = c_lim.get("/v1/models").json()
    add("令牌:模型限制生效",
        r_lim.status_code == 200
        and [x["id"] for x in r_models.get("data") or []] == [target_model],
        "限制 %r 后可见=%s" % (target_model, [x["id"] for x in r_models.get("data") or []]))
    r_upd = a.post("/api/tokens/update", json={"t": "sk-gw-testlimit01", "m": ""})
    with httpx.Client(base_url="http://127.0.0.1:18213", timeout=30,
                      headers={"Authorization": "Bearer sk-gw-testlimit01"}) as c_lim2:
        r_models2 = c_lim2.get("/v1/models").json()
    add("令牌:更新为全部模型",
        r_upd.status_code == 200 and len(r_models2.get("data") or []) > 1,
        "清空限制后可见 %d 个" % len(r_models2.get("data") or []))
    a.post("/api/tokens/delete", json={"t": "sk-gw-testlimit01"})
    a.post("/api/tokens/delete", json={"t": new_tok})
    with httpx.Client(base_url="http://127.0.0.1:18213", timeout=30,
                      headers={"Authorization": "Bearer sk-gw-testlimit01"}) as c_lim3:
        r_gone = c_lim3.get("/v1/models")
    rows_t2 = a.get("/api/tokens").json().get("rows") or []
    add("令牌:删除后立即失效",
        r_gone.status_code == 401
        and all(x["t"] != new_tok for x in rows_t2)
        and all(x["t"] != "sk-gw-testlimit01" for x in rows_t2),
        "删除后请求 st=%s" % r_gone.status_code)

    # 非法 max_tokens(客户端算出超大负数):网关转发前清理,不再被严格上游 400
    r_neg = c.post(
        "/v1/chat/completions",
        json={"model": "mock-model", "max_tokens": -134237,
              "messages": [{"role": "user", "content": "hi"}]},
    )
    add("非法 max_tokens 被清理", r_neg.status_code == 200, "st=%s" % r_neg.status_code)

    # 安全守护:管理端点认证全覆盖(AST 静态检查,Python 3.8 兼容)
    import ast as _ast

    def _has_require(fn):
        for stmt in fn.body[:3]:
            for n in _ast.walk(stmt):
                if isinstance(n, _ast.Call) and isinstance(n.func, _ast.Name) and n.func.id == "_require":
                    return True
        return False

    _missing = []
    _tree = _ast.parse(open(os.path.join(ROOT, "admin_api.py"), encoding="utf-8").read())
    for _n in _tree.body:
        if isinstance(_n, (_ast.FunctionDef, _ast.AsyncFunctionDef)) and _n.name not in ("login", "logout", "remote_update", "remote_rollback"):
            _decs = [d.func.attr for d in _n.decorator_list
                     if isinstance(d, _ast.Call) and isinstance(d.func, _ast.Attribute)]
            if any(x in ("get", "post") for x in _decs) and not _has_require(_n):
                _missing.append(_n.name)
    add("安全:管理端点认证全覆盖", not _missing, "缺失: %s" % (_missing or "无"))
    # remote_update/rollback 单独验证:必须含 Bearer token 鉴权或 _require 双路径
    _rtree = _ast.parse(open(os.path.join(ROOT, "admin_api.py"), encoding="utf-8").read())
    _rt_ok = True
    for _n in _rtree.body:
        if isinstance(_n, _ast.AsyncFunctionDef) and _n.name in ("remote_update", "remote_rollback"):
            _src = _ast.unparse(_n)
            if not ("compare_digest" in _src and "_require" in _src):
                _rt_ok = False
    add("安全:update/rollback 双路径鉴权", _rt_ok,
        "Bearer compare_digest + _require 都在=%s" % _rt_ok)

    # 测试页跳过标记:带 X-NGW-Skip-Training 的请求(后台模型测试页)不进训练集
    a.post("/api/settings", json={"config": {"training_min_chars": 0}})
    a.post("/api/training/clear", json={})
    c.headers["X-NGW-Skip-Training"] = "1"
    c.post(
        "/v1/chat/completions",
        json={"model": "mock-model", "messages": [{"role": "user", "content": "这是模型测试页的对话不应收集"}]},
    )
    del c.headers["X-NGW-Skip-Training"]
    time.sleep(0.5)
    tr_skip = a.get("/api/training?n=20").json()
    add("训练资料:测试页标记跳过收集", (tr_skip.get("total") or 0) == 0, "total=%s" % tr_skip.get("total"))
    a.post("/api/settings", json={"config": {"training_min_chars": 20}})

    # 管理端点健壮性:非 dict JSON body 必须 400 而非 500
    r_bad1 = a.post("/api/tokens", content=b"[1,2]", headers={"content-type": "application/json", "x-csrf": a.headers["X-CSRF"]})
    r_bad2 = a.post("/api/keys/op", content=b'"str"', headers={"content-type": "application/json", "x-csrf": a.headers["X-CSRF"]})
    r_bad3 = a.post("/api/keys/batch", content=b"notjson", headers={"content-type": "application/json", "x-csrf": a.headers["X-CSRF"]})
    add("管理端点:非法请求体返回 400",
        r_bad1.status_code == 400 and r_bad2.status_code == 400 and r_bad3.status_code == 400,
        "tokens=%s keysop=%s batch=%s" % (r_bad1.status_code, r_bad2.status_code, r_bad3.status_code))

    # 退化思考清理:推理栈故障时思考退化成成片 '!'(线上实测)。
    # 三条路径都必须清理;合法分隔线(----------)不能被误伤。
    r_ns = c.post(
        "/v1/chat/completions",
        json={"model": "thinkjunk", "messages": [{"role": "user", "content": "hi"}]},
    )
    j_ns = r_ns.json()
    _rs_ns = ((j_ns.get("choices") or [{}])[0].get("message") or {}).get("reasoning_content")
    add("退化思考:非流式清理", r_ns.status_code == 200 and _rs_ns == "" and
        ((j_ns.get("choices") or [{}])[0].get("message") or {}).get("content") == "答案",
        "reasoning=%r content=%r" % (_rs_ns, ((j_ns.get("choices") or [{}])[0].get("message") or {}).get("content")))

    raw_st = []
    with c.stream(
        "POST", "/v1/chat/completions",
        json={"model": "thinkjunk", "stream": True, "messages": [{"role": "user", "content": "hi"}]},
    ) as r_st:
        st_st = r_st.status_code
        raw_st = r_st.read().decode("utf-8", "replace")
    add("退化思考:流式清理",
        st_st == 200 and "!!!!!!!!!!!!!!!!" not in raw_st and "让我思考一下" in raw_st
        and "----------" in raw_st and "答案" in raw_st,
        "16连感叹号已清=%s 合法思考保留=%s 分隔线保留=%s" % (
            "!!!!!!!!!!!!!!!!" not in raw_st, "让我思考一下" in raw_st, "----------" in raw_st))

    raw_ms = []
    with c.stream(
        "POST", "/v1/messages",
        json={"model": "thinkjunk", "max_tokens": 64, "stream": True,
              "messages": [{"role": "user", "content": "hi"}]},
    ) as r_ms:
        st_ms = r_ms.status_code
        raw_ms = r_ms.read().decode("utf-8", "replace")
    add("退化思考:Messages 流式清理",
        st_ms == 200 and "!!!!!!!!" not in raw_ms and "让我思考一下" in raw_ms
        and '"signature": ""' in raw_ms,
        "思考退化已清=%s 合法思考保留=%s signature 在位=%s" % (
            "!!!!!!!!" not in raw_ms, "让我思考一下" in raw_ms, '"signature": ""' in raw_ms))

    # thinking 块必须带 signature:Anthropic SDK 把 signature 当必填字段,缺了它
    # 整个响应直接 ValidationError(用 anthropic 1.11 实测:非流式 Message 与流式
    # content_block_start 两处都会挂)。上游只要返回 reasoning_content 就暴露。
    r_thns = c.post(
        "/v1/messages",
        json={"model": "thinker", "max_tokens": 64, "messages": [{"role": "user", "content": "hi"}]},
    )
    j_thns = r_thns.json() if r_thns.status_code == 200 else {}
    _th_blocks = [b for b in (j_thns.get("content") or [])
                  if isinstance(b, dict) and b.get("type") == "thinking"]
    add("Messages 非流式:thinking 块带 signature(SDK 必填)",
        r_thns.status_code == 200 and bool(_th_blocks) and all("signature" in b for b in _th_blocks),
        "st=%s thinking=%s" % (r_thns.status_code, json.dumps(_th_blocks, ensure_ascii=False)[:80]))

    # 管理端点的鉴权面:全部端点静态上都调了 _require(单独扫过一遍),这里用真实请求
    # 再确认 —— 密钥列表/导出、配置导出、设置、拦截、日志都是敏感面,漏一个就是凭据泄漏。
    _anon = httpx.Client(base_url="http://127.0.0.1:18213", timeout=30)
    _guard = []
    for _gp in ("/api/keys", "/api/keys/export", "/api/config/export", "/api/settings",
                "/api/intercept", "/api/logs", "/api/tokens", "/api/overview", "/api/poolmap"):
        _gr = _anon.get(_gp)
        if _gr.status_code != 401:
            _guard.append("%s→%s" % (_gp, _gr.status_code))
    _r_anon_post = _anon.post("/api/intercept/toggle", json={"enabled": True})
    # 有会话、但既没有 X-CSRF 头也没有 CSRF cookie 的 POST 必须 403。
    # (cookie 兜底是给表单提交用的;跨站伪造 POST 靠 SameSite=Lax 使 cookie 根本不参与,
    #  两条路合起来才是完整的 CSRF 防护 —— 下面单独验 cookie 标志。)
    _sess_only = {"ngw_session": a.cookies.get("ngw_session")}
    _r_csrf = httpx.post("http://127.0.0.1:18213/api/intercept/toggle", json={"enabled": True},
                         cookies=_sess_only)
    add("管理端点鉴权:未登录 401 / 无 CSRF 的 POST 403",
        not _guard and _r_anon_post.status_code == 401 and _r_csrf.status_code == 403,
        "漏网=%s;未登录 POST→%s;仅有会话无 CSRF→%s"
        % (_guard or "无", _r_anon_post.status_code, _r_csrf.status_code))
    _fresh = httpx.Client(base_url="http://127.0.0.1:18213", timeout=30)
    _lr = _fresh.post("/api/login", json={"username": ADMIN_USER, "password": ADMIN_PW})
    _setc = " | ".join(_lr.headers.get_list("set-cookie")).lower()
    add("会话 cookie 标志:HttpOnly + SameSite=Lax(跨站 POST 不带 cookie)",
        _lr.status_code == 200 and "httponly" in _setc and "samesite=lax" in _setc,
        "st=%s set-cookie=%s" % (_lr.status_code, _setc[:110]))

    # 存储层三条「静默丢数据」路径(独立数据目录 + 临时改模块路径,跑完立刻还原,不碰在跑的实例):
    # ① db.json 损坏必须留 .corrupt-* 备份并告警,而不是悄悄重置成默认值;
    # ② 落盘失败(磁盘满/文件被占用/权限)必须保持 dirty 以便重试并告警;
    # ③ 落盘成功后 memo 指纹必须与文件一致 —— 否则并发 update 会以为 memo 过期而重读磁盘,
    #    把内存里已改未落盘的变更丢掉。
    import contextlib as _cl
    import io as _io2
    import core.store as _st
    _sd_base = os.path.join(TMP, "storeprobe")
    shutil.rmtree(_sd_base, ignore_errors=True)
    _old_dir, _old_path = _st.DATA_DIR, _st.DB_PATH
    _st_res = {}
    try:
        _d1 = os.path.join(_sd_base, "corrupt")
        os.makedirs(_d1, exist_ok=True)
        _st.DATA_DIR, _st.DB_PATH = _d1, os.path.join(_d1, "db.json")
        with open(_st.DB_PATH, "w", encoding="utf-8") as _f:
            _f.write('{"keys": [{"id": "k1"')
        _cap1 = _io2.StringIO()
        with _cl.redirect_stderr(_cap1), _cl.redirect_stdout(_io2.StringIO()):
            _s1 = _st.Store()
            _db1 = _s1.load()
        _st_res["corrupt"] = (
            bool([x for x in os.listdir(_d1) if ".corrupt-" in x]),
            isinstance(_db1.get("config"), dict) and bool((_db1.get("config") or {}).get("session_secret")),
            "解析失败" in _cap1.getvalue(),
        )
        _d2 = os.path.join(_sd_base, "flush")
        os.makedirs(_d2, exist_ok=True)
        _st.DATA_DIR, _st.DB_PATH = _d2, os.path.join(_d2, "db.json")
        _cap2 = _io2.StringIO()
        with _cl.redirect_stdout(_io2.StringIO()):
            _s2 = _st.Store()
            _s2.load()
            _s2.update(lambda db: db.setdefault("keys", []).append({"id": "k_probe"}))
            _real_path = _st.DB_PATH
            _st.DB_PATH = os.path.join(_d2, "nope", "db.json")
            with _cl.redirect_stderr(_cap2):
                _s2.flush()
            _dirty_after = _s2._dirty
            _st.DB_PATH = _real_path
            _s2.flush()
        _written = json.loads(open(_real_path, encoding="utf-8").read())
        _stat2 = os.stat(_real_path)
        _st_res["flush"] = (
            bool(_dirty_after),
            any(k.get("id") == "k_probe" for k in _written.get("keys", [])),
            "落盘失败" in _cap2.getvalue(),
            tuple(_s2._memo[0:2]) == (int(_stat2.st_mtime), _stat2.st_size),
        )
    finally:
        _st.DATA_DIR, _st.DB_PATH = _old_dir, _old_path
    add("存储:损坏留备份+告警 / 落盘失败保持 dirty 并重试 / memo 指纹一致",        _st_res.get("corrupt") == (True, True, True)
        and _st_res.get("flush") == (True, True, True, True)
        and (_st.DATA_DIR, _st.DB_PATH) == (_old_dir, _old_path),
        "损坏=%s 落盘=%s 路径已还原=%s"
        % (_st_res.get("corrupt"), _st_res.get("flush"),
           (_st.DATA_DIR, _st.DB_PATH) == (_old_dir, _old_path)))

    # 渠道保存:① Base URL 粘成完整端点必须当场拒掉 —— 网关自己会拼 /chat/completions,
    # 于是请求打到 .../chat/completions/chat/completions,该渠道全量 404(客户端只看到
    # 「渠道不支持该请求或模型」);② save() 是整行替换,渠道行上的表外元数据必须带过来,
    # 只有「保存为启用」时才清自动停用标记,否则每次保存都会静默抹掉这些字段。
    import core.upstreams as _up

    class _FakeStore:
        def __init__(self, db):
            self.db = db

        def load(self):
            return self.db

        def update(self, fn):
            fn(self.db)
            return self.db

        def flush(self):
            pass

    _up_bad_base = _up.validate_save({"name": "X", "base": "http://h/v1/chat/completions"})[1]
    _up_good_base = _up.validate_save({"name": "X", "base": "https://integrate.api.nvidia.com/v1/"})[0]
    _fdb2 = {"upstreams": [], "config": {}}
    _fake2 = _FakeStore(_fdb2)
    # save() 内部是 `from .store import STORE`(调用时读模块属性),所以 core.store.STORE
    # 与 upstreams.STORE 两处都要临时换掉,才真正隔离到假 store(不碰实例的 db.json)
    _real_store2, _real_store2b = _st.STORE, _up.STORE
    try:
        _st.STORE = _fake2
        _up.STORE = _fake2
        _row2, _ = _up.save({"name": "X", "base": "http://h/v1", "enabled": False})
        _uid2 = _row2["id"]
        _fdb2["upstreams"][0]["auto_disabled_at"] = 123
        _fdb2["upstreams"][0]["auto_reason"] = "连败自动停用"
        _fdb2["upstreams"][0]["future_field"] = "keep"
        _up.save({"id": _uid2, "name": "X", "base": "http://h/v1", "enabled": False})
        _kept_dis = dict(_fdb2["upstreams"][0])
        _up.save({"id": _uid2, "name": "X", "base": "http://h/v1", "enabled": True})
        _kept_en = dict(_fdb2["upstreams"][0])
    finally:
        _st.STORE, _up.STORE = _real_store2, _real_store2b
    add("渠道保存:Base URL 带端点被拒 / 表外元数据不被整行替换抹掉",
        bool(_up_bad_base) and "/v1" in _up_bad_base and _up_good_base is not None
        and _kept_dis.get("future_field") == "keep" and _kept_dis.get("auto_reason") == "连败自动停用"
        and "auto_reason" not in _kept_en and "auto_disabled_at" not in _kept_en
        and _kept_en.get("future_field") == "keep" and _kept_en.get("id") == _uid2,
        "带端点=%r 正常前缀=%s 停用时保留=%s 启用后清除=%s"
        % (_up_bad_base, _up_good_base is not None,
           (_kept_dis.get("auto_reason"), _kept_dis.get("future_field")),
           "auto_reason" not in _kept_en))

    # 渠道级「失败重试最小等待」以前只在运行时被读、保存路径漏字段 → 渠道表单填了永远存不下来,
    # 静默回落到全局。两半一起验:①经真实 API 保存能落库;②_backoff_ms 真按渠道值抬高等待
    # (用假 store 直测,避免跨进程读文件的时间差)。
    _rmw = a.post("/api/upstreams", json={"name": "RMW", "base": "http://127.0.0.1:18212/v1",
                                          "enabled": True, "retry_min_wait_ms": 1500}).json()
    _rmw_id = ((_rmw.get("upstream") or {}).get("id") or "")
    _rmw_row = next((r for r in a.get("/api/upstreams").json()["rows"] if r["id"] == _rmw_id), {})
    _rmw_saved = int(_rmw_row.get("retry_min_wait_ms") or 0)
    import server as _srv2
    _cfg3 = {"retry_backoff_base_ms": 500, "retry_backoff_max_ms": 8000, "retry_min_wait_ms": 0}
    _fdb3 = {"upstreams": [{"id": "u_rmw", "retry_min_wait_ms": 1500},
                           {"id": "u_plain", "retry_min_wait_ms": 0}], "config": {}}
    _real3 = _up.STORE
    try:
        _up.STORE = _FakeStore(_fdb3)
        _wait_with = _srv2._backoff_ms(_cfg3, 1, {"upstream_id": "u_rmw"})
        _wait_without = _srv2._backoff_ms(_cfg3, 1, {"upstream_id": "u_plain"})
    finally:
        _up.STORE = _real3
    add("渠道级「重试最小等待」能保存且运行时生效",
        _rmw_saved == 1500 and _wait_with >= 1500 and _wait_without < 1500,
        "落库=%s 有该渠道=%dms 无该渠道=%dms" % (_rmw_saved, _wait_with, _wait_without))
    a.post("/api/upstreams/delete", json={"id": _rmw_id})

    # 令牌追踪:日志记录调用令牌(遮罩)、令牌页显示最后调用 IP/时间、公开队列不泄漏
    r_trk = c.post(
        "/v1/chat/completions",
        json={"model": "mock-model", "messages": [{"role": "user", "content": "追踪我"}]},
    )
    time.sleep(0.8)  # 等异步 release 落库
    trk_rows = a.get("/api/logs?n=10").json().get("rows") or []
    trk_row = next((x for x in trk_rows if x[2] == "mock-model"), None)
    _tok = toks[0]["t"]
    _tok_mask = _tok[:10] + "…" + _tok[-4:] if len(_tok) > 14 else _tok
    add("令牌:日志记录调用令牌",
        trk_row is not None and len(trk_row) > 14 and trk_row[14] == _tok_mask,
        "row[14]=%r 期望=%r" % ((trk_row or [None] * 15)[14], _tok_mask))
    tok_rows = a.get("/api/tokens").json().get("rows") or []
    tok_ent = next((x for x in tok_rows if x["t"] == _tok), None)
    add("令牌:最后调用 IP/时间被追踪",
        tok_ent is not None and tok_ent.get("last_at", 0) > 0 and tok_ent.get("last_ip"),
        "last_at=%s last_ip=%r" % ((tok_ent or {}).get("last_at"), (tok_ent or {}).get("last_ip")))
    pub = httpx.get("http://127.0.0.1:18213/api/queue/public", timeout=10).json()
    add("令牌:公开队列不泄漏",
        all("tok" not in row for row in (pub.get("rows") or [])),
        "public rows=%d,含 tok 字段的=%d" % (len(pub.get("rows") or []), sum(1 for x in pub.get("rows") or [] if "tok" in x)))

    # 排队条目携带令牌:把账号冷却设长,第一个请求用掉唯一的账号后,第二个请求
    # 在冷却期内必然排队(不再依赖 hold4 那种 4 秒保持的时序,那种判定会随
    # 前面的用例改变账号集合而偶发失效)
    a.post("/api/settings", json={"config": {"acct_concurrency": 1, "account_cooldown_ms": 8000}})
    _all_rows = a.get("/api/keys?page=1").json()["rows"]
    ids4 = [k["id"] for k in _all_rows]
    # 只留主渠道(T,且启用)的**一个**账号:其余全停用 —— 该账号一进冷却,后面
    # 的请求就必须排队(旧写法停用 ids4[1:],剩下的账号可能属于被停用的渠道,
    # 现在会被正确地立刻 404,测不到排队)
    _main_ids = [k["id"] for k in _all_rows if k.get("upstream_id") == uid]
    _keep = _main_ids[:1]
    a.post("/api/keys/batch", json={"op": "enable", "ids": _keep})
    a.post("/api/keys/batch", json={"op": "disable",
                                    "ids": [k["id"] for k in _all_rows if k["id"] not in _keep]})
    _hold = {}
    try:
        _hold["code"] = c.post(
            "/v1/chat/completions", json={"model": "mock-model", "messages": [{"role": "user", "content": "h"}]}
        ).status_code
    except Exception as e:
        _hold["code"] = type(e).__name__
    time.sleep(0.3)  # 唯一账号已用掉,进入 8 秒冷却
    q_status = {}

    def _q_req():
        try:
            q_status["code"] = c.post(
                "/v1/chat/completions", json={"model": "mock-model", "messages": [{"role": "user", "content": "q"}]}
            ).status_code
        except Exception:
            q_status["code"] = 0

    th2 = threading.Thread(target=_q_req, daemon=True)
    th2.start()
    time.sleep(1.2)  # 第二个请求此刻应在队列中
    q_rows = a.get("/api/queue").json().get("rows") or []
    q_tok_ok = any((x.get("tok") or "") == _tok_mask for x in q_rows)
    th2.join(20)
    add("令牌:排队条目携带令牌", q_tok_ok,
        "排队可见令牌=%s(排队中 %d 条;首次请求=%s 排队请求=%s)"
        % (q_tok_ok, len(q_rows), _hold.get("code"), q_status.get("code")))
    a.post("/api/settings", json={"config": {"acct_concurrency": 0, "account_cooldown_ms": -1}})
    a.post("/api/keys/batch", json={"op": "enable", "ids": ids4})

    # in-flight 泄漏守护:重试 continue 路径曾泄漏账号并发计数(线上号池
    # "账户并发 201/共 202"全满、排队 300s 超时的根因)。
    # 检测器:并发 1 + 两个专用账号 —— 泄漏存在时同一账号的 in-flight 永不归零,
    # 后续请求必然排队超时
    a.post("/api/settings", json={"config": {
        "acct_concurrency": 1, "queue_max_wait": 3, "queue_poll_ms": 200,
        "cool_429_seconds": 1, "retry_backoff_base_ms": 100, "retry_backoff_max_ms": 200,
        "max_retries": 5, "warmup_seconds": 0,
        "daily_request_cap": 0, "daily_token_limit": 0, "rate_limit_per_minute": 100000,
    }})
    kids5 = [k["id"] for k in a.get("/api/keys").json()["rows"]]
    a.post("/api/keys/batch", json={"op": "unban", "ids": kids5})  # 清前面测试累计的封禁
    a.post("/api/keys/import", json={
        "text": "leak1@t.com,p,nvapi-leak00000001\nleak2@t.com,p,nvapi-leak00000002",
        "upstream_id": uid,
    })
    a.post("/api/keys/batch", json={"op": "disable", "ids": kids5})  # 只留两个泄漏专用账号
    # 场景1:429 吸收换号重试(mock rl 前两次 429,修复后应换号+真实冷却后重试成功)
    r51 = c.post(
        "/v1/chat/completions",
        json={"model": "rl", "messages": [{"role": "user", "content": "hi"}]},
        timeout=60,
    )
    # 泄漏检测:吸收路径若泄漏 in-flight,两个专用账号都会被幽灵占满 → 必然排队超时
    r52 = c.post(
        "/v1/chat/completions",
        json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
        timeout=60,
    )
    add("in-flight 泄漏:429 吸收路径", r51.status_code == 200 and r52.status_code == 200,
        "429吸收重试=%s 泄漏检测第二请求=%s" % (r51.status_code, r52.status_code))
    # 场景2:思考降级同号重试(kimi + effort=medium 触发上游 400 → reuse 同号降级重试成功)
    r53 = c.post(
        "/v1/chat/completions",
        json={"model": "kimi", "thinking_effort": "medium",
              "messages": [{"role": "user", "content": "hi"}]},
        timeout=60,
    )
    # 泄漏检测:降级路径若泄漏(修复前 continue 重新取号,旧号被 hold 覆盖),此处必然排队超时
    r54 = c.post(
        "/v1/chat/completions",
        json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
        timeout=60,
    )
    add("in-flight 泄漏:思考降级路径", r53.status_code == 200 and r54.status_code == 200,
        "降级重试=%s 泄漏检测第二请求=%s" % (r53.status_code, r54.status_code))
    # 清理专用账号
    leak_ids = [k["id"] for k in a.get("/api/keys").json()["rows"]
                if (k.get("email") or "").startswith("leak")]
    for kid in leak_ids:
        a.post("/api/keys/op", json={"op": "delete", "id": kid})
    # 恢复
    a.post("/api/settings", json={"config": {
        "acct_concurrency": 0, "queue_max_wait": 8, "queue_poll_ms": 150,
        "max_retries": 2, "retry_backoff_base_ms": 10, "retry_backoff_max_ms": 20,
        "cool_429_seconds": 1, "daily_request_cap": 100, "daily_token_limit": 0,
    }})
    a.post("/api/keys/batch", json={"op": "enable", "ids": kids5})
    a.post("/api/keys/batch", json={"op": "unban", "ids": kids5})

    # 漏 await 守护:_stream_convert 是 async 函数,两处调用曾漏 await(线上 500:
    # 'coroutine' object has no attribute 'body_iterator')。触发条件:协议转换端点
    # + 流式 + 慢头/慢首字节,测试 mock 上游此前从不覆盖这两条路径。
    # 路径1:响应头超 12s → _slow_convert_response → 交接 _stream_convert(线上报错点)
    t_slow = time.time()
    ev_sc = []
    with c.stream("POST", "/v1/messages",
                  json={"model": "slowhdr", "max_tokens": 64, "stream": True,
                        "messages": [{"role": "user", "content": "hi"}]}) as r_sc:
        st_sc = r_sc.status_code
        raw_sc = r_sc.read().decode("utf-8", "replace")
    for line in raw_sc.split("\n"):
        if line.startswith("event: "):
            ev_sc.append(line[7:].strip())
    add("转换流:慢头兜底路径不 500",
        st_sc == 200 and "message_start" in ev_sc and "message_stop" in ev_sc,
        "st=%s 事件=%s 耗时=%.0fs(修复前 coroutine 500)" % (st_sc, sorted(set(ev_sc))[:4], time.time() - t_slow))
    # 路径2:首字节超 8s → 心跳透传分支(return await _stream_convert)
    t_sf = time.time()
    ev_sf = []
    with c.stream("POST", "/v1/messages",
                  json={"model": "slowfirst", "max_tokens": 64, "stream": True,
                        "messages": [{"role": "user", "content": "hi"}]}) as r_sf:
        st_sf = r_sf.status_code
        raw_sf = r_sf.read().decode("utf-8", "replace")
    for line in raw_sf.split("\n"):
        if line.startswith("event: "):
            ev_sf.append(line[7:].strip())
    add("转换流:慢首字节心跳路径不 500",
        st_sf == 200 and "message_start" in ev_sf and "message_stop" in ev_sf,
        "st=%s 事件=%s 耗时=%.0fs" % (st_sf, sorted(set(ev_sf))[:4], time.time() - t_sf))

    # 号池热力图端点:分组/统计/状态判定
    pm = a.get("/api/poolmap").json()
    pm_groups = pm.get("groups") or []
    pm_total = sum(g["total"] for g in pm_groups)
    kids_pm = a.get("/api/keys?status=all&page=1").json()
    pm_keycount = kids_pm.get("total") or 0
    pm_consistent = all(g["total"] == g["ok"] + g["busy"] + g["bad"] for g in pm_groups)
    add("热力图:结构一致", bool(pm_groups) and pm_total == pm_keycount and pm_consistent,
        "组=%d 总数=%d/%d 汇总一致=%s" % (len(pm_groups), pm_total, pm_keycount, pm_consistent))
    # 停用一个账号 → 热力图应显示红色(s=2)
    kid_off = kids_pm["rows"][0]["id"]
    a.post("/api/keys/op", json={"op": "disable", "id": kid_off})
    pm2 = a.get("/api/poolmap").json()
    cell_off = None
    for g in pm2.get("groups") or []:
        cell_off = next((x for x in g["cells"] if x["id"] == kid_off), cell_off)
    a.post("/api/keys/op", json={"op": "enable", "id": kid_off})
    add("热力图:停用账号标红", cell_off is not None and cell_off["s"] == 2,
        "s=%s 原因=%r" % ((cell_off or {}).get("s"), (cell_off or {}).get("w")))

    # duplicate field:多轮历史带思考字段被中转二次加工 → 严格上游 400。
    # 网关应清洗历史消息中的 reasoning_content 后同号重试成功
    _hist = [
        {"role": "user", "content": "第一轮"},
        {"role": "assistant", "content": "第一轮回答", "reasoning_content": "第一轮思考"},
        {"role": "user", "content": "第二轮"},
    ]
    r_dup = c.post(
        "/v1/chat/completions",
        json={"model": "dupfield", "messages": _hist},
        timeout=30,
    )
    add("duplicate field:历史思考清洗后重试", r_dup.status_code == 200,
        "st=%s(修复前 400 duplicate)" % r_dup.status_code)
    with c.stream(
        "POST", "/v1/chat/completions",
        json={"model": "dupfield", "stream": True, "messages": _hist},
    ) as r_dups:
        st_dups = r_dups.status_code
        raw_dups = r_dups.read().decode("utf-8", "replace")
    add("duplicate field:流式历史思考清洗",
        st_dups == 200 and "duplicate" not in raw_dups and "ok" in raw_dups,
        "st=%s 含错误=%s" % (st_dups, "duplicate" in raw_dups))

    # enable_thinking(顶层思考开关):严格上游拒绝 → 降级清单已含,清洗后重试成功
    r_et = c.post(
        "/v1/chat/completions",
        json={"model": "enablethink", "enable_thinking": True,
              "messages": [{"role": "user", "content": "hi"}]},
        timeout=30,
    )
    add("enable_thinking:顶层降级(非流式)", r_et.status_code == 200,
        "st=%s(修复前 400 Unsupported)" % r_et.status_code)
    with c.stream(
        "POST", "/v1/chat/completions",
        json={"model": "enablethink", "enable_thinking": True, "stream": True,
              "messages": [{"role": "user", "content": "hi"}]},
    ) as r_ets:
        st_ets = r_ets.status_code
        raw_ets = r_ets.read().decode("utf-8", "replace")
    add("enable_thinking:顶层降级(流式)",
        st_ets == 200 and "Unsupported" not in raw_ets,
        "st=%s 含错误=%s" % (st_ets, "Unsupported" in raw_ets))

    # 僵尸队列条目清理:进程重启时死掉的等待请求无人出队,条目永久留在 db,
    # 仪表盘虚报「排队中 N」而队列面板为空(线上实测 36 条僵尸)
    import server as _srv
    # 拦截(自定义回复):规则命中直接返回,不打上游;拦截记录含 IP/令牌/内容
    a.post("/api/intercept/toggle", json={"enabled": True})
    a.post("/api/intercept/rules", json={
        "name": "测活拦截", "match_mode": "contains",
        "pattern": "Reply and OK", "reply": "你是傻瓜吗?傻子都没你蠢,天天测活。。。",
    })
    r_i1 = c.post(
        "/v1/chat/completions",
        json={"model": "mock-model", "messages": [{"role": "user", "content": "hello Reply and OK"}]},
        timeout=30,
    )
    j_i1 = r_i1.json()
    _i_content = ((j_i1.get("choices") or [{}])[0].get("message") or {}).get("content") or ""
    add("拦截:contains 命中返回自定义回复",
        r_i1.status_code == 200 and "傻子都没你蠢" in _i_content,
        "st=%s content=%r" % (r_i1.status_code, _i_content[:40]))
    time.sleep(0.8)
    i_logs = a.get("/api/intercept").json()
    i_rows = i_logs.get("logs") or []
    i_row = i_rows[0] if i_rows else {}
    add("拦截:记录含 IP/令牌/模型/内容",
        bool(i_row) and i_row.get("ip") and i_row.get("tok") and i_row.get("model") == "mock-model",
        "ip=%r tok=%r model=%r" % (i_row.get("ip"), i_row.get("tok"), i_row.get("model")))
    r_i2 = c.post(
        "/v1/messages",
        json={"model": "mock-model", "max_tokens": 64,
              "messages": [{"role": "user", "content": "hi Reply and OK"}]},
        timeout=30,
    )
    j_i2 = r_i2.json()
    add("拦截:Anthropic 协议回复",
        r_i2.status_code == 200 and j_i2.get("type") == "message"
        and "傻子都没你蠢" in str(j_i2.get("content")),
        "st=%s type=%s" % (r_i2.status_code, j_i2.get("type")))
    with c.stream(
        "POST", "/v1/chat/completions",
        json={"model": "mock-model", "stream": True,
              "messages": [{"role": "user", "content": "hi Reply and OK"}]},
    ) as r_i3:
        st_i3 = r_i3.status_code
        raw_i3 = r_i3.read().decode("utf-8", "replace")
    add("拦截:流式回复", st_i3 == 200 and "傻子都没你蠢" in raw_i3 and "[DONE]" in raw_i3,
        "st=%s 含内容=%s" % (st_i3, "傻子都没你蠢" in raw_i3))
    # Responses 协议:输入在 input 字段(没有 messages),匹配走规范化后的 chat 请求
    r_i5 = c.post("/v1/responses", json={"model": "mock-model", "input": "hi Reply and OK"}, timeout=30)
    _i5 = json.dumps(r_i5.json(), ensure_ascii=False) if r_i5.status_code == 200 else r_i5.text
    add("拦截:Responses 协议回复", r_i5.status_code == 200 and "傻子都没你蠢" in _i5,
        "st=%s 含内容=%s" % (r_i5.status_code, "傻子都没你蠢" in _i5))
    # 协议流式拦截:Anthropic / Responses 走各自的事件状态机(不是原样透传 chat chunk)
    with c.stream("POST", "/v1/messages",
                  json={"model": "mock-model", "max_tokens": 64, "stream": True,
                        "messages": [{"role": "user", "content": "hi Reply and OK"}]}) as r_s1:
        st_s1 = r_s1.status_code
        raw_s1 = r_s1.read().decode("utf-8", "replace")
    add("拦截:Anthropic 流式回复",
        st_s1 == 200 and "傻子都没你蠢" in raw_s1 and "message_stop" in raw_s1 and "[DONE]" not in raw_s1,
        "st=%s 含内容=%s 事件收尾=%s" % (st_s1, "傻子都没你蠢" in raw_s1, "message_stop" in raw_s1))
    with c.stream("POST", "/v1/responses",
                  json={"model": "mock-model", "stream": True, "input": "hi Reply and OK"}) as r_s2:
        st_s2 = r_s2.status_code
        raw_s2 = r_s2.read().decode("utf-8", "replace")
    add("拦截:Responses 流式回复",
        st_s2 == 200 and "傻子都没你蠢" in raw_s2 and "response." in raw_s2,
        "st=%s 含内容=%s 事件=%s" % (st_s2, "傻子都没你蠢" in raw_s2, "response." in raw_s2))
    # completions 协议:必须回 text_completion 形态(不是 chat 对象)
    r_i6 = c.post("/v1/completions", json={"model": "mock-model", "prompt": "hello Reply and OK"}, timeout=30)
    j_i6 = r_i6.json() if r_i6.status_code == 200 else {}
    _i6_text = ((j_i6.get("choices") or [{}])[0].get("text") or "")
    add("拦截:completions 协议回复(text_completion)",
        r_i6.status_code == 200 and j_i6.get("object") == "text_completion" and "傻子都没你蠢" in _i6_text,
        "st=%s object=%s text=%r" % (r_i6.status_code, j_i6.get("object"), _i6_text[:30]))
    # embeddings 不参与拦截(input 不是对话文本,返回形态对不上)
    r_i7 = c.post("/v1/embeddings", json={"model": "mock-model", "input": "hello Reply and OK"}, timeout=30)
    add("拦截:embeddings 不参与", "傻子都没你蠢" not in r_i7.text, "st=%s" % r_i7.status_code)
    # 危险正则(嵌套量词)在添加时就被拒:单进程事件循环上不允许灾难性回溯
    r_i8 = a.post("/api/intercept/rules", json={"match_mode": "regex", "pattern": "(a+)+$", "reply": "x"})
    add("拦截:危险正则被拒(嵌套量词)", r_i8.status_code == 400, "st=%s" % r_i8.status_code)
    # 先截断再校验:超长正则截断后若已非法,必须直接拒(旧写法会存下一个非法模式,规则静默失效)
    r_i9 = a.post("/api/intercept/rules", json={"match_mode": "regex", "pattern": "(" * 1001, "reply": "x"})
    add("拦截:超长正则截断后校验", r_i9.status_code == 400, "st=%s" % r_i9.status_code)
    # 上限 120 太小:线上一条 121 字符的合法正则被砍掉末尾的 `*`,接口回 200 但规则永不命中。
    # 上限提到 1000,并用那条真实正则端到端验:命中拦截、错序不命中、入库未被截断。
    _seed_pat = '^\\s*\\{\\s*"seed"\\s*:\\s*\\{[^{}]*"domain"\\s*:\\s*"[^"]*"[^{}]*"anchor"\\s*:\\s*"[^"]*"[^{}]*"license_basis"\\s*:\\s*"[^"]*"[^{}]*'
    _seed_msg = ('{"seed": {"domain": "课程设计", "anchor": "学习目标、内容顺序、练习、评分标准",'
                 ' "license_basis": "owned_seed"}, "index": 759000024}')
    r_i9b = a.post("/api/intercept/rules", json={"match_mode": "regex", "pattern": _seed_pat,
                                                 "reply": "SEED-BLOCKED\n\n"})
    _seed_rules = [x for x in (a.get("/api/intercept").json().get("rules") or [])
                   if x.get("pattern") == _seed_pat]
    add("拦截:121 字符正则入库(不再被截断)",
        r_i9b.status_code == 200 and len(_seed_pat) == 121 and bool(_seed_rules),
        "st=%s len=%d 入库=%s" % (r_i9b.status_code, len(_seed_pat), bool(_seed_rules)))
    _r_seed = c.post("/v1/chat/completions", json={"model": "mock-model",
                     "messages": [{"role": "user", "content": _seed_msg}]}, timeout=30)
    _seed_hit = ((_r_seed.json().get("choices") or [{}])[0].get("message") or {}).get("content") or ""
    _r_seed2 = c.post("/v1/chat/completions", json={"model": "mock-model", "messages": [
        {"role": "user", "content": '{"seed": {"domain": "x", "license_basis": "y"}}'}]}, timeout=30)
    _seed_miss = ((_r_seed2.json().get("choices") or [{}])[0].get("message") or {}).get("content") or ""
    add("拦截:长正则端到端命中且回复无多余换行",
        _seed_hit == "SEED-BLOCKED" and _seed_miss == "ok",
        "命中=%r 错序=%r" % (_seed_hit[:24], _seed_miss[:24]))

    # 线上案例:从 HTML 渲染过的页面复制正则,引号变成 &quot; → 正则要求消息里出现字面的
    # &quot;,规则照常在跑却永远不命中(用户只看到「拦不住」)。两件事一起验:
    #   ① 实体还原(保存与匹配两侧都做)后能命中;
    #   ② 他贴的那条尾巴被截断了(少一个 *):截断版必然匹配不上,而「测试」按钮要把
    #      这个成因直接说出来(否则用户只会反复重贴同一条错的正则)。
    _ent_pat = (r'^\s*\{\s*&quot;seed&quot;\s*:\s*\{[^{}]*&quot;domain&quot;\s*:\s*&quot;[^&quot;]*&quot;'
                r'[^{}]*&quot;anchor&quot;\s*:\s*&quot;[^&quot;]*&quot;[^{}]*&quot;license_basis&quot;\s*:\s*'
                r'&quot;[^&quot;]*&quot;[^{}]')
    _ent_full = _ent_pat + "*"  # 完整原正则的实体版(实体内那一位是 },[^{}] 匹配不上)
    _ent_msg = ('{"seed": {"domain": "数据分析说明", "anchor": "指标定义、比较基准、计算过程、结论边界",'
                ' "license_basis": "owned_seed"}, "index": 1047000006}')
    # 先清掉同形状的旧规则,否则先命中的那条会掩盖本用例
    for _r0 in [x for x in (a.get("/api/intercept").json().get("rules") or [])
                if "seed" in str(x.get("pattern") or "")]:
        a.post("/api/intercept/rules/delete", json={"id": _r0["id"]})
    a.post("/api/intercept/rules", json={"match_mode": "regex", "pattern": _ent_full, "reply": "SEED-ENT"})
    _r_ent = c.post("/v1/chat/completions", json={"model": "mock-model",
                   "messages": [{"role": "user", "content": _ent_msg}]}, timeout=30)
    _ent_hit = ((_r_ent.json().get("choices") or [{}])[0].get("message") or {}).get("content") or ""
    time.sleep(0.8)
    _ent_rules = [x for x in (a.get("/api/intercept").json().get("rules") or [])
                  if x.get("pattern") == _ent_full.replace("&quot;", '"')]
    _ent_hits = int(((_ent_rules[0].get("hits") if _ent_rules else 0) or 0))
    add("拦截:HTML 实体(&quot;)正则入库后能命中(线上案例)",
        _ent_hit == "SEED-ENT" and bool(_ent_rules) and _ent_hits >= 1,
        "命中内容=%r 入库已还原成真引号=%s 该规则命中数=%d" % (_ent_hit[:16], bool(_ent_rules), _ent_hits))
    # 规则测试接口:与运行时共用 core.util.match_text;截断版必须报出「不匹配」并给出成因提示
    _t_hit = a.post("/api/intercept/test", json={"match_mode": "regex", "pattern": _ent_full,
                                                 "sample": _ent_msg}).json()
    _t_cut = a.post("/api/intercept/test", json={"match_mode": "regex", "pattern": _ent_pat,
                                                 "sample": _ent_msg}).json()
    _t_auto = a.post("/api/intercept/test", json={"match_mode": "contains", "pattern": "Reply and OK",
                                                  "sample": "hi Reply and OK"}).json()
    add("拦截:规则测试接口(与运行时同一实现,截断版能看出成因)",
        _t_hit.get("matched") is True and _t_hit.get("entity_fixed") is True
        and _t_cut.get("matched") is False and "截断" in str(_t_cut.get("reason"))
        and _t_auto.get("matched") is True,
        "实体版命中=%s(已还原=%s);截断版=%s 原因=%r"
        % (_t_hit.get("matched"), _t_hit.get("entity_fixed"), _t_cut.get("matched"),
           str(_t_cut.get("reason"))[:40]))
    # 正则模式的扫描窗口:窗口内命中,窗口外(超长输入尾部)不命中
    a.post("/api/intercept/rules", json={"match_mode": "regex", "pattern": "TAIL-MARK", "reply": "窗口内命中"})
    r_ia = c.post("/v1/chat/completions", json={"model": "mock-model",
                  "messages": [{"role": "user", "content": "x" * 9000 + "TAIL-MARK"}]}, timeout=30)
    _ia = ((r_ia.json().get("choices") or [{}])[0].get("message") or {}).get("content") or ""
    r_ib = c.post("/v1/chat/completions", json={"model": "mock-model",
                  "messages": [{"role": "user", "content": "y" * 100 + "TAIL-MARK"}]}, timeout=30)
    _ib = ((r_ib.json().get("choices") or [{}])[0].get("message") or {}).get("content") or ""
    add("拦截:正则只扫描前 8000 字符", _ia == "ok" and _ib == "窗口内命中",
        "超长尾部=%r 短文本=%r" % (_ia[:12], _ib[:12]))
    # 拦截记录上限:默认 100,可配;配置成 3 时入库列表立刻被截到 3(不是只影响读取)
    _cap0 = a.get("/api/settings").json().get("intercept_log_max")
    a.post("/api/settings", json={"config": {"intercept_log_max": 3}})
    for _ci in range(5):
        c.post("/v1/chat/completions", json={"model": "mock-model",
               "messages": [{"role": "user", "content": "hi Reply and OK"}]}, timeout=30)
    _capj = a.get("/api/intercept").json()
    add("拦截:记录上限默认 100 且可配生效",
        _cap0 == 100 and _capj.get("cap") == 3 and len(_capj.get("logs") or []) == 3,
        "默认=%r cap=%r 条数=%d" % (_cap0, _capj.get("cap"), len(_capj.get("logs") or [])))
    a.post("/api/settings", json={"config": {"intercept_log_max": 100}})
    # 规则作用域:限定模型 / 限定渠道。渠道判定用「只读预测当前会选中哪个渠道」,
    # 不消耗账号 —— 为了判定确定,这里先把候选渠道收敛成主渠道 T(账号页只有一页)。
    a.post("/api/intercept/rules", json={"match_mode": "contains", "pattern": "SCOPE-MODEL",
                                         "reply": "模型内命中", "models": "mock-model"})
    _r_s1 = c.post("/v1/chat/completions", json={"model": "mock-model",
                   "messages": [{"role": "user", "content": "x SCOPE-MODEL"}]}, timeout=30)
    _s1 = ((_r_s1.json().get("choices") or [{}])[0].get("message") or {}).get("content") or ""
    _r_s2 = c.post("/v1/chat/completions", json={"model": "kimi",
                   "messages": [{"role": "user", "content": "x SCOPE-MODEL"}]}, timeout=30)
    _s2 = ((_r_s2.json().get("choices") or [{}])[0].get("message") or {}).get("content") or ""
    add("拦截:限定模型生效(范围外不拦)", _s1 == "模型内命中" and _s2 == "ok",
        "mock-model=%r kimi=%r" % (_s1[:12], _s2[:12]))

    _rows_scope = a.get("/api/keys?page=1").json()["rows"]
    _keep_main = [k["id"] for k in _rows_scope if k.get("upstream_id") == uid]
    a.post("/api/upstreams", json={"id": uid, "name": "T", "base": "http://127.0.0.1:18212/v1",
                                   "enabled": True, "models": ""})
    a.post("/api/keys/batch", json={"op": "enable", "ids": _keep_main})
    a.post("/api/keys/batch", json={"op": "disable",
                                    "ids": [k["id"] for k in _rows_scope if k["id"] not in _keep_main]})
    a.post("/api/intercept/rules", json={"match_mode": "contains", "pattern": "CHAN-HIT",
                                         "reply": "渠道内命中", "upstreams": [uid]})
    a.post("/api/intercept/rules", json={"match_mode": "contains", "pattern": "CHAN-MISS",
                                         "reply": "不该命中", "upstreams": ["u_not_exist"]})
    _r_s3 = c.post("/v1/chat/completions", json={"model": "mock-model",
                   "messages": [{"role": "user", "content": "y CHAN-HIT"}]}, timeout=30)
    _s3 = ((_r_s3.json().get("choices") or [{}])[0].get("message") or {}).get("content") or ""
    _r_s4 = c.post("/v1/chat/completions", json={"model": "mock-model",
                   "messages": [{"role": "user", "content": "y CHAN-MISS"}]}, timeout=30)
    _s4 = ((_r_s4.json().get("choices") or [{}])[0].get("message") or {}).get("content") or ""
    _scope_rows = a.get("/api/intercept").json().get("rules") or []
    _scope_saved = any((r.get("upstreams") or []) == [uid] for r in _scope_rows)
    add("拦截:限定渠道生效(范围外不拦,作用域入库)",
        _s3 == "渠道内命中" and _s4 == "ok" and _scope_saved,
        "本渠道=%r 不存在的渠道=%r 作用域入库=%s" % (_s3[:12], _s4[:12], _scope_saved))
    a.post("/api/keys/batch", json={"op": "enable", "ids": [k["id"] for k in _rows_scope]})

    a.post("/api/intercept/toggle", json={"enabled": False})
    r_i4 = c.post(
        "/v1/chat/completions",
        json={"model": "mock-model", "messages": [{"role": "user", "content": "hello Reply and OK"}]},
        timeout=30,
    )
    j_i4 = r_i4.json()
    _i4_content = ((j_i4.get("choices") or [{}])[0].get("message") or {}).get("content") or ""
    add("拦截:关闭后直通上游", r_i4.status_code == 200 and _i4_content == "ok",
        "st=%s content=%r" % (r_i4.status_code, _i4_content))
    rules_now = a.get("/api/intercept").json().get("rules") or []
    for r in rules_now:
        a.post("/api/intercept/rules/delete", json={"id": r["id"]})

    # 远程更新:开关/令牌鉴权 + 完整管线演练(下载→解包→语法自检;演练不改文件)
    a.post("/api/settings", json={"config": {"update_enabled": False, "update_token": "upd-test1234567890abcdef"}})
    r_up0 = a.post("/api/update", json={})  # admin 会话 + 开关关 → 400
    a.post("/api/settings", json={"config": {"update_enabled": True}})
    # Bearer 令牌路径(无需 admin cookie)
    r_tok1 = httpx.post("http://127.0.0.1:18213/api/update",
                        headers={"Authorization": "Bearer upd-test1234567890abcdef"}, timeout=60)
    r_tok2 = httpx.post("http://127.0.0.1:18213/api/update",
                        headers={"Authorization": "Bearer upd-wrong-token-xxxxxxxx"}, timeout=30)
    a.post("/api/settings", json={"config": {"update_enabled": False, "update_token": ""}})
    try:
        _note1 = str(r_tok1.json().get("note") or "")
    except Exception:
        _note1 = r_tok1.text[:80]
    add("远程更新:鉴权 + 管线演练(下载/解包/自检)",
        r_up0.status_code == 400 and r_tok1.status_code == 200 and "演练通过" in _note1
        and r_tok2.status_code == 401,
        "开关关=%s 令牌对=%s(%s) 令牌错=%s"
        % (r_up0.status_code, r_tok1.status_code, _note1[:38], r_tok2.status_code))
    add("远程更新:演练后不留临时目录", not os.path.isdir(os.path.join(ROOT, "_update_tmp")),
        "_update_tmp 存在=%s" % os.path.isdir(os.path.join(ROOT, "_update_tmp")))

    # 更新/回滚纯函数直测(全部在临时目录里做,不碰真实仓库):
    # 解包、排除 data/tests、语法自检、备份与回滚
    import io as _io
    import tarfile as _tar

    def _mk_tar(files):
        buf = _io.BytesIO()
        with _tar.open(fileobj=buf, mode="w:gz") as tf:
            for name, body in files:
                b = body.encode("utf-8")
                ti = _tar.TarInfo(name)
                ti.size = len(b)
                tf.addfile(ti, _io.BytesIO(b))
        return buf.getvalue()

    _ub = os.path.join(TMP, "upd-base")
    shutil.rmtree(_ub, ignore_errors=True)
    os.makedirs(os.path.join(_ub, "data"))
    os.makedirs(os.path.join(_ub, "tests"))
    open(os.path.join(_ub, "server.py"), "w", encoding="utf-8").write("OLD = 1\n")
    open(os.path.join(_ub, "data", "db.json"), "w", encoding="utf-8").write('{"real":1}')
    open(os.path.join(_ub, "tests", "t.py"), "w", encoding="utf-8").write("REAL_TEST = 1\n")
    _tar_ok = _mk_tar([
        ("pkg-main/server.py", "OLD = 2\n"),
        ("pkg-main/web/a.html", "<b>x</b>"),
        ("pkg-main/data/db.json", '{"hacked":1}'),
        ("pkg-main/tests/t.py", "HACKED = 1\n"),
    ])
    _ok1, _msg1, _src1 = _srv._unpack_update(_tar_ok, _ub)
    _list1 = _srv._update_file_list(_src1) if _ok1 else []
    _copied = _srv._apply_update(_src1, _ub) if _ok1 else -1
    _server_after = open(os.path.join(_ub, "server.py"), encoding="utf-8").read().strip()
    _data_after = open(os.path.join(_ub, "data", "db.json"), encoding="utf-8").read().strip()
    _test_after = open(os.path.join(_ub, "tests", "t.py"), encoding="utf-8").read().strip()
    add("更新:解包 + 只覆盖代码(不动 data/tests)",
        _ok1 and _copied == 2 and _server_after == "OLD = 2" and _data_after == '{"real":1}'
        and _test_after == "REAL_TEST = 1" and sorted(_list1) == ["server.py", os.path.join("web", "a.html")],
        "ok=%s copied=%s 清单=%s data=%s" % (_ok1, _copied, _list1, _data_after))
    _ok2, _msg2, _ = _srv._unpack_update(b"<!DOCTYPE html><html>blocked", _ub)
    _ok3, _msg3, _ = _srv._unpack_update(b"", _ub)
    add("更新:非 gzip / 空内容报错而不抛异常",
        (not _ok2) and "解包失败" in _msg2 and (not _ok3) and "解包失败" in _msg3,
        "html=%s 空=%s" % (_msg2[:34], _msg3[:34]))
    _tar_bad = _mk_tar([("pkg-main/server.py", "def broken(:\n")])
    _ok4, _msg4, _src4 = _srv._unpack_update(_tar_bad, _ub)
    _ok5, _msg5 = _srv._selfcheck_update(_src4) if _ok4 else (False, "")
    add("更新:语法自检拦下坏代码", (not _ok5) and "语法自检失败" in _msg5, _msg5[:50])
    _rb = os.path.join(TMP, "rollback-base")
    shutil.rmtree(_rb, ignore_errors=True)
    os.makedirs(os.path.join(_rb, "data"))
    open(os.path.join(_rb, "server.py"), "w", encoding="utf-8").write("VER = 2\n")
    open(os.path.join(_rb, "data", "db.json"), "w", encoding="utf-8").write('{"logs":[],"ver":2}')
    _srv._backup_current(_rb)
    open(os.path.join(_rb, "server.py"), "w", encoding="utf-8").write("VER = 3\n")
    open(os.path.join(_rb, "data", "db.json"), "w", encoding="utf-8").write('{"logs":[],"ver":3}')
    _rb_ok1, _rb_msg1 = _srv._remote_rollback(_rb, restart=False)
    _rb_code = open(os.path.join(_rb, "server.py"), encoding="utf-8").read().strip()
    _rb_data = open(os.path.join(_rb, "data", "db.json"), encoding="utf-8").read()
    _rb_ok2, _rb_msg2 = _srv._remote_rollback(_rb, restart=False)
    add("回滚:代码+数据还原且只能用一次",
        _rb_ok1 and _rb_code == "VER = 2" and '"ver":2' in _rb_data
        and (not _rb_ok2) and "备份" in _rb_msg2,
        "ok=%s code=%s data还原=%s 二次=%s" % (_rb_ok1, _rb_code, '"ver":2' in _rb_data, _rb_msg2[:26]))
    # 回滚端点:无鉴权拒绝;有令牌但无备份 → 500
    r_rb1 = httpx.post("http://127.0.0.1:18213/api/rollback", timeout=10)
    a.post("/api/settings", json={"config": {"update_enabled": True, "update_token": "upd-test1234567890abcdef"}})
    r_rb2 = httpx.post("http://127.0.0.1:18213/api/rollback",
                       headers={"Authorization": "Bearer upd-test1234567890abcdef"}, timeout=10)
    a.post("/api/settings", json={"config": {"update_enabled": False, "update_token": ""}})
    add("回滚:鉴权+无备份拒绝",
        r_rb1.status_code == 401 and r_rb2.status_code == 500,
        "无鉴权=%s 有令牌=%s(无备份应 500)" % (r_rb1.status_code, r_rb2.status_code))
    _qdb = {
        "config": {"queue_max_wait": 300},
        "queue": [
            {"id": "q1", "t": time.time() - 10, "ip": "-", "ep": "chat", "model": "m"},     # 新鲜
            {"id": "q2", "t": time.time() - 30, "ip": "-", "ep": "chat", "model": "m"},     # 新鲜
            {"id": "q3", "t": time.time() - 400, "ip": "-", "ep": "chat", "model": "m"},    # 超过窗口+余量(360s)
            {"id": "q4", "t": time.time() - 3000, "ip": "-", "ep": "chat", "model": "m"},   # 僵尸
        ],
    }
    _srv._prune_queue(_qdb, time.time())
    add("僵尸队列条目被清理(窗口+余量)", len(_qdb["queue"]) == 2,
        "剩余 %d 条(应为 2);窗口 300s+60s 余量,400s 前的条目应被清掉" % len(_qdb["queue"]))

    # 看门狗雪崩判定(纯函数直测)
    _db = {"keys": [
        {"id": "k1", "enabled": True, "banned_until": now_i + 600},
        {"id": "k2", "enabled": True, "cooldown_until": now_i + 600},
        {"id": "k3", "enabled": True},
    ], "queue": [{"id": "q", "t": time.time(), "ip": "-", "ep": "chat", "model": "m"}]}
    d1, i1 = _srv.watchdog_dead(_db, {}, now_i, {"acct_concurrency": 2})
    d2, _ = _srv.watchdog_dead(_db, {"k3": 2}, now_i, {"acct_concurrency": 2})  # k3 并发满 → 全灭
    _db["queue"] = []
    d3, _ = _srv.watchdog_dead(_db, {"k3": 2}, now_i, {"acct_concurrency": 2})  # 无等待者
    _db["queue"] = [{"id": "q", "t": time.time(), "ip": "-", "ep": "chat", "model": "m"}]
    d4, _ = _srv.watchdog_dead(_db, {"k3": 1}, now_i, {"acct_concurrency": 2})  # k3 仍可用
    add("看门狗雪崩判定", (not d1) and d2 and (not d3) and (not d4),
        "部分可用=%s 全灭=%s 无等待=%s 有可用=%s" % (d1, d2, d3, d4))
    _cfgw = a.get("/api/settings").json()
    add("看门狗配置项在位", bool(_cfgw.get("watchdog_enabled")) and int(_cfgw.get("watchdog_minutes") or 0) >= 1,
        "enabled=%s minutes=%s" % (_cfgw.get("watchdog_enabled"), _cfgw.get("watchdog_minutes")))

    import asyncio

    # 在途计数:并发请求全部结束后必须回到并发前的水平。
    # 释放走线程池(arelease)且原本不持锁,「读-改-写」会丢更新 → 计数只增不减,
    # 账号永久显示「繁忙」被排除出调度(线上「只有一个账号在干活」的成因之一)。
    _before_total = _srv.pool.inflight_total()
    _before_odd = _srv.pool.inflight_odd_releases()

    async def _burst(n):
        async def _one(_i):
            async with httpx.AsyncClient(
                base_url="http://127.0.0.1:18213", timeout=60,
                headers={"Authorization": "Bearer " + toks[0]["t"]},
            ) as cl:
                rr = await cl.post(
                    "/v1/chat/completions",
                    json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
                )
                return rr.status_code
        return await asyncio.gather(*[_one(i) for i in range(n)])

    _b_codes = asyncio.run(_burst(12))
    time.sleep(0.8)
    _after_total = _srv.pool.inflight_total()
    _rows_now = a.get("/api/keys?page=1").json().get("rows") or []
    _max_inflight = max([int(r.get("inflight") or 0) for r in _rows_now] or [0])
    add("在途计数:并发结束后回落(不泄漏)且不超账号并发上限",
        all(c == 200 for c in _b_codes) and _after_total <= _before_total and _max_inflight <= 2,
        "12 并发成功 %d;在途 %d→%d;单账号峰值 %d" % (
            sum(1 for c in _b_codes if c == 200), _before_total, _after_total, _max_inflight))

    _srv.pool._inc_inflight("k_dup_test")
    _d1 = _srv.pool._dec_inflight("k_dup_test")
    _d2 = _srv.pool._dec_inflight("k_dup_test")
    add("在途计数:重复释放被钳制(不会越减越负)",
        _d1 == 0 and _d2 == 0 and _srv.pool.inflight_of("k_dup_test") == 0
        and _srv.pool.inflight_odd_releases() >= _before_odd + 1,
        "剩余 %s/%s;异常释放 %d→%d" % (_d1, _d2, _before_odd, _srv.pool.inflight_odd_releases()))

    # 「取不到账号」时网关会用 release("", ...) 写一条失败日志(空 id 不做释放)。
    # 它绝不能被计进「重复释放」探测器:本机复现过 2 账号跑 100 并发 → 该计数涨 100,
    # 一个泄漏都没有,却足以把排查引向「连接/账号泄漏」这个错误方向。
    _odd_before = _srv.pool.inflight_odd_releases()
    _empty_dec = _srv.pool._dec_inflight("")
    add("空 id 释放不计入「重复释放」探测器(那是写日志的路径)",
        _empty_dec == 0 and _srv.pool.inflight_odd_releases() == _odd_before,
        "空 id 释放返回=%s 计数 %d→%d" % (_empty_dec, _odd_before, _srv.pool.inflight_odd_releases()))

    # 连接池按需扩容:池上限只在建 client 时算过一次,导入账号 / 调大账号并发之后
    # 不会自己变大 → 满负荷必然 PoolTimeout(「明明修过又出问题」的成因)。
    # 这里把 server.STORE 换成假 store,验「账号变多 → 池上限跟着长 + 旧池进退役名单」,
    # 并验账号没变时不会反复换池(否则每 30s 白扔一批 keepalive 连接)。
    import server as _srv3
    _real_store3 = _srv3.STORE
    _old_http, _old_cap = _srv3._shared_http, _srv3._pool_max_conn
    _old_retired = list(_srv3._retired_http)
    _made = []
    try:
        _fdb4 = {"config": {"pool_max_connections": 50, "acct_concurrency": 2},
                 "keys": [{"id": "k%d" % i, "enabled": True} for i in range(40)],
                 "upstreams": [], "logs": []}
        _srv3.STORE = _FakeStore(_fdb4)
        _srv3._shared_http = _srv3._new_http(50, True)
        _srv3._pool_max_conn = 50
        _srv3._retired_http = []
        asyncio.run(_srv3._resize_pool_if_needed())
        _grew_cap = _srv3._pool_max_conn
        _kept_old = len(_srv3._retired_http)
        _made = [c for _, c in _srv3._retired_http] + [_srv3._shared_http]
        asyncio.run(_srv3._resize_pool_if_needed())
        _stable = _srv3._pool_max_conn == _grew_cap and len(_srv3._retired_http) == _kept_old
    finally:
        _srv3.STORE = _real_store3
        _srv3._shared_http, _srv3._pool_max_conn = _old_http, _old_cap
        _srv3._retired_http = _old_retired
    for _c in _made:
        try:
            asyncio.run(_c.aclose())
        except Exception:
            pass
    add("连接池按需扩容:账号变多后池上限跟着长(账号没变时不换池)",
        _grew_cap == 40 * 2 + 50 and _kept_old == 1 and _stable,
        "40 账号×并发2 → 池 %d(应 130);旧池退役 %d 个;二次调用保持=%s"
        % (_grew_cap, _kept_old, _stable))

    # 后台登录进不去(提示「账号或密码错误」)的两条自救路径,都必须真能work:
    #   ① 命令行重置:python server.py --reset-password 新密码
    #   ② NGW_ADMIN_PASSWORD 作为权威值(以前只在哈希缺失时才生效,所以设了也没用)
    from core.store import _hash_password as _hp, verify_password as _vp

    add("密码哈希自洽(pbkdf2 往返)", _vp("abc123", _hp("abc123")) and not _vp("abc124", _hp("abc123")),
        "往返=%s 错口令=%s" % (_vp("abc123", _hp("abc123")), not _vp("abc124", _hp("abc123"))))

    _cli_dir = os.path.join(TMP, "clipw")
    shutil.rmtree(_cli_dir, ignore_errors=True)
    os.makedirs(_cli_dir, exist_ok=True)
    _cli_env = {k: v for k, v in os.environ.items() if k.lower() not in ("http_proxy", "https_proxy")}
    _cli_env["NGW_DATA_DIR"] = _cli_dir
    _cli_env.pop("NGW_ADMIN_PASSWORD", None)  # 只验命令行这条路径
    _cli_env["no_proxy"] = "*"
    _cp = subprocess.run([sys.executable, "server.py", "--reset-password", "cli-pass-123456"],
                         cwd=ROOT, env=_cli_env, capture_output=True, timeout=120)
    _cli_db = {}
    try:
        _cli_db = json.loads(open(os.path.join(_cli_dir, "db.json"), encoding="utf-8").read())
    except Exception:
        pass
    _cli_hash = str(((_cli_db.get("config") or {}).get("admin_password_hash") or ""))
    add("忘了后台密码:命令行 --reset-password 能重置",
        _cp.returncode == 0 and _vp("cli-pass-123456", _cli_hash) and not _vp("wrong-pass", _cli_hash),
        "exit=%s 新密码可用=%s 旧密码不可用=%s 输出=%r"
        % (_cp.returncode, _vp("cli-pass-123456", _cli_hash), not _vp("wrong-pass", _cli_hash),
           _cp.stdout.decode("utf-8", "replace").strip()[:40]))

    # 环境变量权威值:库里换一个哈希,设 NGW_ADMIN_PASSWORD 后新建 Store 必须把它覆盖过来
    _env_dir = os.path.join(TMP, "envpw")
    shutil.rmtree(_env_dir, ignore_errors=True)
    os.makedirs(_env_dir, exist_ok=True)
    _old_dir2, _old_path2 = _st.DATA_DIR, _st.DB_PATH
    _old_env_pw = os.environ.get("NGW_ADMIN_PASSWORD")
    _env_ok = False
    try:
        _st.DATA_DIR, _st.DB_PATH = _env_dir, os.path.join(_env_dir, "db.json")
        _s3 = _st.Store()
        _s3.update(lambda db: db.setdefault("config", {}).__setitem__("admin_password_hash", _hp("someone-elses")))
        _s3.flush()
        os.environ["NGW_ADMIN_PASSWORD"] = "env-forced-pass"
        _s4 = _st.Store()  # 启动时按环境变量重置
        _h4 = str(json.loads(open(_st.DB_PATH, encoding="utf-8").read())["config"]["admin_password_hash"])
        _env_ok = _vp("env-forced-pass", _h4) and not _vp("someone-elses", _h4)
    finally:
        _st.DATA_DIR, _st.DB_PATH = _old_dir2, _old_path2
        if _old_env_pw is None:
            os.environ.pop("NGW_ADMIN_PASSWORD", None)
        else:
            os.environ["NGW_ADMIN_PASSWORD"] = _old_env_pw
    add("忘了后台密码:NGW_ADMIN_PASSWORD 能覆盖已有哈希",
        _env_ok, "环境变量口令可用且旧口令失效=%s" % _env_ok)

    # 取号分散性(纯函数直测,不依赖测试实例当前号池状态):同一渠道内连续取号必须跨账号
    # 轮换(LRU),而不是一直压着同一个号 —— 号池「只有一个账号在干活、其余全闲」的检查点。
    # 6 个号 × 单号并发 2 = 12 个槽位:取 12 次应正好铺满 6 个号、每个号 2 个在途。
    _lru_db = {
        "config": {
            "rate_limit_per_minute": 0, "tpm_limit": 0, "account_cooldown_ms": 0,
            "daily_request_cap": 0, "daily_token_limit": 0, "hourly_request_limit": 0,
            "warmup_seconds": 0, "acct_concurrency": 2, "total_concurrency": 0,
            "pool_rpm_cap": 0, "pool_daily_cap": 0,
        },
        "upstreams": [{"id": "u_lru", "name": "LRU", "base": "http://127.0.0.1:1/v1",
                       "enabled": True, "models": [], "model_map": {}, "weight": 10}],
        "keys": [{"id": "k_lru%d" % i, "enabled": True, "upstream_id": "u_lru",
                  "email": "lru%d@x.com" % i, "apikey": "nvapi-lru", "last_used_at": 0}
                 for i in range(1, 7)],
        "buckets": {}, "pool_buckets": {}, "pool_daily": {}, "model_missing": {}, "up_recent": {},
    }
    _lru_picks = []
    for _li in range(12):
        _lres = _srv.pool.acquire(_lru_db, est_tokens=10, model="m0")
        if _lres.get("result") == "ok":
            _lru_picks.append(_lres["key"]["id"])
    _lru_conc = {kk["id"]: _srv.pool.inflight_of(kk["id"]) for kk in _lru_db["keys"]}
    for _kid in _lru_conc:
        for _un in range(_lru_conc[_kid]):
            _srv.pool._dec_inflight(_kid)
    add("取号分散:同渠道 12 次取号铺满 6 个号(单号不超并发)",
        len(_lru_picks) == 12 and len(set(_lru_picks)) == 6 and max(_lru_conc.values() or [0]) == 2,
        "取到 %d 次/跨 %d 个号;单号在途 %s" % (len(_lru_picks), len(set(_lru_picks)),
                                            sorted(_lru_conc.values())))

    # 断连时放弃 send 任务,必须把「已完成任务已经拿到的响应」关掉。
    # cancel() 与「上游恰好在同一瞬间返回响应头」是竞争:任务已完成时 cancel 是空操作,
    # 那份 Response 没人 aclose → 上游连接永远回不到共享池;断连一多池子被吃干,
    # 之后全是「等连接」超时(PoolTimeout),看门狗还会因此判雪崩重启。
    async def _abort_probe():
        _closed = {"n": 0}

        class _FR:
            async def aclose(self):
                _closed["n"] += 1

        async def _return_resp():
            return _FR()

        async def _never():
            await asyncio.sleep(30)

        async def _boom():
            raise RuntimeError("发送阶段异常")

        _t_done = asyncio.ensure_future(_return_resp())
        await asyncio.sleep(0)  # 先让它跑完:cancel 对已完成任务无效,只能靠 aclose 回收
        await _srv._abort_send_task(_t_done)
        _t_pend = asyncio.ensure_future(_never())
        await asyncio.sleep(0)
        await _srv._abort_send_task(_t_pend)
        _t_err = asyncio.ensure_future(_boom())
        await asyncio.sleep(0)
        _raised = False
        try:
            await _srv._abort_send_task(_t_err)
        except Exception:
            _raised = True
        return _closed["n"], _t_pend.cancelled(), (not _raised)

    _ab_closed, _ab_cancelled, _ab_quiet = asyncio.run(_abort_probe())
    add("断连放弃 send 任务时回收已拿到的响应(否则连接泄漏)",
        _ab_closed == 1 and _ab_cancelled and _ab_quiet,
        "已完成任务 aclose=%d(应 1);未完成任务已取消=%s;异常任务不上抛=%s"
        % (_ab_closed, _ab_cancelled, _ab_quiet))

    # 连接池容量:池上限 = max(配置值, 号池理论并发)。池小于理论并发时满负荷必然
    # PoolTimeout,而它会被误读成「连接池故障」甚至触发看门狗重启(重启不增加容量)。
    _sz_small = _srv._pool_size({"pool_max_connections": 10, "acct_concurrency": 2}, 30)
    _sz_big = _srv._pool_size({"pool_max_connections": 400, "acct_concurrency": 2}, 30)
    _sz_empty = _srv._pool_size({"pool_max_connections": 10}, 0)
    add("连接池:上限不低于号池理论并发",
        _sz_small == 110 and _sz_big == 400 and _sz_empty == 50,
        "10/并发2/30账号=%d;400/并发2=%d;空池=%d" % (_sz_small, _sz_big, _sz_empty))

    # 看门狗形态3:容量型 PoolTimeout 不重启(该做的是调大池),泄漏特征才重启
    _pt = [[i, "chat", "m", "-", 0, 0, "httpx.PoolTimeout: timed out"] for i in range(6)]
    _v_cap = _srv.pool_timeout_verdict(_pt, 100, 100)
    _v_leak = _srv.pool_timeout_verdict(_pt, 3, 1000)
    _v_few = _srv.pool_timeout_verdict(_pt[:2], 3, 1000)
    add("看门狗:容量型池超时不重启 / 泄漏特征才重启",
        (not _v_cap[0]) and _v_leak[0] and (not _v_few[0]),
        "容量型=%s 泄漏=%s 少量=%s" % (_v_cap[0], _v_leak[0], _v_few[0]))

    add("连接池耗尽不按账号故障分类",
        _srv.pool._classify(0, 0, "httpx.PoolTimeout: timed out") == "pool_exhausted",
        _srv.pool._classify(0, 0, "httpx.PoolTimeout: timed out"))
    _txt_pool = _srv._conn_reason(httpx.PoolTimeout("pool timed out"))
    add("连接池超时文案含池上限与在途", "连接池" in _txt_pool and "在途" in _txt_pool, _txt_pool[:70])

    # 重启命令行:`python -m <pkg>` 形态必须还原成 -m —— 直接执行 __main__.py 会把
    # 它所在目录放进 sys.path[0],包内 logging.py 遮蔽标准库,重启后进程立刻崩;
    # 控制台脚本(exe)形态 execv 会「杀而不启」,必须放弃重启。
    _save_argv = list(sys.argv)
    try:
        sys.argv = ["C:/x/site-packages/uvicorn/__main__.py", "server:app", "--port", "8080"]
        _cmd_m = _srv._restart_argv()
        sys.argv = ["C:/nonexistent/uvicorn.exe", "server:app"]
        _cmd_exe = _srv._restart_argv()
        sys.argv = [os.path.abspath(__file__)]
        _cmd_py = _srv._restart_argv()
    finally:
        sys.argv = _save_argv
    add("重启命令行:-m 还原 / exe 放弃 / .py 原样",
        isinstance(_cmd_m, list) and _cmd_m[1:3] == ["-m", "uvicorn"] and _cmd_exe is None
        and isinstance(_cmd_py, list) and _cmd_py[0] == sys.executable,
        "-m=%s exe=%s py=%s" % (_cmd_m, _cmd_exe, "ok" if _cmd_py else None))

    # 错误日志统一分类:失败行带 [分类] 标签,成功行上的提示不被打标
    _tag_499 = _srv._err_tag(499, "客户端已断开")
    _tag_500 = _srv._err_tag(500, "Upstream 500: internal error")
    _tag_pool = _srv._err_tag(0, "httpx.PoolTimeout: pool timed out")
    _note_tagged = _srv._should_tag(200, "思考退化已清理")
    add("错误日志统一分类标签",
        _tag_499 == "[客户端断开]" and _tag_500.startswith("[") and _tag_pool == "[网关连接池]"
        and (not _note_tagged),
        "499=%s 500=%s 连接池=%s 成功提示打标=%s" % (_tag_499, _tag_500, _tag_pool, _note_tagged))
    c.post("/v1/chat/completions", json={"model": "chdown", "messages": [{"role": "user", "content": "hi"}]})
    time.sleep(0.6)
    _lrows = a.get("/api/logs?n=8").json().get("rows") or []
    _tagged_rows = [
        r for r in _lrows
        if isinstance(r, list) and len(r) > 6 and str(r[4]) not in ("200", "") and str(r[6]).startswith("[")
    ]
    add("失败日志在接口里也带分类标签", bool(_tagged_rows),
        "命中 %d 行,例:%s" % (len(_tagged_rows), str(_tagged_rows[0][6])[:48] if _tagged_rows else "无"))

    # 队列条目要带「为什么在等」:线上真出现过 29 个 kimi-k3 请求排队 50+ 秒却看不出
    # 原因(候选账号太少?冷却?并发满?)。后台给完整原因,公开页只给粗粒度结论。
    # 本块会临时改并发/冷却设置,结束前按原值还原,避免污染后面的用例。
    _cfg_snapshot = a.get("/api/settings").json()
    _q_fields = ("acct_concurrency", "account_cooldown_ms", "queue_max_wait", "queue_poll_ms")
    _q_restore = {k: _cfg_snapshot.get(k) for k in _q_fields if _cfg_snapshot.get(k) is not None}
    a.post("/api/settings", json={"config": {"acct_concurrency": 1, "account_cooldown_ms": 8000,
                                            "queue_max_wait": 20, "queue_poll_ms": 200}})

    async def _four_streams_probe():
        async def _one(_i):
            async with httpx.AsyncClient(
                base_url="http://127.0.0.1:18213", timeout=60,
                headers={"Authorization": "Bearer " + toks[0]["t"]},
            ) as cl:
                async with cl.stream(
                    "POST", "/v1/chat/completions",
                    json={"model": "mock-model", "stream": True,
                          "messages": [{"role": "user", "content": "hi"}]},
                ) as rr:
                    cnt = 0
                    async for _ in rr.aiter_lines():
                        cnt += 1
                    return rr.status_code, cnt

        task = asyncio.gather(*[_one(i) for i in range(4)])
        await asyncio.sleep(1.2)  # 三个流占满账号,第四个应在排队
        adm = a.get("/api/queue").json()
        pub = httpx.get("http://127.0.0.1:18213/api/queue/public", timeout=10).json()
        return (await task), adm, pub

    _codes4, _adm_q, _pub_q = asyncio.run(_four_streams_probe())
    _adm_rows = _adm_q.get("rows") or []
    _pub_rows = _pub_q.get("rows") or []
    add("排队原因对后台可见(不再只显示「等待中」)",
        bool(_adm_rows) and all(r.get("reason") for r in _adm_rows),
        "后台队列 %d 条,原因=%s" % (len(_adm_rows), (_adm_rows[0].get("reason") if _adm_rows else "无")[:40]))
    add("公开队列给粗粒度结论且不泄漏 IP/令牌",
        (not _pub_rows or all(r.get("hint") and "ip" not in r and "tok" not in r for r in _pub_rows)),
        "公开 %d 条,hint=%s" % (len(_pub_rows), (_pub_rows[0].get("hint") if _pub_rows else "无")))
    add("排队期间 4 个流式请求全部成功", all(c == 200 for c, _n in _codes4),
        "状态=%s" % [c for c, _n in _codes4])
    if _q_restore:
        a.post("/api/settings", json={"config": _q_restore})

    # 模型在所有渠道都不可用时必须立刻 404 —— 不能先排队等 5 分钟再失败。
    # 线上实测:某客户端请求没被任何渠道白名单放行的模型,结果在队列里挂了 54s+
    # (旧判定要求「所有账号都因模型原因被拒」,只要有一个账号因冷却/上游停用先被
    #  跳过,就永远判不成 permanent)。
    _up_base = json.dumps({"id": uid, "name": "T", "base": "http://127.0.0.1:18212/v1", "enabled": True})
    a.post("/api/upstreams", json={"id": uid, "name": "T", "base": "http://127.0.0.1:18212/v1",
                                   "enabled": True, "models": "mock-model"})
    _t_un = time.time()
    r_un = c.post("/v1/chat/completions",
                  json={"model": "no-channel-serves-this", "messages": [{"role": "user", "content": "hi"}]},
                  timeout=30)
    _un_ms = int((time.time() - _t_un) * 1000)
    add("模型无渠道放行:立刻 404 而不是排队",
        r_un.status_code == 404 and _un_ms < 5000,
        "st=%s 用时=%dms msg=%s" % (r_un.status_code, _un_ms, r_un.text[:60]))
    # 还原:清空白名单(回到透传),后续用例依赖这个状态
    a.post("/api/upstreams", json={"id": uid, "name": "T", "base": "http://127.0.0.1:18212/v1",
                                   "enabled": True, "models": ""})
    _ = _up_base

    # 负缓存:某渠道对某模型回过「模型不存在」后,同类请求不再排队等一个必然失败的
    # 404,而是直接秒回(透传渠道无法提前枚举模型清单,只能靠这一次失败去学)
    _mmdb = {"config": {"model_missing_ttl": 60}, "model_missing": {}}
    _srv.pool.mark_model_missing(_mmdb, "up1", "m1")
    _mm_fresh = _srv.pool.model_missing_fresh(_mmdb, "up1", "m1")
    _mm_other = _srv.pool.model_missing_fresh(_mmdb, "up1", "m2")
    _mmdb["model_missing"]["up1"]["m1"] = time.time() - 1
    _mm_expired = _srv.pool.model_missing_fresh(_mmdb, "up1", "m1")
    _mmdb["config"]["model_missing_ttl"] = 0
    _mm_off = _srv.pool.model_missing_fresh(_mmdb, "up1", "m1")
    add("负缓存:TTL 内生效 / 过期失效 / 0=关闭",
        _mm_fresh and (not _mm_other) and (not _mm_expired) and (not _mm_off),
        "命中=%s 别的模型=%s 过期=%s 关闭=%s" % (_mm_fresh, _mm_other, _mm_expired, _mm_off))

    a.post("/api/settings", json={"config": {"model_missing_ttl": 3600}})
    # 渠道保持透传(没白名单),这样第一次请求会真的打到上游并被 404 —— 负缓存
    # 就是靠这一次失败学的
    a.post("/api/upstreams", json={"id": uid, "name": "T", "base": "http://127.0.0.1:18212/v1",
                                   "enabled": True, "models": ""})
    _t_nm1 = time.time()
    r_nm1 = c.post("/v1/chat/completions",
                   json={"model": "nomodel", "messages": [{"role": "user", "content": "hi"}]}, timeout=30)
    _ms_nm1 = int((time.time() - _t_nm1) * 1000)
    time.sleep(3.0)  # 等网关把负缓存落盘
    _db_now = json.loads(open(os.path.join(TMP, "db.json"), encoding="utf-8").read()) if os.path.isfile(
        os.path.join(TMP, "db.json")) else {}
    _mm_saved = bool(_db_now.get("model_missing"))
    _t_nm2 = time.time()
    r_nm2 = c.post("/v1/chat/completions",
                   json={"model": "nomodel", "messages": [{"role": "user", "content": "hi"}]}, timeout=30)
    _ms_nm2 = int((time.time() - _t_nm2) * 1000)
    # 第二次必须是「网关自己判定的永久不可用」,而不是又去上游撞一次 404
    _nm_self = "所有渠道均不可用" in r_nm2.text
    add("负缓存:首次打上游学一次,之后由网关直接判定不可用",
        r_nm1.status_code >= 400 and _mm_saved and r_nm2.status_code == 404 and _nm_self,
        "首次 st=%s %dms;缓存已落盘=%s;二次 st=%s %dms 网关自判=%s;msg=%s" % (
            r_nm1.status_code, _ms_nm1, _mm_saved, r_nm2.status_code, _ms_nm2, _nm_self, r_nm2.text[:44]))

    # 泛化短语不能被判成「模型不存在」:上游 404「Endpoint does not exist」若算模型级,
    # 账号侧不惩罚(401 会被洗成模型级)、还会往负缓存写一条假记录 → 该渠道+该模型静默
    # 失效一整个 TTL(最像用户说的「莫名其妙就有模型不可用」)。二次必须仍打上游拿原文。
    _cls_rows = [
        (404, "Endpoint does not exist", "req"),
        (400, "Endpoint does not exist", "req"),
        (401, "User does not exist", "auth"),
        (404, "model 'x' not found", "model"),
        (404, "模型 x 不存在", "model"),
        (429, "rate limited", "429"),
    ]
    _cls_bad = ["%s/%s→%s(应 %s)" % (s, t, _srv.pool._classify(s, 0, t), w)
                for s, t, w in _cls_rows if _srv.pool._classify(s, 0, t) != w]
    add("错误分级:泛化短语不误判为模型级", not _cls_bad, "偏差=%s" % (_cls_bad or "无"))
    r_p1 = c.post("/v1/chat/completions",
                  json={"model": "path404", "messages": [{"role": "user", "content": "hi"}]}, timeout=30)
    time.sleep(3.0)
    _db_p = json.loads(open(os.path.join(TMP, "db.json"), encoding="utf-8").read())
    _p_fake = "path404" in json.dumps(_db_p.get("model_missing") or {})
    r_p2 = c.post("/v1/chat/completions",
                  json={"model": "path404", "messages": [{"role": "user", "content": "hi"}]}, timeout=30)
    # 客户端面按设计隐藏上游原文(回「渠道不支持该请求或模型」),原文必须留在后台日志里
    _plogs = a.get("/api/logs?n=50").json().get("rows") or []
    _p_logged = any("Endpoint does not exist" in str(x[6])
                    for x in _plogs if isinstance(x, list) and len(x) > 6)
    add("错误分级:泛化 404 不写假负缓存(二次仍打上游,原文留日志)",
        (not _p_fake) and r_p2.status_code == 404 and "所有渠道均不可用" not in r_p2.text and _p_logged,
        "假缓存=%s 首次 st=%s 二次 st=%s 网关自判=%s 上游原文进日志=%s"
        % (_p_fake, r_p1.status_code, r_p2.status_code, "所有渠道均不可用" in r_p2.text, _p_logged))

    # 熔断中的排队条目必须写清原因:线上实测 kimi-k3 熔断时 19 条排队全无原因,
    # 面板上只能看到「排队中」,根本看不出是模型熔断(熔断期间取号整段被跳过)
    _br_txt = _srv._breaker_queue_reason({"fails": 3, "left": 42})
    add("熔断中的排队原因与公开提示",
        ("熔断" in _br_txt) and ("3" in _br_txt) and ("42" in _br_txt)
        and _srv.queue.public_hint(_br_txt) == "模型熔断恢复中",
        "原因=%s;公开提示=%s" % (_br_txt[:34], _srv.queue.public_hint(_br_txt)))

    # 排队原因的刷新不能把存储锁打满:等待者每轮询周期都会调 set_reason,
    # 内容没变就必须直接返回、不落写(几百个等待者 × 每 400ms 一次的写非常可观)
    class _CountingStore:
        def __init__(self, db):
            self.db = db
            self.n = 0

        def load(self):
            return self.db

        def update(self, fn):
            self.n += 1
            fn(self.db)

    _q_orig_store = _srv.queue.STORE
    try:
        _fake = _CountingStore({"queue": [{"id": "q_t1", "t": time.time(), "reason": "冷却 3 / 共 6"}]})
        _srv.queue.STORE = _fake
        _srv.queue.set_reason("q_t1", "冷却 3 / 共 6")   # 内容相同 → 不该写
        _n_same = _fake.n
        _srv.queue.set_reason("q_t1", "账户并发 2 / 共 6")  # 变了 → 写一次
        _n_diff = _fake.n
        _kept = _fake.db["queue"][0]["reason"]
    finally:
        _srv.queue.STORE = _q_orig_store
    add("排队原因:内容没变不写存储(避免打满存储锁)",
        _n_same == 0 and _n_diff == 1 and _kept == "账户并发 2 / 共 6",
        "相同内容写入=%d 变化后写入=%d 结果=%s" % (_n_same, _n_diff, _kept))

    first_bad = []

    async def w(_):
        async with httpx.AsyncClient(
            base_url="http://127.0.0.1:18213", timeout=60, headers={"Authorization": "Bearer " + toks[0]["t"]}
        ) as cl:
            ok = 0
            for _i in range(3):
                rr = await cl.post(
                    "/v1/chat/completions",
                    json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
                )
                if rr.status_code == 200:
                    ok += 1
                elif len(first_bad) < 2:
                    first_bad.append("%s %s" % (rr.status_code, rr.text[:150]))
            return ok

    async def main():
        return await asyncio.gather(*[w(i) for i in range(20)])

    total_ok = sum(asyncio.run(main()))
    if total_ok != 60:
        kk = a.get("/api/keys").json()["rows"]
        first_bad.append(
            " | 账号状态: "
            + "; ".join(
                "%s ban=%s daily=%s fails=%s"
                % (
                    k["email"][:3],
                    k.get("ban_reason"),
                    sum((v or {}).get("requests", 0) for v in (k.get("daily") or {}).values()),
                    k.get("consecutive_failures"),
                )
                for k in kk
            )
        )
    add("20并发x3=60请求", total_ok == 60, "%d/60 %s" % (total_ok, first_bad))

    # ---- 2026-09-19 兼容性与稳定性修复批次 ----

    # 1) 流式成功请求不得触发「兜底释放」：return StreamingResponse 会走 _proxy 的
    #    finally，此前 hold 未置空导致每个流式请求都被双重释放 + 记一条假 500 日志，
    #    账号在流式传输期间就被提前放回号池（配合并发限制会触发上游 429）。
    a.post("/api/logs/clear", json={})
    c.post("/v1/chat/completions", json={"model": "mock-model", "stream": True,
                                          "messages": [{"role": "user", "content": "hi"}]})
    logs_after = a.get("/api/logs?n=100").json()
    rows_l = logs_after.get("logs") if isinstance(logs_after, dict) else logs_after
    double_rel = [row for row in (rows_l or []) if isinstance(row, list) and len(row) > 6
                  and "兜底释放" in str(row[6])]
    add("流式成功不双重释放账号", not double_rel,
        ("发现 %d 条假「兜底释放」日志" % len(double_rel)) if double_rel else "无兜底释放日志")

    # 2) CORS 头必须覆盖所有 /v1 响应(含 401 错误响应):浏览器直连客户端读不到
    #    无 CORS 头的响应,连报错都只会显示成 CORS 错误。
    r401 = httpx.get("http://127.0.0.1:18213/v1/models")
    add("错误响应也带 CORS 头", r401.headers.get("access-control-allow-origin") == "*",
        "401 响应 ACAO=%s" % r401.headers.get("access-control-allow-origin"))

    # 3) 预检回显客户端申请的头(浏览器端 OpenAI/Anthropic SDK 带 x-stainless-*/anthropic-beta)
    ro = httpx.options(
        "http://127.0.0.1:18213/v1/chat/completions",
        headers={
            "Origin": "https://example.com",
            "Access-Control-Request-Method": "POST",
            "Access-Control-Request-Headers": "authorization,content-type,anthropic-beta,x-stainless-lang",
        },
    )
    ah = (ro.headers.get("access-control-allow-headers") or "").lower()
    add("预检回显自定义头", "anthropic-beta" in ah and "x-stainless-lang" in ah,
        "allow-headers=%s" % ah[:80])

    # 4) BOM 容忍:部分 Windows 客户端发的 JSON 带 UTF-8 BOM,json.loads 会直接失败
    rb = c.post(
        "/v1/chat/completions",
        content=b'\xef\xbb\xbf{"model":"mock-model","messages":[{"role":"user","content":"hi"}]}',
        headers={"content-type": "application/json"},
    )
    add("BOM JSON 可解析", rb.status_code == 200, "st=%s" % rb.status_code)

    # 5) 尾斜杠兼容:有些客户端拼出 /v1/chat/completions/,网关必须直接处理
    rs = c.post(
        "/v1/chat/completions/",
        json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
    )
    add("尾斜杠路由兼容", rs.status_code == 200, "st=%s" % rs.status_code)

    # 6) Anthropic 流事件的 data 必须带 type 字段;上游流无 [DONE] 也要补终端事件
    ev_types = {}
    with c.stream("POST", "/v1/messages",
                  json={"model": "nodone", "max_tokens": 32, "stream": True,
                        "messages": [{"role": "user", "content": "hi"}]}) as rm:
        ok_st = rm.status_code == 200
        raw = rm.read().decode("utf-8", "replace")
    cur_ev, cur_data = "", ""
    for line in raw.split("\n"):
        if line.startswith("event: "):
            cur_ev = line[7:].strip()
        elif line.startswith("data: ") and cur_ev:
            try:
                j = json.loads(line[6:])
            except Exception:
                j = {}
            ev_types.setdefault(cur_ev, []).append(j)
            cur_ev = ""
    md = (ev_types.get("message_delta") or [{}])[0]
    ms = (ev_types.get("message_stop") or [{}])[0]
    has_types = "type" in md and "type" in ms
    # 全量校验:每个 Anthropic 事件 data 的 type 必须与事件名一致(SDK 按 data.type 分发)
    ant_bad = [ev for ev, lst in ev_types.items() for j in lst if not isinstance(j, dict) or j.get("type") != ev]
    add("Anthropic 流事件带 type 且无 [DONE] 也能收尾",
        ok_st and has_types and ev_types.get("content_block_delta") and not ant_bad,
        "st=%s message_delta.type=%s message_stop.type=%s 事件=%s type不符=%s"
        % (rm.status_code, md.get("type"), ms.get("type"), sorted(ev_types), ant_bad[:3]))

    # 7) Responses 流无 [DONE] 也必须有 response.completed 终端事件;
    #    且每个事件的 data 必须带 "type" 判别字段(OpenAI SDK 按 type 构造事件,
    #    只发 SSE event 行不够 —— 曾经全部事件都缺 type,SDK 直接构造失败)
    ev2 = []
    type_bad = []
    with c.stream("POST", "/v1/responses",
                  json={"model": "nodone", "stream": True,
                        "input": "hi"}) as rr2:
        ok_st2 = rr2.status_code == 200
        raw2 = rr2.read().decode("utf-8", "replace")
    cur_ev, cur_data = "", ""
    for line in raw2.split("\n"):
        if line.startswith("event: "):
            cur_ev = line[7:].strip()
        elif line.startswith("data: ") and cur_ev:
            cur_data = line[6:].strip()
            ev2.append(cur_ev)
            try:
                j = json.loads(cur_data)
            except Exception:
                j = None
            if not isinstance(j, dict) or j.get("type") != cur_ev:
                type_bad.append("%s->%r" % (cur_ev, j if isinstance(j, dict) else j))
            cur_ev = ""
    add("Responses 流无 [DONE] 也收尾", ok_st2 and "response.completed" in ev2,
        "st=%s 事件=%s" % (rr2.status_code, sorted(set(ev2))[:6]))
    add("Responses 事件 data 带 type 判别字段", ok_st2 and not type_bad,
        "缺失/不符 %d 条%s" % (len(type_bad), (" 如 " + ";".join(type_bad[:3])) if type_bad else ""))

    # 8) 概览 RPM 统计不为 0(buckets 是时间戳列表,以前按 dict+小时键统计恒为 0)
    ov = a.get("/api/overview").json()
    add("概览 RPM 统计正常", ov.get("rpm", -1) >= 0, "rpm=%s" % ov.get("rpm"))

    # 登录限流：以前只过滤时间戳、从不记录本次尝试，len() 恒为 0 → 限流完全失效，
    # 口令可以无限暴力尝试。这里用错口令连续打满配额，必须被 429 挡住。
    # 本用例会把这个来源 IP 锁 5 分钟，所以放在最后（成功登录会清零配额，用错口令不会）。
    lcodes = []
    for _ in range(12):
        rr = httpx.post(
            "http://127.0.0.1:18213/api/login", json={"username": ADMIN_USER, "password": "definitely-wrong"}
        )
        lcodes.append(rr.status_code)
    add("登录尝试限流生效", 401 in lcodes and 429 in lcodes, "末尾状态码=%s" % lcodes[-4:])

    print("\n===== RESULTS =====")
    all_ok = True
    for n, ok, d in results:
        print(("PASS" if ok else "FAIL"), n, ("| " + d) if d else "")
        if not ok:
            all_ok = False
    print("ALL PASS" if all_ok else "FAILURES")
finally:
    # 收尾不能无限等:子进程若卡住(或 TerminateProcess 没生效),整个套件就永远挂着,
    # 外面只会看到超时,连一行结果都没有。terminate → 限时等 → kill 兜底。
    for _proc in (gw, mk):
        try:
            _proc.terminate()
            _proc.wait(timeout=10)
        except Exception:
            try:
                _proc.kill()
                _proc.wait(timeout=5)
            except Exception:
                pass
    try:
        shutil.copy(os.path.join(TMP, "gateway-stderr.log"),
                    os.path.join(ROOT, "rust", "tests-blackbox", "last-gateway-stderr.log"))
    except Exception:
        pass
    shutil.rmtree(TMP, ignore_errors=True)
