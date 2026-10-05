"""后台循环：容器健康、WARP 出口探测、配额聚合、漂移自愈、登录会话。

上游对配额查询限速，轮询频率刻意放低。
"""
from __future__ import annotations

import asyncio
import collections
import json
import logging
import re
import time
import uuid

import httpx

from .composegen import CRED_DIR, warp_proxy, zc_base
from .config import Registry, Settings
from .dockerctl import DockerCtl

log = logging.getLogger("zcm.pool")

URL_RE = re.compile(r"https?://[^\s\"'<>]+", re.I)
TRACE_URL = "https://www.cloudflare.com/cdn-cgi/trace"

LOGIN_TIMEOUT = 900.0  # OAuth 等用户操作，给足 15 分钟
CLAIM_TIMEOUT = 300.0  # 含验证码解题耗时


def parse_trace(text: str) -> dict[str, str]:
    """解析 cloudflare /cdn-cgi/trace 输出（ip=、warp= 等）。"""
    out: dict[str, str] = {}
    for ln in text.splitlines():
        if "=" in ln:
            k, _, v = ln.partition("=")
            out[k.strip()] = v.strip()
    return out


def new_status() -> dict:
    return {
        "container": "unknown",
        "warp_container": "unknown",
        "healthy": None,
        "login_ok": None,
        "egress_ip": None,
        "warp_mode": None,
        "quota": None,
        "requests": 0,
        "last_used": None,
        "last_probe": 0.0,
        "last_error": None,
    }


class Pool:
    def __init__(
        self,
        reg: Registry,
        s: Settings,
        docker: DockerCtl | None,
        http_transport: httpx.AsyncBaseTransport | None = None,
    ):
        self.reg = reg
        self.s = s
        self.docker = docker
        self._transport = http_transport
        self.status: dict[str, dict] = {}
        self.logins: dict[str, dict] = {}
        self.events = collections.deque(maxlen=200)
        self._quota_ts: dict[str, float] = {}
        self._claim_seen: dict[str, tuple] = {}
        self.last_apply = 0.0
        self._client = self._mk_client()
        self._egress: dict[str, httpx.AsyncClient] = {}
        self._tasks: set[asyncio.Task] = set()
        self._stopped = False
        self.compose_ok: bool | None = None

    def _mk_client(self, **kw) -> httpx.AsyncClient:
        base = dict(
            timeout=httpx.Timeout(15, read=None, write=60, pool=15),
            limits=httpx.Limits(max_connections=512, max_keepalive_connections=64),
        )
        if self._transport is not None:
            base["transport"] = self._transport
        base.update(kw)
        return httpx.AsyncClient(**base)

    # ---- 状态 -------------------------------------------------------------
    def st(self, acc_id: str) -> dict:
        return self.status.setdefault(acc_id, new_status())

    def note_request(self, acc_id: str) -> None:
        st = self.st(acc_id)
        st["requests"] = int(st.get("requests") or 0) + 1
        st["last_used"] = time.strftime("%m-%d %H:%M:%S")

    def client(self) -> httpx.AsyncClient:
        return self._client

    # ---- 生命周期 ---------------------------------------------------------
    async def start(self) -> None:
        self._stopped = False
        for name, interval, fn in (
            ("health", self.s.health_interval, self._health_cycle),
            ("egress", self.s.egress_interval, self._egress_cycle),
            ("quota", self.s.quota_interval, self._quota_cycle),
            ("drift", self.s.drift_interval, self._drift_cycle),
        ):
            task = asyncio.create_task(self._loop(name, interval, fn))
            task.add_done_callback(self._tasks.discard)
            self._tasks.add(task)

    async def stop(self) -> None:
        self._stopped = True
        for t in list(self._tasks):
            t.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        self._tasks.clear()
        await self._client.aclose()
        for c in self._egress.values():
            await c.aclose()
        self._egress.clear()

    async def _loop(self, name: str, interval: int, fn) -> None:
        while not self._stopped:
            try:
                await fn()
            except asyncio.CancelledError:
                if self._stopped:
                    raise  # 正常关停
                # 非关停期的取消：记日志继续跑，循环不能死
                log.warning("[%s] 任务被意外取消，重启循环", name)
            except Exception as e:  # 后台循环永不因单次异常退出
                log.warning("[%s] 周期任务异常: %s: %s", name, type(e).__name__, e)
            try:
                await asyncio.sleep(interval)
            except asyncio.CancelledError:
                raise

    # ---- 探测 -------------------------------------------------------------
    def _enabled(self) -> list[dict]:
        return [a for a in self.reg.accounts() if a.get("enabled", True)]

    def _auth_headers(self, acc: dict) -> dict[str, str]:
        return {"authorization": f"Bearer {acc['upstream_key']}"}

    async def probe_health(self, acc: dict) -> None:
        st = self.st(acc["id"])
        try:
            r = await self._client.get(
                zc_base(acc, self.s) + "/health", headers=self._auth_headers(acc)
            )
            st["healthy"] = r.status_code < 500
            if st["healthy"]:
                st["last_error"] = None
            else:
                st["last_error"] = f"health {r.status_code}"
        except (httpx.HTTPError, OSError) as e:
            st["healthy"] = False
            st["last_error"] = f"health: {type(e).__name__}"
        st["last_probe"] = time.time()

    async def probe_egress(self, acc: dict) -> None:
        st = self.st(acc["id"])
        url = warp_proxy(acc, self.s)
        if url is None:
            st["egress_ip"] = None
            st["warp_mode"] = "direct"
            return
        cli = self._egress.get(url)
        if cli is None:
            cli = self._mk_client(proxy=url, timeout=httpx.Timeout(10, read=15))
            self._egress[url] = cli
        try:
            r = await cli.get(TRACE_URL)
            d = parse_trace(r.text)
            st["egress_ip"] = d.get("ip")
            st["warp_mode"] = d.get("warp", "off")
            st["last_error"] = None
        except httpx.ConnectError:
            st["warp_mode"] = "error"
            st["last_error"] = "egress: 连不上 warp 容器，容器未运行或端口未发布，看容器日志"
        except (httpx.HTTPError, OSError) as e:
            st["warp_mode"] = "error"
            st["last_error"] = f"egress: {type(e).__name__}"

    def _detect_claimable(self, acc: dict, st: dict) -> None:
        """识别可领套餐：套餐集合变化时记入操作记录，便于及时发现新活动。"""
        try:
            j = json.loads(st.get("quota") or "{}")
            plans = j.get("claimablePlans") or []
        except Exception:
            return
        ids = tuple(sorted(str(p.get("planId") or "") for p in plans))
        if not ids:
            self._claim_seen.pop(acc["id"], None)
            return
        if self._claim_seen.get(acc["id"]) != ids:
            self._claim_seen[acc["id"]] = ids
            names = "、".join(str(p.get("name") or p.get("planId") or "?") for p in plans)
            self.events.append({
                "t": time.strftime("%m-%d %H:%M:%S"),
                "msg": f"{acc['id']} 检测到可领套餐：{names}",
            })

    def schedule_probe(self, acc: dict, delays: tuple[float, ...] = (3.0,)) -> None:
        """操作后延迟重探：docker 拉起容器、WARP 重注册都有延迟，
        立即探测只会拿到旧状态，按给定节奏补探几轮。"""

        async def _run() -> None:
            for d in delays:
                await asyncio.sleep(d)
                try:
                    await self.probe_health(acc)
                    if acc.get("egress", "warp") == "warp":
                        await self.probe_egress(acc)
                except asyncio.CancelledError:
                    raise
                except Exception:
                    pass

        task = asyncio.create_task(_run())
        task.add_done_callback(self._tasks.discard)
        self._tasks.add(task)

    async def fetch_quota(self, acc: dict, force: bool = True) -> None:
        now = time.time()
        # 最小间隔保护：上游对配额查询限速，手抖连点探测也不打爆接口
        if not force and now - self._quota_ts.get(acc["id"], 0) < 120:
            return
        self._quota_ts[acc["id"]] = now
        st = self.st(acc["id"])
        try:
            r = await self._client.get(
                zc_base(acc, self.s) + "/quota", headers=self._auth_headers(acc)
            )
            if r.status_code == 200:
                st["quota"] = r.text[:8000]
                st["login_ok"] = True
                self._detect_claimable(acc, st)
            elif r.status_code in (401, 403):
                st["login_ok"] = False
                st["quota"] = None
            else:
                st["last_error"] = f"quota {r.status_code}"
        except (httpx.HTTPError, OSError) as e:
            st["last_error"] = f"quota: {type(e).__name__}"

    # ---- 周期任务 ---------------------------------------------------------
    async def _health_cycle(self) -> None:
        accs = self._enabled()
        if not accs:
            return
        states: dict[str, str] = {}
        if self.docker is not None and await self.docker.available():
            states = await self.docker.ps_states()
        for acc in accs:
            st = self.st(acc["id"])
            st["container"] = states.get(f"zc-{acc['id']}", "unknown" if not states else "missing")
            if acc.get("egress", "warp") == "warp":
                st["warp_container"] = states.get(
                    f"warp-{acc['id']}", "unknown" if not states else "missing"
                )
        await asyncio.gather(*(self._probe_one(a) for a in accs))

    async def _probe_one(self, acc: dict) -> None:
        await self.probe_health(acc)
        if acc.get("egress", "warp") == "warp":
            await self.probe_egress(acc)

    async def _egress_cycle(self) -> None:
        await asyncio.gather(*(self.probe_egress(a) for a in self._enabled()))

    async def _quota_cycle(self) -> None:
        await asyncio.gather(*(self.fetch_quota(a, force=False) for a in self._enabled()))

    async def _drift_cycle(self) -> None:
        """compose 文件是期望状态，定期收敛。"""
        if self.docker is None or self.reg.fresh_file or self.compose_ok is False:
            return
        if not any(a.get("enabled", True) for a in self.reg.accounts()):
            return
        if time.time() - self.last_apply < 120:
            return
        if not self.s.compose_file.exists():
            return
        if not await self.docker.available():
            return
        async with self.docker.op_lock:
            await self.docker.up()
        self.last_apply = time.time()

    # ---- 登录/领取会话 ----------------------------------------------------
    def _gc_sessions(self) -> None:
        now = time.time()
        for k in [k for k, v in self.logins.items()
                  if v.get("done") and v.get("_ts", now) < now - 3600]:
            self.logins.pop(k, None)

    async def start_claim(self, acc: dict, rounds: int = 1) -> str:
        """手动领取：一次性容器里跑 `claim now`；多轮用于领多个套餐。"""
        sid = uuid.uuid4().hex[:8]
        sess: dict = {
            "id": sid, "account": acc["id"], "provider": acc["provider"],
            "lines": [], "urls": [], "done": False, "ok": None, "exit": None,
            "started": time.strftime("%H:%M:%S"),
        }
        self._gc_sessions()
        self.logins[sid] = sess
        sess["_ts"] = time.time()

        def on_line(line: str) -> None:
            sess["lines"].append(line)
            if len(sess["lines"]) > 300:
                del sess["lines"][:-300]

        # 领取出口自动跟随账号配置：账号挂了 WARP 就走 WARP，
        # 直连账号没有代理环境，天然走宿主机网络
        async def run() -> None:
            ok_all = True
            try:
                for i in range(max(1, rounds)):
                    rc, _ = await self.docker.run_service(
                        f"zc-{acc['id']}",
                        ["bun", "run", "src/index.ts", "claim", "now"],
                        timeout=CLAIM_TIMEOUT,
                        on_line=on_line,
                    )
                    if rc != 0:
                        ok_all = False
                        sess["lines"].append(f"[manager] 第 {i + 1} 轮失败")
                        break
                    if i + 1 < rounds:
                        on_line(f"[manager] 第 {i + 1}/{rounds} 轮完成，继续下一轮")
                        await asyncio.sleep(2.0)
                sess["exit"] = 0 if ok_all else 1
                sess["ok"] = ok_all
                if ok_all:
                    await self.fetch_quota(acc)
                    self.schedule_probe(acc, delays=(5.0, 20.0))
            except Exception as e:
                sess["ok"] = False
                sess["exit"] = -1
                sess["lines"].append(f"[manager] {type(e).__name__}: {e}")
            finally:
                sess["done"] = True

        sess["task"] = asyncio.create_task(run())
        return sid

    async def start_login(self, acc: dict, provider: str) -> str:
        sid = uuid.uuid4().hex[:8]
        sess: dict = {
            "id": sid,
            "account": acc["id"],
            "provider": provider,
            "lines": [],
            "urls": [],
            "done": False,
            "ok": None,
            "exit": None,
            "started": time.strftime("%H:%M:%S"),
        }
        self._gc_sessions()
        self.logins[sid] = sess
        sess["_ts"] = time.time()

        def on_line(line: str) -> None:
            sess["lines"].append(line)
            if len(sess["lines"]) > 200:
                del sess["lines"][:-200]
            for m in URL_RE.finditer(line):
                u = m.group(0)
                if u not in sess["urls"]:
                    sess["urls"].append(u)

        async def run() -> None:
            try:
                # 先修凭据卷属主：命名卷初始 root:root，bun 写不进去（EACCES）
                rc_fix, fix_out = await self.docker.fix_volume_owner(
                    f"{self.s.project}_cred-{acc['id']}", self.s.image_zcode
                )
                if rc_fix != 0:
                    sess["lines"].append(
                        "[manager] 卷属主修复失败，继续尝试登录：" + " ".join(fix_out[-3:])
                    )
                rc, _ = await self.docker.run_service(
                    f"zc-{acc['id']}",
                    ["bun", "run", "src/index.ts", "auth", "login", provider],
                    timeout=LOGIN_TIMEOUT,
                    on_line=on_line,
                )
                sess["exit"] = rc
                sess["ok"] = rc == 0
                if sess["ok"]:
                    self.st(acc["id"])["login_ok"] = True
                    # 主容器要等下一次重启才带着凭据站稳，稍后复探
                    self.schedule_probe(acc, delays=(8.0, 25.0))
            except Exception as e:
                sess["ok"] = False
                sess["exit"] = -1
                sess["lines"].append(f"[manager] {type(e).__name__}: {e}")
            finally:
                sess["done"] = True

        sess["task"] = asyncio.create_task(run())
        return sid

    def login_snapshot(self, sid: str) -> dict | None:
        sess = self.logins.get(sid)
        if not sess:
            return None
        # 白名单输出：session 里的 task 等对象不可 JSON 序列化
        return {
            "id": sess["id"],
            "account": sess["account"],
            "provider": sess["provider"],
            "lines": sess["lines"][-30:],
            "urls": sess["urls"][:5],
            "done": sess["done"],
            "ok": sess["ok"],
            "exit": sess["exit"],
            "started": sess["started"],
        }

    async def logout(self, acc: dict) -> None:
        """清掉容器内凭据并重启。"""
        assert self.docker is not None
        await self.docker.run_service(
            f"zc-{acc['id']}",
            ["sh", "-c", f"rm -f {CRED_DIR}/credentials.json"],
            timeout=30,
        )
        # rm -sf + up 重建：主容器可能正处于重启循环，restart 会失败
        async with self.docker.op_lock:
            await self.docker.stop_service(f"zc-{acc['id']}")
            await self.docker.up_service(f"zc-{acc['id']}")
        st = self.st(acc["id"])
        st["login_ok"] = None
        st["quota"] = None
