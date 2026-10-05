"""注册表（accounts.yaml）与运行设置。

accounts.yaml 是唯一状态源：账号、密钥、端口槽位都存这里；
runtime/ 下的 compose 与每账号 config.yaml 全部由它派生，可随时重建。
"""
from __future__ import annotations

import datetime as _dt
import logging
import os
import re
import secrets
import socket
import threading
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import yaml

from .auth import hash_password, new_session_secret

PROVIDERS = ("zai", "bigmodel")
PLANS = ("coding-plan", "start-plan")
EGRESS_MODES = ("warp", "direct", "socks5")
ID_RE = re.compile(r"^[a-z0-9][a-z0-9-]{0,15}$")

log = logging.getLogger("zcm.config")


def port_free(port: int) -> bool:
    """探测 127.0.0.1 端口此刻能否绑定；宿主机上任何占用者都算不可用。"""
    # 探测时不应设置 SO_REUSEADDR，否则 Windows 会立即重用端口，失去探测意义
    # 实际容器启动时会由 Docker handle REUSEADDR
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        try:
            sock.bind(("127.0.0.1", port))
            return True
        except OSError:
            return False


class ConfigError(ValueError):
    """配置不合法——消息直接面向面板展示。"""


def _now() -> str:
    return _dt.datetime.now().isoformat(timespec="seconds")


@dataclass
class Settings:
    home: Path
    addr: str
    admin_token_env: str | None
    mode: str  # host | container
    port_base: int
    project: str
    mesh: str
    image_zcode: str
    image_warp: str
    idle_timeout: float
    health_interval: int
    egress_interval: int
    quota_interval: int
    drift_interval: int

    @classmethod
    def from_env(cls) -> "Settings":
        def _int(name: str, default: int) -> int:
            try:
                return int(os.environ.get(name, "") or default)
            except ValueError:
                return default

        def _float(name: str, default: float) -> float:
            try:
                return float(os.environ.get(name, "") or default)
            except ValueError:
                return default

        mode = (os.environ.get("ZCM_MODE") or "auto").strip().lower()
        if mode not in ("auto", "host", "container"):
            mode = "auto"
        if mode == "auto":
            mode = "container" if Path("/.dockerenv").exists() else "host"
        return cls(
            home=Path(os.environ.get("ZCM_HOME") or "data").resolve(),
            addr=os.environ.get("ZCM_ADDR") or "0.0.0.0:9000",
            admin_token_env=(os.environ.get("ZCM_ADMIN_TOKEN") or "").strip() or None,
            mode=mode,
            port_base=_int("ZCM_PORT_BASE", 21100),
            project=(os.environ.get("ZCM_PROJECT") or "zcm").strip(),
            mesh=(os.environ.get("ZCM_MESH") or "zcm-mesh").strip(),
            image_zcode=os.environ.get("ZCM_IMAGE_ZCODE")
            or "ghcr.io/tridefender/zcode-proxy:latest",
            image_warp=os.environ.get("ZCM_IMAGE_WARP") or "caomingjun/warp:latest",
            idle_timeout=_float("ZCM_IDLE_TIMEOUT", 300.0),
            health_interval=_int("ZCM_HEALTH_INTERVAL", 30),
            egress_interval=_int("ZCM_EGRESS_INTERVAL", 300),
            quota_interval=_int("ZCM_QUOTA_INTERVAL", 600),
            drift_interval=_int("ZCM_DRIFT_INTERVAL", 300),
        )

    @property
    def accounts_path(self) -> Path:
        return self.home / "accounts.yaml"

    @property
    def runtime_dir(self) -> Path:
        return self.home / "runtime"

    @property
    def compose_file(self) -> Path:
        return self.runtime_dir / "docker-compose.yaml"

    @property
    def conf_dir(self) -> Path:
        return self.runtime_dir / "conf"

    @property
    def status_path(self) -> Path:
        return self.home / "status.json"


def _new_account(
    acc_id: str, provider: str, plan: str, egress: str, port: int, note: str, socks5: str = ""
) -> dict:
    return {
        "id": acc_id,
        "key": f"sk-zc-{secrets.token_hex(16)}",
        "provider": provider,
        "plan": plan,
        "enabled": True,
        "egress": egress,
        "socks5": socks5,
        "credential_secret": secrets.token_urlsafe(24),
        "upstream_key": f"up-{secrets.token_hex(16)}",
        "device_mid": str(uuid.uuid4()),
        "port": port,
        "created_at": _now(),
        "note": note,
    }


class Registry:
    """accounts.yaml 的读写与账号增删改；所有写操作原子落盘。"""

    def __init__(self, path: Path, settings: Settings):
        self.path = path
        self.s = settings
        self._lock = threading.Lock()
        self.fresh_file = False
        self.data: dict[str, Any] = {}
        self.load()

    # ---- 载入与落盘 -------------------------------------------------------
    def load(self) -> None:
        if self.path.exists():
            try:
                self.data = yaml.safe_load(self.path.read_text(encoding="utf-8")) or {}
            except yaml.YAMLError as e:
                # 不静默重置
                raise ConfigError(f"accounts.yaml 解析失败：{e}") from e
            if not isinstance(self.data, dict):
                raise ConfigError("accounts.yaml 顶层必须是映射")
        else:
            self.data = {}
            self.fresh_file = True
        self.data.setdefault("port_base", self.s.port_base)
        accounts = self.data.setdefault("accounts", [])
        if not isinstance(accounts, list):
            raise ConfigError("accounts 段必须是列表")
        # 管理面凭据
        self.initial_password: str | None = None
        self.data.setdefault("admin_username", "admin")
        if not self.data.get("session_secret"):
            self.data["session_secret"] = new_session_secret()
        if not self.data.get("admin_password_hash"):
            # 首次初始化随机密码，横幅打印
            self.initial_password = secrets.token_urlsafe(9)
            self.data["admin_password_hash"] = hash_password(self.initial_password)
        env_pw = (os.environ.get("ZCM_ADMIN_PASSWORD") or "").strip()
        if env_pw:
            # 权威值，启动即重置
            self.data["admin_password_hash"] = hash_password(env_pw)
            self.initial_password = None  # 不打印初始密码横幅
        if not self.data.get("admin_token"):
            self.data["admin_token"] = secrets.token_urlsafe(24)  # 脚本令牌
        # 端口槽位冲突不影响启动，但必须告警
        seen_ports: dict[int, str] = {}
        for a in accounts:
            p = a.get("port")
            if p is None:
                continue
            if p in seen_ports:
                log.warning(
                    "accounts.yaml 里 %s 与 %s 端口槽位冲突（%s），"
                    "部署前请手工改开其中一个",
                    seen_ports[p], a.get("id"), p,
                )
            else:
                seen_ports[p] = a.get("id", "?")
        # 旧版注册表补齐 device_mid：每账号固定设备标识
        self.backfilled = False
        for a in accounts:
            if not a.get("device_mid"):
                a["device_mid"] = str(uuid.uuid4())
                self.backfilled = True

    def save(self) -> None:
        with self._lock:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            tmp = self.path.with_name(self.path.name + ".tmp")
            tmp.write_text(
                yaml.safe_dump(self.data, sort_keys=False, allow_unicode=True),
                encoding="utf-8",
            )
            os.replace(tmp, self.path)

    # ---- 读取 -------------------------------------------------------------
    @property
    def admin_token(self) -> str:
        return self.s.admin_token_env or str(self.data["admin_token"])

    def accounts(self) -> list[dict]:
        return self.data["accounts"]

    def get(self, acc_id: str) -> dict | None:
        for a in self.data["accounts"]:
            if a.get("id") == acc_id:
                return a
        return None

    def find_by_key(self, key: str) -> dict | None:
        if not key:
            return None
        for a in self.data["accounts"]:
            if a.get("key") == key:
                return a
        return None

    # ---- 端口与 ID 分配 ---------------------------------------------------
    def _alloc_id(self) -> str:
        used = {a.get("id") for a in self.data["accounts"]}
        n = 1
        while f"a{n}" in used:
            n += 1
        return f"a{n}"

    def _alloc_port(self, probe=None) -> int:
        # 每账号两个槽位：zc 用 p，warp 用 p+1
        used: set[int] = set()
        for a in self.data["accounts"]:
            try:
                p = int(a.get("port") or 0)
            except (TypeError, ValueError):
                continue
            used.update((p, p + 1))
        p = int(self.data.get("port_base") or self.s.port_base)
        while p in used or p + 1 in used or (probe is not None and not (probe(p) and probe(p + 1))):
            p += 2
            if p > 65000:
                raise ConfigError("端口槽位耗尽（>65000）：请调大 port_base 或清理账号")
        return p

    # ---- 增删改 -----------------------------------------------------------
    @staticmethod
    def _validate(provider: str, plan: str, egress: str) -> None:
        if provider not in PROVIDERS:
            raise ConfigError(f"provider 只能是 {'/'.join(PROVIDERS)}，收到：{provider!r}")
        if plan not in PLANS:
            raise ConfigError(f"plan 只能是 {'/'.join(PLANS)}，收到：{plan!r}")
        if egress not in EGRESS_MODES:
            raise ConfigError(f"egress 只能是 {'/'.join(EGRESS_MODES)}，收到：{egress!r}")

    def add(
        self,
        provider: str,
        plan: str,
        egress: str = "warp",
        note: str = "",
        count: int = 1,
        acc_id: str | None = None,
        probe=None,
        socks5: str = "",
    ) -> list[dict]:
        """probe 返回 False 表示端口被占。"""
        count = max(1, min(int(count or 1), 50))
        created: list[dict] = []
        for i in range(count):
            if acc_id and i == 0:
                if not ID_RE.match(acc_id):
                    raise ConfigError(f"账号 ID 只允许小写字母/数字/短横线（≤16 位），收到：{acc_id!r}")
                if self.get(acc_id):
                    raise ConfigError(f"账号 ID 已存在：{acc_id}")
                aid = acc_id
            else:
                aid = self._alloc_id()
            acc = _new_account(aid, provider, plan, egress, self._alloc_port(probe), note, socks5)
            self.data["accounts"].append(acc)
            created.append(acc)
        self.save()
        return created

    _UPDATABLE = ("provider", "plan", "egress", "enabled", "note", "socks5")

    def update(self, acc_id: str, fields: dict) -> dict:
        acc = self.get(acc_id)
        if not acc:
            raise ConfigError(f"账号不存在：{acc_id}")
        changes = {}
        for k in self._UPDATABLE:
            if k in fields and fields[k] is not None:
                changes[k] = fields[k]
        if "provider" in changes or "plan" in changes or "egress" in changes:
            self._validate(
                changes.get("provider", acc["provider"]),
                changes.get("plan", acc["plan"]),
                changes.get("egress", acc["egress"]),
            )
        if changes.get("egress") == "socks5" and not (
            changes.get("socks5") or acc.get("socks5")
        ):
            raise ConfigError("SOCKS5 出口需要填写代理地址（socks5://host:port）")
        if "enabled" in changes:
            changes["enabled"] = bool(changes["enabled"])
        acc.update(changes)
        self.save()
        return acc

    def delete(self, acc_id: str) -> dict:
        acc = self.get(acc_id)
        if not acc:
            raise ConfigError(f"账号不存在：{acc_id}")
        self.data["accounts"].remove(acc)
        self.save()
        return acc

    def rotate_key(self, acc_id: str) -> str:
        """轮换对外 Key：旧 Key 立即失效（Key 泄漏应急）。"""
        acc = self.get(acc_id)
        if not acc:
            raise ConfigError(f"账号不存在：{acc_id}")
        acc["key"] = f"sk-zc-{secrets.token_hex(16)}"
        self.save()
        return acc["key"]
