# -*- coding: utf-8 -*-
"""01Code 客户端接口端到端验证。

覆盖：广告下发/过滤、后台广告校验、attest HMAC 登记、请求 Ed25519 验签、
官方客户端折扣与免费模型、验签失败时按普通调用处理（不拒绝请求）。
"""
import base64
import hashlib
import hmac
import json
import os
import socket
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import httpx
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

ROOT = Path(__file__).resolve().parents[1]
SECRET = "client01-e2e-secret-0123456789"


def check(value, label):
    assert value, label
    print("PASS", label, flush=True)


def b64url(raw):
    return base64.urlsafe_b64encode(raw).decode().rstrip("=")


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path.endswith("/models"):
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({"object": "list", "data": [{"id": "price-model"}]}).encode())
        else:
            self.send_response(404)
            self.end_headers()

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        body = json.loads(raw or b"{}")
        usage = {"prompt_tokens": 1000, "completion_tokens": 100, "total_tokens": 1100}
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps({
            "id": "c1",
            "object": "chat.completion",
            "model": body.get("model", ""),
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": usage,
        }).encode())


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    port = free_port()
    base = f"http://127.0.0.1:{port}"
    password = "client01-e2e-password"
    work = ROOT / "target" / "e2e-tmp"
    work.mkdir(parents=True, exist_ok=True)
    with __import__("tempfile").TemporaryDirectory(prefix="ngw-client01-", dir=work) as tmp:
        env = {**os.environ, "NGW_DATA_DIR": tmp, "NGW_ADMIN_PASSWORD": password}
        binary = os.environ.get("NGW_GW_EXE", str(ROOT / "target" / "debug" / "nim-gateway.exe"))
        with open(Path(tmp) / "stderr.log", "w+") as log:
            process = subprocess.Popen([binary, "--port", str(port)], cwd=ROOT, env=env,
                                       stdout=subprocess.DEVNULL, stderr=log)
            try:
                with httpx.Client(base_url=base, trust_env=False, timeout=30) as admin, \
                        httpx.Client(base_url=base, trust_env=False, timeout=30) as user:
                    for _ in range(150):
                        try:
                            if admin.get("/").status_code == 200:
                                break
                        except httpx.ConnectError:
                            pass
                        time.sleep(.1)

                    r = admin.post("/api/login", json={"username": "admin", "password": password})
                    check(r.status_code == 200, "管理员登录")
                    admin.headers["X-CSRF"] = r.json()["csrf"]

                    # 未启用 attest 时接口返回 404，客户端静默跳过
                    check(admin.post("/api/client/attest", json={}).status_code == 404,
                          "未启用 attest 时返回 404")
                    check(admin.post("/api/client01/config", json={"config": {"attest_enabled": True}}).status_code == 400,
                          "启用 attest 前必须配置共享密钥")
                    check(admin.post("/api/client01/config", json={
                        "config": {"discount_ratio": 0.01}}).status_code == 400,
                        "拒绝低于 10% 的折扣比例")

                    # 广告校验
                    bad = [{"id": "bad", "title": "x", "url": "http://example.com"}]
                    check(admin.post("/api/client01/ads", json={"ads": bad}).status_code == 400,
                          "拒绝非 HTTPS 广告链接")
                    check(admin.post("/api/client01/ads", json={"ads": [
                        {"id": "bad2", "title": "x", "accent": "red"}]}).status_code == 400,
                        "拒绝非法主色")
                    ads = [
                        {"id": "free-ad", "title": "免费模型专属", "body": "免费模型展示", "badge": "专属",
                         "cta": "查看", "url": "https://ai.gzmath.cn", "accent": "#315FE9",
                         "placement": "corner", "free_only": True, "models": ["price-model"],
                         "max_displays": 2, "cooldown_secs": 3600},
                        {"id": "all-ad", "title": "全部模型", "body": "全部模型展示", "placement": "tool",
                         "image": "https://cdn.example.com/ad.png", "layout": "compact", "image_ratio": "banner"},
                        {"id": "off-ad", "title": "已停用", "body": "不应下发", "enabled": False},
                        {"id": "new-ad", "title": "高版本", "body": "版本不足", "min_version": "9.9.9"},
                    ]
                    r = admin.post("/api/client01/ads", json={"ads": ads})
                    check(r.status_code == 200 and len(r.json()["ads"]) == 4, "保存并规范化广告")
                    check(r.json()["ads"][0]["accent"] == "#315FE9" and r.json()["ads"][0]["url"] == "https://ai.gzmath.cn",
                          "广告主色与链接原样保留")
                    check(r.json()["ads"][1]["image"] == "https://cdn.example.com/ad.png"
                          and r.json()["ads"][1]["layout"] == "compact"
                          and r.json()["ads"][1]["image_ratio"] == "banner",
                          "广告支持图片、版式与图片比例")
                    r = admin.post("/api/client01/config", json={"config": {
                        "ads_enabled": True, "ads_free_only": False,
                        "attest_enabled": True, "discount_ratio": 0.5,
                        "discount_min_version": "0.0.1",
                    }, "secret": SECRET})
                    check(r.status_code == 200 and r.json()["config"]["secret_set"] is True,
                          "保存 attest 密钥并返回已配置标记")
                    check("client01_attest_secret" not in admin.get("/api/settings").json(),
                          "设置接口不回显共享密钥")

                    # 用户与渠道
                    check(admin.post("/api/users", json={
                        "username": "c01-user", "password": password, "balance": 1}).status_code == 200,
                        "创建用户")
                    channel = {"id": "c01-channel", "name": "01Code", "enabled": True,
                               "base": f"http://127.0.0.1:{server.server_port}/v1",
                               "models": ["price-model", "paid-model"],
                               "prices": {"price-model": {"mode": "token", "input": 2, "output": 8},
                                          "paid-model": {"mode": "token", "input": 4, "output": 16}}}
                    check(admin.post("/api/upstreams", json=channel).status_code == 200, "创建计费渠道")
                    check(admin.post("/api/keys/import", json={
                        "text": "c01@example.test,password,nvapi-c01-key",
                        "upstream_id": "c01-channel"}).status_code == 200, "导入上游 Key")
                    r = user.post("/api/user/login", json={"username": "c01-user", "password": password})
                    check(r.status_code == 200, "用户登录")
                    user.headers["X-CSRF"] = r.json()["csrf"]
                    key = user.post("/api/user/test-key").json()["key"]
                    with httpx.Client(base_url=base, trust_env=False, timeout=20) as c:
                        h = {"Authorization": "Bearer " + key, "X-01Code-Client": "01Code/0.0.1"}
                        rows = {m["id"]: m for m in c.get("/v1/models", headers=h).json()["data"]}
                        check(rows["price-model"]["discount"] == 50.0
                              and rows["price-model"]["limited_free"] is False,
                              "模型列表向 01Code 暴露绿色折扣比例")
                        check(rows["paid-model"]["discount"] == 50.0, "优惠模型按比例返回折扣")

                    # 广告下发过滤
                    with httpx.Client(base_url=base, trust_env=False, timeout=20) as c:
                        h = {"Authorization": "Bearer " + key, "X-01Code-Client": "01Code/0.0.1"}
                        got = c.get("/v1/client/ads?free=1&model=price-model", headers=h)
                        ids = {a["id"] for a in got.json()["ads"]}
                        check(got.status_code == 200 and ids == {"free-ad", "all-ad"},
                              "免费模型只下发时间/版本/状态均有效的广告")
                        paid = c.get("/v1/client/ads?free=0&model=price-model", headers=h)
                        check({a["id"] for a in paid.json()["ads"]} == {"all-ad"},
                              "付费模型过滤 free_only 广告")
                        other = c.get("/v1/client/ads?free=1&model=other-model", headers=h)
                        check({a["id"] for a in other.json()["ads"]} == {"all-ad"},
                              "指定模型广告不投放到其他模型")
                        check(c.get("/v1/client/ads", headers={"Authorization": "Bearer bad"}).status_code == 401,
                              "广告接口拒绝无效 Key")
                        check(c.get("/v1/client/ads", headers={"Authorization": "Bearer " + key}).json()["ads"] == [],
                              "缺少客户端标识时不投广告")

                    # 投放统计：曝光/独立用户/点击/关闭
                    with httpx.Client(base_url=base, trust_env=False, timeout=20) as c:
                        h = {"Authorization": "Bearer " + key, "X-01Code-Client": "01Code/0.0.1"}
                        for _ in range(2):
                            check(c.post("/v1/client/ads/event", headers=h,
                                         json={"ad_id": "all-ad", "event": "impression"}).status_code == 200,
                                  "上报广告曝光")
                        check(c.post("/v1/client/ads/event", headers=h,
                                     json={"ad_id": "all-ad", "event": "click"}).status_code == 200,
                              "上报广告点击")
                        check(c.post("/v1/client/ads/event", headers=h,
                                     json={"ad_id": "all-ad", "event": "close"}).status_code == 200,
                              "上报广告关闭")
                        check(c.post("/v1/client/ads/event", headers=h,
                                     json={"ad_id": "missing", "event": "impression"}).status_code == 400,
                              "拒绝不存在的广告统计")
                        check(c.post("/v1/client/ads/event", headers=h,
                                     json={"ad_id": "all-ad", "event": "view"}).status_code == 400,
                              "拒绝未知统计事件")
                    stats = admin.get("/api/client01").json()["stats"]
                    row = next(r for r in stats["ads"] if r["id"] == "all-ad")
                    check(stats["totals"]["impressions"] == 2 and stats["totals"]["unique"] == 1
                          and stats["totals"]["clicks"] == 1 and stats["totals"]["closes"] == 1,
                          "后台统计广告总曝光/独立用户/点击/关闭")
                    check(row["ctr"] == 50.0, "广告点击率按曝光计算")
                    check(admin.post("/api/client01/stats/reset", json={}).json()["stats"]["totals"]["impressions"] == 0,
                          "后台可清空广告统计")

                    # Ed25519 + attest
                    private = Ed25519PrivateKey.generate()
                    public_raw = private.public_key().public_bytes_raw()
                    public_b64 = base64.b64encode(public_raw).decode()
                    key_id = b64url(hashlib.sha256(public_raw).digest()[:16])
                    version = "0.0.1"

                    def proof_for(ts, nonce, bad=False):
                        text = f"01code\n{version}\n{public_b64}\n{key_id}\n{ts}\n{nonce}"
                        mac = hmac.new(SECRET.encode(), text.encode(), hashlib.sha256).digest()
                        if bad:
                            mac = bytes([mac[0] ^ 1]) + mac[1:]
                        return base64.b64encode(mac).decode()

                    def attest(nonce, bad=False):
                        ts = int(time.time())
                        return user.post("/api/client/attest", headers={
                            "Authorization": "Bearer " + key,
                            "X-01Code-Client": "01Code/" + version,
                        }, json={
                            "client_id": "01code", "version": version,
                            "public_key": public_b64, "key_id": key_id,
                            "timestamp": ts, "nonce": nonce,
                            "proof": proof_for(ts, nonce, bad),
                        })

                    nonce = b64url(os.urandom(16))
                    r = attest(nonce)
                    check(r.status_code == 200 and r.json().get("attest_token") and r.json().get("expires_in") == 3600,
                          "attest proof 通过并签发短期令牌")
                    token = r.json()["attest_token"]
                    check(attest(nonce).status_code == 400, "attest nonce 防重放")
                    check(attest(b64url(os.urandom(16)), bad=True).status_code == 401,
                          "错误 attest proof 被拒绝")

                    def signed_call(body, token_value=token, break_sig=False, nonce_value=None):
                        raw = json.dumps(body, separators=(",", ":")).encode()
                        ts = int(time.time())
                        nonce_value = nonce_value or b64url(os.urandom(16))
                        canonical = f"POST\n/v1/chat/completions\n{ts}\n{nonce_value}\n{b64url(hashlib.sha256(raw).digest())}"
                        sig = private.sign(canonical.encode())
                        if break_sig:
                            sig = bytes([sig[0] ^ 1]) + sig[1:]
                        headers = {
                            "Authorization": "Bearer " + key,
                            "Content-Type": "application/json",
                            "X-01Code-Client": "01Code/" + version,
                            "X-01Code-Sig-Version": "1",
                            "X-01Code-Key-Id": key_id,
                            "X-01Code-Public-Key": public_b64,
                            "X-01Code-Timestamp": str(ts),
                            "X-01Code-Nonce": nonce_value,
                            "X-01Code-Signature": b64url(sig),
                        }
                        if token_value:
                            headers["X-01Code-Attest"] = token_value
                        return user.post("/v1/chat/completions", content=raw, headers=headers)

                    def balance():
                        return user.get("/api/user/wallet").json()["wallet"]["balance"]

                    before = balance()
                    r = signed_call({"model": "price-model", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 100})
                    check(r.status_code == 200 and "ok" in r.text, "签名 + attest 的模型调用成功")
                    check(abs((before - balance()) - 0.0014) < 1e-9,
                          "验证通过的 01Code 调用按 50% 计费")

                    before = balance()
                    r = signed_call({"model": "price-model", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 100},
                                    break_sig=True)
                    check(r.status_code == 200, "验签失败不拒绝正常请求")
                    check(abs((before - balance()) - 0.0028) < 1e-9,
                          "验签失败按原价计费")

                    # 免费模型：验证通过时价格为 0
                    r = admin.post("/api/client01/config", json={"config": {"free_models": "price-model"}})
                    check(r.status_code == 200, "保存 01Code 免费模型")
                    with httpx.Client(base_url=base, trust_env=False, timeout=20) as c:
                        h = {"Authorization": "Bearer " + key, "X-01Code-Client": "01Code/0.0.1"}
                        rows = {m["id"]: m for m in c.get("/v1/models", headers=h).json()["data"]}
                        check(rows["price-model"]["limited_free"] is True
                              and rows["price-model"]["discount"] == 0.0,
                              "限免模型在模型列表标记 limited_free")
                        check(rows["paid-model"]["limited_free"] is False
                              and rows["paid-model"]["discount"] == 50.0,
                              "非限免付费模型继续返回绿色折扣")
                    before = balance()
                    r = signed_call({"model": "price-model", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 100})
                    check(r.status_code == 200 and abs(before - balance()) < 1e-9,
                          "验证通过时 01Code 免费模型不计费")

                    # 全局仅免费模型：付费模型请求即使带 free=0 也不投广告
                    admin.post("/api/client01/config", json={"config": {"ads_free_only": True}})
                    with httpx.Client(base_url=base, trust_env=False, timeout=20) as c:
                        h = {"Authorization": "Bearer " + key, "X-01Code-Client": "01Code/0.0.1"}
                        check(c.get("/v1/client/ads?free=0&model=price-model", headers=h).json()["ads"] == [],
                              "全局仅免费模型时付费模型不下发广告")
                        check(len(c.get("/v1/client/ads?free=1&model=price-model", headers=h).json()["ads"]) == 2,
                              "全局仅免费模型时免费模型正常下发")
            finally:
                process.terminate()
                process.wait(timeout=10)
                server.shutdown()


if __name__ == "__main__":
    main()
