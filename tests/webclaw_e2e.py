# -*- coding: utf-8 -*-
"""WebClaw 测试台端到端验证。

用法：cargo build --locked && python tests/webclaw_e2e.py

覆盖：
- 未登录访问 /webclaw 返回登录页（与 /admin 同一道门）
- 登录后 /webclaw 返回测试台页面，且资源引用打进版本号
- /assets/webclaw.js 可访问、禁缓存、含本地存储键
- 后台 /admin 不再残留旧「模型测试」面板，且侧栏指向 /webclaw
- 后端对 /webclaw 与 /assets/webclaw.js 都挂了路由（不是 404 兜底）

独立临时数据目录，不碰生产 data/。
"""

import os, sys, time, shutil, pathlib, subprocess, tempfile

try:
    import httpx
except ImportError:
    print("需要 httpx：pip install -r requirements-dev.txt")
    sys.exit(2)

os.environ["NO_PROXY"] = "*"
os.environ["no_proxy"] = "*"

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TMP = pathlib.Path(tempfile.mkdtemp(prefix="ngw-webclaw-e2e-"))
ADMIN_USER, ADMIN_PW = "admin", "ngw-webclaw-pass"
GW_PORT = int(os.environ.get("NGW_E2E_PORT", "18731"))


# 默认用 debug 二进制：release 可能正被运行中的实例占用
GW_EXE = os.environ.get("NGW_GW_EXE") or os.path.join(
    ROOT, "target", "debug", "nim-gateway" + (".exe" if os.name == "nt" else "")
)
if not os.path.isfile(GW_EXE):
    print("找不到网关二进制：%s\n请先执行 cargo build --locked" % GW_EXE)
    sys.exit(2)

env = {**os.environ, "NGW_DATA_DIR": str(TMP), "NGW_ADMIN_PASSWORD": ADMIN_PW}
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
    base = "http://127.0.0.1:%d" % GW_PORT
    up = False
    for _ in range(100):
        try:
            if httpx.get(base + "/", timeout=2).status_code == 200:
                up = True
                break
        except Exception:
            pass
        time.sleep(0.4)
    add("网关启动", up, base)
    if not up:
        raise SystemExit(1)

    anon = httpx.Client(base_url=base, timeout=30)

    # 1) 未登录：必须被挡回登录页，不能泄露测试台
    r = anon.get("/webclaw")
    add(
        "未登录被引导到用户中心登录",
        r.status_code in (302, 303, 307, 308) and r.headers.get("location", "").endswith("/user"),
        "st=%s loc=%s" % (r.status_code, r.headers.get("location")),
    )

    # 2) 登录
    a = httpx.Client(base_url=base, timeout=30)
    r = a.post("/api/login", json={"username": ADMIN_USER, "password": ADMIN_PW})
    add("admin 登录", r.status_code == 200, "st=%s" % r.status_code)
    a.headers["X-CSRF"] = r.json()["csrf"]

    # 3) 页面本体
    rw = a.get("/webclaw")
    add(
        "WebClaw 页面可访问",
        rw.status_code == 200 and bool(rw.headers.get("x-frame-options")),
        "st=%s xfo=%s" % (rw.status_code, rw.headers.get("x-frame-options")),
    )
    add(
        "页面结构完整",
        'id="wc-chat"' in rw.text
        and 'id="wc-send"' in rw.text
        and 'id="wc-projects"' in rw.text
        and 'id="wc-export-all"' in rw.text
        and 'id="wc-import"' in rw.text
        and 'id="wc-import-file"' in rw.text,
        "核心节点齐全",
    )
    add(
        "资源引用带版本号",
        "/assets/webclaw.js?v=" in rw.text,
        "避免升级后浏览器复用旧脚本",
    )
    add(
        "CSRF 已注入",
        "{{CSRF}}" not in rw.text and 'window.NGW' in rw.text,
        "模板占位符必须被替换",
    )
    add(
        "页面禁缓存",
        "no-cache" in (rw.headers.get("cache-control") or ""),
        rw.headers.get("cache-control") or "",
    )

    # 4) 脚本
    rj = a.get("/assets/webclaw.js?v=1.7.0")
    add(
        "脚本可访问",
        rj.status_code == 200 and "javascript" in (rj.headers.get("content-type") or ""),
        "st=%s ct=%s" % (rj.status_code, rj.headers.get("content-type")),
    )
    add(
        "脚本禁缓存",
        "no-cache" in (rj.headers.get("cache-control") or ""),
        rj.headers.get("cache-control") or "",
    )
    add(
        "本地存储键在位",
        "webclaw.projects.v1" in rj.text,
        "项目持久化",
    )
    add(
        "导出/导入实现在位",
        "kind: 'bundle'" in rj.text
        and "kind: 'single'" in rj.text
        and "extractProjects" in rj.text,
        "整包与单独项目格式",
    )

    # 5) 旧面板必须退役，否则两处重复且 nav 会被 activate() 拦住
    ra = a.get("/admin")
    add(
        "后台已移除旧模型测试面板",
        ra.status_code == 200 and 'id="pane-test"' not in ra.text,
        "st=%s" % ra.status_code,
    )
    add(
        "侧栏指向 /webclaw",
        'href="/webclaw"' in ra.text and 'data-pane="test"' not in ra.text,
        "外链导航",
    )
    rja = a.get("/assets/admin.js?v=1.7.0")
    add(
        "admin.js 无残留测试台逻辑",
        "loadTestModels" not in rja.text and "testMsgs" not in rja.text,
        "已整体迁出",
    )
    add(
        "admin.js 外链导航不被拦截",
        "if (!a.dataset.pane) return;" in rja.text,
        "否则点击 /webclaw 无反应",
    )

    # 6) 用户中心也必须能进 WebClaw，且用户侧看不到后台能力
    add(
        "管理员页注入 admin 模式",
        'mode: "admin"' in rw.text and 'who: "admin"' in rw.text,
        "模式与身份由服务端注入",
    )
    ru = a.get("/user")
    add(
        "用户中心已改为外链 WebClaw",
        ru.status_code == 200
        and 'href="/webclaw"' in ru.text
        and "t-send" not in ru.text,
        "st=%s" % ru.status_code,
    )
    rju = a.get("/assets/user.js?v=1.7.0")
    add(
        "user.js 无残留测试台逻辑",
        "tSend" not in rju.text and "t-chat" not in rju.text,
        "已整体迁出",
    )
    add(
        "user.js 外链导航不被拦截",
        "if (!a.dataset.p) return;" in rju.text or "querySelectorAll('.sidebar nav a[data-p]')" in rju.text,
        "否则点击 /webclaw 无反应",
    )

    # 建一个普通用户并登录，验证同一个页面在用户身份下可用
    u = httpx.Client(base_url=base, timeout=30)
    r = a.post("/api/users", json={"username": "wcuser", "password": "wc-user-pass", "balance": 10})
    add("创建测试用户", r.status_code == 200, "st=%s" % r.status_code)
    r = u.post("/api/user/login", json={"username": "wcuser", "password": "wc-user-pass"})
    add("普通用户登录", r.status_code == 200, "st=%s" % r.status_code)
    u.headers["X-CSRF"] = r.json().get("csrf", "")
    rw2 = u.get("/webclaw")
    add(
        "用户身份可打开 WebClaw",
        rw2.status_code == 200 and 'mode: "user"' in rw2.text and 'id="wc-chat"' in rw2.text,
        "st=%s" % rw2.status_code,
    )
    add(
        "用户侧身份独立分区",
        'who: ""' not in rw2.text and 'who: "admin"' not in rw2.text,
        "存储键不会与管理员共用",
    )
    # 用户会话拿不到后台接口：这是不能让普通用户越过的那条线
    add(
        "用户会话无法读取后台设置",
        u.get("/api/settings").status_code == 401,
        "st=%s" % u.get("/api/settings").status_code,
    )
    rk = u.post("/api/user/test-key")
    add(
        "用户可取自己的测试 Key",
        rk.status_code == 200 and str(rk.json().get("key", "")).startswith("sk-"),
        "st=%s" % rk.status_code,
    )
    add(
        "用户可见模型来自模型广场",
        u.get("/api/user/models").status_code == 200,
        "st=%s" % u.get("/api/user/models").status_code,
    )

finally:
    gw.terminate()
    try:
        gw.wait(timeout=5)
    except Exception:
        gw.kill()
        gw.wait()
    shutil.rmtree(TMP, ignore_errors=True)

ok_n = sum(1 for _, ok, _ in results if ok)
print("\n%d/%d 通过" % (ok_n, len(results)))
sys.exit(0 if ok_n == len(results) else 1)
