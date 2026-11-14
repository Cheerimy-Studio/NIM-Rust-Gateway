"""Console regression checks against an isolated gateway instance.

cargo build --locked
python tests/console_e2e.py          # HTTP checks
python tests/console_e2e.py --ui     # also run Chromium checks (Playwright)
"""
import argparse
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
        page.locator('#model-kind').select_option('free')
        expect(page.locator('.empty-state')).to_be_visible()
        page.locator('#model-kind').select_option('paid')
        with context.expect_page() as popup:
            page.locator('.model-actions a').click()
        model_page = popup.value
        model_page.wait_for_load_state()
        expect(model_page.locator('#wc-model')).to_have_value('test-paid', timeout=30000)
        check(True, '模型搜索筛选并携带选择进入测试台')
        # User mode is selected by the server; use a context without the admin cookie.
        user_context = browser.new_context(viewport={'width':390,'height':844})
        user_context.route('**/*', route)
        user_context.add_cookies([c for c in context.cookies() if c['name'].startswith('ngw_u')])
        mobile = user_context.new_page()
        mobile.on('pageerror', lambda error: errors.append(str(error)))
        mobile.goto(base + '/user#models')
        expect(mobile.locator('.model-card')).to_have_count(2)
        check(mobile.locator('.model-card .heat-sq').count() == 20, '模型广场每个模型展示 10 格热力图')
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
        expect(headers).to_have_count(11)
        check(headers.nth(7).inner_text().strip() == '定价', '模型接入表头与定价列对齐')
        check(headers.nth(9).inner_text().strip() == '近期', '模型接入表头包含近期热力图')
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
            page.goto(base + '/admin#catalog')
            page.reload()
            expect(page.locator('#cat-rows tr').first).to_be_visible(timeout=15000)
            page.screenshot(path=str(destination / 'catalog.png'), full_page=True)
            page.goto(base + '/admin#upstreams')
            page.reload()
            expect(page.locator('#up-rows tr').first).to_be_visible(timeout=15000)
            page.screenshot(path=str(destination / 'upstreams.png'), full_page=True)
            mobile.goto(base + '/user#models')
            expect(mobile.locator('.model-card')).to_have_count(2)
            mobile.screenshot(path=str(destination / 'mobile.png'), full_page=True)
        user_context.close()
        context.close()
        browser.close()


if __name__ == '__main__':
    main()
