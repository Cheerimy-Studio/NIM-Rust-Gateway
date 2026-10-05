"""FastAPI 组装：/api 管理面、/admin 面板、catch-all 反代。

注册顺序即匹配顺序：管理面在前，反代兜底在后，且反代内排除
api/admin/health 前缀，管理面路径不会漏到容器。
"""
from __future__ import annotations

import asyncio
import hmac
import logging
import shutil
import time
from contextlib import asynccontextmanager
from pathlib import Path

import httpx
from fastapi import Body, Depends, FastAPI, HTTPException, Request, Response
from fastapi.responses import HTMLResponse, JSONResponse, PlainTextResponse
from fastapi.routing import APIRouter

from . import __version__
from .auth import LoginRateLimiter, csrf_token, hash_password, session_cookie, verify_password
from .composegen import write_runtime, zc_base
from .config import ConfigError, Registry, Settings, port_free
from .dockerctl import DockerCtl
from .pool import Pool
from .proxy import UpstreamError, forward
from .tunnel import register_tunnel

log = logging.getLogger("zcm.web")

RESERVED_PREFIXES = ("api", "admin", "health")
COMPOSE_HINT = (
    "docker compose 插件不可用或 docker CLI 过旧，安装 compose 插件后重试："
    "mkdir -p /usr/local/lib/docker/cli-plugins && curl -SL "
    "https://github.com/docker/compose/releases/latest/download/docker-compose-linux-$(uname -m) "
    "-o /usr/local/lib/docker/cli-plugins/docker-compose && chmod +x "
    "/usr/local/lib/docker/cli-plugins/docker-compose；或用包管理器 "
    "apt install docker-compose-plugin / dnf install docker-compose-plugin；"
    "插件要求 docker CLI ≥ 20.10，过旧请先升级 docker。"
)
CORS_HEADERS = {
    "access-control-allow-origin": "*",
    "access-control-allow-methods": "GET, POST, PUT, PATCH, DELETE, OPTIONS, HEAD",
    "access-control-allow-headers": "*",
    "access-control-max-age": "86400",
}
SESSION_COOKIE = "zcm_session"
CSRF_COOKIE = "zcm_csrf"
MUTATING = {"POST", "PUT", "PATCH", "DELETE"}
COOKIE_MAX_AGE = 7 * 86400


def create_app(
    settings: Settings | None = None,
    registry: Registry | None = None,
    docker: DockerCtl | None = None,
    enable_docker: bool = True,
    http_transport: httpx.AsyncBaseTransport | None = None,
) -> FastAPI:
    s = settings or Settings.from_env()
    reg = registry or Registry(s.accounts_path, s)
    ctl = docker
    if ctl is None and enable_docker:
        ctl = DockerCtl(s)
    pool = Pool(reg, s, ctl if enable_docker else None, http_transport=http_transport)
    limiter = LoginRateLimiter()
    locks: dict[str, asyncio.Lock] = {}

    def _lock(name: str) -> asyncio.Lock:
        # 同 DockerCtl.op_lock：必须在使用处（运行中的循环里）惰性创建
        lk = locks.get(name)
        if lk is None:
            lk = asyncio.Lock()
            locks[name] = lk
        return lk

    def audit(msg: str) -> None:
        pool.events.append({"t": time.strftime("%m-%d %H:%M:%S"), "msg": msg})

    async def compose_ok() -> bool:
        if ctl is None:
            return False
        try:
            rc, _ = await ctl._run([ctl.bin, "compose", "version"], timeout=15)
            ok = rc == 0
        except Exception:
            ok = False
        pool.compose_ok = ok
        return ok

    # ---- 通用小工具 -------------------------------------------------------
    async def apply() -> str:
        """注册表 → 重新生成 runtime 文件 → compose 收敛。"""
        async with _lock("apply"):
            # 应用前先记下被目录占位的 conf：up 之后要强制重建对应容器
            healed = [
                p.stem for p in s.conf_dir.glob("*.yaml") if p.is_dir()
            ] if s.conf_dir.exists() else []
            write_runtime(reg, s)
            if not any(a.get("enabled", True) for a in reg.accounts()):
                return "(无启用账号，跳过部署)"
            if ctl is None:
                return "(docker 未启用，仅生成配置文件)"
            if not await ctl.available():
                return "(docker 不可用，仅生成配置文件)"
            if not await compose_ok():
                raise HTTPException(503, COMPOSE_HINT)
            await ctl.ensure_network()
            async with ctl.op_lock:
                rc, out = await ctl.up()
            pool.last_apply = time.time()
            tail = "\n".join(out[-8:])
            if rc != 0:
                raise HTTPException(500, f"compose up 失败：\n{tail}")
            for acc_id in healed:
                # 该账号的配置文件曾被目录占位挂进容器，强制重建挂回真文件
                await ctl.recreate_service(f"zc-{acc_id}")
            return tail

    def need_acc(acc_id: str) -> dict:
        acc = reg.get(acc_id)
        if not acc:
            raise HTTPException(404, f"账号不存在：{acc_id}")
        return acc

    async def need_docker() -> DockerCtl:
        if ctl is None or not await ctl.available():
            raise HTTPException(503, "docker 不可用")
        if not await compose_ok():
            raise HTTPException(503, COMPOSE_HINT)
        return ctl

    def overview() -> dict:
        accounts = []
        for a in reg.accounts():
            st = dict(pool.st(a["id"]))
            accounts.append(
                {
                    "id": a["id"],
                    "key": a["key"],
                    "provider": a["provider"],
                    "plan": a["plan"],
                    "enabled": a.get("enabled", True),
                    "egress": a.get("egress", "warp"),
                    "port": a.get("port"),
                    "note": a.get("note", ""),
                    "created_at": a.get("created_at"),
                    "status": st,
                }
            )
        return {
            "version": __version__,
            "mode": s.mode,
            "project": s.project,
            "mesh": s.mesh,
            "home": str(s.home),
            "accounts": accounts,
        }

    # ---- 管理面鉴权 -------------------------------------------------------
    async def guard(request: Request) -> None:
        """脚本令牌或会话 Cookie；Cookie 路径的变更请求校验 CSRF。"""
        auth = request.headers.get("authorization", "")
        supplied = auth[7:].strip() if auth[:7].lower() == "bearer " else ""
        supplied = supplied or request.headers.get("x-admin-token", "")
        if supplied:
            if not hmac.compare_digest(supplied, reg.admin_token):
                raise HTTPException(401, "管理令牌错误")
            return
        secret = str(reg.data["session_secret"])
        sess = request.cookies.get(SESSION_COOKIE, "")
        if sess and hmac.compare_digest(sess, session_cookie(secret)):
            if request.method.upper() in MUTATING:
                got = request.headers.get("x-csrf", "")
                if not got or not hmac.compare_digest(got, csrf_token(secret)):
                    raise HTTPException(403, "CSRF 校验失败，请刷新页面后重试")
            return
        raise HTTPException(401, "需要登录")

    open_router = APIRouter(prefix="/api")

    @open_router.post("/login")
    async def login(request: Request, payload: dict = Body(default={})):
        ip = request.client.host if request.client else "unknown"
        wait = limiter.retry_after(ip)
        if wait:
            return JSONResponse(
                {"error": {"message": f"尝试过于频繁，请 {wait} 秒后重试"}}, status_code=429
            )
        username = str(payload.get("username") or "")
        password = str(payload.get("password") or "")
        ok_user = username == str(reg.data.get("admin_username") or "admin")
        ok_pass = verify_password(password, str(reg.data.get("admin_password_hash") or ""))
        if not (ok_user and ok_pass):
            limiter.record(ip)
            raise HTTPException(401, "账号或密码错误")
        limiter.clear(ip)
        secret = str(reg.data["session_secret"])
        resp = JSONResponse({"ok": True, "username": reg.data.get("admin_username", "admin")})
        resp.set_cookie(
            SESSION_COOKIE, session_cookie(secret),
            max_age=COOKIE_MAX_AGE, httponly=True, samesite="lax", path="/",
        )
        resp.set_cookie(
            CSRF_COOKIE, csrf_token(secret),
            max_age=COOKIE_MAX_AGE, httponly=False, samesite="lax", path="/",
        )
        return resp

    @open_router.post("/logout")
    async def logout():
        resp = JSONResponse({"ok": True})
        resp.delete_cookie(SESSION_COOKIE, path="/")
        resp.delete_cookie(CSRF_COOKIE, path="/")
        return resp

    router = APIRouter(prefix="/api", dependencies=[Depends(guard)])

    @router.get("/me")
    async def me():
        return {
            "username": reg.data.get("admin_username", "admin"),
            "version": __version__,
            "mode": s.mode,
        }

    @router.post("/password")
    async def change_password(payload: dict = Body(default={})):
        old = str(payload.get("old") or "")
        new = str(payload.get("new") or "")
        if len(new) < 8:
            raise HTTPException(400, "新密码至少 8 位")
        if not verify_password(old, str(reg.data.get("admin_password_hash") or "")):
            raise HTTPException(400, "当前密码错误")
        reg.data["admin_password_hash"] = hash_password(new)
        reg.save()
        audit("修改管理密码")
        return {"ok": True}

    @router.get("/whoami")
    async def whoami():
        docker_ok = bool(ctl is not None and await ctl.available())
        return {
            "version": __version__,
            "mode": s.mode,
            "project": s.project,
            "home": str(s.home),
            "docker": docker_ok,
            "compose": bool(docker_ok and await compose_ok()),
        }

    @router.get("/overview")
    async def get_overview():
        return overview()

    @router.get("/events")
    async def events_list():
        return {"events": list(reversed(list(pool.events)))[:100]}

    @router.get("/helper-token")
    async def helper_token():
        # 面板把管理令牌转交给本机脚本（交给的就是已登录的管理员自己）
        return {"token": reg.admin_token}

    @router.post("/accounts")
    async def add_accounts(payload: dict = Body(default={})):
        # host 模式把端口发布到 127.0.0.1，分配前实测绑定跳过占用者
        probe = port_free if s.mode == "host" else None
        created = reg.add(
            provider=payload.get("provider") or "zai",
            plan=payload.get("plan") or "coding-plan",
            egress=payload.get("egress") or "warp",
            note=str(payload.get("note") or ""),
            count=payload.get("count") or 1,
            acc_id=payload.get("id"),
            probe=probe,
        )
        log.info("新增账号 %s，应用 compose", [a["id"] for a in created])
        tail = await apply()
        for a in created:
            pool.schedule_probe(a, delays=(2.5, 10.0))
        audit("添加账号 " + "、".join(a["id"] for a in created))
        return {"created": [a["id"] for a in created], "accounts": overview()["accounts"], "apply": tail}

    @router.patch("/accounts/{acc_id}")
    async def patch_account(acc_id: str, payload: dict = Body(default={})):
        need_acc(acc_id)
        acc = reg.update(acc_id, payload)
        # 停用时先显式移除容器：禁用后服务不在 compose 文件里，up 无法再清理它
        if payload.get("enabled") is False and ctl is not None and await ctl.available():
            async with ctl.op_lock:
                for svc in (f"zc-{acc_id}", f"warp-{acc_id}"):
                    await ctl.stop_service(svc)
            st = pool.st(acc_id)
            st["container"] = "missing"
            st["healthy"] = None
        tail = await apply()
        if acc.get("enabled", True):
            pool.schedule_probe(acc, delays=(2.5, 10.0))
        audit(f"更新 {acc_id}：{'、'.join(payload.keys()) or '无字段变更'}")
        return {"ok": True, "apply": tail}

    @router.delete("/accounts/{acc_id}")
    async def delete_account(acc_id: str, purge: bool = False):
        acc = need_acc(acc_id)
        # 先显式移除容器（此刻 compose 文件里还有服务定义），
        # 不依赖 up --remove-orphans 的孤儿清理；卷的清理放在容器移除之后，
        # 否则 docker 会因卷仍被挂载而拒绝删除
        removed: list[str] = []
        if ctl is not None and await ctl.available() and await compose_ok():
            svcs = [f"zc-{acc_id}"] + (
                [f"warp-{acc_id}"] if acc.get("egress") == "warp" else []
            )
            async with ctl.op_lock:
                for svc in svcs:
                    await ctl.stop_service(svc)
                    removed.append(svc)
        reg.delete(acc_id)
        pool.status.pop(acc_id, None)
        audit(f"删除账号 {acc_id}" + ("，含数据卷" if purge else ""))
        tail = await apply()
        purged = []
        purge_error = None
        if purge and ctl is not None:
            # 卷清理尽力而为：失败不能推翻“注册表已删除”的结果
            try:
                if await ctl.available():
                    await ctl.remove_volumes(
                        [f"{s.project}_cred-{acc_id}", f"{s.project}_warpstate-{acc_id}"]
                    )
                    purged = ["cred", "warpstate"]
                else:
                    purge_error = "docker 不可用，数据卷未清理"
            except Exception as e:
                purge_error = f"数据卷清理失败：{type(e).__name__}"
        return {
            "ok": True,
            "removed": removed,
            "purged": purged,
            "purge_error": purge_error,
            "apply": tail,
        }

    @router.post("/apply")
    async def apply_now():
        tail = await apply()
        # 容器重建/启动有就绪延迟：按节奏补探，面板很快收敛到真状态
        for a in reg.accounts():
            if a.get("enabled", True):
                pool.schedule_probe(a, delays=(3.0, 12.0))
        return {"ok": True, "apply": tail}

    @router.post("/pull-update")
    async def pull_update():
        d = await need_docker()
        if not any(a.get("enabled", True) for a in reg.accounts()):
            return {"ok": True, "note": "无启用账号，跳过"}
        async with _lock("apply"), d.op_lock:
            write_runtime(reg, s)
            await d.ensure_network()
            rc, out = await d.pull()
            if rc != 0:
                raise HTTPException(500, "compose pull 失败：\n" + "\n".join(out[-8:]))
            rc, out = await d.up()
            pool.last_apply = time.time()
            if rc != 0:
                raise HTTPException(500, "compose up 失败：\n" + "\n".join(out[-8:]))
        audit("拉取镜像更新")
        # 镜像变化会触发容器重建，重建后立即补探状态
        for a in reg.accounts():
            if a.get("enabled", True):
                pool.schedule_probe(a, delays=(3.0, 12.0))
        return {"ok": True}

    @router.get("/accounts/{acc_id}/logs")
    async def account_logs(acc_id: str, tail: int = 300, target: str = "zc"):
        need_acc(acc_id)
        d = await need_docker()
        svc = f"{target}-{acc_id}" if target in ("zc", "warp") else f"zc-{acc_id}"
        try:
            tail_n = max(10, min(int(tail or 300), 2000))
        except (TypeError, ValueError):
            tail_n = 300
        text = await d.logs(svc, tail=tail_n)
        return PlainTextResponse(text)

    @router.post("/accounts/{acc_id}/restart")
    async def account_restart(acc_id: str):
        acc = need_acc(acc_id)
        d = await need_docker()
        # 重启循环中的容器 compose restart 会失败，用 rm -sf + up 确定性重建
        svcs = [f"zc-{acc_id}"] + (
            [f"warp-{acc_id}"] if acc.get("egress") == "warp" else []
        )
        async with d.op_lock:
            for svc in svcs:
                await d.stop_service(svc)
            for svc in svcs:
                rc, out = await d.up_service(svc)
                if rc != 0:
                    raise HTTPException(500, f"{svc} 启动失败：\n" + "\n".join(out[-5:]))
        pool.schedule_probe(acc, delays=(2.5, 8.0))
        audit(f"重启 {acc_id}")
        return {"ok": True}

    @router.post("/accounts/{acc_id}/reroll-warp")
    async def reroll_warp(acc_id: str):
        acc = need_acc(acc_id)
        if acc.get("egress", "warp") != "warp":
            raise HTTPException(400, "该账号不是 warp 出口")
        d = await need_docker()
        async with _lock("apply"), d.op_lock:
            write_runtime(reg, s)
            await d.stop_service(f"warp-{acc_id}")
            # 删注册卷重建，重新注册随机新 IP
            await d.remove_volumes([f"{s.project}_warpstate-{acc_id}"])
            rc, out = await d.up_service(f"warp-{acc_id}")
        if rc != 0:
            raise HTTPException(500, "重建 warp 容器失败：\n" + "\n".join(out[-8:]))
        st = pool.st(acc_id)
        st["egress_ip"] = None
        st["warp_mode"] = None
        # WARP 重注册耗时不定：几轮补探覆盖 12s~70s
        pool.schedule_probe(acc, delays=(12.0, 35.0, 70.0))
        audit(f"重掷 {acc_id} 的 WARP 出口 IP")
        return {"ok": True, "hint": "WARP 重新注册中，出口 IP 稍后自动刷新"}

    @router.post("/accounts/{acc_id}/claim")
    async def account_claim(acc_id: str, payload: dict = Body(default={})):
        acc = need_acc(acc_id)
        await need_docker()
        try:
            rounds = max(1, min(int(payload.get("rounds") or 1), 5))
        except (TypeError, ValueError):
            rounds = 1
        sid = await pool.start_claim(acc, rounds)
        audit(f"{acc_id} 手动领取套餐（{rounds} 轮）")
        audit(f"{acc_id} 手动领取套餐（{rounds} 轮）")
        return {"claim_id": sid}

    @router.post("/accounts/{acc_id}/login")
    async def account_login(acc_id: str, payload: dict = Body(default={})):
        acc = need_acc(acc_id)
        provider = payload.get("provider") or acc["provider"]
        if provider not in ("zai", "bigmodel"):
            raise HTTPException(400, "provider 不合法")
        if ctl is None or not await ctl.available():
            raise HTTPException(503, "docker 不可用，无法在容器内登录")
        sid = await pool.start_login(acc, provider)
        audit(f"{acc_id} 发起 {provider} 登录")
        return {"login_id": sid}

    @router.get("/login/{sid}")
    async def login_status(sid: str):
        snap = pool.login_snapshot(sid)
        if not snap:
            raise HTTPException(404, "登录会话不存在或已过期")
        return snap

    @router.post("/accounts/{acc_id}/logout")
    async def account_logout(acc_id: str):
        acc = need_acc(acc_id)
        await need_docker()
        await pool.logout(acc)
        audit(f"{acc_id} 清除登录凭据")
        return {"ok": True}

    @router.post("/accounts/{acc_id}/probe")
    async def account_probe(acc_id: str):
        acc = need_acc(acc_id)
        await asyncio.gather(
            pool.probe_health(acc), pool.probe_egress(acc), pool.fetch_quota(acc)
        )
        return pool.st(acc_id)

    @router.post("/accounts/{acc_id}/rotate-key")
    async def rotate_key(acc_id: str):
        need_acc(acc_id)
        new_key = reg.rotate_key(acc_id)
        audit(f"轮换 {acc_id} 的访问 Key")
        return {"key": new_key}

    @router.get("/diagnostics")
    async def diagnostics():
        checks: list[dict] = []

        def chk(name: str, ok: bool, detail: str = "") -> None:
            checks.append({"name": name, "ok": bool(ok), "detail": str(detail)[:220]})

        docker_ok = bool(ctl is not None and await ctl.available())
        chk("docker 守护进程", docker_ok, "可达" if docker_ok else "不可达")
        compose_flag = bool(docker_ok and await compose_ok())
        chk("docker compose 插件", compose_flag,
            "可用" if compose_flag else ("未安装，见 README 排障" if docker_ok else "依赖 docker"))
        if docker_ok:
            rc, _o = await ctl._run([ctl.bin, "network", "inspect", s.mesh], timeout=15)
            chk(f"容器网络 {s.mesh}", rc == 0, "存在" if rc == 0 else "不存在（应用配置时自动创建）")
            for label, img in (("zcode 镜像", s.image_zcode), ("warp 镜像", s.image_warp)):
                rci, _oi = await ctl._run([ctl.bin, "image", "inspect", img], timeout=30)
                chk(label, rci == 0, "已拉取" if rci == 0 else "未拉取（首次应用配置时拉取）")
        try:
            du = shutil.disk_usage(s.home)
            pct = int(du.used / du.total * 100)
            chk("磁盘空间", pct < 90, f"已用 {pct}%")
        except Exception as e:
            chk("磁盘空间", True, f"无法检测：{type(e).__name__}")
        try:
            probe_file = s.home / ".diag"
            probe_file.write_text("x", encoding="utf-8")
            probe_file.unlink()
            chk("数据目录可写", True, str(s.home))
        except Exception as e:
            chk("数据目录可写", False, f"{type(e).__name__}")
        if docker_ok:
            states = await ctl.ps_states()
            bad = [f"{k}={v}" for k, v in states.items() if v != "running"]
            chk("账号容器状态", not bad, "全部运行" if not bad else "异常：" + "，".join(bad))
        return {"checks": checks}

    app = FastAPI(title="Aocker", version=__version__, lifespan=_lifespan(pool, reg, s, ctl, enable_docker, apply))
    app.state.pool = pool
    app.state.registry = reg
    app.state.settings = s
    app.state.docker = ctl
    app.state.locks = locks
    register_tunnel(app, reg, s)

    @app.exception_handler(ConfigError)
    async def _config_error(request: Request, exc: ConfigError):
        # 校验类错误是调用方的输入问题，返回 400 与可读原因
        return JSONResponse(
            {"error": {"message": str(exc), "type": "invalid_request"}},
            status_code=400,
        )

    @app.exception_handler(Exception)
    async def _unhandled(request: Request, exc: Exception):
        # 全局兜底：未处理异常（含响应序列化失败）统一返回干净 JSON，
        # 完整堆栈进日志，不再向客户端吐半截 traceback
        log.exception("未处理异常 %s %s", request.method, request.url.path)
        return JSONResponse(
            {
                "error": {
                    "message": f"服务内部错误：{type(exc).__name__}: {str(exc)[:200]}",
                    "type": "internal_error",
                }
            },
            status_code=500,
        )
    app.include_router(open_router)
    app.include_router(router)

    # ---- 面板与探活 -------------------------------------------------------
    @app.get("/health", include_in_schema=False)
    async def manager_health():
        return {"ok": True, "version": __version__}

    @app.get("/admin", include_in_schema=False)
    @app.get("/admin/", include_in_schema=False)
    async def admin_page():
        html = (Path(__file__).parent / "panel.html").read_text(encoding="utf-8")
        return HTMLResponse(html)

    # ---- 反代兜底 ---------------------------------------------------------
    @app.api_route(
        "/{p:path}",
        methods=["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "HEAD"],
        include_in_schema=False,
    )
    async def proxy_route(p: str, request: Request):
        # CORS 预检：浏览器客户端不携带密钥，先答资格再走鉴权
        if request.method == "OPTIONS" and request.headers.get("access-control-request-method"):
            return Response(status_code=204, headers=CORS_HEADERS)
        if p in RESERVED_PREFIXES or p.startswith(tuple(x + "/" for x in RESERVED_PREFIXES)):
            return JSONResponse(
                {"error": {"message": "not found", "type": "not_found"}},
                status_code=404,
                headers=CORS_HEADERS,
            )

        auth = request.headers.get("authorization", "")
        key = auth[7:].strip() if auth[:7].lower() == "bearer " else ""
        key = key or request.headers.get("x-api-key", "")
        acc = reg.find_by_key(key)
        if not acc:
            return JSONResponse(
                {
                    "error": {
                        "message": "无效的 API Key（每个 Key 绑定一个账号容器）",
                        "type": "authentication_error",
                    }
                },
                status_code=401,
                headers=CORS_HEADERS,
            )
        if not acc.get("enabled", True):
            return JSONResponse(
                {"error": {"message": f"账号 {acc['id']} 已停用", "type": "disabled"}},
                status_code=503,
                headers=CORS_HEADERS,
            )
        pool.note_request(acc["id"])
        try:
            return await forward(
                request, zc_base(acc, s), acc["upstream_key"], pool.client(), s.idle_timeout
            )
        except UpstreamError as e:
            st = pool.st(acc["id"])
            st["healthy"] = False
            st["last_error"] = str(e)[:200]
            return JSONResponse(
                {
                    "error": {
                        "message": f"账号 {acc['id']} 的容器不可达，稍后重试",
                        "type": "bad_gateway",
                    }
                },
                status_code=502,
                headers=CORS_HEADERS,
            )

    return app


def _lifespan(
    pool: Pool,
    reg: Registry,
    s: Settings,
    ctl: DockerCtl | None,
    enable_docker: bool,
    apply,
):
    @asynccontextmanager
    async def lifespan(app: FastAPI):
        app.state.loop = asyncio.get_running_loop()
        s.home.mkdir(parents=True, exist_ok=True)
        if reg.fresh_file or reg.backfilled:
            reg.save()
        if reg.initial_password:
            print("=" * 56)
            print("  Aocker 管理器")
            print(f"    面板     http://{s.addr}/admin")
            print(
                f"    初始密码 {reg.initial_password}  账号 {reg.data.get('admin_username', 'admin')}，登录后请修改"
            )
            print(f"    脚本令牌 {reg.admin_token}")
            print(f"    （已写入 {s.accounts_path}；ZCM_ADMIN_PASSWORD 可设权威密码）")
            print("=" * 56, flush=True)
        if enable_docker and ctl is not None:
            try:
                if await ctl.available():
                    await ctl.ensure_network()
                    await apply()
                else:
                    log.warning("docker 守护进程不可用：仅面板模式，容器操作将在恢复后自动进行")
            except Exception as e:
                log.warning("启动自愈失败（不影响面板）：%s: %s", type(e).__name__, e)
        await pool.start()
        try:
            yield
        finally:
            await pool.stop()

    return lifespan
