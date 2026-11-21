"""WebClaw 真实浏览器 Agent 回归：用 mock 模型模拟 mini/kimi-k3 的 DSML 工具调用。

覆盖：
- 模型不返回原生 tool_calls，而是在正文里输出 DSML（截图里的格式）时，WebClaw 能解析并执行工具
- 工具结果回填后继续下一轮，最终完成项目文件
- 工具调用在聊天区渲染为卡片（不是裸文本）
- 模型上下文元数据注入 agent 提示词

用法：cargo build --locked && python tests/webclaw_agent_e2e.py
"""
import json
import os
import socket
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import httpx

os.environ["NO_PROXY"] = "*"
os.environ["no_proxy"] = "*"

ROOT = Path(__file__).resolve().parents[1]
GW = os.environ.get("NGW_GW_EXE") or str(ROOT / "target" / "debug" / "nim-gateway.exe")
ADMIN_PW = "webclaw-agent-pass"

WRITE = '<|DSML|parameter name="name" string="true">workspace_write\n' \
        '<|DSML|parameter name="arguments" string="false">' \
        '{"path":"notes.md","content":"# WebAgent 项目\\n\\n由 mini/kimi-k3 通过 WebClaw 创建。"}' \
        '</|DSML|parameter>'
READ = '<|DSML|parameter name="name" string="true">workspace_read\n' \
       '<|DSML|parameter name="arguments" string="false">{"path":"notes.md"}</|DSML|parameter>'
DONE = "已完成：已创建 notes.md 并读回内容。"


class Mock(BaseHTTPRequestHandler):
    calls = []

    def log_message(self, *args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("content-length", "0"))))
        self.calls.append(body)
        tool_results = sum(1 for m in body.get("messages", []) if m.get("role") == "tool")
        content = WRITE if tool_results == 0 else READ if tool_results == 1 else DONE
        if body.get("stream"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            frames = [
                {"model": "mini/kimi-k3", "choices": [{"delta": {"content": content}}]},
                {"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 20}},
            ]
            for frame in frames:
                self.wfile.write(("data: " + json.dumps(frame, ensure_ascii=False) + "\n\n").encode())
            self.wfile.write(b"data: [DONE]\n\n")
        else:
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({
                "model": "mini/kimi-k3",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120},
            }, ensure_ascii=False).encode())


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def main():
    upstream = ThreadingHTTPServer(("127.0.0.1", 0), Mock)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    port = free_port()
    base = f"http://127.0.0.1:{port}"
    with tempfile.TemporaryDirectory(prefix="ngw-webclaw-agent-") as tmp:
        env = {**os.environ, "NGW_DATA_DIR": tmp, "NGW_ADMIN_PASSWORD": ADMIN_PW}
        proc = subprocess.Popen([GW, "--port", str(port)], cwd=str(ROOT), env=env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            with httpx.Client(base_url=base, trust_env=False, timeout=30) as admin:
                for _ in range(120):
                    try:
                        if admin.get("/").status_code == 200:
                            break
                    except httpx.ConnectError:
                        pass
                    time.sleep(.1)
                r = admin.post("/api/login", json={"username": "admin", "password": ADMIN_PW})
                assert r.status_code == 200, r.text
                admin.headers["X-CSRF"] = r.json()["csrf"]
                r = admin.post("/api/settings", json={"config": {
                    "gateway_tokens": ["root-token"], "transparent_upstream": False,
                    "model_chains": "", "account_cooldown_ms": 0, "warmup_seconds": 0}})
                assert r.status_code == 200, r.text
                r = admin.post("/api/upstreams", json={
                    "id": "mock-channel", "name": "Mock", "enabled": True,
                    "base": f"http://127.0.0.1:{upstream.server_port}/v1",
                    "models": ["mini/kimi-k3"], "prices": {"mini/kimi-k3": 0}})
                assert r.status_code == 200, r.text
                r = admin.post("/api/keys/import", json={
                    "text": "mock@example.test,password,nvapi-mock-key", "upstream_id": "mock-channel"})
                assert r.status_code == 200, r.text
                r = admin.post("/api/upstream-models/op", json={
                    "channel_id": "mock-channel", "upstream": "mini/kimi-k3",
                    "alias": "mini/kimi-k3", "ctx": 8000, "out": 1024, "tps": 60})
                assert r.status_code == 200, r.text

                from playwright.sync_api import sync_playwright
                shots = Path(os.environ.get("NGW_SCREENSHOT_DIR", Path(tmp) / "shots"))
                shots.mkdir(parents=True, exist_ok=True)
                with sync_playwright() as pw:
                    browser = pw.chromium.launch(headless=True, executable_path=os.environ.get("NGW_CHROMIUM"))
                    context = browser.new_context(viewport={"width": 1440, "height": 1000})
                    context.add_cookies([{"name": c.name, "value": c.value, "url": base} for c in admin.cookies.jar])
                    page = context.new_page()
                    errors = []
                    page.on("pageerror", lambda e: errors.append(str(e)))
                    page.goto(base + "/webclaw")
                    page.wait_for_function("() => document.querySelector('#wc-model')?.options.length > 0")
                    page.select_option("#wc-model", "mini/kimi-k3")
                    page.check("#wc-agent")
                    page.fill("#wc-input", "创建一个 notes.md，然后读回内容")
                    page.click("#wc-send")
                    page.wait_for_selector(".tool-card", timeout=30000)
                    page.wait_for_function("() => document.querySelectorAll('.tool-card').length >= 2", timeout=30000)
                    page.wait_for_function("() => document.querySelector('#wc-chat')?.innerText.includes('已完成')", timeout=30000)
                    page.wait_for_timeout(300)
                    # 弹窗必须是全屏遮罩并居中，而不是从页面底部冒出来
                    page.evaluate("() => { window.__dlg = window.NGW_UI.dialog({title:'测试', body:'内容'}); }")
                    page.wait_for_selector(".ui-mask")
                    box = page.evaluate("""() => {
                      const m = document.querySelector('.ui-mask').getBoundingClientRect();
                      const d = document.querySelector('.ui-dialog').getBoundingClientRect();
                      return {mw:m.width, mh:m.height, my:m.y, dx:d.x, dy:d.y, dw:d.width, dh:d.height,
                              vw:innerWidth, vh:innerHeight, pos:getComputedStyle(document.querySelector('.ui-mask')).position};
                    }""")
                    assert box["pos"] == "fixed", box
                    assert box["my"] == 0 and box["mh"] >= box["vh"], box
                    assert abs(box["dx"] + box["dw"] / 2 - box["vw"] / 2) < 4, box
                    assert abs(box["dy"] + box["dh"] / 2 - box["vh"] / 2) < 4, box
                    page.keyboard.press("Escape")
                    page.screenshot(path=str(shots / "webclaw-agent.png"), full_page=True)
                    cards = page.locator(".tool-card").count()
                    body = page.locator("#wc-chat").inner_text()
                    files = page.evaluate("""() => {
                      const s = JSON.parse(localStorage.getItem('webclaw.projects.v1::admin') || '{}');
                      const p = (s.projects || []).find(x => x.id === s.active) || {};
                      return p.files || {};
                    }""")
                    assert cards >= 2, f"tool cards={cards}"
                    assert "DSML" not in body and "｜DSML｜" not in body, "正文不应出现 DSML 标签"
                    assert "notes.md" in body, body[:500]
                    assert files.get("notes.md", "").startswith("# WebAgent 项目"), files
                    assert not errors, errors
                    print(f"PASS WebClaw DSML 工具调用完成项目 | cards={cards} | files={list(files)}")
                    print(f"截图: {shots / 'webclaw-agent.png'}")
                    browser.close()
        finally:
            proc.terminate()
            proc.wait(timeout=10)
            upstream.shutdown()


if __name__ == "__main__":
    main()