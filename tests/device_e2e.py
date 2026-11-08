"""Run against an isolated gateway; forwarding headers emulate a trusted TLS proxy."""
import concurrent.futures
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import httpx

ROOT=Path(__file__).resolve().parents[1]
def check(ok,msg):
    assert ok,msg
    print('PASS',msg,flush=True)
def main():
    with socket.socket() as s:s.bind(('127.0.0.1',0));port=s.getsockname()[1]
    with tempfile.TemporaryDirectory(prefix='ngw-device-') as tmp:
        # 不配置任何设备登录环境变量：全部走后台设置，验证「功能可用」
        env={**os.environ,'NGW_DATA_DIR':tmp,'NGW_ADMIN_PASSWORD':'device-test-password'}
        with open(Path(tmp)/'server.log','w+') as log:
            p=subprocess.Popen([str(ROOT/'target/debug/nim-gateway'),'--port',str(port)],env=env,stdout=log,stderr=log)
            try:
                with httpx.Client(base_url=f'http://127.0.0.1:{port}',trust_env=False,timeout=30) as c:
                    for _ in range(100):
                        try:
                            if c.get('/').status_code==200:break
                        except httpx.ConnectError:pass
                        time.sleep(.1)
                    check(c.post('/api/device/code',json={'client_id':'01code','scope':'llm'}).status_code==400,'reject plaintext/untrusted protocol')
                    c.headers.update({'X-Forwarded-Proto':'https','X-Real-IP':'192.0.2.1'})
                    a=c.post('/api/login',json={'username':'admin','password':'device-test-password'}).json();c.headers['X-CSRF']=a['csrf']
                    for cookie in c.cookies.jar:cookie.secure=False
                    # 仅通过后台设置启用设备登录：公网入口 + 信任全部来源（*）
                    check(c.post('/api/settings',json={'config':{'public_origin':'https://gateway.example','trusted_proxies':'*'}}).status_code==200,'configure device login in admin settings')
                    check(c.post('/api/users',json={'username':'device-user','password':'device-test-password','balance':2,'free_rpm':10,'paid_rpm':10}).status_code==200,'create user')
                    u=c.post('/api/user/login',json={'username':'device-user','password':'device-test-password'}).json();c.headers['X-CSRF']=u['csrf']
                    # HTTP fixture emulates TLS at proxy; httpx correctly withholds Secure cookies on HTTP.
                    for cookie in c.cookies.jar:cookie.secure=False
                    def issue():
                        r=c.post('/api/device/code',json={'client_id':'01code','scope':'llm'});check(r.status_code==200,'issue code');return r.json()
                    def poll(g,client='01code'):
                        return c.post('/api/device/token',json={'client_id':client,'grant_type':'urn:ietf:params:oauth:grant-type:device_code','device_code':g['device_code']})
                    g=issue();check(len(g['user_code'])==8 and not set('0O1I')&set(g['user_code']),'readable code')
                    check(poll(g).json()['error']=='authorization_pending','pending')
                    check(poll(g).json()['error']=='slow_down','slow polling backoff')
                    check(poll(g,'other').json()['error']=='invalid_grant','client binding')
                    c.headers['X-Real-IP']='192.0.2.2';check(poll(g).json()['error']=='invalid_grant','IP binding');c.headers['X-Real-IP']='192.0.2.1'
                    check(c.get('/user/device',params={'code':g['user_code']}).status_code==200,'authorization page')
                    csrf=c.headers.pop('X-CSRF');check(c.post('/api/device/authorize',json={'code':g['user_code'],'decision':'allow'}).status_code==403,'authorization CSRF');c.headers['X-CSRF']=csrf
                    if os.environ.get('NGW_DEVICE_UI'):
                        from playwright.sync_api import sync_playwright
                        with sync_playwright() as pw:
                            browser=pw.chromium.launch(headless=True,executable_path=os.environ.get('NGW_CHROMIUM','/usr/bin/chromium'))
                            context=browser.new_context(extra_http_headers={'X-Forwarded-Proto':'https','X-Real-IP':'192.0.2.1'})
                            context.add_cookies([{'name':cookie.name,'value':cookie.value,'domain':'127.0.0.1','path':'/','secure':False,'httpOnly':True,'sameSite':'Lax'} for cookie in c.cookies.jar])
                            page=context.new_page();page.goto(str(c.base_url).rstrip('/')+f"/user/device?code={g['user_code']}")
                            check(page.get_by_text(g['user_code'],exact=True).is_visible(),'browser displays device code')
                            page.get_by_role('button',name='允许登录').click();page.wait_for_url(str(c.base_url).rstrip('/')+'/user')
                            page.locator('[data-p="devices"]').click();page.get_by_text('暂无已授权设备',exact=True).wait_for()
                            check(True,'browser authorization and devices pane')
                            browser.close()
                    else:
                        check(c.post('/api/device/authorize',json={'code':g['user_code'],'decision':'allow'}).status_code==200,'approve')
                    check(c.post('/api/device/authorize',json={'code':g['user_code'],'decision':'allow'}).status_code==400,'approval single use')
                    time.sleep(10.1)
                    # Race redemption: only one request receives a Key.
                    def redeem(_):
                        with httpx.Client(base_url=c.base_url,trust_env=False,headers={'X-Forwarded-Proto':'https','X-Real-IP':'192.0.2.1'}) as other:
                            return other.post('/api/device/token',json={'client_id':'01code','grant_type':'urn:ietf:params:oauth:grant-type:device_code','device_code':g['device_code']})
                    with concurrent.futures.ThreadPoolExecutor(2) as pool:results=list(pool.map(redeem,range(2)))
                    check(sum(r.status_code==200 for r in results)==1,'atomic redemption')
                    token=next(r.json()['access_token'] for r in results if r.status_code==200)
                    c.headers['Authorization']='Bearer '+token
                    check(c.get('/v1/me').json()['username']=='device-user','Bearer account')
                    check(len(c.get('/v1/usage?days=7').json())==7,'daily usage')
                    check(c.get('/v1/models').status_code==200,'device models')
                    check(c.delete('/api/device/token').status_code==204,'revoke')
                    check(c.get('/v1/me').status_code==401,'revoked identity rejected')
                    check(c.get('/v1/models').status_code==401,'revoked model calls rejected')
                    del c.headers['Authorization']
                    denied=issue();check(c.post('/api/device/authorize',json={'code':denied['user_code'],'decision':'deny'}).status_code==200,'deny');check(poll(denied).json()['error']=='access_denied','denied polling')
                    # 后台设置：公网入口/可信代理优先于环境变量；开关可整体停用。
                    # 此时 c 是用户会话，管理设置走独立的 admin 会话。
                    with httpx.Client(base_url=f'http://127.0.0.1:{port}',trust_env=False,timeout=30) as adm:
                        ar=adm.post('/api/login',json={'username':'admin','password':'device-test-password'})
                        adm.headers['X-CSRF']=ar.json()['csrf']
                        check(adm.post('/api/settings',json={'config':{'public_origin':'https://cfg.example'}}).status_code==200,'settings origin saved')
                        g2=issue();check(g2['verification_uri'].startswith('https://cfg.example/user/device'),'config origin takes precedence')
                        check(adm.post('/api/settings',json={'config':{'public_origin':'http://bad'}}).status_code==400,'reject non-https origin in settings')
                        check(adm.post('/api/settings',json={'config':{'trusted_proxies':'not-an-ip'}}).status_code==400,'reject non-ip trusted proxy')
                        check(adm.post('/api/settings',json={'config':{'trusted_proxies':'203.0.113.9'}}).status_code==200,'settings trusted saved')
                        check(c.post('/api/device/code',json={'client_id':'01code','scope':'llm'}).status_code==400,'config overrides env trust')
                        check(adm.post('/api/settings',json={'config':{'public_origin':'https://gateway.example','trusted_proxies':'*','device_login_enabled':False}}).status_code==200,'settings disable device login')
                        check(c.post('/api/device/code',json={'client_id':'01code','scope':'llm'}).status_code==404,'device login disabled => 404')
                        check(adm.post('/api/settings',json={'config':{'device_login_enabled':True}}).status_code==200,'settings re-enable device login')
                    for _ in range(2):issue()
                    check(c.post('/api/device/code',json={'client_id':'01code','scope':'llm'}).status_code==429,'five codes per IP per minute')
                    check(poll({'device_code':'unknown'}).json()['error']=='expired_token','unknown/expired token')
                    check(c.get('/api/oauth/providers').status_code == 404,'OAuth removed')
            finally:
                p.terminate();p.wait(timeout=10)
if __name__=='__main__':main()
