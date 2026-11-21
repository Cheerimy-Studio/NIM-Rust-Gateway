"""Console regression checks against an isolated gateway instance.

cargo build --locked
python tests/console_e2e.py          # HTTP checks
python tests/console_e2e.py --ui     # also run Chromium checks (Playwright)
"""
import argparse
import json
import os
from pathlib import Path
import socket
import secrets
import subprocess
import tempfile
import time

import httpx


def check(condition, message):
    if not condition:
        raise AssertionError(message)
    print('PASS', message, flush=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--ui', action='store_true')
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    binary = Path(os.environ.get('NGW_GW_EXE', root / 'target' / 'debug' / ('nim-gateway.exe' if os.name == 'nt' else 'nim-gateway')))
    password = 'console-regression-password'
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    base = f'http://127.0.0.1:{port}'
    with tempfile.TemporaryDirectory(prefix='ngw-console-') as tmp:
        env = {**os.environ, 'NGW_DATA_DIR': tmp, 'NGW_ADMIN_PASSWORD': password}
        with open(Path(tmp) / 'stderr.log', 'w+') as log:
            process = subprocess.Popen([str(binary), '--port', str(port)], cwd=root, env=env,
                                       stdout=subprocess.DEVNULL, stderr=log)
            try:
                with httpx.Client(base_url=base, trust_env=False, timeout=30) as admin:
                    for _ in range(150):
                        try:
                            if admin.get('/').status_code == 200:
                                break
                        except httpx.ConnectError:
                            pass
                        if process.poll() is not None:
                            log.seek(0)
                            raise RuntimeError(log.read())
                        time.sleep(.1)
                    response = admin.post('/api/login', json={'username': 'admin', 'password': password})
                    check(response.status_code == 200, '管理员登录')
                    admin.headers['X-CSRF'] = response.json()['csrf']
                    with httpx.Client(base_url=base, trust_env=False, timeout=30) as probe:
                        forged = probe.post('/api/login', content='{"username":"admin","password":"x"}',
                                            headers={'Content-Type': 'text/plain'})
                        check(forged.status_code == 415, '后台登录拒绝 text/plain 表单（登录 CSRF）')
                        cross = probe.post('/api/login', json={'username': 'admin', 'password': password},
                                           headers={'Origin': 'https://evil.example'})
                        check(cross.status_code == 403, '后台登录拒绝跨站 Origin')
                        forged = probe.post('/api/user/login', content='{"username":"x","password":"y"}',
                                            headers={'Content-Type': 'text/plain'})
                        check(forged.status_code == 415, '用户登录拒绝 text/plain 表单（登录 CSRF）')
                        cross = probe.post('/api/user/login', json={'username': 'console-user', 'password': password},
                                           headers={'Sec-Fetch-Site': 'cross-site'})
                        check(cross.status_code == 403, '用户登录拒绝 cross-site 请求')
                        check(admin.post('/api/settings', json={'config': {
                            'allowed_origins': 'https://cdn.example, *.example.net'}}).status_code == 200,
                              '保存允许的登录来源')
                        cdn = probe.post('/api/login', json={'username': 'admin', 'password': password},
                                         headers={'Origin': 'https://cdn.example'})
                        check(cdn.status_code == 200, 'CDN 域名可登录')
                        wildcard = probe.post('/api/login', json={'username': 'admin', 'password': password},
                                              headers={'Origin': 'https://sub.example.net'})
                        check(wildcard.status_code == 200, '通配域名可登录')
                        cross = probe.post('/api/login', json={'username': 'admin', 'password': password},
                                           headers={'Origin': 'https://evil.example'})
                        check(cross.status_code == 403, '未允许的域名仍拒绝登录')
                    response = admin.post('/api/users', json={'username': 'console-user', 'password': password, 'balance': 2})
                    check(response.status_code == 200, '创建测试用户')
                    response = admin.post('/api/upstreams', json={
                        'id': 'console-channel', 'name': '测试渠道', 'base': 'http://127.0.0.1:9/v1',
                        'enabled': True, 'models': ['test-free', 'test-paid'], 'prices': {'test-paid': .1}})
                    check(response.status_code == 200, '保存模型渠道')
                    response = admin.post('/api/settings', json={'config': {
                        'transparent_upstream': False, 'sign_enabled': True, 'sign_min': .01, 'sign_max': .01}})
                    check(response.status_code == 200, '保存签到与路由设置')
                    for path in ['/api/wheels','/api/promo','/api/prize-keys','/api/user-draws']:
                        check(admin.get(path).status_code == 404, '已移除旧接口 ' + path)
                    with httpx.Client(base_url=base, trust_env=False, timeout=30) as user:
                        for _ in range(10):
                            response = user.post('/api/user/login', json={'username': 'locked-account', 'password': 'wrong'})
                            check(response.status_code == 401, '限流预算内拒绝错误密码')
                        response = user.post('/api/user/login', json={'username': 'locked-account', 'password': 'wrong'})
                        check(response.status_code == 429 and response.headers.get('retry-after') == '300', '第 11 次验证前限流')
                        response = user.post('/api/user/login', json={'username': 'console-user', 'password': password})
                        check(response.status_code == 200, '共享来源下另一账户仍可登录')
                        user.headers['X-CSRF'] = response.json()['csrf']
                        reset = user.post('/api/user/forgot', headers={'Host': 'evil.example'},
                                          json={'email': 'console-user@example.test'})
                        check(reset.status_code == 503 and 'public_origin' in reset.text,
                              '未配置公开入口时拒绝生成重置链接，不信任 Host')
                        page_resp = admin.get('/admin')
                        check("frame-ancestors 'none'" in (page_resp.headers.get('content-security-policy') or ''),
                              '管理页返回 CSP 安全头')
                        models = user.get('/api/user/models').json()['rows']
                        check({m['model'] for m in models} == {'test-free', 'test-paid'}, '用户模型广场与渠道配置联动')
                        check(user.post('/api/user/sign').status_code == 200, '签到成功')
                        check(user.post('/api/user/sign').status_code == 400, '重复签到不再入账')
                        for path in ['/api/user/wheels','/api/user/prize-keys','/api/user/promo','/api/user/draw-logs']:
                            check(user.get(path).status_code == 404, '用户旧接口已移除 ' + path)
                        for path, mime in [('ui.css', 'text/css'), ('ui.js', 'javascript'),
                            ('vendor/bootstrap.min.css', 'text/css'), ('vendor/bootstrap.bundle.min.js', 'javascript'),
                            ('vendor/fonts/bootstrap-icons.woff2', 'font/woff2'), ('vendor/purify.min.js', 'javascript')]:
                            response = user.get('/assets/' + path)
                            check(response.status_code == 200 and mime in response.headers.get('content-type', ''), '内嵌资源 ' + path)
                        check(user.get('/assets/vendor/unknown').status_code == 404, '资源路径采用白名单')
                        check('{{VERSION}}' not in user.get('/user').text, '用户页面版本占位已替换')
                        old_user_cookie = user.cookies.get('ngw_user')
                        check(user.post('/api/user/logout').status_code == 200, '用户退出登录')
                        with httpx.Client(base_url=base, trust_env=False, timeout=30,
                                          cookies={'ngw_user': old_user_cookie}) as replay:
                            check(replay.get('/api/user/me').status_code == 401, '退出后旧用户 Cookie 立即失效')
                    old_admin_cookie = admin.cookies.get('ngw_session')
                    check(admin.post('/api/logout').status_code == 200, '管理员退出登录')
                    with httpx.Client(base_url=base, trust_env=False, timeout=30,
                                      cookies={'ngw_session': old_admin_cookie}) as replay:
                        check(replay.get('/api/overview').status_code == 401, '退出后旧管理员 Cookie 立即失效')
                    response = admin.post('/api/login', json={'username': 'admin', 'password': password})
                    check(response.status_code == 200, '重新登录后台继续 UI 检查')
                    admin.headers['X-CSRF'] = response.json()['csrf']
                    if args.ui:
                        run_browser(base, admin, password)
            finally:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


def run_browser(base, admin, password):
    from playwright.sync_api import sync_playwright, expect
    with sync_playwright() as pw:
        browser = pw.chromium.launch(headless=True, executable_path=os.environ.get('NGW_CHROMIUM'))
        context = browser.new_context(viewport={'width': 1440, 'height': 1000})
        errors, external = [], []
        context.on('page', lambda page: page.on('pageerror', lambda error: errors.append(str(error))))
        def route(request):
            if not request.request.url.startswith(base):
                external.append(request.request.url)
                request.abort()
            else:
                request.continue_()
        context.route('**/*', route)
        context.add_cookies([{'name': c.name, 'value': c.value, 'url': base} for c in admin.cookies.jar])
        session_cookie = next((c['value'] for c in context.cookies() if c['name'] == 'ngw_session'), '')
        check(session_cookie.startswith('v2.') and session_cookie.count('.') == 3, '管理员会话带签发时间、会话 ID 与签名')
        admin_page = context.request.get(base + '/admin')
        check("frame-ancestors 'none'" in (admin_page.headers.get('content-security-policy') or ''), '管理页启用 CSP 防嵌入与外部脚本')
        check((context.request.get(base + '/queue').headers.get('x-frame-options') or '') == 'DENY', '排队页禁止被第三方页面嵌入')
        bad_context = browser.new_context(viewport={'width': 900, 'height': 700})
        bad_context.add_cookies([{'name': 'ngw_session', 'value': 'v1.1.deadbeef', 'url': base}])
        bad_page = bad_context.new_page()
        bad_page.goto(base + '/admin')
        expect(bad_page.locator('#login-form')).to_be_visible()
        check(True, '伪造或过期管理员会话被拒绝')
        bad_context.close()
        page = context.new_page()
        page.goto(base + '/admin#users')
        expect(page.locator('#pane-users')).to_be_visible()
        expect(page.locator('#usr-rows')).to_contain_text('console-user')
        check(page.locator('.sidebar').evaluate('(e) => getComputedStyle(e).backgroundColor') == 'rgb(255, 255, 255)', '管理端统一浅色导航')
        page.reload()
        expect(page.locator('#pane-users')).to_be_visible()
        check(True, '刷新保留管理端当前页面')
        page.locator('#usr-rows button').filter(has_text='赠金').click()
        dialog = page.get_by_role('dialog')
        expect(dialog).to_be_visible()
        expect(dialog.locator('input')).to_be_focused()
        page.keyboard.press('Escape')
        expect(dialog).to_have_count(0)
        check(True, '管理弹窗支持键盘关闭与焦点')
        page.goto(base + '/user')
        page.evaluate("localStorage.setItem('yz-notice-ok', '1')")
        page.reload()
        expect(page.locator('#login-view')).to_be_visible()
        page.locator('#login-user').fill('console-user')
        page.locator('#login-pass').fill(password)
        page.locator('#login-go').click()
        expect(page.locator('#panel-view')).to_be_visible(timeout=30000)
        page.locator('[data-p="keys"]').click()
        page.locator('#key-name').fill('浏览器回归 Key')
        page.locator('#key-add').click()
        expect(page.locator('#key-rows')).to_contain_text('浏览器回归 Key')
        expect(page.locator('.toast').last).to_be_visible()
        check(page.locator('#key-rows tr').first.locator('td').count() == 6, 'Key 列表列数与表头一致且提示可见')
        page.locator('#key-rows button').filter(has_text='删除').click()
        dialog = page.get_by_role('dialog')
        expect(dialog).to_be_visible()
        expect(dialog.get_by_role('button', name='取消')).to_be_focused()
        page.keyboard.press('Escape')
        expect(page.locator('#key-rows')).to_contain_text('浏览器回归 Key')
        check(True, '取消删除保留 Key')
        page.locator('[data-p="models"]').click()
        expect(page.locator('.model-card')).to_have_count(2)
        page.locator('#model-search').fill('paid')
        expect(page.locator('.model-card')).to_have_count(1)
        page.locator('#model-kind [data-kind="free"]').click()
        expect(page.locator('.empty-state')).to_be_visible()
        page.locator('#model-kind [data-kind="paid"]').click()
        with context.expect_page() as popup:
            page.locator('.model-actions a').click()
        model_page = popup.value
        model_page.wait_for_load_state()
        expect(model_page.locator('#wc-model')).to_have_value('test-paid', timeout=30000)
        check(True, '模型搜索筛选并携带选择进入测试台')
        check(model_page.locator('#wc-context').count() == 1 and model_page.locator('#wc-effort-btn').count() == 1,
              'WebClaw 提供上下文容量与思考强度入口')
        model_page.locator('#wc-context').click()
        expect(model_page.locator('#wc-context-pop')).to_be_visible()
        check(model_page.locator('#wc-ctx-rows .ctx-row').count() == 5, '上下文面板展示分类占用')
        model_page.locator('#wc-effort-btn').click()
        expect(model_page.locator('#wc-effort-pop')).to_be_visible()
        check(model_page.locator('#wc-effort-range').count() == 1, '思考强度使用滑杆')
        def github_stub(route):
            url = route.request.url
            if url.endswith('/repos/owner/repo'):
                body = {'full_name': 'owner/repo', 'default_branch': 'main', 'html_url': 'https://github.com/owner/repo'}
            elif '/actions/runs?' in url:
                body = {'workflow_runs': [{'id': 1, 'name': 'CI', 'status': 'completed', 'conclusion': 'success',
                    'event': 'push', 'created_at': '2026-01-01T00:00:00Z',
                    'html_url': 'https://github.com/owner/repo/actions/runs/1'}]}
            else:
                body = {}
            route.fulfill(status=200, content_type='application/json', body=json.dumps(body))
        model_page.route('https://api.github.com/**', github_stub)
        model_page.locator('#gh-repo').fill('owner/repo')
        model_page.locator('#gh-branch').fill('main')
        model_page.locator('#gh-token').fill('ghp_test_token')
        model_page.locator('#gh-allow').check()
        model_page.locator('#gh-bind').click()
        expect(model_page.locator('#gh-status')).to_contain_text('已绑定', timeout=5000)
        expect(model_page.locator('#gh-runs .gh-run')).to_have_count(1, timeout=5000)
        check(True, 'WebClaw 可绑定 GitHub 仓库并显示 Actions 运行')
        # User mode is selected by the server; use a context without the admin cookie.
        user_context = browser.new_context(viewport={'width':390,'height':844})
        user_context.route('**/*', route)
        user_context.add_cookies([c for c in context.cookies() if c['name'].startswith('ngw_u')])
        mobile = user_context.new_page()
        mobile.on('pageerror', lambda error: errors.append(str(error)))
        mobile.goto(base + '/user#models')
        expect(mobile.locator('.model-card')).to_have_count(2)
        check(mobile.locator('.model-card .heat-sq').count() == 40, '模型广场每个模型展示 20 格热力图')
        check(mobile.locator('.heat-legend').count() == 1, '模型广场展示热力图图例')
        check(mobile.evaluate('document.documentElement.scrollWidth <= innerWidth'), '390px 用户端无横向溢出')
        mobile.goto(base + '/webclaw?model=test-free')
        expect(mobile.locator('#wc-model')).to_have_value('test-free')
        expect(mobile.locator('#wc-scope')).to_contain_text('调用计入余额')
        check(True, '普通用户测试台沿用自己的身份和 Key')
        # Markdown sanitizer is exercised in a real browser, including an event-handler payload.
        check(model_page.evaluate("""() => {
          const clean = DOMPurify.sanitize(marked.parse('<img src=x onerror="window.pwned=1"><script>window.pwned=1</scr'+'ipt>'));
          return !clean.includes('onerror') && !clean.includes('<script');
        }"""), 'Markdown 危险标签与事件属性已过滤')
        page.goto(base + '/queue')
        expect(page.locator('#queue-status')).to_contain_text('更新于')
        page.route('**/api/queue/public', lambda request: request.fulfill(status=503, body='{}'))
        page.locator('#queue-retry').click()
        expect(page.locator('#queue-status')).to_contain_text('更新失败')
        check(True, '队列请求失败时显示过期数据提示')
        page.goto(base + '/admin#routing')
        page.reload()
        expect(page.locator('#pane-routing')).to_be_visible()
        expect(page.locator('#sec-chains')).to_be_visible()
        expect(page.locator('#sec-auto-routes')).to_be_visible()
        page.locator('#chain-add').click()
        chain = page.locator('#chain-rows tr').last
        chain.locator('.chain-name').fill('auto-best')
        chain.locator('.chain-members').fill('test-free, test-paid')
        with page.expect_response(lambda r: r.url.endswith('/api/settings') and r.request.method == 'POST'):
            page.locator('#chain-save').click()
        expect(page.locator('#chain-map .chain-card')).to_have_count(1, timeout=3000)
        check(True, '路由链路保存后生成成员流向图')
        page.locator('#auto-add').click()
        expect(page.locator('.auto-route-card')).to_have_count(1)
        card = page.locator('.auto-route-card').first
        card.locator('.auto-route-name').fill('auto-test')
        card.locator('.auto-rule-target').fill('auto-best')
        card.locator('.auto-test-text').fill('帮我画图')
        card.locator('.auto-test-run').click()
        expect(card.locator('.auto-test-result')).to_contain_text('auto-best')
        expect(card.locator('.auto-map .auto-branch')).to_have_count(1, timeout=3000)
        with page.expect_response(lambda r: r.url.endswith('/api/settings') and r.request.method == 'POST'):
            page.locator('#auto-save-all').click()
        page.reload()
        expect(page.locator('.auto-route-card')).to_have_count(1)
        expect(page.locator('.auto-route-card .auto-map .auto-branch')).to_have_count(1, timeout=3000)
        page.locator('#auto-add').click()
        expect(page.locator('.auto-route-card')).to_have_count(2)
        card2 = page.locator('.auto-route-card').nth(1)
        card2.locator('.auto-route-name').fill('auto-test-2')
        card2.locator('.auto-rule-target').fill('test-paid')
        with page.expect_response(lambda r: r.url.endswith('/api/settings') and r.request.method == 'POST'):
            page.locator('#auto-save-all').click()
        page.reload()
        expect(page.locator('.auto-route-card')).to_have_count(2)
        check(True, 'Auto 路由可新增多个、保存并按内容分支预览')
        page.goto(base + '/admin#analytics')
        page.reload()
        expect(page.locator('#pane-analytics')).to_be_visible()
        expect(page.locator('#an-cards .stat')).to_have_count(4)
        check(page.locator('#an-range button').count() == 3 and page.locator('#an-trend').count() == 1,
              '调用看板提供今日/7 天/14 天范围与每日趋势')
        check(page.locator('#an-models svg, #an-models .an-empty').count() == 1
              and page.locator('#an-channels svg, #an-channels .an-empty').count() == 1
              and page.locator('#an-paid svg, #an-paid .an-empty').count() == 1,
              '模型、渠道、付费占比区域渲染圆环或空状态')
        page.locator('#an-range button[data-days="1"]').click()
        expect(page.locator('#an-range button[data-days="1"]')).to_have_class('active')
        check(True, '调用看板可切换统计范围')
        page.goto(base + '/admin#catalog')
        page.reload()
        expect(page.locator('#pane-catalog')).to_be_visible()
        row = page.locator('#price-rows tr').filter(has_text='test-paid').first
        expect(row).to_be_visible(timeout=15000)
        check(page.locator('#price-add').count() == 0, '定价区不再提供任意模型加价入口')
        row.locator('.price-mode').select_option('token')
        expect(row.locator('.price-cache')).to_be_visible()
        check(True, '按 token 定价可直接填写缓存命中价')
        expect(page.locator('.cat-toggle').first).to_have_text('取消接入', timeout=15000)
        check(True, '模型接入页提供取消接入操作')
        headers = page.locator('#pane-catalog table').first.locator('thead th')
        expect(headers).to_have_count(12)
        check(headers.nth(7).inner_text().strip() == '定价', '模型接入表头与定价列对齐')
        check(headers.nth(9).inner_text().strip() == '近期', '模型接入表头包含近期热力图')
        check(page.locator('#cat-search').count() == 1 and page.locator('.cat-test').count() > 0 and page.locator('#cat-enable').count() == 1, '模型接入支持搜索、批量接入与测速')
        check(page.locator('#up-fetch').count() == 1 and page.locator('#up-hourcap').count() == 1 and page.locator('#up-fivehourcap').count() == 1, '渠道表单支持拉取模型与小时配额')
        check(page.locator('#up-probe').count() == 1, '渠道 Base URL 支持自动检测')
        check(page.locator('#up-internal').count() == 1, '渠道支持内部模型（仅 Auto 路由）')
        check('改 ID 会同步迁移' not in page.locator('#pane-upstreams').inner_text(), '渠道表单已去除冗余提示')
        check(page.locator('#cat-sum-exposed').inner_text() == '2', '概览条统计已接入模型')
        check(page.locator('#cat-sum-free').inner_text() == '1', '概览条统计免费模型')
        check(page.locator('#cat-rows tr').first.locator('td[title*="最近 10 次"]').count() == 1, '模型接入行包含近期状态列')
        check(not external, '浏览器不依赖外部 CDN')
        check(not errors, '页面无未处理 JavaScript 异常: ' + repr(errors))
        if os.environ.get('NGW_SCREENSHOT_DIR'):
            destination = Path(os.environ['NGW_SCREENSHOT_DIR'])
            destination.mkdir(parents=True, exist_ok=True)
            page.goto(base + '/admin')
            expect(page.locator('#dash-cards .stat')).to_have_count(6)
            page.screenshot(path=str(destination / 'admin.png'), full_page=True)
            page.goto(base + '/admin#analytics')
            page.reload()
            expect(page.locator('#pane-analytics')).to_be_visible()
            page.wait_for_timeout(400)
            page.screenshot(path=str(destination / 'analytics.png'), full_page=True)
            page.goto(base + '/admin#catalog')
            page.reload()
            expect(page.locator('#cat-rows tr').first).to_be_visible(timeout=15000)
            page.screenshot(path=str(destination / 'catalog.png'), full_page=True)
            page.goto(base + '/admin#upstreams')
            page.reload()
            expect(page.locator('#up-rows tr').first).to_be_visible(timeout=15000)
            page.screenshot(path=str(destination / 'upstreams.png'), full_page=True)
            page.goto(base + '/admin#routing')
            page.reload()
            expect(page.locator('#pane-routing')).to_be_visible()
            page.wait_for_timeout(300)
            page.screenshot(path=str(destination / 'routing.png'), full_page=True)
            mobile.goto(base + '/user#models')
            expect(mobile.locator('.model-card')).to_have_count(5)
            mobile.wait_for_timeout(350)
            mobile.screenshot(path=str(destination / 'mobile.png'), full_page=True)
            desk = browser.new_context(viewport={'width':1440,'height':1000})
            desk.route('**/*', route)
            desk.add_cookies([c for c in context.cookies() if c['name'].startswith('ngw_u')])
            dpage = desk.new_page()
            dpage.goto(base + '/user#models')
            expect(dpage.locator('.model-card')).to_have_count(5)
            dpage.wait_for_timeout(350)
            dpage.screenshot(path=str(destination / 'user-models.png'), full_page=True)
            dpage.goto(base + '/user#overview')
            dpage.reload()
            dpage.wait_for_timeout(350)
            dpage.screenshot(path=str(destination / 'user-overview.png'), full_page=True)
            dpage.goto(base + '/user#keys')
            dpage.reload()
            dpage.wait_for_timeout(350)
            dpage.screenshot(path=str(destination / 'user-keys.png'), full_page=True)
            dpage.goto(base + '/user#logs')
            dpage.reload()
            dpage.wait_for_timeout(350)
            dpage.screenshot(path=str(destination / 'user-logs.png'), full_page=True)
            desk.close()
            login_ctx = browser.new_context(viewport={'width':1440,'height':1000})
            login_ctx.route('**/*', route)
            lp = login_ctx.new_page()
            lp.goto(base + '/admin')
            lp.wait_for_timeout(350)
            lp.screenshot(path=str(destination / 'login.png'), full_page=True)
            lp.goto(base + '/user')
            expect(lp.locator('#login-view')).to_be_visible()
            lp.wait_for_timeout(350)
            lp.screenshot(path=str(destination / 'user-login.png'), full_page=True)
            login_ctx.close()
        user_context.close()
        context.close()
        browser.close()


if __name__ == '__main__':
    main()