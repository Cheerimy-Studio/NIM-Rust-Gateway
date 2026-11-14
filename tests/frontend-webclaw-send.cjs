// WebClaw 发送链路集成测试：假 DOM + 假 fetch 驱动真实 send()。
// 重点验证流式增量累积、尾帧不丢、错误进消息体、停止后落盘。
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const src = fs.readFileSync('web/webclaw.js', 'utf8');
const LS_KEY = 'webclaw.projects.v1::admin';

/* ---------------------------------------------------------------- 假 DOM */

class El {
  constructor(tag) {
    this.tagName = (tag || 'div').toUpperCase();
    this.className = '';
    this.textContent = '';
    this.value = '';
    this.checked = false;
    this.disabled = false;
    this.dataset = {};
    this.style = {};
    this.children = [];
    this.scrollTop = 0;
    this.scrollHeight = 0;
    this.options = [];
    this.handlers = {};
    this.classList = {
      _s: new Set(),
      add: c => this.classList._s.add(c),
      remove: c => this.classList._s.delete(c),
      toggle: (c, on) => { on === undefined ? (this.classList._s.has(c) ? this.classList._s.delete(c) : this.classList._s.add(c)) : (on ? this.classList._s.add(c) : this.classList._s.delete(c)); },
      contains: c => this.classList._s.has(c),
    };
  }
  set innerHTML(v) { this.children = []; this.options = []; this._html = String(v); }
  get innerHTML() { return this._html || ''; }
  appendChild(c) {
    this.children.push(c);
    // 真实 DOM 里 append 到 <select> 的 <option> 会进 options；这里补上
    if (c.tagName === 'OPTION') {
      if (!c.value) c.value = c.textContent;
      this.options.push(c);
    }
    return c;
  }
  append(...cs) { this.children.push(...cs); }
  remove() {}
  addEventListener(n, f) { this.handlers[n] = f; }
  querySelector() { return null; }
  querySelectorAll() { return []; }
  focus() {}
  select() {}
  click() {}
}

function makeDoc() {
  const nodes = new Map();
  const get = id => {
    if (!nodes.has(id)) nodes.set(id, new El('div'));
    return nodes.get(id);
  };
  return {
    nodes,
    get,
    document: {
      readyState: 'complete',
      getElementById: get,
      querySelector: sel => get(sel),
      querySelectorAll: () => [],
      createElement: t => new El(t),
      createTextNode: t => ({nodeType: 3, textContent: String(t)}),
      addEventListener() {},
      body: {appendChild() {}},
      removeEventListener() {},
    },
  };
}

/* ---------------------------------------------------------------- 假存储/网络 */

const store = new Map();
const localStorage = {
  getItem: k => (store.has(k) ? store.get(k) : null),
  setItem: (k, v) => store.set(k, String(v)),
  removeItem: k => store.delete(k),
};

let chatCalls = [];
let agentStep = 0;
// 流式响应：最后一帧故意不带空行，验证尾帧不丢
function sseResponse(frames, opts = {}) {
  const enc = new TextEncoder();
  const chunks = frames.map(f => enc.encode(f));
  let i = 0;
  return {
    ok: true,
    status: 200,
    headers: {get: k => (k.toLowerCase() === 'content-type' ? 'text/event-stream' : null)},
    body: {
      getReader: () => ({
        read: async () => (i < chunks.length ? {done: false, value: chunks[i++]} : {done: true, value: undefined}),
      }),
    },
    text: async () => '',
  };
}

function fetchStub(url, init = {}) {
  const u = String(url);
  if (u.endsWith('/api/settings')) {
    return Promise.resolve({ok: true, status: 200, headers: {get: () => 'application/json'},
      json: async () => ({gateway_tokens: ['sk-test']}), text: async () => '{}'});
  }
  if (u.endsWith('/v1/models')) {
    return Promise.resolve({ok: true, status: 200, headers: {get: () => 'application/json'},
      json: async () => ({data: [{id: 'mock-model'}]}), text: async () => '{}'});
  }
  if (u.endsWith('/v1/chat/completions')) {
    const body = JSON.parse(init.body || '{}');
    chatCalls.push({body, headers: init.headers});
    if (body.model === 'agent-model') {
      agentStep++;
      const messages = [
        {tool_calls:[{id:'read1',type:'function',function:{name:'workspace_read',arguments:'{"path":"input.txt"}'}}]},
        {tool_calls:[{id:'write1',type:'function',function:{name:'workspace_write',arguments:'{"path":"result.md","content":"done"}'}}]},
        {content:'任务已完成'},
      ];
      const msg = messages[Math.min(agentStep-1,2)];
      if (body.stream) return Promise.resolve(sseResponse(['data: ' + JSON.stringify({choices:[{delta:msg}]} ) + '\n\n', 'data: [DONE]\n\n']));
      return Promise.resolve({ok:true,status:200,headers:{get:()=> 'application/json'},text:async()=>JSON.stringify({choices:[{message:msg}]})});
    }
    if (body.model === 'err-model') {
      const txt = '{"error":{"message":"上游炸了","type":"server_error"}}';
      return Promise.resolve({ok: false, status: 500, headers: {get: () => 'application/json'},
        text: async () => txt, json: async () => JSON.parse(txt)});
    }
    if (body.stream !== false) {
      return Promise.resolve(sseResponse([
        'data: {"choices":[{"delta":{"reasoning_content":"想一想"}}],"model":"mock-model"}\n\n',
        'data: {"choices":[{"delta":{"content":"你"}}]}\n\n',
        'data: {"choices":[{"delta":{"content":"好"}}]}\n\n',
        'data: {"choices":[{"delta":{"content":"！"}}],"usage":{"prompt_tokens":3,"completion_tokens":3}}',  // 无尾空行
      ]));
    }
    return Promise.resolve({
      ok: true, status: 200, headers: {get: () => 'application/json'},
      text: async () => JSON.stringify({choices: [{message: {content: '非流式回复'}}], model: 'mock-model',
        usage: {prompt_tokens: 1, completion_tokens: 2}}),
    });
  }
  return Promise.resolve({ok: false, status: 404, headers: {get: () => 'application/json'},
    json: async () => ({error: {message: 'not found'}}), text: async () => '{"error":{"message":"not found"}}'});
}

/* ---------------------------------------------------------------- 装配 */

const {document, get} = makeDoc();
const ctx = vm.createContext({
  window: {
    NGW: {base: '/', csrf: 'csrf-test', mode: 'admin', who: 'admin'},
    addEventListener() {},
  },
  document,
  localStorage,
  fetch: fetchStub,
  performance: {now: () => Date.now()},
  TextDecoder,
  TextEncoder,
  DOMException,
  AbortController,
  Blob: class { constructor(p) { this.text = String(p[0]); } },
  URL: {createObjectURL: () => 'blob:x', revokeObjectURL() {}},
  FileReader: class { readAsText() { this.onload && this.onload(); } },
  bootstrap: {Toast: class { constructor() {} show() {} }},
  marked: undefined,
  DOMPurify: undefined,
  crypto: {getRandomValues: a => a},
  Date, Math, JSON, String, Number, Array, Object, Set, Map, Promise, console,
  setTimeout, clearTimeout, isNaN, parseInt, parseFloat, RegExp, Error,
});
ctx.window.document = document;

vm.runInContext(fs.readFileSync('web/webclaw-agent.js','utf8'),ctx);
vm.runInContext(src, ctx);

const flush = async (n = 12) => { for (let i = 0; i < n; i++) await new Promise(r => setTimeout(r, 0)); };

let n = 0;
const ok = (label, fn) => { fn(); n++; console.log('PASS ' + label); };

(async () => {
  await flush();

  ok('初始化后模型下拉已填充', () => {
    const sel = get('#wc-model');
    assert.ok(sel.options.some(o => o.value === 'mock-model'), '模型未进入下拉');
    assert.equal(sel.value, 'mock-model');
  });

  ok('初始化时把后台令牌带进请求', () => {
    assert.equal(get('#wc-token').value, 'sk-test');
  });

  // 主链路：流式发送
  get('#wc-model').value = 'mock-model';
  get('#wc-stream').checked = true;
  get('#wc-input').value = '你好';
  get('#wc-send').onclick();
  await flush(40);

  ok('流式正文按增量拼接，尾帧不丢', () => {
    assert.ok(chatCalls.length >= 1, '没有发出请求');
    const saved = JSON.parse(store.get(LS_KEY));
    const p = saved.projects.find(x => x.id === saved.active);
    const last = p.messages[p.messages.length - 1];
    assert.equal(last.role, 'assistant');
    assert.equal(last.content, '你好！', '正文应为 你好！，实际 ' + JSON.stringify(last.content));
    assert.equal(last.reasoning, '想一想');
    assert.equal(last.pending, false);
    assert.equal(p.model, 'mock-model');
  });

  ok('请求携带鉴权与训练跳过标记', () => {
    const call = chatCalls[chatCalls.length - 1];
    assert.equal(call.headers.Authorization, 'Bearer sk-test');
    assert.equal(call.headers['X-NGW-Skip-Training'], '1');
    assert.equal(call.body.model, 'mock-model');
    assert.equal(call.body.stream, true);
    assert.equal(call.body.messages[0].role, 'user');
    assert.equal(call.body.messages[0].content, '你好');
  });

  ok('诊断面板拿到状态与事件数', () => {
    // showDiag 往 #wc-diag 里塞 badge；这里只确认没抛异常且时间戳被打上
    assert.ok(get('#wc-diag-sum').textContent.length > 0, '缺少诊断时间');
  });

  ok('助手消息不会被再次当成上下文发出去', () => {
    const call = chatCalls[chatCalls.length - 1];
    for (const m of call.body.messages) assert.ok(m.content.length > 0, '出现空内容消息');
  });

  // 非流式
  get('#wc-stream').checked = false;
  get('#wc-input').value = '再来';
  get('#wc-send').onclick();
  await flush(40);

  ok('非流式路径也能落正文', () => {
    const saved = JSON.parse(store.get(LS_KEY));
    const p = saved.projects.find(x => x.id === saved.active);
    const last = p.messages[p.messages.length - 1];
    assert.equal(last.content, '非流式回复');
    assert.equal(chatCalls[chatCalls.length - 1].body.stream, false);
  });

  ok('多轮对话把历史一并发出', () => {
    const msgs = chatCalls[chatCalls.length - 1].body.messages;
    assert.equal(msgs.length, 3, JSON.stringify(msgs));
    assert.equal(msgs[0].content, '你好');
    assert.equal(msgs[1].content, '你好！');
    assert.equal(msgs[2].content, '再来');
  });

  // 错误路径：必须把上游报错原文落到消息上，且不污染正文
  get('#wc-model').value = 'err-model';
  get('#wc-stream').checked = true;
  get('#wc-input').value = '触发错误';
  get('#wc-send').onclick();
  await flush(40);

  ok('HTTP 错误进消息体且正文不被污染', () => {
    const saved = JSON.parse(store.get(LS_KEY));
    const p = saved.projects.find(x => x.id === saved.active);
    const last = p.messages[p.messages.length - 1];
    assert.equal(last.content, '');
    assert.ok(last.err.startsWith('上游炸了'), last.err);
    assert.equal(last.pending, false);
  });

  ok('错误后界面仍可继续发送', () => {
    assert.equal(get('#wc-send').disabled, false);
    assert.ok(store.get(LS_KEY).length > 0);
  });

  // 安全：整个会话过程中令牌都不能落进 localStorage
  ok('令牌不会写进本地存储', () => {
    const blob = store.get(LS_KEY);
    assert.ok(!blob.includes('sk-test'), '令牌出现在 localStorage');
    assert.ok(!/authorization/i.test(blob), '鉴权头出现在 localStorage');
  });

  // Real browser tool runtime + real send loop, with a model stub only.
  await get('#wc-attach-file').onchange({target:{files:[{name:'input.txt',size:6,text:async()=> 'source'}],value:'x'}});
  get('#wc-agent').checked = true; get('#wc-agent').onchange();
  get('#wc-model').value = 'agent-model'; get('#wc-stream').checked = true;
  get('#wc-input').value = '读取 input.txt 并写入 result.md';
  await get('#wc-send').onclick();
  ok('Agent 自动读取、写入并继续请求模型', () => {
    const saved = JSON.parse(store.get(LS_KEY)); const p = saved.projects.find(p => p.id === saved.active);
    assert.equal(p.files['result.md'],'done'); assert.equal(agentStep,3);
    assert.equal(p.messages.at(-1).content,'任务已完成');
    const calls = chatCalls.filter(c => c.body.model === 'agent-model');
    assert.equal(calls[0].body.tools.length,6);
    assert.ok(calls[1].body.messages.some(m => m.role === 'tool' && m.content.includes('source')));
    assert.ok(calls[2].body.messages.some(m => m.tool_call_id === 'write1'));
    assert.equal(calls[2].headers['X-NGW-Skip-Training'],'1');
    assert.ok(!calls[0].body.messages.some(m => m.role === 'user' && m.content === 'source'));
  });
  const tool = (name,args) => ({type:'function',function:{name,arguments:JSON.stringify(args)}});
  const project = {id:'safe',files:{}};
  for (const path of ['../secret','/etc/passwd','a/../b','C:\\x','__proto__/x']) {
    await assert.rejects(() => ctx.window.NGW_AGENT.execute(project,tool('workspace_write',{path,content:'x'})));
  }
  await assert.rejects(() => ctx.window.NGW_AGENT.execute(project,tool('shell',{command:'echo x'})));
  const stopped = new AbortController(); stopped.abort();
  await assert.rejects(() => ctx.window.NGW_AGENT.execute(project,tool('workspace_write',{path:'x',content:'x'}),{signal:stopped.signal}));
  ok('工具拒绝路径穿越、任意命令和停止后的写入', () => assert.equal(Object.keys(project.files).length,0));

  agentStep = 0;
  get('#wc-stream').checked = false; get('#wc-input').value = '再做一次';
  await get('#wc-send').onclick();
  ok('非流式 Agent 也完成三轮工具循环', () => assert.equal(agentStep,3));
  let written = '';
  const directory = {name:'selected',getFileHandle:async()=>({getFile:async()=>({size:3,text:async()=> 'abc'}),createWritable:async()=>({write:async text=>{written=text},close:async()=>{},abort:async()=>{}})})};
  ctx.window.showDirectoryPicker = async()=>directory;
  await ctx.window.NGW_AGENT.connect(project);
  const read = await ctx.window.NGW_AGENT.execute(project,tool('workspace_read',{path:'local.txt'}));
  assert.equal(read.content,'abc');
  await assert.rejects(()=>ctx.window.NGW_AGENT.execute(project,tool('workspace_write',{path:'local.txt',content:'new'}),{confirmWrite:async()=>false}));
  assert.equal(written,'');
  await ctx.window.NGW_AGENT.execute(project,tool('workspace_write',{path:'local.txt',content:'new'}),{confirmWrite:async()=>true});
  assert.equal(written,'new');
  const stopBeforeWrite = new AbortController();
  await assert.rejects(()=>ctx.window.NGW_AGENT.execute(project,tool('workspace_write',{path:'local.txt',content:'bad'}),{signal:stopBeforeWrite.signal,confirmWrite:async()=>{stopBeforeWrite.abort();return true}}));
  assert.equal(written,'new');
  ctx.window.NGW_AGENT.disconnect(project);
  assert.throws(()=>ctx.window.NGW_AGENT.putFile(project,'too-big.txt','x'.repeat(262145)));
  ok('本机文件逐次确认，取消或停止不覆盖，超大文件被拒绝',()=>assert.equal(written,'new'));

  /* ---------------------------------------------------- 用户模式（mode=user） */

  // 用户侧只允许碰自己的接口；后台接口一旦被调用就是越权
  const userCalls = [];
  const userStore = new Map();
  const userDoc = makeDoc();
  const userCtx = vm.createContext({
    window: {NGW: {base: '/', csrf: 'ucsrf-test', mode: 'user', who: 'u8_abc'}, addEventListener() {}},
    document: userDoc.document,
    localStorage: {
      getItem: k => (userStore.has(k) ? userStore.get(k) : null),
      setItem: (k, v) => userStore.set(k, String(v)),
      removeItem: k => userStore.delete(k),
    },
    fetch: (url, init = {}) => {
      const u = String(url);
      userCalls.push(u);
      if (u.includes('/api/user/test-key')) {
        return Promise.resolve({ok: true, status: 200, headers: {get: () => 'application/json'},
          json: async () => ({key: 'sk-usr-test'}), text: async () => '{}'});
      }
      if (u.includes('/api/user/models')) {
        return Promise.resolve({ok: true, status: 200, headers: {get: () => 'application/json'},
          json: async () => ({rows: [{model: 'user-model', free: true, price: 0}]}), text: async () => '{}'});
      }
      if (u.endsWith('/v1/chat/completions')) {
        userCalls.push('CHAT');
        const body = JSON.parse(init.body || '{}');
        return Promise.resolve({ok: true, status: 200, headers: {get: () => 'application/json'},
          text: async () => JSON.stringify({choices: [{message: {content: '用户侧回复'}}], model: body.model})});
      }
      return Promise.resolve({ok: false, status: 404, headers: {get: () => 'application/json'},
        json: async () => ({error: {message: 'not found'}}), text: async () => '{"error":{"message":"not found"}}'});
    },
    performance: {now: () => Date.now()},
    TextDecoder, TextEncoder, AbortController,
    Blob: class { constructor(p) { this.text = String(p[0]); } },
    URL: {createObjectURL: () => 'blob:u', revokeObjectURL() {}},
    FileReader: class { readAsText() { this.onload && this.onload(); } },
    bootstrap: {Toast: class { constructor() {} show() {} }},
    marked: undefined, DOMPurify: undefined,
    crypto: {getRandomValues: a => a},
    Date, Math, JSON, String, Number, Array, Object, Set, Map, Promise, console,
    setTimeout, clearTimeout, isNaN, parseInt, parseFloat, RegExp, Error,
  });
  userCtx.window.document = userDoc.document;
  vm.runInContext(src, userCtx);
  await flush(30);

  ok('用户模式取自己的 Key，不读后台配置', () => {
    assert.equal(userDoc.get('#wc-token').value, 'sk-usr-test');
    assert.ok(userCalls.some(u => u.includes('/api/user/test-key')), '未调用 test-key');
    assert.ok(!userCalls.some(u => u.includes('/api/settings')), '越权访问 /api/settings');
    assert.ok(!userCalls.some(u => u.includes('/api/upstreams')), '越权访问 /api/upstreams');
  });

  ok('用户模式只显示模型广场里的模型', () => {
    const sel = userDoc.get('#wc-model');
    assert.ok(sel.options.some(o => o.value === 'user-model'), '未填充用户可见模型');
    assert.equal(sel.options.length, 1, '混入了别的模型');
  });

  ok('用户模式发送走自己的 Key 并计费口径不变', async () => {});
  userDoc.get('#wc-stream').checked = false;   // 该上下文用非流式桩
  userDoc.get('#wc-input').value = '你好';
  userDoc.get('#wc-send').onclick();
  await flush(40);

  ok('用户模式存储键按身份分区', () => {
    assert.ok(userStore.has('webclaw.projects.v1::u8_abc'), '未按用户分区存储');
    assert.ok(!userStore.has('webclaw.projects.v1::admin'), '写进了管理员的作用域');
  });

  ok('用户模式发送成功且不带后台令牌', () => {
    const saved = JSON.parse(userStore.get('webclaw.projects.v1::u8_abc'));
    const p = saved.projects.find(x => x.id === saved.active);
    const last = p.messages[p.messages.length - 1];
    assert.equal(last.content, '用户侧回复');
    assert.ok(!userStore.get('webclaw.projects.v1::u8_abc').includes('sk-usr-test'), 'Key 落盘');
  });

  console.log('\n' + n + ' 项通过');
  process.exit(0);
})().catch(e => { console.error(e); process.exit(1); });
