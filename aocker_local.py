#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Aocker 本地代理脚本。

登录授权时，让浏览器流量经由 Aocker 服务器、并按账号出口（WARP/直连）出去，
保证授权 IP 与账号日常出口一致，降低风控错配。

用法：
  python aocker_local.py                # 打开可视化窗口（默认）
  python aocker_local.py --no-gui --server http://服务器:9000 --token 令牌 --account a1

依赖：pip install websockets
"""
from __future__ import annotations

import argparse
import asyncio
import base64
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse

try:
    import websockets
    from websockets.exceptions import ConnectionClosed, InvalidStatus
except ImportError:
    print("缺少依赖：pip install websockets")
    sys.exit(1)

VERSION = "1.0"  # 配置串格式 AOCKER:1 与隧道协议自此冻结

CONFIG_PATH = os.path.join(
    os.environ.get("APPDATA") or os.path.expanduser("~"), "aocker-local.json"
)
BROWSER_CANDIDATES = [
    os.path.expandvars(r"%ProgramFiles%\Google\Chrome\Application\chrome.exe"),
    os.path.expandvars(r"%ProgramFiles(x86)%\Google\Chrome\Application\chrome.exe"),
    os.path.expandvars(r"%LocalAppData%\Google\Chrome\Application\chrome.exe"),
    # 没有 Chrome 时退回 Edge：同为 Chromium 内核，无痕与代理参数通用
    os.path.expandvars(r"%ProgramFiles(x86)%\Microsoft\Edge\Application\msedge.exe"),
    os.path.expandvars(r"%ProgramFiles%\Microsoft\Edge\Application\msedge.exe"),
]


def load_config() -> dict:
    try:
        with open(CONFIG_PATH, encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return {}


def save_config(cfg: dict) -> None:
    try:
        with open(CONFIG_PATH, "w", encoding="utf-8") as f:
            json.dump(cfg, f, ensure_ascii=False, indent=2)
    except Exception:
        pass


class State:
    """运行期共享状态（GUI 与 asyncio 线程共用，字段赋值均为原子操作）。"""

    def __init__(self, cfg: dict):
        self.server = cfg.get("server") or ""
        self.token = cfg.get("token") or ""
        self.account = cfg.get("account") or "a1"
        self.proxy_port = int(cfg.get("proxy_port") or 7788)
        self.browser = cfg.get("browser") or ""
        self.running = False

    def ws_uri(self, account: str) -> str:
        base = self.server.strip().rstrip("/")
        if base.startswith("https://"):
            base = "wss://" + base[len("https://"):]
        elif base.startswith("http://"):
            base = "ws://" + base[len("http://"):]
        return f"{base}/api/tunnel/{account}?token={urllib.parse.quote(self.token)}"


LOG_QUEUE: "list[tuple[str, str]]" = []


def log(msg: str, level: str = "info") -> None:
    LOG_QUEUE.append((level, f"{time.strftime('%H:%M:%S')} {msg}"))
    print(f"[{level}] {msg}", flush=True)


# ---------------------------------------------------------------- 隧道代理
async def pipe(reader, writer, ws) -> None:
    async def c2t():
        while True:
            data = await reader.read(65536)
            if not data:
                break
            await ws.send(data)

    async def t2c():
        while True:
            msg = await ws.recv()
            if isinstance(msg, str):
                continue
            writer.write(msg)
            await writer.drain()

    tasks = {asyncio.ensure_future(c2t()), asyncio.ensure_future(t2c())}
    done, pending = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
    for t in pending:
        t.cancel()
    for t in done:
        exc = t.exception()
        if exc and not isinstance(exc, (ConnectionClosed, ConnectionError)):
            log(f"隧道断开：{type(exc).__name__} {exc}", "warn")
    writer.close()
    try:
        await ws.close()
    except Exception:
        pass


async def handle_connect(reader, writer, state: State) -> None:
    established = False
    try:
        line = await reader.readline()
        if not line.startswith(b"CONNECT"):
            writer.write(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n")
            await writer.drain()
            writer.close()
            return
        target = line.split()[1].decode()
        host, _, port_s = target.rpartition(":")
        port = int(port_s)
        while True:
            head = await reader.readline()
            if head in (b"\r\n", b"\n", b""):
                break
        ws = await websockets.connect(
            state.ws_uri(state.account), max_size=None, open_timeout=15
        )
        await ws.send(json.dumps({"host": host, "port": port}))
        reply = await asyncio.wait_for(ws.recv(), 15)
        if reply != "ok":
            detail = reply
            try:
                detail = json.loads(reply).get("error", reply)
            except Exception:
                pass
            raise RuntimeError(detail)
        writer.write(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        await writer.drain()
        established = True
        log(f"{host}:{port} 隧道已建立（出口跟随账号 {state.account}）")
        await pipe(reader, writer, ws)
    except InvalidStatus:
        log("服务器拒绝隧道连接：管理令牌不对。请填服务器 data/accounts.yaml 里的 admin_token，"
            "不是 sk-zc 开头的账号 Key", "warn")
    except ConnectionClosed:
        log("隧道被服务器关闭")
    except Exception as e:
        log(f"代理连接结束：{type(e).__name__} {e}", "warn")
        if not established:
            try:
                writer.write(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                await writer.drain()
            except Exception:
                pass
        try:
            writer.close()
        except Exception:
            pass


async def start_proxy(state: State) -> None:
    if state.running:
        log("本地代理已在运行")
        return
    server = None
    for port in range(state.proxy_port, state.proxy_port + 11):
        try:
            server = await asyncio.start_server(
                lambda r, w: handle_connect(r, w, state), "127.0.0.1", port
            )
            state.proxy_port = port
            break
        except OSError:
            continue
    if server is None:
        log(f"本地代理启动失败：{state.proxy_port}~{state.proxy_port + 10} 均被占用", "warn")
        return
    state.running = True
    log(f"本地代理已启动：127.0.0.1:{state.proxy_port}（脚本打开的浏览器会自动走它）")
    async with server:
        await server.serve_forever()


# ---------------------------------------------------------------- 浏览器拉起
def _registry_browser() -> str | None:
    """从注册表 App Paths 找 Chrome（装在非默认目录也能找到）。"""
    try:
        import winreg

        for root in (winreg.HKEY_CURRENT_USER, winreg.HKEY_LOCAL_MACHINE):
            try:
                with winreg.OpenKey(
                    root,
                    r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\chrome.exe",
                ) as k:
                    path = str(winreg.QueryValue(k, None) or "")
                    if path and os.path.isfile(path):
                        return path
            except OSError:
                continue
    except Exception:
        pass
    return None


def find_browser(state: State | None = None) -> str | None:
    if state is not None and state.browser and os.path.isfile(state.browser):
        return state.browser
    reg = _registry_browser()
    if reg:
        return reg
    for c in BROWSER_CANDIDATES:
        if os.path.isfile(c):
            return c
    return None


def open_in_browser(url: str, state: State, dry_run: bool = False) -> bool:
    exe = find_browser(state)
    if not exe:
        log("未找到 Chrome（也没找到 Edge）：请安装 Chrome，或用 --browser 指定路径", "warn")
        return False
    # 无痕 + 独立配置目录：代理只作用于这个窗口，日常浏览不受影响；
    # 目录名带端口，代理端口变化时必定开新进程而不是复用旧代理的实例
    profile = os.path.join(tempfile.gettempdir(), f"aocker-chrome-{state.proxy_port}")
    cmd = [
        exe,
        "--incognito",
        f"--proxy-server=http://127.0.0.1:{state.proxy_port}",
        f"--user-data-dir={profile}",
        "--no-first-run",
        "--no-default-browser-check",
        url,
    ]
    if dry_run:
        log("浏览器命令（dry-run）：" + " ".join(cmd))
        return True
    subprocess.Popen(cmd, close_fds=True)
    log("已打开无痕窗口（仅该窗口走隧道，日常浏览不受影响）")
    return True


# ---------------------------------------------------------------- GUI
def parse_config_string(text: str) -> dict:
    """解析面板生成的配置串。

    AOCKER:<base64(JSON)>  → {server, token, account, url}
    或直接粘贴授权链接（http/https）→ {url}，沿用当前配置。
    """
    text = (text or "").strip()
    if text.startswith(("http://", "https://")):
        return {"url": text}
    if text.upper().startswith("AOCKER:"):
        raw = "".join(text[len("AOCKER:"):].split())
        raw += "=" * (-len(raw) % 4)
        try:
            data = json.loads(base64.b64decode(raw).decode("utf-8"))
        except Exception as e:
            raise ValueError(f"配置串内容无法解析：{e}") from e
        return {
            "server": str(data.get("s") or "").strip().rstrip("/"),
            "token": str(data.get("t") or "").strip(),
            "account": str(data.get("a") or "").strip(),
            "url": str(data.get("u") or "").strip(),
        }
    raise ValueError("无法识别的字符串：请粘贴面板「复制配置串」生成的内容，或完整授权链接")


def run_gui(state: State, loop, log) -> None:
    import tkinter as tk
    from tkinter import scrolledtext

    root = tk.Tk()
    root.title("Aocker 本地代理")
    root.geometry("640x500")
    root.minsize(580, 440)

    head = tk.Frame(root, bg="#0e1729")
    head.pack(fill="x")
    tk.Label(head, text="  Aocker 本地代理", bg="#0e1729", fg="#ffffff",
             font=("Microsoft YaHei UI", 12, "bold")).pack(side="left", pady=10)
    tk.Label(head, text=f"v{VERSION}  ", bg="#0e1729", fg="#8d9bb2",
             font=("Microsoft YaHei UI", 9)).pack(side="right", pady=12)

    frm = tk.Frame(root, padx=14, pady=12)
    frm.pack(fill="both", expand=True)
    frm.columnconfigure(0, weight=1)

    tk.Label(frm, text="粘贴授权密钥：", anchor="w", fg="#1b2436",
             font=("Microsoft YaHei UI", 10, "bold")).grid(row=0, column=0, sticky="we")
    e_conf = tk.Entry(frm, font=("Consolas", 10), relief="solid", bd=1)
    e_conf.grid(row=1, column=0, sticky="we", pady=(3, 4), ipady=4)
    tk.Label(frm, text="从面板「复制授权密钥」获得；也可直接粘贴授权链接",
             anchor="w", fg="#98a3b8", font=("Microsoft YaHei UI", 9)).grid(
        row=2, column=0, sticky="we", pady=(0, 10))

    btn_row = tk.Frame(frm)
    btn_row.grid(row=3, column=0, sticky="w", pady=(0, 8))
    status_var = tk.StringVar(value="○ 未启动")
    status_lbl = tk.Label(btn_row, textvariable=status_var, fg="#98a3b8")

    def current_conf() -> dict:
        return {"server": state.server, "token": state.token, "account": state.account,
                "proxy_port": state.proxy_port, "browser": state.browser}

    def apply_conf() -> None:
        text = e_conf.get().strip()
        if not text:
            log("请先粘贴面板生成的配置串", "warn")
            return
        try:
            info = parse_config_string(text)
        except ValueError as e:
            log(str(e), "warn")
            return
        if info.get("server"):
            state.server = info["server"]
        if info.get("token"):
            state.token = info["token"]
        if info.get("account"):
            state.account = info["account"]
        save_config(current_conf())
        if state.server and not state.running:
            asyncio.run_coroutine_threadsafe(start_proxy(state), loop)
            status_var.set("● 运行中")
            status_lbl.config(fg="#16a34a")
        log(f"配置已应用：{state.server or '（未含服务器）'} · 账号 {state.account}")
        if info.get("url"):
            open_in_browser(info["url"], state)
        else:
            log("配置串里没有授权链接：稍后可单独粘贴链接再点一次")

    def start_bg() -> None:
        if not state.server:
            log("还没有服务器地址：请先粘贴配置串", "warn")
            return
        if state.running:
            log("本地代理已在运行")
            return
        asyncio.run_coroutine_threadsafe(start_proxy(state), loop)
        status_var.set("● 运行中")
        status_lbl.config(fg="#16a34a")

    tk.Button(btn_row, text="应用并打开 Chrome 无痕", command=apply_conf, bg="#2f6fed", fg="white",
              activebackground="#2a5ce0", activeforeground="white", relief="flat",
              padx=16, pady=4, cursor="hand2").pack(side="left", padx=(0, 8))
    tk.Button(btn_row, text="仅启动代理", command=start_bg, relief="flat", padx=12, pady=4,
              cursor="hand2").pack(side="left", padx=(0, 10))
    status_lbl.pack(side="left")

    tk.Label(frm, text=f"本地代理 127.0.0.1:{state.proxy_port}",
             anchor="w", fg="#98a3b8").grid(row=4, column=0, sticky="we", pady=(0, 6))

    logbox = scrolledtext.ScrolledText(frm, height=13, state="disabled", font=("Consolas", 9))
    logbox.grid(row=5, column=0, sticky="we", pady=(2, 0))
    frm.rowconfigure(5, weight=1)

    def push_log(msg: str, level: str = "info") -> None:
        LOG_QUEUE.append((level, msg))

    def drain() -> None:
        while LOG_QUEUE:
            level, msg = LOG_QUEUE.pop(0)
            logbox.configure(state="normal")
            logbox.insert("end", msg + "\n")
            logbox.see("end")
            logbox.configure(state="disabled")
        root.after(400, drain)

    drain()
    root.mainloop()

# ---------------------------------------------------------------- 入口
def main() -> None:
    cfg = load_config()
    ap = argparse.ArgumentParser(description="Aocker 本地代理：登录授权走服务器出口")
    ap.add_argument("--server", default=cfg.get("server") or "")
    ap.add_argument("--token", default=cfg.get("token") or "")
    ap.add_argument("--account", default=cfg.get("account") or "a1")
    ap.add_argument("--proxy-port", type=int, default=int(cfg.get("proxy_port") or 7788))
    ap.add_argument("--browser", default=cfg.get("browser") or "",
                    help="浏览器可执行文件路径（默认自动找 Chrome，找不到退回 Edge）")
    ap.add_argument("--no-gui", action="store_true", help="不开窗口，纯命令行运行代理")
    ap.add_argument("--dry-run", action="store_true", help="只打印浏览器命令，不真正打开")
    args = ap.parse_args()

    state = State({
        "server": args.server, "token": args.token, "account": args.account,
        "proxy_port": args.proxy_port, "browser": args.browser,
    })

    loop = asyncio.new_event_loop()
    threading.Thread(target=loop.run_forever, daemon=True).start()
    asyncio.run_coroutine_threadsafe(start_proxy(state), loop)

    if args.no_gui:
        log("无界面模式运行中，Ctrl+C 退出")
        try:
            while True:
                time.sleep(1)
        except KeyboardInterrupt:
            pass
        return
    run_gui(state, loop, log)


if __name__ == "__main__":
    main()
