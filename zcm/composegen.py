"""由注册表派生运行时文件：每账号 config.yaml + docker-compose.yaml。

每账号两个服务：zc-<id> 是 zcode-proxy，warp-<id> 是它的 WARP 出口
（gost 在 1080 端口同收 HTTP 与 SOCKS5，zcode 侧设 HTTP_PROXY 走出）。
"""
from __future__ import annotations

import hashlib
import shutil
from pathlib import Path

import yaml

from .config import Registry, Settings

ZCODE_PORT = 8080
CONF_MOUNT = "/data/config.yaml"
CRED_DIR = "/home/bun/.zcode-proxy"
WARP_DATA = "/var/lib/cloudflare-warp"


def account_config(acc: dict) -> dict:
    return {
        "server": {"port": ZCODE_PORT, "host": "0.0.0.0"},
        "auth": {"proxyApiKey": acc["upstream_key"]},
        "provider": acc["provider"],
        "plan": acc["plan"],
        # deviceMid 由管理器固定生成；否则 zcode 首次运行要写配置，只读挂载会 EACCES
        "identity": {"deviceMid": acc["device_mid"]},
        "logging": {"level": "info"},
    }


def account_config_yaml(acc: dict) -> str:
    return yaml.safe_dump(account_config(acc), sort_keys=False, allow_unicode=True)


def conf_hash(acc: dict) -> str:
    return hashlib.sha1(account_config_yaml(acc).encode("utf-8")).hexdigest()[:8]


def zc_base(acc: dict, s: Settings) -> str:
    """反代数据面访问 zc 容器的基地址。"""
    if s.mode == "host":
        return f"http://127.0.0.1:{acc['port']}"
    return f"http://zc-{acc['id']}:{ZCODE_PORT}"


def warp_proxy(acc: dict, s: Settings) -> str | None:
    """该账号的 WARP 出口代理，直连账号返回 None。"""
    if acc.get("egress", "warp") != "warp":
        return None
    if s.mode == "host":
        return f"http://127.0.0.1:{int(acc['port']) + 1}"
    return f"http://warp-{acc['id']}:1080"


def build_compose(reg: Registry, s: Settings) -> dict:
    services: dict = {}
    volumes: dict = {}
    for acc in reg.accounts():
        if not acc.get("enabled", True):
            continue
        i = acc["id"]
        env: dict[str, str] = {
            # bind mount 内容变化不触发重建，用内容哈希当变更信号
            "ZCODE_PROXY_CREDENTIAL_SECRET": str(acc["credential_secret"]),
            "ZCM_CONF_HASH": conf_hash(acc),
        }
        zc: dict = {
            "image": s.image_zcode,
            "restart": "unless-stopped",
            "networks": [s.mesh],
            "environment": env,
            "volumes": [f"cred-{i}:{CRED_DIR}", f"./conf/{i}.yaml:{CONF_MOUNT}:ro"],
        }
        if s.mode == "host":
            zc["ports"] = [f"127.0.0.1:{acc['port']}:{ZCODE_PORT}"]
        if acc.get("egress", "warp") == "warp":
            proxy = f"http://warp-{i}:1080"
            env["HTTP_PROXY"] = proxy
            env["HTTPS_PROXY"] = proxy
            env["NO_PROXY"] = "localhost,127.0.0.1"
            zc["depends_on"] = [f"warp-{i}"]
            warp: dict = {
                "image": s.image_warp,
                "restart": "unless-stopped",
                "networks": [s.mesh],
                "cap_add": ["NET_ADMIN"],
                "sysctls": {
                    "net.ipv6.conf.all.disable_ipv6": "0",
                    "net.ipv4.conf.all.src_valid_mark": "1",
                },
                # 放行 tun 设备（c 10:200）
                "device_cgroup_rules": ["c 10:200 rwm"],
                "environment": {"WARP_SLEEP": "3"},
                "volumes": [f"warpstate-{i}:{WARP_DATA}"],
            }
            if s.mode == "host":
                warp["ports"] = [f"127.0.0.1:{int(acc['port']) + 1}:1080"]
            services[f"warp-{i}"] = warp
            volumes[f"warpstate-{i}"] = {}
        services[f"zc-{i}"] = zc
        volumes[f"cred-{i}"] = {}
    return {
        "name": s.project,
        "networks": {s.mesh: {"external": True}},
        "volumes": volumes,
        "services": services,
    }


def write_runtime(reg: Registry, s: Settings) -> Path:
    """重写 runtime/ 下的 compose 与每账号配置；返回 compose 文件路径。"""
    s.conf_dir.mkdir(parents=True, exist_ok=True)
    active: set[str] = set()
    for acc in reg.accounts():
        if not acc.get("enabled", True):
            continue
        i = acc["id"]
        active.add(i)
        cp = s.conf_dir / f"{i}.yaml"
        if cp.is_dir():
            # 容器创建时 bind 源缺失，docker 会把该路径建成目录；清掉重写
            shutil.rmtree(cp)
        cp.write_text(account_config_yaml(acc), encoding="utf-8")
    # 清理已删除或停用账号的 conf
    for p in s.conf_dir.glob("*.yaml"):
        if p.stem not in active:
            if p.is_dir():
                shutil.rmtree(p)
            else:
                p.unlink(missing_ok=True)
    s.compose_file.write_text(
        yaml.safe_dump(build_compose(reg, s), sort_keys=False, allow_unicode=True),
        encoding="utf-8",
    )
    return s.compose_file
