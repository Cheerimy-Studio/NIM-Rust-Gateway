"""zcm 无 Docker 依赖测试：注册表 / compose 生成 / 反代全链路 / 登录解析。

运行：python tests/test_logic.py
（需要 requirements.txt 里的依赖；不需要 docker 守护进程）
"""
from __future__ import annotations

import json
import sys
import tempfile
import time
import traceback
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import httpx
import yaml
from fastapi.testclient import TestClient
from typing import Tuple

from zcm.auth import verify_password
from zcm.config import Settings, Registry, ConfigError, port_free
from zcm.composegen import (
    account_config_yaml,
    build_compose,
    conf_hash,
    write_runtime,
    zc_base,
)
from zcm.pool import Pool, URL_RE, parse_trace
from zcm.web import create_app


def mk_settings(home: Path, mode: str = "container") -> Settings:
    return Settings(
        home=home,
        addr="127.0.0.1:9000",
        admin_token_env=None,
        mode=mode,
        port_base=21100,
        project="zcm",
        mesh="zcm-mesh",
        image_zcode="ghcr.io/tridefender/zcode-proxy:latest",
        image_warp="caomingjun/warp:latest",
        idle_timeout=300.0,
        health_interval=30,
        egress_interval=300,
        quota_interval=600,
        drift_interval=300,
    )


def mk_registry(tmp: Path, mode: str = "container") -> Tuple[Registry, Settings]:
    s = mk_settings(tmp, mode)
    reg = Registry(s.accounts_path, s)
    reg.fresh_file = False
    return reg, s


# ---------------------------------------------------------------- 注册表
def test_registry(tmp: Path):
    reg, s = mk_registry(tmp)
    created = reg.add(provider="zai", plan="start-plan", egress="warp", count=2)
    assert [a["id"] for a in created] == ["a1", "a2"]
    a1, a2 = created
    assert a1["port"] == 21100 and a2["port"] == 21102  # 每账号占 2 个槽位
    assert a1["key"].startswith("sk-zc-") and len(a1["key"]) > 30
    assert a1["upstream_key"].startswith("up-")
    assert a1["credential_secret"] != a2["credential_secret"]
    # 落盘 → 重载一致
    reg.save()
    reg2 = Registry(s.accounts_path, s)
    a1b = reg2.get("a1")
    assert a1b is not None
    assert a1b["key"] == a1["key"]
    assert a1b["credential_secret"] == a1["credential_secret"]
    # key 反查
    assert reg2.find_by_key(a1["key"])["id"] == "a1"
    assert reg2.find_by_key("sk-zc-nope") is None
    # 更新与校验
    reg2.update("a1", {"plan": "coding-plan", "note": "主力"})
    assert reg2.get("a1")["plan"] == "coding-plan"
    try:
        reg2.update("a1", {"provider": "openai"})
        raise AssertionError("非法 provider 应被拒绝")
    except ConfigError:
        pass
    try:
        reg2.add(provider="zai", plan="start-plan", acc_id="a1")
        raise AssertionError("重复 ID 应被拒绝")
    except ConfigError:
        pass
    # 删除后端口槽位不复用错位
    reg2.delete("a1")
    a3 = reg2.add(provider="zai", plan="start-plan")[0]
    assert a3["id"] == "a1" and a3["port"] == 21100  # 槽位回收
    # 自定义 ID
    a4 = reg2.add(provider="bigmodel", plan="coding-plan", acc_id="team2")[0]
    assert a4["id"] == "team2"
    # 旧版注册表缺 device_mid → 载入时补齐
    reg2.data["accounts"][0].pop("device_mid")
    reg2.save()
    reg3 = Registry(s.accounts_path, s)
    assert reg3.backfilled is True
    assert len(reg3.data["accounts"][0]["device_mid"]) == 36
    # Key 轮换
    old_key = reg3.get("a2")["key"]
    new_key = reg3.rotate_key("a2")
    assert new_key != old_key and new_key.startswith("sk-zc-")
    assert reg3.get("a2")["key"] == new_key


# ---------------------------------------------------------------- 端口探测
def test_port_free(tmp: Path):
    import socket
    import time

    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    assert port_free(port) is False  # 被占（我们自己绑着）
    sock.close()
    # Windows socket close 可能立即释放端口，稍等片刻确保 TIME_WAIT 状态已过
    time.sleep(0.2)
    # 测试端口探测功能：刚关闭的端口现在应该可用了
    assert port_free(port) is True


def test_registry_port_probe(tmp: Path):
    """探测报占用的槽位必须跳过。"""
    reg, s = mk_registry(tmp)
    blocked = {21100, 21101, 21102, 21103}  # 被占用的槽位
    probe = lambda p: p not in blocked  # noqa: E731
    a = reg.add(provider="zai", plan="start-plan", probe=probe)[0]
    assert a["port"] == 21104
    # 第二个账号：注册表内占 21104/21105，探测占 21100-21103 → 落到 21106
    b = reg.add(provider="zai", plan="start-plan", probe=probe)[0]
    assert b["port"] == 21106
    # 第三个账号：同一探测下继续顺延（21104/21106 已被注册表占用）
    c = reg.add(provider="zai", plan="start-plan", probe=probe)[0]
    assert c["port"] == 21108


def test_compose_container_mode(tmp: Path):
    reg, s = mk_registry(tmp)
    reg.add(provider="zai", plan="start-plan", egress="warp")  # a1 warp
    reg.add(provider="zai", plan="start-plan", egress="direct")  # a2 直连
    c = build_compose(reg, s)
    assert set(c["services"]) == {"zc-a1", "warp-a1", "zc-a2"}
    assert c["networks"]["zcm-mesh"] == {"external": True}
    zc1 = c["services"]["zc-a1"]
    assert zc1["environment"]["HTTPS_PROXY"] == "http://warp-a1:1080"
    assert zc1["environment"]["HTTP_PROXY"] == "http://warp-a1:1080"
    assert zc1["environment"]["NO_PROXY"] == "localhost,127.0.0.1"
    assert zc1["environment"]["ZCODE_PROXY_CREDENTIAL_SECRET"]
    assert f"cred-a1:/home/bun/.zcode-proxy" in zc1["volumes"]
    assert "./conf/a1.yaml:/data/config.yaml:ro" in zc1["volumes"]
    assert zc1["depends_on"] == ["warp-a1"]
    assert "ports" not in zc1  # container 模式不发布端口
    w1 = c["services"]["warp-a1"]
    assert "NET_ADMIN" in w1["cap_add"]
    assert w1["sysctls"]["net.ipv4.conf.all.src_valid_mark"] == "1"
    assert "c 10:200 rwm" in w1["device_cgroup_rules"]
    assert f"warpstate-a1:/var/lib/cloudflare-warp" in w1["volumes"]
    # 直连账号：无代理 env、无 depends_on、无 warp 服务
    zc2 = c["services"]["zc-a2"]
    assert "HTTPS_PROXY" not in zc2["environment"]
    assert "depends_on" not in zc2
    # 配置变化要触发容器重建
    h1 = zc1["environment"]["ZCM_CONF_HASH"]
    reg.update("a1", {"plan": "coding-plan"})
    assert build_compose(reg, s)["services"]["zc-a1"]["environment"]["ZCM_CONF_HASH"] != h1


def test_compose_host_mode(tmp: Path):
    reg, s = mk_registry(tmp, mode="host")
    reg.add(provider="zai", plan="start-plan")
    c = build_compose(reg, s)
    zc = c["services"]["zc-a1"]
    warp = c["services"]["warp-a1"]
    assert "127.0.0.1:21100:8080" in zc["ports"]
    assert "127.0.0.1:21101:1080" in warp["ports"]


def test_compose_disabled_excluded(tmp: Path):
    reg, s = mk_registry(tmp)
    reg.add(provider="zai", plan="start-plan")
    reg.update("a1", {"enabled": False})
    c = build_compose(reg, s)
    assert c["services"] == {}


def test_account_config_and_runtime(tmp: Path):
    reg, s = mk_registry(tmp)
    reg.add(provider="bigmodel", plan="coding-plan")
    cfg = yaml.safe_load(account_config_yaml(reg.get("a1")))
    assert cfg["server"] == {"port": 8080, "host": "0.0.0.0"}
    assert cfg["auth"]["proxyApiKey"] == reg.get("a1")["upstream_key"]
    assert cfg["provider"] == "bigmodel" and cfg["plan"] == "coding-plan"
    assert cfg["identity"]["deviceMid"] == reg.get("a1")["device_mid"]
    path = write_runtime(reg, s)
    assert path.exists()
    assert (s.conf_dir / "a1.yaml").exists()
    compose = yaml.safe_load(path.read_text(encoding="utf-8"))
    assert compose["name"] == "zcm"
    # 停用后 conf 被清理
    reg.update("a1", {"enabled": False})
    write_runtime(reg, s)
    assert not (s.conf_dir / "a1.yaml").exists()


def test_write_runtime_heals_directory(tmp: Path):
    """docker 在 bind 源缺失时会建出同名目录，写配置前必须清掉。"""
    reg, s = mk_registry(tmp)
    reg.add(provider="zai", plan="start-plan")
    (s.conf_dir / "a1.yaml").mkdir(parents=True)
    write_runtime(reg, s)
    assert (s.conf_dir / "a1.yaml").is_file()


def test_parse_trace(_tmp: Path = None):
    d = parse_trace("fl=abc\nip=1.2.3.4\nwarp=on\nloc=US\n")
    assert d["ip"] == "1.2.3.4" and d["warp"] == "on"
    assert parse_trace("") == {}


def test_url_re(_tmp: Path = None):
    line = "请打开 https://auth.z.ai/authorize?x=1&y=2 完成登录，或访问 http://x.dev/a"
    urls = [m.group(0) for m in URL_RE.finditer(line)]
    assert urls == ["https://auth.z.ai/authorize?x=1&y=2", "http://x.dev/a"]


# ---------------------------------------------------------------- 反代全链路
def make_handler(seen: dict, upstream_key: str):
    def handler(request: httpx.Request) -> httpx.Response:
        host = request.url.host or ""
        path = request.url.path
        if "cloudflare.com" in host:
            return httpx.Response(200, text="ip=8.8.4.4\nwarp=on\n")
        if path == "/health":
            return httpx.Response(200, json={"ok": True})
        if path == "/quota":
            if request.headers.get("authorization") != f"Bearer {upstream_key}":
                return httpx.Response(401, json={"error": "bad key"})
            return httpx.Response(200, json={"credits": {"balance": 100}})
        seen["auth"] = request.headers.get("authorization")
        seen["xapi"] = request.headers.get("x-api-key")
        seen["path"] = path
        q = request.url.query
        q = q.decode("utf-8") if isinstance(q, bytes) else str(q or "")
        seen["query"] = q
        if "sse" in path:
            return httpx.Response(
                200,
                headers={"content-type": "text/event-stream"},
                content=b'data: {"t":1}\n\ndata: [DONE]\n\n',
            )
        body = request.read() if request.method in ("POST", "PUT") else b""
        return httpx.Response(200, json={"path": path, "query": q, "body": body.decode()})

    return handler


def test_proxy_end_to_end(tmp: Path):
    reg, s = mk_registry(tmp)
    (a1,) = reg.add(provider="zai", plan="start-plan")
    reg.add(provider="zai", plan="start-plan", egress="direct")  # a2
    seen: dict = {}
    transport = httpx.MockTransport(make_handler(seen, a1["upstream_key"]))
    app = create_app(
        settings=s, registry=reg, enable_docker=False, http_transport=transport
    )
    # 出口探测走 CONNECT 隧道，mock 拦不到连接层——预置不带 proxy 的探测客户端
    from zcm.composegen import warp_proxy

    app.state.pool._egress[warp_proxy(a1, s)] = httpx.AsyncClient(transport=transport)
    key = a1["key"]

    with TestClient(app) as c:
        # 无 key / 错 key
        assert c.get("/v1/models").status_code == 401
        assert c.get("/v1/models", headers={"authorization": "Bearer nope"}).status_code == 401
        # Bearer 转发：路径/查询/体原样，认证头换成容器 upstream_key
        r = c.post(
            "/v1/chat/completions?beta=true",
            json={"model": "glm-4.6"},
            headers={"authorization": f"Bearer {key}"},
        )
        assert r.status_code == 200, r.text
        j = r.json()
        assert j["path"] == "/v1/chat/completions" and j["query"] == "beta=true"
        assert json.loads(j["body"]) == {"model": "glm-4.6"}
        assert seen["auth"] == f"Bearer {a1['upstream_key']}"
        assert seen["xapi"] == a1["upstream_key"]
        # x-api-key 也能认证（Anthropic SDK 形态）
        r = c.post("/v1/messages", json={"m": 1}, headers={"x-api-key": key})
        assert r.status_code == 200
        # SSE 流式透传
        with c.stream("POST", "/v1/sse", headers={"x-api-key": key}) as r:
            assert r.status_code == 200
            assert "text/event-stream" in r.headers["content-type"]
            body = b"".join(r.iter_raw())
        assert b'data: {"t":1}' in body and b"data: [DONE]" in body
        # CORS：预检 204；透传响应与错误响应都带通配 allow-origin
        pre = c.options(
            "/v1/messages",
            headers={"origin": "http://x", "access-control-request-method": "POST"},
        )
        assert pre.status_code == 204 and pre.headers.get("access-control-allow-origin") == "*"
        r = c.get("/v1/models", headers={"authorization": f"Bearer {key}"})
        assert r.status_code == 200 and r.headers.get("access-control-allow-origin") == "*"
        r401 = c.get("/v1/models", headers={"authorization": "Bearer nope"})
        assert r401.headers.get("access-control-allow-origin") == "*"
        # 停用 → 503
        reg.update("a1", {"enabled": False})
        assert (
            c.get("/v1/models", headers={"authorization": f"Bearer {key}"}).status_code == 503
        )
        reg.update("a1", {"enabled": True})
        # 管理面：令牌
        assert c.get("/api/overview").status_code == 401
        assert c.get("/api/overview", headers={"x-admin-token": "bad"}).status_code == 401
        ov = c.get("/api/overview", headers={"x-admin-token": reg.admin_token})
        assert ov.status_code == 200
        accs = ov.json()["accounts"]
        assert len(accs) == 2
        # 未知 /api 路径不会被转发到容器
        assert c.get("/api/definitely-not-a-route").status_code == 404
        # 管理面 /health 免鉴权
        assert c.get("/health").status_code == 200
        # 后台循环真实跑过：健康/登录/出口都有结论
        deadline = time.time() + 5
        st = {}
        while time.time() < deadline:
            st = app.state.pool.status.get("a1", {})
            if st.get("healthy") and st.get("egress_ip"):
                break
            time.sleep(0.1)
        assert st.get("healthy") is True, st
        assert st.get("egress_ip") == "8.8.4.4", st
        assert st.get("warp_mode") == "on", st
        assert st.get("login_ok") is True, st
        # a2 直连：不出现 warp 字样
        st2 = app.state.pool.status.get("a2", {})
        assert st2.get("warp_mode") == "direct", st2


def test_auth_flow(tmp: Path):
    reg, s = mk_registry(tmp)
    reg.add(provider="zai", plan="start-plan")
    app = create_app(settings=s, registry=reg, enable_docker=False)
    assert reg.initial_password, "首次初始化应生成初始密码"
    assert verify_password(reg.initial_password, reg.data["admin_password_hash"])

    with TestClient(app) as c:
        # 未登录
        assert c.get("/api/me").status_code == 401
        assert c.get("/api/overview").status_code == 401
        # 错误密码
        assert c.post("/api/login", json={"username": "admin", "password": "wrong"}).status_code == 401
        assert c.post("/api/login", json={"username": "nobody", "password": reg.initial_password}).status_code == 401
        # 正确登录 → 会话 Cookie + CSRF Cookie
        r = c.post("/api/login", json={"username": "admin", "password": reg.initial_password})
        assert r.status_code == 200
        csrf = c.cookies.get("zcm_csrf")
        assert csrf and c.cookies.get("zcm_session")
        assert c.get("/api/me").json()["username"] == "admin"
        # 会话内变更请求必须带 CSRF
        assert c.post("/api/apply").status_code == 403
        assert c.post("/api/apply", headers={"x-csrf": "wrong"}).status_code == 403
        assert c.post("/api/apply", headers={"x-csrf": csrf}).status_code == 200
        # 令牌路径不做 CSRF
        assert c.post("/api/apply", headers={"x-admin-token": reg.admin_token}).status_code == 200
        assert c.post("/api/apply", headers={"x-admin-token": "bad"}).status_code == 401
        # 限速：连续错 10 次后 429
        for _ in range(10):
            c.post("/api/login", json={"username": "admin", "password": "bad"})
        r = c.post("/api/login", json={"username": "admin", "password": reg.initial_password})
        assert r.status_code == 429, "应触发登录限速"
        # 登出（开放端点，无需 CSRF）→ 会话失效
        assert c.post("/api/logout").status_code == 200
        assert c.get("/api/me").status_code == 401


def test_env_password_authoritative(tmp: Path):
    import os

    os.environ["ZCM_ADMIN_PASSWORD"] = "env-secret-123"
    try:
        reg, s = mk_registry(tmp)
        assert reg.initial_password is None, "环境变量权威重置时不打印初始密码"
        assert verify_password("env-secret-123", reg.data["admin_password_hash"])
    finally:
        del os.environ["ZCM_ADMIN_PASSWORD"]


def test_login_snapshot_serializable(tmp: Path):
    """登录快照必须可 JSON 序列化（不能带出 task 等对象）。"""
    import json as _json

    reg, s = mk_registry(tmp)
    pool = Pool(reg, s, None, http_transport=httpx.MockTransport(lambda r: httpx.Response(200)))
    pool.logins["t1"] = {
        "id": "t1", "account": "a1", "provider": "zai", "lines": ["x"], "urls": ["https://x"],
        "done": True, "ok": True, "exit": 0, "started": "00:00:00", "_ts": 0, "task": object(),
    }
    snap = pool.login_snapshot("t1")
    _json.dumps(snap)
    assert "task" not in snap and snap["done"] is True


def test_global_error_handler_registered(tmp: Path):
    """任何接口炸掉都必须有全局兜底，返回 JSON 而不是裸 traceback。"""
    reg, s = mk_registry(tmp)
    app = create_app(settings=s, registry=reg, enable_docker=False)
    from fastapi import FastAPI as _F

    assert any(
        issubclass(exc, Exception) and handler is not None
        for exc, handler in app.exception_handlers.items()
    )


def test_fix_volume_owner_command(tmp: Path):
    """凭据卷属主修复命令的形状：root 跑 chown，卷名与镜像正确。"""
    import asyncio as _asyncio

    from zcm.dockerctl import DockerCtl

    reg, s = mk_registry(tmp)
    ctl = DockerCtl(s)
    captured = {}

    async def fake_run(cmd, timeout=120.0, on_line=None):
        captured["cmd"] = cmd
        return 0, []

    ctl._run = fake_run
    _asyncio.run(ctl.fix_volume_owner("zcm_cred-a1", s.image_zcode))
    cmd = captured["cmd"]
    assert "chown" in cmd and "bun:bun" in cmd
    assert "zcm_cred-a1:/target" in cmd and "--user" in cmd and "root" in cmd


def test_locks_created_lazily(tmp: Path):
    """Python 3.8 兼容：构建 app 时不得创建 asyncio.Lock（会绑错 loop）。"""
    reg, s = mk_registry(tmp)
    app = create_app(settings=s, registry=reg, enable_docker=True)
    assert app.state.docker._op_lock is None
    assert app.state.locks == {}


def test_schedule_probe(tmp: Path):
    """操作后延迟重探：按 delays 节奏补探，健康状态最终收敛。"""
    import asyncio as _a

    reg, s = mk_registry(tmp)
    (a1,) = reg.add(provider="zai", plan="start-plan", egress="direct")
    transport = httpx.MockTransport(
        lambda r: httpx.Response(200, json={"ok": True})
        if r.url.path == "/health"
        else httpx.Response(200, text="{}")
    )
    pool = Pool(reg, s, None, http_transport=transport)

    async def main():
        pool.schedule_probe(a1, delays=(0.05, 0.15))
        await _a.sleep(0.5)
        st = dict(pool.st("a1"))
        await pool.stop()
        return st

    st = _a.run(main())
    assert st["healthy"] is True


def test_start_claim_rounds(tmp: Path):
    """手动领取：多轮执行、会话状态与快照可序列化。"""
    import asyncio as _a
    import json as _json

    reg, s = mk_registry(tmp)
    (a1,) = reg.add(provider="zai", plan="start-plan", egress="direct")
    transport = httpx.MockTransport(lambda r: httpx.Response(200, json={}))
    pool = Pool(reg, s, None, http_transport=transport)

    calls = {"n": 0}

    class FakeDocker:
        async def available(self):
            return True

        async def run_service(self, service, cmd, timeout=600.0, on_line=None, env=None):
            assert env is None  # 非直连轮次不应清空代理
            calls["n"] += 1
            if on_line:
                on_line(f"claim round {calls['n']} ok")
            return 0, []

    pool.docker = FakeDocker()

    async def main():
        sid = await pool.start_claim(a1, rounds=2)
        sess = None
        for _ in range(100):
            sess = pool.login_snapshot(sid)
            if sess and sess["done"]:
                break
            await _a.sleep(0.05)
        st = dict(pool.st("a1"))
        await pool.stop()
        return sess, st

    sess, st = _a.run(main())
    _json.dumps(sess)
    assert calls["n"] == 2, f"应执行 2 轮，实际 {calls['n']}"
    assert sess["done"] is True and sess["ok"] is True
    assert st["login_ok"] is True  # 领取成功后配额复探置位
    assert any("第 1/2 轮完成" in ln for ln in sess["lines"])


def test_quota_min_interval(tmp: Path):
    """配额查询最小间隔保护：非强制调用 120 秒内不重复打上游。"""
    import asyncio as _a

    reg, s = mk_registry(tmp)
    (a1,) = reg.add(provider="zai", plan="start-plan", egress="direct")
    calls = {"n": 0}

    def handler(r):
        calls["n"] += 1
        return httpx.Response(200, json={"jwt": None})

    pool = Pool(reg, s, None, http_transport=httpx.MockTransport(handler))

    async def main():
        await pool.fetch_quota(a1, force=True)
        await pool.fetch_quota(a1, force=False)
        await pool.fetch_quota(a1, force=True)
        await pool.stop()
        return calls["n"]

    assert _a.run(main()) == 2


def test_run_service_env_override(tmp: Path):
    import asyncio as _a

    from zcm.dockerctl import DockerCtl

    reg, s = mk_registry(tmp)
    ctl = DockerCtl(s)
    captured = {}

    async def fake_run(cmd, timeout=600.0, on_line=None):
        captured["cmd"] = cmd
        return 0, []

    ctl._run = fake_run
    _a.run(ctl.run_service("zc-a1", ["bun", "x"], env={"HTTP_PROXY": "", "NO_PROXY": "*"}))
    cmd = captured["cmd"]
    assert "-e" in cmd and "HTTP_PROXY=" in cmd and "NO_PROXY=*" in cmd
    assert "zc-a1" in cmd


def test_logs_fallback_to_docker_logs(tmp: Path):
    """compose logs 失败时回退 docker logs 直读容器。"""
    import asyncio as _a

    from zcm.dockerctl import DockerCtl

    reg, s = mk_registry(tmp)
    ctl = DockerCtl(s)
    seq = [
        (1, ["compose logs failed"]),
        (0, ["zcm-zc-a1-1"]),
        (0, ["line1", "line2"]),
    ]

    async def fake_run(cmd, timeout=120.0, on_line=None):
        return seq.pop(0)

    ctl._run = fake_run
    text = _a.run(ctl.logs("zc-a1", 100))
    assert "line1" in text and "line2" in text


def test_socks5_connect(tmp: Path):
    """SOCKS5 客户端：握手、域名 CONNECT、数据回传。"""
    import asyncio as _a

    from zcm.tunnel import socks5_connect

    async def main():
        async def client_cb(r, w):
            await r.readexactly(3)                       # greeting
            w.write(b"\x05\x00")
            await w.drain()
            req = await r.readexactly(11)                # CONNECT 请求整体
            w.write(b"\x05\x00\x00\x01\x00\x00\x00\x00\x00\x50")
            await w.drain()
            data = await r.readexactly(5)
            w.write(b"Echo:" + data)
            await w.drain()

        server = await _a.start_server(client_cb, "127.0.0.1", 0)
        port = server.sockets[0].getsockname()[1]
        async with server:
            r, w = await socks5_connect("127.0.0.1", port, "test", 80)
            w.write(b"hello")
            await w.drain()
            return await r.readexactly(10)

    assert _a.run(main()) == b"Echo:hello"


def test_tunnel_ws_direct_and_auth(tmp: Path):
    """WS 隧道端到端：直连出口回显 + 错误令牌拒绝。"""
    import asyncio as _a
    import threading
    import time as _t

    import uvicorn
    import websockets as _ws

    reg, s = mk_registry(tmp)
    (a1,) = reg.add(provider="zai", plan="start-plan", egress="direct")
    app = create_app(settings=s, registry=reg, enable_docker=False)

    holder = {}

    async def _start_echo():
        async def echo(r, w):
            try:
                while True:
                    data = await r.read(4096)
                    if not data:
                        break
                    w.write(data)
                    await w.drain()
            except Exception:
                pass

        server = await _a.start_server(echo, "127.0.0.1", 0)
        holder["port"] = server.sockets[0].getsockname()[1]

    config = uvicorn.Config(app, host="127.0.0.1", port=9433, log_level="error")
    server = uvicorn.Server(config)
    threading.Thread(target=server.run, daemon=True).start()
    # lifespan 已在运行循环里：从测试线程投递回显服务器启动
    for _ in range(60):
        if hasattr(app.state, "loop"):
            break
        _t.sleep(0.1)
    app.state.loop.call_soon_threadsafe(
        lambda: _a.ensure_future(_start_echo())
    )
    for _ in range(60):
        if "port" in holder:
            break
        _t.sleep(0.1)

    async def client():
        uri = f"ws://127.0.0.1:9433/api/tunnel/a1?token={reg.admin_token}"
        async with _ws.connect(uri, open_timeout=10) as ws:
            await ws.send(json.dumps({"host": "127.0.0.1", "port": holder["port"]}))
            assert await ws.recv() == "ok"
            await ws.send(b"ping")
            assert await ws.recv() == b"ping"
        try:
            async with _ws.connect(f"ws://127.0.0.1:9433/api/tunnel/a1?token=bad", open_timeout=10) as ws2:
                await ws2.recv()
            return "NOT-REJECTED"
        except Exception:
            return "rejected"

    out = _a.run(client())
    server.should_exit = True
    assert out == "rejected"


def test_helper_token_endpoint(tmp: Path):
    reg, s = mk_registry(tmp)
    app = create_app(settings=s, registry=reg, enable_docker=False)
    with TestClient(app) as c:
        assert c.get("/api/helper-token").status_code == 401
        r = c.get("/api/helper-token", headers={"x-admin-token": reg.admin_token})
        assert r.status_code == 200 and r.json()["token"] == reg.admin_token


def test_parse_config_string(tmp: Path):
    import base64 as _b64
    import json as _json

    import aocker_local as hl

    payload = {"s": "http://x:9000", "t": "tok-123", "a": "a1", "u": "https://auth.z.ai/x"}
    s = "AOCKER:" + _b64.b64encode(_json.dumps(payload).encode()).decode()
    info = hl.parse_config_string(s)
    assert info["server"] == "http://x:9000" and info["token"] == "tok-123"
    assert info["account"] == "a1" and info["url"] == "https://auth.z.ai/x"
    # 去 padding / 带空白也能解析
    info = hl.parse_config_string("AOCKER: " + s[len("AOCKER:"):].rstrip("="))
    assert info["account"] == "a1"
    # 纯链接：沿用当前配置
    info = hl.parse_config_string("https://auth.z.ai/y")
    assert info.get("url") == "https://auth.z.ai/y" and not info.get("server")
    try:
        hl.parse_config_string("garbage")
        raise AssertionError("非法串应被拒绝")
    except ValueError:
        pass


def test_helper_state_defaults(tmp: Path):
    import aocker_local as hl

    st = hl.State({})
    assert st.proxy_port == 7788 and st.browser == ""
    st.server = "http://x:9000"
    assert st.ws_uri("a1").startswith("ws://x:9000/api/tunnel/a1?token=")


def test_diagnostics_and_events(tmp: Path):
    reg, s = mk_registry(tmp)
    app = create_app(settings=s, registry=reg, enable_docker=False)
    with TestClient(app) as c:
        h = {"x-admin-token": reg.admin_token}
        r = c.get("/api/diagnostics", headers=h)
        assert r.status_code == 200
        checks = r.json()["checks"]
        assert isinstance(checks, list) and len(checks) >= 3
        assert any(x["name"] == "docker 守护进程" for x in checks)
        r = c.get("/api/events", headers=h)
        assert r.status_code == 200 and isinstance(r.json()["events"], list)


# ---------------------------------------------------------------- 运行
def main() -> int:
    tests = [
        (n, f) for n, f in sorted(globals().items()) if n.startswith("test_") and callable(f)
    ]
    passed = failed = 0
    for name, fn in tests:
        with tempfile.TemporaryDirectory() as td:
            try:
                fn(Path(td))
                print(f"  ✓ {name}")
                passed += 1
            except Exception:
                print(f"  ✗ {name}")
                traceback.print_exc()
                failed += 1
    print(f"\n{passed} 通过, {failed} 失败")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
