"""Real gateway + local upstream: pricing APIs, token tiers, streams and chain settlement."""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import httpx

ROOT = Path(__file__).resolve().parents[1]
def check(value, label):
    assert value, label
    print('PASS', label, flush=True)
def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1',0)); return sock.getsockname()[1]
class Upstream(BaseHTTPRequestHandler):
    calls = []
    def log_message(self,*args): pass
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['content-length'])))
        self.calls.append(body)
        if body.get('model') == 'native-model' and self.path.endswith('/messages'):
            payload = {'id':'msg_native','type':'message','role':'assistant','model':'native-model',
                'content':[{'type':'text','text':'ok'}],'stop_reason':'end_turn','stop_sequence':None,
                'usage':{'input_tokens':1000,'output_tokens':20,'cache_read_input_tokens':600,'cache_creation_input_tokens':0}}
            self.send_response(200)
            self.send_header('Content-Type','application/json'); self.end_headers()
            self.wfile.write(json.dumps(payload).encode()); return
        usage = {'prompt_tokens':100,'completion_tokens':20,'total_tokens':120}
        if str(body.get('model','')).startswith('cache-'):
            # OpenAI 语义：cached_tokens 已包含在 prompt_tokens 内
            usage = {'prompt_tokens':1000,'completion_tokens':20,'total_tokens':1020,
                     'prompt_tokens_details':{'cached_tokens':600}}
        self.send_response(200)
        if body.get('stream'):
            self.send_header('Content-Type','text/event-stream'); self.end_headers()
            frames = [{'model':body['model'],'choices':[{'index':0,'delta':{'content':'ok'},'finish_reason':None}]},
                {'choices':[{'index':0,'delta':{},'finish_reason':'stop'}]}, {'choices':[],'usage':usage}]
            for frame in frames: self.wfile.write(('data: '+json.dumps(frame)+'\n\n').encode())
            self.wfile.write(b'data: [DONE]\n\n')
        else:
            self.send_header('Content-Type','application/json'); self.end_headers()
            self.wfile.write(json.dumps({'model':body['model'],'choices':[{'index':0,'message':{'role':'assistant','content':'ok'},'finish_reason':'stop'}],'usage':usage}).encode())

def main():
    server = ThreadingHTTPServer(('127.0.0.1',0),Upstream)
    threading.Thread(target=server.serve_forever,daemon=True).start()
    port = free_port(); base=f'http://127.0.0.1:{port}'; password='billing-e2e-password'
    with tempfile.TemporaryDirectory(prefix='ngw-billing-') as tmp:
        with open(Path(tmp)/'stderr.log','w+') as log:
            process = subprocess.Popen([os.environ.get('NGW_GW_EXE',str(ROOT/'target/debug/nim-gateway')),'--port',str(port)],cwd=ROOT,
                env={**os.environ,'NGW_DATA_DIR':tmp,'NGW_ADMIN_PASSWORD':password,'NGW_PUBLIC_ORIGIN':'https://gateway.example','NGW_TRUSTED_PROXIES':'127.0.0.1'},stdout=subprocess.DEVNULL,stderr=log)
            try:
                with httpx.Client(base_url=base,trust_env=False,timeout=30) as admin, httpx.Client(base_url=base,trust_env=False,timeout=30) as user:
                    for _ in range(150):
                        try:
                            if admin.get('/').status_code == 200: break
                        except httpx.ConnectError: pass
                        time.sleep(.1)
                    r=admin.post('/api/login',json={'username':'admin','password':password}); check(r.status_code==200,'管理员登录');admin.headers['X-CSRF']=r.json()['csrf']
                    settings={'account_cooldown_ms':0,'rate_limit_per_minute':0,'daily_request_cap':0,'hourly_request_limit':0,'warmup_seconds':0,
                        'training_log_max':0,'tool_compact_enabled':False,'model_chains':'route = token-model, request-model','gateway_tokens':['root-token']}
                    check(admin.post('/api/settings',json={'config':settings}).status_code==200,'保存计费测试设置')
                    price={'mode':'token','input':2,'output':8,'tiers':[{'min_input':100,'input':4,'output':16}]}
                    channel={'id':'billing-channel','name':'Billing','enabled':True,'base':f'http://127.0.0.1:{server.server_port}/v1',
                        'models':['token-model','request-model','cache-model'],'prices':{'token-model':price,'request-model':.1,
                            'cache-model':{'mode':'token','input':2,'output':8,'cache':0.5}}}
                    native_channel={'id':'native-channel','name':'Native','enabled':True,'base':f'http://127.0.0.1:{server.server_port}/v1',
                        'api_endpoint':'messages','models':['native-model'],
                        'prices':{'native-model':{'mode':'token','input':2,'output':8,'cache':0.5}}}
                    r=admin.post('/api/upstreams',json=channel);check(r.status_code==200,'创建按次与 token 混合定价渠道')
                    check(admin.post('/api/upstreams',json=native_channel).status_code==200,'创建原生 Anthropic 渠道')
                    check(admin.post('/api/keys/import',json={'text':'billing@example.test,password,nvapi-billing-test-key','upstream_id':'billing-channel'}).status_code==200,'导入本地上游 Key')
                    check(admin.post('/api/keys/import',json={'text':'native@example.test,password,nvapi-native-test-key','upstream_id':'native-channel'}).status_code==200,'导入原生渠道 Key')
                    r=admin.get('/api/prices'); check(any(row['price']==price for row in r.json()['rows']),'定价读取保留 token 对象')
                    check(admin.post('/api/prices',json={'channel_id':'billing-channel','prices':{'token-model':{'mode':'token','input':-1,'output':2}}}).status_code==400,'拒绝非法价格且不覆盖配置')
                    check(admin.post('/api/prices',json={'channel_id':'missing','prices':{}}).status_code==404,'不存在渠道返回 404')
                    check(admin.post('/api/upstream-models/op',json={'channel_id':'billing-channel','upstream':'token-model','alias':'token-model','ctx':100000,'out':4096}).status_code==200,'模型标注保存成功')
                    check(any(row['price']==price for row in admin.get('/api/prices').json()['rows']),'模型标注不抹掉 token 定价')
                    r=admin.post('/api/users',json={'username':'billing-user','password':password,'balance':10});check(r.status_code==200,'创建计费用户')
                    r=user.post('/api/user/login',json={'username':'billing-user','password':password});check(r.status_code==200,'用户登录');user.headers['X-CSRF']=r.json()['csrf']
                    key=user.post('/api/user/test-key').json()['key']; headers={'Authorization':'Bearer '+key,'X-NGW-Skip-Training':'1','X-01Code-Client':'01Code/0.2.1'}
                    user.post('/api/user/keys',json={'name':'free-only','kind':'free'})
                    check(user.post('/api/user/test-key').json()['key']==key,'WebClaw 不复用免费专用 Key')
                    paid_key=user.post('/api/user/keys',json={'name':'paid-only','kind':'paid'}).json()['key']['key']
                    visible=user.get('/v1/models',headers={'Authorization':'Bearer '+paid_key}).json()['data']
                    check(any(m['id']=='route' for m in visible),'付费 Key 的模型列表包含付费路由')
                    expected=10.0
                    for endpoint,extra in [('chat/completions',{'messages':[{'role':'user','content':'hi'}]}),('responses',{'input':'hi'}),('messages',{'messages':[{'role':'user','content':'hi'}]})]:
                        for stream in [False,True]:
                            r=user.post('/v1/'+endpoint,headers=headers,json={**extra,'model':'route','stream':stream,'max_tokens':30})
                            check(r.status_code==200 and 'ok' in r.text,f'{endpoint} stream={stream} 返回成功')
                            expected-=.00072
                            actual=user.get('/api/user/me').json()['balance']
                            check(abs(actual-expected)<.0000001,f'{endpoint} stream={stream} 按实际 token 及档位扣费: actual={actual} expected={expected}')
                    logs=user.get('/api/user/logs').json()['rows']
                    check(len(logs)==6 and all(r['billing']['mode']=='token' and r['billing']['min_input']==100 and r['model']=='token-model' for r in logs),'所有协议和路由日志保留实际计费模型及价格档位')
                    check(all(r.get('client')=='01Code/0.2.1' for r in logs),'01Code 客户端版本写入调用日志')
                    daily=user.get('/v1/usage?days=7',headers={'Authorization':'Bearer '+key,'X-Forwarded-Proto':'https'}).json()
                    check(sum(r['requests'] for r in daily)==6 and sum(r['input_tokens'] for r in daily)==600 and sum(r['output_tokens'] for r in daily)==120,'日聚合保留六次调用与完整 token')
                    square=user.get('/api/user/models').json()['rows']; check(any(m['model']=='route' and any(p['rule']==price for p in m['pricing']) for m in square),'模型广场披露路由成员 token 价格')
                    route_sq=next(m for m in square if m['model']=='route')
                    check(len(route_sq.get('recent',[]))>=6 and route_sq.get('health')==100,'模型广场返回最近请求热力图与成功率')
                    check(all(v in (0,1,2) for v in route_sq['recent']),'热力图状态仅含成功/重试成功/失败')
                    up_row=next(r for r in admin.get('/api/upstreams').json()['rows'] if r['id']=='billing-channel')
                    check(len(up_row.get('recent_flags',[]))>=6,'渠道行返回最近请求状态')
                    cat_row=next(r for r in admin.get('/api/upstream-models').json()['rows'] if r['channel_id']=='billing-channel' and r['upstream']=='token-model')
                    check(isinstance(cat_row.get('recent'),list),'模型接入行返回近期状态字段')
                    # 缓存命中计费：1000 输入中 600 命中，命中价 0.5、普通输入 2、输出 8
                    cached_before=user.get('/api/user/me').json()['balance']
                    r=user.post('/v1/chat/completions',headers=headers,json={'model':'cache-model','messages':[{'role':'user','content':'hi'}]})
                    check(r.status_code==200,'缓存命中模型返回成功')
                    cached_charge=round(cached_before-user.get('/api/user/me').json()['balance'],6)
                    check(cached_charge==0.00126,f'缓存命中按 cache 价与未命中输入分开计费: charge={cached_charge}')
                    clog=user.get('/api/user/logs').json()['rows'][0]
                    check(clog.get('cached_tok')==600 and clog.get('in_tok')==1000 and clog.get('billing',{}).get('cache')==0.5,
                        '调用日志保留缓存命中 token 与缓存价: '+json.dumps(clog.get('billing'),ensure_ascii=False))
                    cat_cache=next(r for r in admin.get('/api/upstream-models').json()['rows'] if r['channel_id']=='billing-channel' and r['upstream']=='cache-model')
                    check(len(cat_cache.get('recent',[]))>=1,'模型接入行返回最近请求状态')
                    daily=user.get('/v1/usage?days=7',headers={'Authorization':'Bearer '+key,'X-Forwarded-Proto':'https'}).json()
                    check(sum(r['requests'] for r in daily)==7 and sum(r['input_tokens'] for r in daily)==1600,'日聚合计入缓存命中的完整输入 token')
                    # 原生 Anthropic usage：input_tokens 不含缓存读，需合计后计费
                    native_before=user.get('/api/user/me').json()['balance']
                    r=user.post('/v1/messages',headers=headers,json={'model':'native-model','max_tokens':30,'messages':[{'role':'user','content':'hi'}]})
                    check(r.status_code==200,'原生 Anthropic usage 响应成功')
                    native_charge=round(native_before-user.get('/api/user/me').json()['balance'],6)
                    check(native_charge==0.00246,f'原生 Anthropic usage 按实际 token 与缓存价计费: charge={native_charge}')
                    nlog=user.get('/api/user/logs').json()['rows'][0]
                    check(nlog.get('in_tok')==1600 and nlog.get('cached_tok')==600,'原生 Anthropic 缓存读计入输入且单独记录命中')
                    daily=user.get('/v1/usage?days=7',headers={'Authorization':'Bearer '+key,'X-Forwarded-Proto':'https'}).json()
                    check(sum(r['requests'] for r in daily)==8 and sum(r['input_tokens'] for r in daily)==3200,'日聚合累加原生 Anthropic 输入')
                    # 定价生命周期：取消接入/改名清价，未接入模型不可定价
                    life={'id':'lifecycle-channel','name':'Lifecycle','enabled':True,'base':f'http://127.0.0.1:{server.server_port}/v1',
                        'models':['life-a','life-b'],'prices':{'life-a':.1,'life-b':.2}}
                    check(admin.post('/api/upstreams',json=life).status_code==200,'创建定价生命周期渠道')
                    rows=admin.get('/api/prices').json()['rows']
                    check(any(r['channel_id']=='lifecycle-channel' and r['model']=='life-b' for r in rows),'接入模型出现在定价列表')
                    check(admin.post('/api/prices',json={'channel_id':'lifecycle-channel','prices':{'life-a':.1,'ghost-model':.3}}).status_code==400,'拒绝给未接入模型定价')
                    rows=admin.get('/api/prices').json()['rows']
                    check(any(r['channel_id']=='lifecycle-channel' and r['model']=='life-a' and r['price']==.1 for r in rows),'拒绝幽灵定价时不覆盖已有价格')
                    check(admin.post('/api/upstreams',json={'id':'lifecycle-channel','name':'Lifecycle','base':f'http://127.0.0.1:{server.server_port}/v1','models':['life-a']}).status_code==200,'取消接入 life-b')
                    rows=admin.get('/api/prices').json()['rows']
                    check(not any(r['channel_id']=='lifecycle-channel' and r['model']=='life-b' for r in rows),'取消接入后定价同步消失')
                    check(any(r['channel_id']=='lifecycle-channel' and r['model']=='life-a' for r in rows),'仍接入的模型定价保留')
                    check(admin.post('/api/upstream-models/op',json={'channel_id':'lifecycle-channel','upstream':'life-a','alias':'life-c'}).status_code==200,'别名改为 life-c')
                    rows=admin.get('/api/prices').json()['rows']
                    check(not any(r['channel_id']=='lifecycle-channel' and r['model']=='life-a' for r in rows),'改名后旧名定价清除')
                    check(any(r['channel_id']=='lifecycle-channel' and r['model']=='life-c' and r['free'] for r in rows),'新名以未定价出现')
                    before=len(Upstream.calls)
                    # Separate zero-balance user verifies preflight without relying on adjustment semantics.
                    admin.post('/api/users',json={'username':'empty-user','password':password,'balance':0.000001})
                    with httpx.Client(base_url=base,trust_env=False,timeout=30) as empty:
                        r=empty.post('/api/user/login',json={'username':'empty-user','password':password});empty.headers['X-CSRF']=r.json()['csrf']
                        empty_key=empty.post('/api/user/test-key').json()['key']
                        r=empty.post('/v1/chat/completions',headers={'Authorization':'Bearer '+empty_key},json={'model':'token-model','messages':[{'role':'user','content':'hi'}],'max_tokens':4096})
                        check(r.status_code==402 and len(Upstream.calls)==before,'余额为正但不足预占时，在请求上游前拒绝')
                    for path in ['/v1/models','/v1/chat/completions']:
                        r=user.get(path,headers={'Authorization':'Bearer sk-prz-retired'}) if path.endswith('models') else user.post(path,headers={'Authorization':'Bearer sk-prz-retired'},json={'model':'token-model','messages':[]})
                        check(r.status_code==401,'旧奖品凭据被拒绝 '+path)
            finally:
                process.terminate(); process.wait(timeout=10);server.shutdown()
if __name__=='__main__':main()
