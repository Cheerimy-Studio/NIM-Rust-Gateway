(() => {
'use strict';

const B = (window.NGW && window.NGW.base) || '/';
const CSRF = (window.NGW && window.NGW.csrf) || '';
// 管理员在后台打开 /webclaw，普通用户在用户中心打开；两者令牌来源与可见模型不同
const MODE = (window.NGW && window.NGW.mode) === 'user' ? 'user' : 'admin';
const AGENT = window.NGW_AGENT;
const WHO = (window.NGW && window.NGW.who) || 'admin';
// 按身份分键：同一台机器上换了账号也不会看到上一个人的项目
const LS_KEY = 'webclaw.projects.v1::' + WHO;
const LEGACY_KEY = 'webclaw.projects.v1';

// 上限：全部用于挡住异常输入（超大导入 / 超长回复）把页面拖死
const MAX_RAW = 400;                 // 原始事件最多保留多少条
const MAX_RAW_CHARS = 2000;          // 单条原始事件最多保留多少字符
const MAX_IMPORT_BYTES = 16 * 1024 * 1024;
const MAX_PROJECTS = 200;
const MAX_MESSAGES = 500;
const MAX_CONTENT = 200000;
const MAX_NAME = 60;

/* ---------------------------------------------------------------- 基础工具 */

const $ = (s, r = document) => r.querySelector(s);

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}

const uid = () => 'p' + Date.now().toString(36) + Math.random().toString(36).slice(2, 8);

function fmtAgo(ts) {
  if (!ts) return '—';
  const s = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  if (s < 60) return '刚刚';
  if (s < 3600) return Math.floor(s / 60) + ' 分钟前';
  if (s < 86400) return Math.floor(s / 3600) + ' 小时前';
  return Math.floor(s / 86400) + ' 天前';
}

function toast(msg, type = 'success') {
  let box = document.getElementById('wc-toasts');
  if (!box) { box = el('div', 'toasts'); box.id = 'wc-toasts'; document.body.appendChild(box); }
  const t = el('div', 'toast-item ' + type, String(msg));
  box.appendChild(t);
  setTimeout(() => {
    t.classList.add('out');
    setTimeout(() => t.remove(), 220);
  }, 2600);
}

/* ---------------------------------------------------------------- 弹窗 */

let lastFocus = null;

function openMask(build, keyHandler) {
  lastFocus = document.activeElement;
  const mask = el('div', 'mask');
  const box = el('div', 'dialog');
  build(box);
  mask.appendChild(box);
  document.body.appendChild(mask);
  const onKey = e => keyHandler(e);
  document.addEventListener('keydown', onKey);
  return {
    mask,
    close() {
      mask.remove();
      document.removeEventListener('keydown', onKey);
      if (lastFocus && lastFocus.focus) { try { lastFocus.focus(); } catch (e) { /* 元素已移除 */ } }
      lastFocus = null;
    },
  };
}

function writePreview(content) {
  const text = String(content || '');
  const max = 800;
  return text.length <= max ? text : text.slice(0, max) + '\n…（已隐藏 ' + (text.length - max) + ' 字符）';
}

function uiConfirm(msg, opts = {}) {
  if (window.NGW_UI) return window.NGW_UI.dialog({title:opts.title || '确认操作',body:msg,danger:!!opts.danger,okText:opts.okText || '确定',signal:opts.signal});
  return new Promise(resolve => {
    let dlg = null;
    const done = v => { if (dlg) dlg.close(); resolve(v); };
    dlg = openMask(box => {
      box.appendChild(el('div', 'dialog-title', opts.title || '确认'));
      box.appendChild(el('div', 'dialog-body', msg));
      const foot = el('div', 'dialog-foot');
      const cancel = el('button', 'btn btn-sm btn-outline-secondary', opts.cancelText || '取消');
      const ok = el('button', 'btn btn-sm ' + (opts.danger ? 'btn-danger' : 'btn-primary'), opts.okText || '确定');
      cancel.onclick = () => done(false);
      ok.onclick = () => done(true);
      foot.append(cancel, ok);
      box.appendChild(foot);
      setTimeout(() => ok.focus(), 0);
    }, e => {
      if (e.key === 'Escape') done(false);
      if (e.key === 'Enter') done(true);
    });
    dlg.mask.onclick = e => { if (e.target === dlg.mask) done(false); };
  });
}

function uiPrompt(msg, value = '') {
  return new Promise(resolve => {
    let dlg = null;
    let input = null;
    const done = v => { if (dlg) dlg.close(); resolve(v); };
    dlg = openMask(box => {
      box.appendChild(el('div', 'dialog-title', '重命名'));
      box.appendChild(el('div', 'dialog-body', msg));
      input = el('input', 'form-control form-control-sm mt-2');
      input.value = value;
      input.maxLength = MAX_NAME;
      box.appendChild(input);
      const foot = el('div', 'dialog-foot');
      const cancel = el('button', 'btn btn-sm btn-outline-secondary', '取消');
      const ok = el('button', 'btn btn-sm btn-primary', '确定');
      cancel.onclick = () => done(null);
      ok.onclick = () => done(input.value.trim());
      foot.append(cancel, ok);
      box.appendChild(foot);
      setTimeout(() => { input.focus(); input.select(); }, 0);
    }, e => {
      if (e.key === 'Escape') done(null);
      if (e.key === 'Enter') done(input.value.trim());
    });
    dlg.mask.onclick = e => { if (e.target === dlg.mask) done(null); };
  });
}

function uiChoose(msg, choices) {
  return new Promise(resolve => {
    let dlg = null;
    const done = v => { if (dlg) dlg.close(); resolve(v); };
    dlg = openMask(box => {
      box.appendChild(el('div', 'dialog-title', '导入'));
      box.appendChild(el('div', 'dialog-body', msg));
      const foot = el('div', 'dialog-foot');
      for (const c of choices) {
        const b = el('button', 'btn btn-sm ' + (c.cls || 'btn-outline-secondary'), c.label);
        b.onclick = () => done(c.value);
        foot.appendChild(b);
      }
      box.appendChild(foot);
    }, e => { if (e.key === 'Escape') done(null); });
    dlg.mask.onclick = e => { if (e.target === dlg.mask) done(null); };
  });
}

/* ---------------------------------------------------------------- API */

async function api(path, opts = {}) {
  const init = {method: opts.method || 'GET', headers: {'X-CSRF': CSRF}, credentials: 'same-origin'};
  if (opts.json !== undefined) {
    init.headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(opts.json);
  }
  const res = await fetch(B + 'api/' + path, init);
  let data = null;
  try { data = await res.json(); } catch (e) { data = null; }
  if (!res.ok) throw new Error((data && data.error && data.error.message) || ('HTTP ' + res.status));
  return data;
}

const guard = fn => async (...a) => {
  try { return await fn(...a); } catch (e) { toast(e && e.message ? e.message : e, 'danger'); }
};

/* ---------------------------------------------------------------- 状态与存储 */

let state = {version: 1, active: '', projects: []};
let busy = false;
let aborter = null;
let tokenCache = null;
const MODEL_META = new Map();
let saveTimer = null;
let renderTimer = null;
let renderPartial = false;
let lastBubble = null;

function blankProject(name) {
  const t = Date.now();
  return {id: uid(), name: name || '未命名任务', model: '', system: '', stream: true, effort: '',
    agent: false, files: {}, messages: [], github: {repo:'',branch:'main',token:'',allowWrite:false},
    cacheHit: 0, cacheTotal: 0, created_at: t, updated_at: t};
}

function clip(s, n) {
  return typeof s === 'string' ? (s.length > n ? s.slice(0, n) : s) : '';
}

function normalizeStored(raw) {
  if (!raw || typeof raw !== 'object') return null;
  const messages = [];
  const list = Array.isArray(raw.messages) ? raw.messages : [];
  for (const m of list) {
    if (messages.length >= MAX_MESSAGES) break;
    if (!m || typeof m !== 'object') continue;
    // 只接受对话双方；system 等角色由系统提示单独承载，混进历史会改变上游语义
    if (!['user','assistant','tool'].includes(m.role)) continue;
    const content = clip(m.content, MAX_CONTENT);
    const reasoning = clip(m.reasoning, MAX_CONTENT);
    const err = clip(m.err, 2000);
    const tool_calls = sanitizeCalls(m.tool_calls);
    const tool_call_id = clip(m.tool_call_id,100);
    if (m.role === 'tool' && !tool_call_id) continue;
    if (!content && !reasoning && !err && !tool_calls.length) continue;
    messages.push({role:m.role, content, reasoning, err, at:Number(m.at) || 0,
      ...(tool_calls.length ? {tool_calls} : {}), ...(tool_call_id ? {tool_call_id} : {})});
  }
  return {
    id: (typeof raw.id === 'string' && raw.id) ? raw.id : uid(),
    name: (typeof raw.name === 'string' && raw.name.trim()) ? raw.name.trim().slice(0, MAX_NAME) : '未命名项目',
    model: clip(raw.model, 200),
    system: clip(raw.system, MAX_CONTENT),
    stream: raw.stream !== false,
    effort: ['off','minimal','low','medium','high','max'].includes(raw.effort) ? raw.effort : '',
    agent: raw.agent === true,
    github: (() => { const g = raw.github && typeof raw.github === 'object' ? raw.github : {}; return {repo:clip(g.repo,200), branch:clip(g.branch,200)||'main', token:clip(g.token,500), allowWrite:g.allowWrite===true}; })(),
    cacheHit: Number(raw.cacheHit) || 0,
    cacheTotal: Number(raw.cacheTotal) || 0,
    files: AGENT ? AGENT.cleanFiles(raw.files) : {},
    messages,
    created_at: Number(raw.created_at) || Date.now(),
    updated_at: Number(raw.updated_at) || Date.now(),
  };
}

function loadState() {
  let text = localStorage.getItem(LS_KEY);
  if (text === null && MODE === 'admin') {
    // 1.7.50 的键没有身份作用域；管理员沿用旧数据，避免升级后项目「消失」
    const legacy = localStorage.getItem(LEGACY_KEY);
    if (legacy !== null) {
      text = legacy;
      try { localStorage.setItem(LS_KEY, legacy); } catch (e) { /* 配额满就只读一次 */ }
    }
  }
  let raw = null;
  try { raw = JSON.parse(text || 'null'); } catch (e) { raw = null; }
  const st = {version: 1, active: '', projects: []};
  if (raw && typeof raw === 'object') {
    st.active = typeof raw.active === 'string' ? raw.active : '';
    const seen = new Set();
    for (const item of (Array.isArray(raw.projects) ? raw.projects : [])) {
      if (st.projects.length >= MAX_PROJECTS) break;
      const p = normalizeStored(item);
      if (!p) continue;
      if (seen.has(p.id)) p.id = uid();
      seen.add(p.id);
      st.projects.push(p);
    }
  }
  if (!st.projects.length) {
    const p = blankProject('默认任务');
    st.projects.push(p);
    st.active = p.id;
  }
  if (!st.projects.some(p => p.id === st.active)) st.active = st.projects[0].id;
  return st;
}

function save() {
  try {
    localStorage.setItem(LS_KEY, JSON.stringify(state));
    return true;
  } catch (e) {
    const full = e && (e.name === 'QuotaExceededError' || e.code === 22);
    toast(full ? '本地存储已满，先导出再删除旧项目' : ('保存失败：' + ((e && e.message) || e)), 'danger');
    return false;
  }
}

function scheduleSave() {
  if (saveTimer) return;
  saveTimer = setTimeout(() => { saveTimer = null; save(); }, 800);
}

function flushSave() {
  if (!saveTimer) return;
  clearTimeout(saveTimer);
  saveTimer = null;
  save();
}

function active() {
  return state.projects.find(p => p.id === state.active) || null;
}

/* ---------------------------------------------------------------- 项目操作 */

function createProject() {
  if (busy) { toast('请先停止当前任务，再操作项目', 'warning'); return; }
  if (state.projects.length >= MAX_PROJECTS) {
    toast('项目数量已达上限，先删掉一些', 'warning');
    return;
  }
  stop();
  const used = new Set(state.projects.map(p => p.name));
  let n = state.projects.length + 1;
  while (used.has('任务 ' + n)) n++;
  const p = blankProject('任务 ' + n);
  state.projects.push(p);
  state.active = p.id;
  save();
  renderAll();
}

function switchProject(id) {
  if (busy) { toast('请先停止当前任务，再操作项目', 'warning'); return; }
  if (id === state.active) return;
  if (!state.projects.some(p => p.id === id)) return;
  stop();
  flushSave();
  state.active = id;
  save();
  renderAll();
}

async function renameProject(id) {
  const p = state.projects.find(x => x.id === id);
  if (!p) return;
  const name = await uiPrompt('新名称', p.name);
  if (name === null) return;
  if (!name) { toast('名称不能为空', 'warning'); return; }
  p.name = name.slice(0, MAX_NAME);
  p.updated_at = Date.now();
  save();
  renderAll();
}

async function deleteProject(id) {
  if (busy) { toast('请先停止当前任务，再操作项目', 'warning'); return; }
  const p = state.projects.find(x => x.id === id);
  if (!p) return;
  const ok = await uiConfirm('删除「' + p.name + '」？删掉就找不回来了。', {danger: true, okText: '删除'});
  if (!ok) return;
  stop();
  if (AGENT) AGENT.disconnect(p);
  state.projects = state.projects.filter(x => x.id !== id);
  if (!state.projects.length) state.projects.push(blankProject('默认任务'));
  if (!state.projects.some(x => x.id === state.active)) state.active = state.projects[0].id;
  save();
  renderAll();
}

async function clearChat() {
  if (busy) { toast('请先停止当前任务，再操作项目', 'warning'); return; }
  const p = active();
  if (!p || !p.messages.length) return;
  const ok = await uiConfirm('清空当前对话？', {danger: true, okText: '清空'});
  if (!ok) return;
  stop();
  p.messages = [];
  p.cacheHit = 0;
  p.cacheTotal = 0;
  p.updated_at = Date.now();
  save();
  renderAll();
}

/* ---------------------------------------------------------------- 渲染 */

const mdRender = (text, node) => {
  // 先 sanitize 再落 DOM；marked 缺失时退化为纯文本，任何情况下都不执行上游返回的标签
  if (typeof marked !== 'undefined' && typeof DOMPurify !== 'undefined') {
    node.innerHTML = DOMPurify.sanitize(marked.parse(text, {breaks: true, gfm: true}), {ADD_ATTR: ['target']});
    node.querySelectorAll('a').forEach(a => { a.target = '_blank'; a.rel = 'noopener noreferrer'; });
  } else {
    node.textContent = text;
  }
};

function iconButton(cls, icon, title, op) {
  const b = el('button', cls || '');
  b.type = 'button';
  b.dataset.op = op;
  b.title = title;
  b.appendChild(el('i', 'bi ' + icon));
  return b;
}

function renderProjects() {
  const box = $('#wc-projects');
  if (!box) return;
  box.innerHTML = '';
  if (!state.projects.length) {
    box.appendChild(el('div', 'proj-empty', '没有项目'));
    return;
  }
  for (const p of state.projects) {
    const item = el('div', 'proj' + (p.id === state.active ? ' active' : ''));
    item.dataset.id = p.id;
    item.appendChild(el('div', 'nm', p.name));
    const turns = p.messages.filter(m => m.role === 'user').length;
    item.appendChild(el('div', 'mt', turns + ' 轮 · ' + fmtAgo(p.updated_at)));
    const ops = el('div', 'ops');
    ops.append(
      iconButton('', 'bi-pencil', '重命名', 'rename'),
      iconButton('', 'bi-download', '导出', 'export'),
      iconButton('del', 'bi-trash', '删除', 'delete'),
    );
    item.appendChild(ops);
    box.appendChild(item);
  }
}

function callArgs(call) {
  try {
    const v = JSON.parse(call?.function?.arguments || '{}');
    return v && typeof v === 'object' && !Array.isArray(v) ? v : {};
  } catch (e) { return {}; }
}
function toolInfo(name) {
  return (AGENT && AGENT.meta && AGENT.meta[name]) || {label:'工具', icon:'bi-tools'};
}
function toolTarget(name, args) {
  if (name === 'workspace_list') return '工作区';
  if (name === 'workspace_search') return (args.pattern || '') + (args.path ? ' @ ' + args.path : '');
  if (name === 'workspace_move') return (args.from || '') + ' → ' + (args.to || '');
  if (name === 'github_tree') return '仓库' + (args.path ? ' @ ' + args.path : '');
  if (name === 'github_commit') return (Array.isArray(args.files) ? args.files.length : 0) + ' 个文件';
  if (name === 'github_actions_run') return args.workflow || '';
  if (name === 'github_actions_status') return args.run_id ? '#' + args.run_id : '最近一次';
  if (name === 'github_branch') return args.name || '';
  if (name === 'github_pr') return args.title || '';
  return args.path || '';
}
function toolResultValue(result) {
  if (!result) return null;
  try { return JSON.parse(result.content || ''); } catch (e) { return null; }
}
function toolState(result, pending) {
  if (!result) return pending ? {cls:'run', text:'调用中'} : {cls:'idle', text:'已调用'};
  const value = toolResultValue(result);
  if (value && value.error) return {cls:'err', text:'失败'};
  return {cls:'ok', text:'完成'};
}
function toolPreview(result) {
  if (!result) return '等待工具返回…';
  const value = toolResultValue(result);
  if (value && value.error) return String(value.error);
  if (value && typeof value.content === 'string') return value.content.slice(0, 8000);
  if (value && Array.isArray(value.files)) return value.files.join('\n').slice(0, 8000);
  if (value && Array.isArray(value.matches)) {
    return value.matches.map(m => m.path + ':' + m.line + '  ' + m.text).join('\n').slice(0, 8000);
  }
  if (value) return JSON.stringify(value, null, 2).slice(0, 8000);
  return String(result.content || '').slice(0, 8000);
}
function buildToolCard(call, result, pending) {
  const name = call?.function?.name || '';
  const args = callArgs(call);
  const info = toolInfo(name);
  const state = toolState(result, pending);
  const card = el('div', 'tool-card');
  const head = el('div', 'tool-head');
  const icon = el('span', 'tool-ico');
  icon.innerHTML = '<i class="bi ' + info.icon + '"></i>';
  head.append(icon, el('span', 'tool-name', info.label));
  const target = toolTarget(name, args);
  if (target) head.append(el('code', 'tool-target', target));
  head.append(el('span', 'tool-state ' + state.cls, state.text), el('i', 'bi bi-chevron-down tool-chev'));
  const body = el('div', 'tool-body');
  body.hidden = state.cls !== 'err';
  body.append(el('pre', 'tool-pre', toolPreview(result)));
  head.onclick = () => { body.hidden = !body.hidden; card.classList.toggle('open', !body.hidden); };
  card.append(head, body);
  return card;
}
function appendThinking(bubble, m) {
  if (!m.reasoning) return;
  const th = el('div', 'think');
  th.appendChild(el('div', 'think-hd', '思考'));
  const tb = el('div', 'think-body');
  mdRender(m.reasoning, tb);
  th.appendChild(tb);
  bubble.appendChild(th);
}
function buildToolGroup(m, results) {
  const bubble = el('div', 'tmsg ai tool-bubble');
  appendThinking(bubble, m);
  const content = stripToolMarkup(m.content || '');
  if (content) {
    const body = el('div', 'md');
    mdRender(content, body);
    bubble.appendChild(body);
  }
  const byId = new Map(results.map(r => [r.tool_call_id, r]));
  const list = el('div', 'tool-list');
  for (const call of m.tool_calls) list.appendChild(buildToolCard(call, byId.get(call.id), m.pending));
  bubble.appendChild(list);
  if (m.err) bubble.appendChild(el('div', 'err', m.err));
  if (m.pending) bubble.classList.add('pending');
  return bubble;
}
function buildBubble(m) {
  if (m.role === 'tool') {
    const bubble = el('div', 'tmsg ai tool-bubble');
    const list = el('div', 'tool-list');
    list.appendChild(buildToolCard({function:{name:'tool_result',arguments:'{}'}},
      {content:m.content}, false));
    bubble.appendChild(list);
    return {bubble, md:null};
  }
  const mine = m.role === 'user';
  const bubble = el('div', 'tmsg ' + (mine ? 'me' : 'ai'));
  const out = {bubble, md: null};
  if (mine) {
    bubble.textContent = m.content || '';
    return out;
  }
  appendThinking(bubble, m);
  const body = el('div', 'md');
  const visible = m.role === 'assistant' ? stripToolMarkup(m.content) : m.content;
  if (visible) mdRender(visible, body);
  else if (!m.err) body.textContent = m.pending ? '…' : '（空）';
  bubble.appendChild(body);
  out.md = body;
  if (m.err) bubble.appendChild(el('div', 'err', m.err));
  if (m.pending) bubble.classList.add('pending');
  return out;
}

function nearBottom(box) {
  return box.scrollHeight - box.scrollTop - box.clientHeight < 64;
}

function renderChat() {
  const box = $('#wc-chat');
  const p = active();
  if (!box || !p) return;
  const stick = nearBottom(box) || p.messages.some(m => m.pending);
  box.innerHTML = '';
  lastBubble = null;
  if (!p.messages.length) {
    const empty = el('div', 'dsh-empty');
    empty.innerHTML = '<div class="mark">言</div><div class="empty-title">开始一个新任务</div>' +
      '<div class="empty-sub">让 Agent 读写工作区或 GitHub 仓库，运行测试并修复问题。</div>';
    const prompts = el('div', 'empty-prompts');
    for (const [label, text] of [
      ['检查项目', '检查当前工作区的文件结构，指出可能存在的问题。'],
      ['修复构建', '查看 GitHub 仓库最近的 Actions 运行，定位构建失败原因并修复。'],
      ['实现功能', '根据工作区代码实现一个新功能，并补上必要测试。'],
    ]) {
      const b = el('button', 'empty-prompt', label);
      b.type = 'button';
      b.onclick = () => { const input = $('#wc-input'); if (input) { input.value = text; input.focus(); } };
      prompts.appendChild(b);
    }
    empty.appendChild(prompts);
    box.appendChild(empty);
    return;
  }
  for (let i = 0; i < p.messages.length; i++) {
    const m = p.messages[i];
    const row = el('div', 'mb-2 d-flex ' + (m.role === 'user' ? 'justify-content-end' : 'justify-content-start'));
    if (m.role === 'assistant' && m.tool_calls?.length) {
      const results = [];
      let j = i + 1;
      while (j < p.messages.length && p.messages[j].role === 'tool') results.push(p.messages[j++]);
      const bubble = buildToolGroup(m, results);
      row.appendChild(bubble);
      box.appendChild(row);
      lastBubble = {row, msg: m, bubble};
      i = j - 1;
      continue;
    }
    const built = buildBubble(m);
    row.appendChild(built.bubble);
    box.appendChild(row);
    lastBubble = {row, msg: m, bubble: built.bubble};
  }
  if (stick) box.scrollTop = box.scrollHeight;
}

// 流式增量：只重建最后一个气泡，避免整段历史每 80ms 重新解析 markdown
function refreshLastBubble() {
  if (!lastBubble) return;
  const box = $('#wc-chat');
  if (!box || !lastBubble.row.parentNode) { lastBubble = null; return; }
  const stick = nearBottom(box);
  const built = buildBubble(lastBubble.msg);
  lastBubble.row.replaceChild(built.bubble, lastBubble.bubble);
  lastBubble.bubble = built.bubble;
  if (stick) box.scrollTop = box.scrollHeight;
}

function scheduleRender(partial) {
  if (partial) renderPartial = true;
  else renderPartial = false;
  if (renderTimer) return;
  renderTimer = setTimeout(() => {
    const wasPartial = renderPartial;
    renderTimer = null;
    renderPartial = false;
    if (wasPartial && lastBubble) refreshLastBubble();
    else renderChat();
  }, 80);
}

function syncInputs() {
  const p = active();
  if (!p) return;
  const sel = $('#wc-model');
  const sys = $('#wc-system');
  const st = $('#wc-stream');
  if (sys) sys.value = p.system || '';
  if ($('#wc-agent')) $('#wc-agent').checked = !!p.agent;
  if (st) st.checked = p.stream !== false;
  const ef = $('#wc-effort');
  if (ef) ef.value = p.effort || '';
  syncEffortUI();
  const pn = $('#wc-project-name');
  if (pn) pn.textContent = p.name || '未命名任务';
  ghRender();
  if (sel && p.model) {
    const has = Array.from(sel.options).some(o => o.value === p.model);
    if (!has) sel.appendChild(el('option', '', p.model));
    sel.value = p.model;
  }
  renderContext();
}

function ghRender() {
  const p = active();
  if (!p) return;
  const g = p.github || {};
  const fill = (id, value) => {
    const n = $(id);
    if (n && document.activeElement !== n) n.value = value;
  };
  fill('#gh-repo', g.repo || '');
  fill('#gh-branch', g.branch || 'main');
  fill('#gh-token', g.token || '');
  const allow = $('#gh-allow');
  if (allow) allow.checked = g.allowWrite === true;
  const bound = !!(g.repo && g.token);
  const status = $('#gh-status');
  if (status) {
    status.textContent = bound ? ('已绑定 · ' + (g.branch || 'main')) : '未绑定';
    status.classList.toggle('ok', bound);
  }
  const unbind = $('#gh-unbind');
  if (unbind) unbind.disabled = !bound;
  const refresh = $('#gh-refresh-runs');
  if (refresh) refresh.disabled = !bound;
  if (!bound) {
    const box = $('#gh-runs');
    if (box) {
      box.innerHTML = '';
      box.appendChild(el('div', 'gh-hint', '绑定仓库后显示最近运行'));
    }
  }
}

async function ghBind() {
  const p = active();
  if (!p) return;
  const repo = ($('#gh-repo').value || '').trim();
  if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repo)) throw new Error('仓库格式应为 owner/repo');
  const token = ($('#gh-token').value || '').trim();
  if (!token) throw new Error('请填写 GitHub Token');
  const branch = ($('#gh-branch').value || '').trim() || 'main';
  p.github = {repo, branch, token, allowWrite: $('#gh-allow').checked};
  p.updated_at = Date.now();
  save();
  ghRender();
  try {
    const info = await NGW_AGENT.github.repo(p);
    if (!p.github.branch || p.github.branch === 'main') {
      if (info.default_branch && p.github.branch === 'main') {
        p.github.branch = info.default_branch;
        save();
        ghRender();
      }
    }
    toast('已连接 ' + (info.full_name || repo));
    await ghRefreshRuns();
  } catch (e) {
    toast('绑定已保存，但验证失败：' + e.message, 'warning');
  }
}

function ghUnbind() {
  const p = active();
  if (!p) return;
  p.github = {repo: '', branch: 'main', token: '', allowWrite: false};
  p.updated_at = Date.now();
  save();
  ghRender();
  toast('已解绑 GitHub');
}

async function ghRefreshRuns() {
  const p = active();
  if (!p || !p.github || !p.github.repo || !p.github.token) return;
  const box = $('#gh-runs');
  if (box) {
    box.innerHTML = '';
    box.appendChild(el('div', 'gh-hint', '读取中…'));
  }
  try {
    const runs = await NGW_AGENT.github.runs(p);
    if (!box) return;
    box.innerHTML = '';
    if (!runs.length) {
      box.appendChild(el('div', 'gh-hint', '该分支还没有 Actions 运行'));
      return;
    }
    for (const r of runs) {
      const row = el('div', 'gh-run');
      const top = el('div', 'gh-run-top');
      top.appendChild(el('span', 'gh-run-name', r.name || 'workflow'));
      const done = r.status === 'completed';
      const cls = done ? (r.conclusion === 'success' ? 'ok' : (r.conclusion === 'failure' ? 'bad' : '')) : 'run';
      const label = done ? (r.conclusion === 'success' ? '成功' : (r.conclusion === 'failure' ? '失败' : (r.conclusion || '结束'))) : (r.status === 'in_progress' ? '运行中' : '排队');
      top.appendChild(el('span', 'gh-badge ' + cls, label));
      row.append(top, el('div', 'gh-run-meta', (r.event || '') + ' · ' + fmtAgo(r.created_at ? Date.parse(r.created_at) / 1000 : 0)));
      row.onclick = () => window.open(r.html_url, '_blank', 'noopener');
      box.appendChild(row);
    }
  } catch (e) {
    if (box) {
      box.innerHTML = '';
      box.appendChild(el('div', 'gh-hint', e.message));
    }
  }
}

function renderAll() {
  renderWorkspace();
  renderProjects();
  syncInputs();
  renderChat();
}

function renderDiag(d) {
  const box = $('#wc-diag');
  const sum = $('#wc-diag-sum');
  const wrap = $('#wc-raw-wrap');
  const raw = $('#wc-raw');
  if (!box) return;
  box.innerHTML = '';
  const chip = (label, val, cls) => {
    const c = el('span', 'chip ' + (cls || ''), null);
    c.appendChild(el('b', '', val));
    c.appendChild(document.createTextNode(' ' + label));
    box.appendChild(c);
  };
  const okStatus = d.status >= 200 && d.status < 400;
  chip('状态', String(d.status), okStatus ? 'ok' : 'bad');
  chip('首字', d.ttfb == null ? '—' : Math.round(d.ttfb) + 'ms');
  chip('耗时', Math.round(d.total) + 'ms');
  chip('token', (d.inTok || 0) + '↑ ' + (d.outTok || 0) + '↓');
  if (d.cachedTok != null) chip('缓存', d.cachedTok + '↑');
  if (d.events != null) chip('事件', d.events + ' 个');
  if (d.up && d.up !== d.model) chip('映射', d.model + ' → ' + d.up, 'info');
  if (d.format) chip('类型', d.format);
  if (d.err) box.appendChild(el('div', 'diag-err', d.err));
  if (sum) sum.textContent = new Date().toLocaleTimeString('zh-CN', {hour12: false});
  if (raw && wrap) {
    raw.textContent = d.raw || '';
    wrap.classList.toggle('d-none', !d.raw);
  }
}

/* ---------------------------------------------------------------- 令牌与模型 */

function tokenValue() {
  const input = $('#wc-token');
  return (input && input.value.trim()) || '';
}

function tokenHint() {
  const hint = $('#wc-hint');
  if (!hint) return;
  hint.className = 'scope-chip';
  const t = tokenValue();
  if (!t) { hint.textContent = MODE === 'user' ? '未取到 Key' : '全部'; return; }
  hint.textContent = '全部';
  if (MODE === 'user') {
    // 用户 Key 有免费/付费/全部三种范围：按当前使用的 Key 显示
    fetch('api/user/keys', {credentials:'same-origin'})
      .then(r => r.ok ? r.json() : null)
      .then(d => {
        const row = ((d && d.rows) || []).find(k => k.key === t);
        if (row) hint.textContent = row.kind === 'free' ? '免费' : row.kind === 'paid' ? '付费' : '全部';
      })
      .catch(() => {});
  }
}

async function ensureToken(force) {
  if (force) tokenCache = null;
  if (tokenCache !== null) return tokenCache;
  // 用户侧不接受手填/浏览器回填的值：Key 只能由服务端签发，避免陈旧值被当成令牌用
  const typed = MODE === 'user' ? '' : tokenValue();
  if (typed) { tokenCache = typed; tokenHint(); return tokenCache; }
  try {
    if (MODE === 'user') {
      // 用户侧只能取自己名下的测试 Key；绝不碰后台令牌配置
      const d = await api('user/test-key', {method: 'POST'});
      tokenCache = (d && typeof d.key === 'string') ? d.key : '';
    } else {
      const s = await api('settings');
      const toks = (s && s.gateway_tokens) || [];
      const first = toks[0];
      const t = first ? (first.t || first) : '';
      tokenCache = typeof t === 'string' ? t : '';
    }
  } catch (e) {
    tokenCache = '';
    if (/未登录|会话已过期|已被停用|HTTP 401/.test((e && e.message) || '')) {
      toast('登录状态已失效，重新登录后再试', 'danger');
    }
  }
  const input = $('#wc-token');
  if (input && tokenCache) {
    // 用户模式以服务端签发为准，覆盖任何残留值（含浏览器密码管理器回填）
    if (MODE === 'user' || !input.value.trim()) input.value = tokenCache;
  }
  tokenHint();
  return tokenCache;
}

async function loadModels(preferred) {
  const sel = $('#wc-model');
  if (!sel) return;
  const keep = preferred || sel.value;
  const models = new Set();
  MODEL_META.clear();

  if (MODE === 'user') {
    // 用户只看模型广场（含定价与免费标记），不接触渠道配置
    try {
      const d = await api('user/models');
      for (const r of (d.rows || [])) {
        if (!r || !r.model) continue;
        models.add(r.model);
        MODEL_META.set(r.model, {ctx:Number(r.ctx)||0, out:Number(r.out)||0, tps:Number(r.tps)||0});
      }
    } catch (e) { /* 会话失效时留空，下面按无模型提示 */ }
  } else {
    const tok = tokenValue() || tokenCache || '';
    try {
      const r = await fetch(B + 'v1/models', {
        headers: tok ? {Authorization: 'Bearer ' + tok} : {},
        credentials: 'same-origin',
      });
      if (r.ok) {
        const d = await r.json();
        for (const m of (d.data || [])) {
          if (!m || !m.id) continue;
          models.add(m.id);
          MODEL_META.set(m.id, {
            ctx:Number(m.context_length || m.context_window) || 0,
            out:Number(m.max_output_tokens) || 0,
            tps:Number(m.tps) || 0,
          });
        }
      }
    } catch (e) { /* 网关不可达时退回渠道配置 */ }
    if (!models.size) {
      try {
        const d = await api('upstreams');
        for (const u of (d.rows || [])) {
          for (const m of (u.models || [])) models.add(m);
          for (const k of Object.keys(u.model_map || {})) models.add(k);
        }
      } catch (e) { /* 未登录后台时没有渠道信息 */ }
    }
  }

  sel.innerHTML = '';
  if (!models.size) {
    sel.appendChild(el('option', '', '无可用模型'));
    return;
  }
  for (const m of models) sel.appendChild(el('option', '', m));
  sel.value = (keep && models.has(keep)) ? keep : models.values().next().value;
  renderModelMeta();
}

function renderModelMeta() {
  const node = $('#wc-model-meta');
  const meta = MODEL_META.get($('#wc-model').value);
  if (node) node.textContent = meta && meta.tps ? '约 ' + meta.tps + ' tok/s' : '';
  renderContext();
}

const EFFORTS = ['', 'off', 'minimal', 'low', 'medium', 'high', 'max'];
const EFFORT_LABELS = {'': '默认', off: '关闭', minimal: '最低', low: '低', medium: '中', high: '高', max: '最高'};
const effortLabel = value => EFFORT_LABELS[value] || '默认';

function syncEffortUI() {
  const sel = $('#wc-effort');
  if (!sel) return;
  const value = EFFORTS.includes(sel.value) ? sel.value : '';
  const label = effortLabel(value);
  const button = $('#wc-effort-btn-label');
  if (button) button.textContent = '思考：' + label;
  const range = $('#wc-effort-range');
  if (range) range.value = String(Math.max(0, EFFORTS.indexOf(value)));
  const head = $('#wc-effort-label');
  if (head) head.textContent = label;
  const note = $('#wc-effort-note');
  if (note) note.textContent = label;
}

function setEffort(value) {
  const sel = $('#wc-effort');
  if (!sel) return;
  sel.value = EFFORTS.includes(value) ? value : '';
  sel.dispatchEvent(new Event('change'));
  syncEffortUI();
}

function cycleEffort() {
  const sel = $('#wc-effort');
  if (!sel) return;
  const index = Math.max(0, EFFORTS.indexOf(sel.value || ''));
  setEffort(EFFORTS[(index + 1) % EFFORTS.length]);
}

function closePops(except) {
  for (const id of ['#wc-context-pop', '#wc-effort-pop']) {
    const node = $(id);
    if (node && id !== except) node.hidden = true;
  }
}

function togglePop(id) {
  const node = $(id);
  if (!node) return;
  const opening = node.hidden;
  closePops(id);
  node.hidden = !opening;
  if (!node.hidden) {
    if (id === '#wc-context-pop') renderContext();
    syncEffortUI();
  }
}

function estTokens(text) {
  let n = 0;
  for (const ch of String(text || '')) {
    const code = ch.codePointAt(0);
    const cjk = (code >= 0x2e80 && code <= 0x9fff) || (code >= 0xf900 && code <= 0xfaff) || (code >= 0xff00 && code <= 0xffef);
    n += cjk ? 1 : 0.25;
  }
  return Math.ceil(n);
}

function fmtCtx(n) {
  n = Math.max(0, Math.round(Number(n) || 0));
  if (n >= 10000) {
    const v = n / 10000;
    return (v >= 100 ? Math.round(v) : v.toFixed(1).replace(/\.0$/, '')) + '万';
  }
  if (n >= 1000) {
    const v = n / 1000;
    return (v >= 100 ? Math.round(v) : v.toFixed(1).replace(/\.0$/, '')) + 'k';
  }
  return String(n);
}

function contextSnapshot() {
  const p = active();
  if (!p) return null;
  const meta = MODEL_META.get($('#wc-model').value) || {};
  const limit = Number(meta.ctx) || 200000;
  const estimated = !Number(meta.ctx);
  const agentEnabled = !!$('#wc-agent')?.checked;
  const userSys = ($('#wc-system')?.value || '').trim();
  const sys = agentEnabled && AGENT?.systemPrompt
    ? [AGENT.systemPrompt,
       meta.ctx ? '当前模型上下文上限约 ' + meta.ctx + ' token；长文件请分块处理，避免一次塞满上下文。' : '',
       userSys].filter(Boolean).join(String.fromCharCode(10, 10))
    : userSys;
  let msgTokens = 0;
  for (const m of p.messages || []) {
    msgTokens += estTokens(m.content || '');
    msgTokens += estTokens(m.reasoning || '');
    if (m.tool_calls) msgTokens += estTokens(JSON.stringify(m.tool_calls));
  }
  let toolTokens = 0;
  try { toolTokens = agentEnabled && AGENT?.tools ? estTokens(JSON.stringify(AGENT.tools)) : 0; } catch (e) { toolTokens = 0; }
  const fileTokens = Object.values(p.files || {}).reduce((n, text) => n + estTokens(text), 0);
  const sysTokens = estTokens(sys);
  const parts = [
    {name: '消息', tokens: msgTokens, color: '#2f6fed'},
    {name: '系统工具', tokens: toolTokens, color: '#22c55e'},
    {name: '工作区文件', tokens: fileTokens, color: '#f59e0b'},
    {name: '系统提示词', tokens: sysTokens, color: '#8b5cf6'},
    {name: '其他', tokens: 0, color: '#cbd5e1'},
  ];
  const used = parts.reduce((n, part) => n + part.tokens, 0);
  const pct = Math.min(100, used / limit * 100);
  const cacheRate = (p.cacheTotal || 0) > 0 ? Math.min(100, (p.cacheHit || 0) / p.cacheTotal * 100) : null;
  return {used, limit, pct, parts, cacheRate, out: Number(meta.out) || 0, estimated};
}

function renderContext() {
  const p = active();
  const label = $('#wc-ctx-label');
  if (!p) { if (label) label.textContent = '上下文'; return; }
  const snap = contextSnapshot();
  if (!snap) return;
  if (label) label.textContent = '上下文 ' + (snap.used > 0 && snap.pct < 1 ? '<1%' : Math.round(snap.pct) + '%');
  const ratio = $('#wc-ctx-ratio');
  if (ratio) ratio.textContent = fmtCtx(snap.used) + '/' + fmtCtx(snap.limit) + ' (' + snap.pct.toFixed(1) + '%)';
  const fill = $('#wc-ctx-fill');
  if (fill) {
    fill.style.width = Math.min(100, snap.pct) + '%';
    fill.style.background = snap.pct >= 90 ? 'linear-gradient(90deg,#f59e0b,#ef4444)' : '';
  }
  const rows = $('#wc-ctx-rows');
  if (rows) {
    rows.innerHTML = '';
    for (const part of snap.parts) {
      const row = el('div', 'ctx-row');
      const dot = el('span', 'ctx-dot');
      dot.style.background = part.color;
      row.append(dot, el('span', '', part.name), el('span', 'ctx-pct', (snap.used ? part.tokens / snap.used * 100 : 0).toFixed(1) + '%'));
      rows.appendChild(row);
    }
  }
  const out = $('#wc-ctx-out');
  if (out) {
    out.textContent = (snap.out ? '输出上限 ≤ ' + fmtCtx(snap.out) : '输出上限未标注') +
      (snap.estimated ? ' · 上下文按 200k 估算' : '');
  }
  const cache = $('#wc-ctx-cache');
  if (cache) cache.textContent = snap.cacheRate == null ? '—' : snap.cacheRate.toFixed(1) + '%';
}

/* ---------------------------------------------------------------- 发送 */

function sanitizeCalls(calls) {
  if (!Array.isArray(calls)) return [];
  return calls.slice(0,16).filter(c => c && typeof c.id === 'string' && c.id.length <= 100 && c.type === 'function'
    && c.function && typeof c.function.name === 'string' && typeof c.function.arguments === 'string' && c.function.arguments.length <= 512 * 1024)
    .map(c => ({id:c.id,type:'function',function:{name:c.function.name,arguments:c.function.arguments}}));
}
const TOOL_TEXT_LIMIT = 512 * 1024;
function dsmlNormalize(text) {
  return String(text || '').replace(/<\|DSML\|/g, '<｜DSML｜').replace(/<\/\|DSML\|/g, '</｜DSML｜');
}
function tagAttr(tag, key) {
  const m = new RegExp(key + '\s*=\s*"([^"]*)"', 'i').exec(tag || '');
  return m ? m[1] : '';
}
function textCallArgs(raw) {
  if (raw && typeof raw === 'object' && !Array.isArray(raw)) return raw;
  const text = String(raw == null ? '' : raw).trim();
  if (!text) return {};
  try {
    const v = JSON.parse(text);
    return v && typeof v === 'object' && !Array.isArray(v) ? v : null;
  } catch (e) { return null; }
}
function addTextCall(calls, name, args, allowed, seq) {
  name = String(name || '').trim();
  if (!allowed.includes(name) || !args || typeof args !== 'object' || Array.isArray(args)) return false;
  const argText = JSON.stringify(args);
  if (argText.length > TOOL_TEXT_LIMIT) return false;
  if (calls.some(c => c.function.name === name && c.function.arguments === argText)) return true;
  calls.push({id:'call_txt_' + seq.stamp + '_' + (++seq.n), type:'function',
    function:{name, arguments:argText}});
  return true;
}
function jsonTextCall(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
  const fn = value.function && typeof value.function === 'object' ? value.function : null;
  const name = fn ? fn.name : value.name;
  const raw = fn ? fn.arguments : (value.arguments !== undefined ? value.arguments : value.parameters);
  const args = textCallArgs(raw);
  return name && args ? {name:String(name), args} : null;
}
// 兼容三种文本工具格式：<tool_call>{json}</tool_call>、DSML invoke/parameter、
// 以及截图里那种扁平 parameter(name/arguments) 形式。解析失败的内容原样保留。
function parseTextToolCalls(text, allowed) {
  if (!text || !(/DSML|<tool_call>/i.test(text))) return {content:text, calls:[]};
  const work = dsmlNormalize(text);
  const calls = [];
  const seq = {stamp:Date.now().toString(36), n:0};
  let clean = work;
  const invokeRe = /<｜DSML｜\s*invoke\b([^>]*)>([\s\S]*?)<\/｜DSML｜\s*invoke>/gi;
  clean = clean.replace(invokeRe, (block, head, body) => {
    const name = tagAttr(head, 'name').trim();
    const args = {};
    const paramRe = /<｜DSML｜\s*parameter\b([^>]*)>([\s\S]*?)<\/｜DSML｜\s*parameter>/gi;
    let pm;
    while ((pm = paramRe.exec(body))) {
      const pn = tagAttr(pm[1], 'name');
      if (!pn) continue;
      const raw = pm[2].trim();
      const isString = tagAttr(pm[1], 'string').toLowerCase() !== 'false';
      args[pn] = isString ? raw : textCallArgs(raw);
    }
    return addTextCall(calls, name, args, allowed, seq) ? '' : block;
  });
  if (!calls.length) {
    const paramRe = /<｜DSML｜\s*parameter\b([^>]*)>([\s\S]*?)(?:<\/｜DSML｜\s*parameter>|(?=<｜DSML｜|$))/gi;
    let pending = '', pm;
    while ((pm = paramRe.exec(work))) {
      const pn = tagAttr(pm[1], 'name');
      const raw = pm[2].trim();
      if (pn === 'name') pending = raw;
      else if (pn === 'arguments' && pending) {
        const args = textCallArgs(raw);
        if (addTextCall(calls, pending, args, allowed, seq)) pending = '';
      }
    }
    if (calls.length) clean = work.replace(/<｜DSML｜[\s\S]*$/, '');
  }
  const toolRe = /<tool_call>\s*([\s\S]*?)\s*<\/tool_call>/gi;
  clean = clean.replace(toolRe, (block, body) => {
    let value = null;
    try { value = JSON.parse(body); } catch (e) { return block; }
    const list = Array.isArray(value) ? value : [value];
    let any = false;
    for (const item of list) {
      const call = jsonTextCall(item);
      if (call && addTextCall(calls, call.name, call.args, allowed, seq)) any = true;
    }
    return any ? '' : block;
  });
  if (calls.length) {
    clean = clean.replace(/<｜DSML｜[^>]*>[\s\S]*?(<\/｜DSML｜[^>]*>|$)/g, '');
  }
  return {content:clean.trim(), calls};
}
function stripToolMarkup(text) {
  if (!text || !(/DSML|<tool_call>/i.test(text))) return text;
  let out = dsmlNormalize(text)
    .replace(/<｜DSML｜\s*(?:invoke|parameter)\b[^>]*>[\s\S]*?<\/｜DSML｜\s*(?:invoke|parameter)>/gi, '')
    .replace(/<tool_call>[\s\S]*?<\/tool_call>/gi, '');
  out = out.replace(/<｜DSML｜[\s\S]*$/i, '').replace(/<tool_call>[\s\S]*$/i, '');
  return out.trim();
}
function toolResultText(result, meta) {
  let text;
  try { text = JSON.stringify(result); } catch (e) { text = String(result); }
  const ctx = meta && meta.ctx ? meta.ctx : 0;
  const limit = ctx ? Math.max(4000, Math.min(24000, Math.floor(ctx * 0.6))) : 24000;
  if (text.length <= limit) return text;
  return JSON.stringify({truncated:true,
    note:'工具结果过长，已按当前模型上下文截断；需要更多内容请分块读取',
    preview:text.slice(0, limit)});
}
function buildMessages(p, sys) {
  const out = [];
  if (sys) out.push({role:'system',content:sys});
  for (let index = 0; index < p.messages.length; index++) {
    const m = p.messages[index];
    if (m.pending || m.err) continue;
    if (m.role === 'assistant' && m.tool_calls?.length) {
      const results = [];
      for (let i = index + 1; i < p.messages.length && p.messages[i].role === 'tool'; i++) results.push(p.messages[i]);
      // Match only the immediately following batch; model call IDs may recur in later turns.
      if (results.length !== m.tool_calls.length || !m.tool_calls.every(c => results.filter(r => r.tool_call_id === c.id).length === 1)) continue;
      out.push({role:'assistant',content:m.content || null,tool_calls:m.tool_calls});
      for (const r of results) out.push({role:'tool',tool_call_id:r.tool_call_id,content:r.content || ''});
      index += results.length;
    } else if ((m.role === 'user' || m.role === 'assistant') && m.content) out.push({role:m.role,content:m.content});
  }
  return out;
}

function setBusy(b) {
  busy = b;
  const sendBtn = $('#wc-send');
  const stopBtn = $('#wc-stop');
  if (sendBtn) sendBtn.disabled = b;
  if (stopBtn) stopBtn.disabled = !b;
}

function stop() {
  if (aborter) { try { aborter.abort(); } catch (e) { /* 已结束 */ } }
}

function consumeFrames(buf, onFrame) {
  for (;;) {
    // SSE 规范接受 CRLF / LF / CR 三种行尾，三者都要认
    const m = /\r\n\r\n|\n\n|\r\r/.exec(buf);
    if (!m) break;
    const frame = buf.slice(0, m.index);
    buf = buf.slice(m.index + m[0].length);
    onFrame(frame);
  }
  return buf;
}

function usageCached(u) {
  if (!u || typeof u !== 'object') return 0;
  const details = u.prompt_tokens_details && typeof u.prompt_tokens_details === 'object' ? u.prompt_tokens_details : {};
  return Number(details.cached_tokens ?? u.cached_tokens ?? u.cache_read_input_tokens ?? u.prompt_cache_hit_tokens ?? 0) || 0;
}

function parseDelta(j, reply, stats) {
  const choice = (j.choices || [])[0] || {};
  const d = choice.delta || choice.message || {};
  if (typeof d.content === 'string') reply.content += d.content;
  const rc = typeof d.reasoning_content === 'string' ? d.reasoning_content
    : (typeof d.reasoning === 'string' ? d.reasoning : '');
  if (rc) reply.reasoning = clip(reply.reasoning + rc, MAX_CONTENT);
  if (Array.isArray(d.tool_calls)) {
    reply.tool_calls ||= [];
    for (const call of d.tool_calls) {
      const index = Number.isInteger(call.index) ? call.index : reply.tool_calls.length;
      if (index < 0 || index >= 16) throw new Error('一次最多调用 16 个工具');
      const target = reply.tool_calls[index] ||= {id:'',type:'function',function:{name:'',arguments:''}};
      if (call.id) target.id = clip(call.id,100);
      if (call.function?.name) target.function.name += clip(call.function.name,100);
      if (call.function?.arguments) target.function.arguments += call.function.arguments;
      if (target.function.arguments.length > 512 * 1024 || target.function.name.length > 100) throw new Error('工具参数超过上限');
    }
  }
  if (j.model) stats.upModel = j.model;
  if (j.usage) {
    if (j.usage.prompt_tokens) stats.inTok = j.usage.prompt_tokens;
    if (j.usage.completion_tokens) stats.outTok = j.usage.completion_tokens;
    stats.cachedTok = usageCached(j.usage);
  }
}

async function send() {
  if (busy) { toast('上一条还没结束', 'warning'); return; }
  const p = active();
  if (!p) return;
  const model = ($('#wc-model').value || '').trim();
  const input = $('#wc-input');
  const text = (input && input.value || '').trim();
  if (!model) { toast('先配置一个模型', 'warning'); return; }
  if (!text) return;
  if (text.length > MAX_CONTENT) { toast('消息过长，请拆分后发送', 'warning'); return; }
  if (p.messages.length >= MAX_MESSAGES - 40) { toast('对话已达上限，请导出后新建项目', 'warning'); return; }

  const agentEnabled = !!(AGENT && $('#wc-agent')?.checked);
  const userSys = ($('#wc-system').value || '').trim();
  const modelMeta = MODEL_META.get(model);
  const sys = agentEnabled && AGENT.systemPrompt
    ? [AGENT.systemPrompt,
       modelMeta && modelMeta.ctx ? '当前模型上下文上限约 ' + modelMeta.ctx + ' token；长文件请分块处理，避免一次塞满上下文。' : '',
       userSys].filter(Boolean).join(String.fromCharCode(10, 10))
    : userSys;
  const stream = $('#wc-stream').checked;
  const effort = ($('#wc-effort')?.value || '').trim();
  p.model = model;
  p.system = userSys;
  p.stream = stream;
  p.effort = effort;

  p.messages.push({role: 'user', content: text, reasoning: '', err: '', at: Date.now()});
  let reply = {role: 'assistant', content: '', reasoning: '', err: '', pending: true, at: Date.now()};
  p.messages.push(reply);
  p.updated_at = Date.now();
  input.value = '';
  save();
  renderAll();

  setBusy(true);
  aborter = new AbortController();

  p.agent = agentEnabled;
  let messages = buildMessages(p, sys);
  const stats = {upModel: '', inTok: 0, outTok: 0, cachedTok: 0};
  const t0 = performance.now();
  const raw = [];
  let ttfb = null, events = 0, format = '', status = 0;
  const note = s => { if (raw.length < MAX_RAW) raw.push(clip(s, MAX_RAW_CHARS)); };

  try {
    const headers = {'Content-Type': 'application/json', 'X-NGW-Skip-Training': '1'};
    const tok = await ensureToken(false);
    if (tok) headers.Authorization = 'Bearer ' + tok;

    for (let round = 0; round < (agentEnabled ? 8 : 1); round++) {
    if (aborter.signal.aborted) throw new DOMException('已停止','AbortError');
    const roundStats = {upModel:'',inTok:0,outTok:0,cachedTok:0};
    const request = {model,messages,stream};
    if (effort) request.reasoning_effort = effort;
    if (agentEnabled) {
      request.tools = AGENT.tools; request.tool_choice = 'auto';
      // 按网关同步的输出上限收口；网关还会再按上下文/输出限制夹一次
      if (modelMeta && modelMeta.out > 0) request.max_tokens = Math.min(modelMeta.out, 32768);
    }
    if (stream) request.stream_options = {include_usage:true};
    const r = await fetch(B + 'v1/chat/completions', {
      method: 'POST', headers, signal: aborter.signal, credentials: 'same-origin',
      body: JSON.stringify(request),
    });
    status = r.status;
    format = (r.headers.get('content-type') || '').split(';')[0];

    if (!r.ok) {
      const txt = await r.text();
      note(txt);
      let msg = txt.slice(0, 300);
      try {
        const j = JSON.parse(txt);
        if (j && j.error) msg = j.error.message || msg;
      } catch (e) { /* 非 JSON 就原样显示 */ }
      reply.err = msg || ('HTTP ' + status);
      if (status >= 500) reply.err += '（请到后台「模型接入」确认模型已接入、渠道有可用账号）';
      const rid = r.headers.get('x-request-id') || '';
      if (/^[A-Za-z0-9_-]{1,64}$/.test(rid)) reply.err += ' [请求 ' + rid + ']';
    } else if (!stream) {
      const txt = await r.text();
      note(txt);
      ttfb = performance.now() - t0;
      let j = null;
      try { j = JSON.parse(txt); } catch (e) { j = null; }
      if (j) {
        const msg = ((j.choices || [])[0] || {}).message || {};
        reply.content = clip(typeof msg.content === 'string' ? msg.content : '', MAX_CONTENT);
        reply.reasoning = clip(typeof msg.reasoning_content === 'string' ? msg.reasoning_content
          : (typeof msg.reasoning === 'string' ? msg.reasoning : ''), MAX_CONTENT);
        roundStats.upModel = j.model || '';
        roundStats.inTok = (j.usage || {}).prompt_tokens || 0;
        roundStats.outTok = (j.usage || {}).completion_tokens || 0;
        roundStats.cachedTok = usageCached(j.usage);
        if (msg.tool_calls) {
          reply.tool_calls = sanitizeCalls(msg.tool_calls);
          if (!Array.isArray(msg.tool_calls) || reply.tool_calls.length !== msg.tool_calls.length) throw new Error('模型返回的工具调用格式无效');
        }
        if (j.error) reply.err = j.error.message || JSON.stringify(j.error);
      } else {
        reply.err = txt.slice(0, 300) || '响应不是合法 JSON';
      }
    } else if (r.body) {
      const reader = r.body.getReader();
      const dec = new TextDecoder();
      let buf = '';
      const handleFrame = frame => {
        for (const line of frame.split(/\r?\n/)) {
          const t = line.trim();
          if (!t || t.startsWith(':')) continue;
          if (!t.startsWith('data:')) continue;
          const payload = t.slice(5).trim();
          if (!payload) continue;
          note(payload);
          if (payload === '[DONE]') continue;
          if (ttfb === null) ttfb = performance.now() - t0;
          events++;
          let j = null;
          try { j = JSON.parse(payload); } catch (e) { return; }
          if (j.error) { reply.err = j.error.message || JSON.stringify(j.error); return; }
          parseDelta(j, reply, roundStats);
          if (reply.content.length > MAX_CONTENT) reply.content = reply.content.slice(0, MAX_CONTENT);
          scheduleRender(true);
          scheduleSave();
        }
      };
      for (;;) {
        const chunk = await reader.read();
        if (chunk.done) break;
        buf += dec.decode(chunk.value, {stream: true});
        if (buf.length > 1024 * 1024) throw new Error('事件流单帧过大，已停止接收');
        buf = consumeFrames(buf, handleFrame);
      }
      // 上游可能不发末尾空行就断开：残留的最后一帧仍然算数
      buf += dec.decode();
      if (buf.trim()) handleFrame(buf);
    } else {
      reply.err = '无响应体（HTTP ' + status + '）';
    }
    stats.inTok += roundStats.inTok; stats.outTok += roundStats.outTok; stats.cachedTok += roundStats.cachedTok; stats.upModel = roundStats.upModel;
    p.cacheHit = (p.cacheHit || 0) + roundStats.cachedTok;
    p.cacheTotal = (p.cacheTotal || 0) + roundStats.inTok;
    if (agentEnabled) {
      const allowed = AGENT.tools.map(t => t.function.name);
      const parsed = parseTextToolCalls(reply.content, allowed);
      if (parsed.calls.length) {
        reply.content = parsed.content;
        if (!reply.tool_calls?.length) reply.tool_calls = parsed.calls;
      } else {
        reply.content = stripToolMarkup(reply.content);
      }
    }
    if (reply.err || !reply.tool_calls?.length) break;
    if (!agentEnabled) { reply.err = '模型返回了工具调用，请启用本地工具后重试'; break; }
    const calls = sanitizeCalls(reply.tool_calls);
    if (p.messages.length + calls.length + 1 > MAX_MESSAGES) throw new Error('对话已达上限，请导出后新建项目');
    if (calls.length !== reply.tool_calls.length || new Set(calls.map(c => c.id)).size !== calls.length || calls.some(c => !c.id)) throw new Error('模型返回的工具调用格式无效');
    reply.pending = false;
    for (const call of calls) {
      if (aborter.signal.aborted) throw new DOMException('已停止','AbortError');
      let result;
      try {
        result = await AGENT.execute(p,call,{signal:aborter.signal,
          confirmWrite: (path,content) => uiConfirm('将写入本机目录中的 ' + path + '，已有文件会被替换。\n\n内容预览：\n' + writePreview(content), {title:'写入文件',okText:'写入',danger:true,signal:aborter.signal}),
          confirmDelete: (path) => uiConfirm('删除 ' + path + '？删除后无法从工作区恢复。', {title:'删除文件',okText:'删除',danger:true,signal:aborter.signal}),
          confirmMove: (from,to) => uiConfirm('将 ' + from + ' 移动到 ' + to + '？', {title:'移动文件',okText:'移动',danger:true,signal:aborter.signal})});
      } catch (e) { if (e.name === 'AbortError') throw e; result = {error:e.message || String(e)}; }
      p.messages.push({role:'tool', tool_call_id:call.id, content:toolResultText(result, modelMeta), at:Date.now()});
      save(); renderWorkspace(); renderChat();
    }
    if (round === 7) {
      reply = {role:'assistant',content:'',reasoning:'',err:'已达到 8 轮上限。可继续发送消息处理剩余任务。',at:Date.now()};
      p.messages.push(reply); break;
    }
    messages = buildMessages(p,sys);
    reply = {role:'assistant',content:'',reasoning:'',err:'',pending:true,at:Date.now()};
    p.messages.push(reply); renderChat();
    }
  } catch (e) {
    if (aborter) aborter.abort();
    reply.err = e && e.name === 'AbortError'
      ? '已停止'
      : String((e && e.message) || e).slice(0, 300);
  } finally {
    reply.pending = false;
    aborter = null;
    setBusy(false);
    if (renderTimer) { clearTimeout(renderTimer); renderTimer = null; }
    renderPartial = false;
    p.updated_at = Date.now();
    save();
    renderAll();
    renderDiag({
      status, ttfb, total: performance.now() - t0, events,
      inTok: stats.inTok, outTok: stats.outTok, cachedTok: stats.cachedTok,
      model, up: stats.upModel, format, err: reply.err, raw: raw.join('\n'),
    });
  }
}

/* ---------------------------------------------------------------- 导入导出 */

function projectForExport(p) {
  return {
    name: p.name,
    model: p.model || '',
    system: p.system || '',
    stream: p.stream !== false,
    effort: p.effort || '',
    agent: p.agent === true,
    cacheHit: p.cacheHit || 0,
    cacheTotal: p.cacheTotal || 0,
    github: p.github ? {repo:p.github.repo||'',branch:p.github.branch||'main',allowWrite:p.github.allowWrite===true} : undefined,
    files: p.files || {},
    created_at: p.created_at || 0,
    updated_at: p.updated_at || 0,
    messages: p.messages
      .filter(m => !m.pending)
      .map(m => ({
        role: m.role,
        ...(m.tool_calls ? {tool_calls:m.tool_calls} : {}),
        ...(m.tool_call_id ? {tool_call_id:m.tool_call_id} : {}),
        content: m.content || '',
        reasoning: m.reasoning || '',
        err: m.err || '',
        at: m.at || 0,
      })),
  };
}

function slug(s) {
  const out = String(s || 'project')
    .replace(/[\\/:*?"<>|\r\n\t ]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 40);
  return out || 'project';
}

function stamp() {
  const d = new Date();
  const p = n => String(n).padStart(2, '0');
  return '' + d.getFullYear() + p(d.getMonth() + 1) + p(d.getDate()) + '-' + p(d.getHours()) + p(d.getMinutes());
}

function download(filename, obj) {
  const blob = new Blob([JSON.stringify(obj, null, 2)], {type: 'application/json'});
  const url = URL.createObjectURL(blob);
  const a = el('a');
  a.href = url;
  a.download = filename;
  a.rel = 'noopener';
  document.body.appendChild(a);
  a.click();
  // 立刻 revoke 会让部分浏览器来不及取内容，留出余量再回收
  setTimeout(() => { URL.revokeObjectURL(url); a.remove(); }, 2000);
}

function exportAll() {
  if (!state.projects.length) { toast('没有可导出的项目', 'warning'); return; }
  download('webclaw-all-' + stamp() + '.json', {
    app: 'webclaw',
    version: 1,
    kind: 'bundle',
    exported_at: new Date().toISOString(),
    projects: state.projects.map(projectForExport),
  });
  toast('已导出 ' + state.projects.length + ' 个项目');
}

function exportOne(id) {
  const p = state.projects.find(x => x.id === id);
  if (!p) return;
  download('webclaw-' + slug(p.name) + '-' + stamp() + '.json', {
    app: 'webclaw',
    version: 1,
    kind: 'single',
    exported_at: new Date().toISOString(),
    projects: [projectForExport(p)],
  });
  toast('已导出「' + p.name + '」');
}

function readFile(file) {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => resolve(String(r.result || ''));
    r.onerror = () => reject(r.error || new Error('读取失败'));
    r.readAsText(file, 'utf-8');
  });
}

// 导入重名时一直加后缀直到唯一：文件里自带重名时单次加「（导入）」仍会撞车
function uniqueName(taken, base) {
  if (!taken.has(base)) return base;
  for (let k = 1; ; k++) {
    const candidate = base + '（导入' + (k > 1 ? ' ' + k : '') + '）';
    if (!taken.has(candidate)) return candidate;
  }
}

function extractProjects(data) {
  let arr = [];
  if (Array.isArray(data)) arr = data;
  else if (data && Array.isArray(data.projects)) arr = data.projects;
  else if (data && Array.isArray(data.messages)) arr = [data];
  const out = [];
  for (const raw of arr) {
    if (out.length >= MAX_PROJECTS) break;
    const p = normalizeStored(raw);
    if (!p) continue;
    p.id = uid();          // 一律换新 id：避免导入覆盖本地同 id 项目
    out.push(p);
  }
  return out;
}

async function handleImport(file) {
  if (busy) { toast('请先停止当前任务，再操作项目', 'warning'); return; }
  if (!file) return;
  if (file.size > MAX_IMPORT_BYTES) {
    toast('文件过大（上限 ' + Math.round(MAX_IMPORT_BYTES / 1048576) + 'MB）', 'danger');
    return;
  }
  let data = null;
  try {
    data = JSON.parse(await readFile(file));
  } catch (e) {
    toast('不是有效的 JSON 文件', 'danger');
    return;
  }
  const list = extractProjects(data);
  if (!list.length) { toast('文件里没有可导入的项目', 'danger'); return; }

  const how = await uiChoose('文件含 ' + list.length + ' 个项目，选择导入方式：', [
    {label: '合并', value: 'merge', cls: 'btn-primary'},
    {label: '覆盖全部', value: 'replace', cls: 'btn-outline-danger'},
    {label: '取消', value: null, cls: 'btn-outline-secondary'},
  ]);
  if (!how) return;

  if (how === 'replace') {
    const ok = await uiConfirm('覆盖会删掉本机现有的 ' + state.projects.length + ' 个项目。', {danger: true, okText: '覆盖'});
    if (!ok) return;
    stop();
    if (AGENT) state.projects.forEach(p => AGENT.disconnect(p));
    state.projects = list;
    state.active = list[0].id;
  } else {
    if (state.projects.length + list.length > MAX_PROJECTS) {
      toast('项目数量会超过上限 ' + MAX_PROJECTS, 'danger');
      return;
    }
    const names = new Set(state.projects.map(p => p.name));
    for (const p of list) {
      p.name = uniqueName(names, p.name);
      names.add(p.name);
      state.projects.push(p);
    }
    if (!state.projects.some(p => p.id === state.active)) state.active = state.projects[0].id;
  }

  save();
  renderAll();
  toast('已导入 ' + list.length + ' 个项目');
}

function renderWorkspace() {
  const box = $('#wc-files'), p = active();
  if (!box || !p || !AGENT) return;
  box.innerHTML = '';
  const connected = AGENT.connected(p);
  $('#wc-workspace-status').textContent = connected ? '已连接本机目录' : Object.keys(p.files || {}).length + ' 个浏览器文件';
  $('#wc-disconnect').hidden = !connected;
  for (const [path,content] of Object.entries(p.files || {})) {
    const row = el('div','workspace-file');
    const download = el('button','',path); download.title = '下载文件';
    download.onclick = () => {
      const blob = new Blob([content],{type:'text/plain;charset=utf-8'}), url = URL.createObjectURL(blob);
      const a = el('a'); a.href = url; a.download = path.split('/').pop(); document.body.appendChild(a); a.click(); a.remove();
      setTimeout(() => URL.revokeObjectURL(url),1000);
    };
    const remove = el('button','','移除');
    remove.onclick = guard(async () => { if (busy) throw new Error('请先停止当前任务'); if (await uiConfirm('移除工作区文件 ' + path + '？')) { delete p.files[path]; save(); renderWorkspace(); } });
    row.append(download,remove); box.appendChild(row);
  }
  renderContext();
}
function bindWorkspace() {
  if (!AGENT) return;
  $('#wc-agent').onchange = () => { const p = active(); if (p) { p.agent = $('#wc-agent').checked; save(); renderContext(); } };
  $('#wc-attach').onclick = () => { if (busy) return toast('请先停止当前任务','warning'); $('#wc-attach-file').click(); };
  $('#wc-attach-file').onchange = guard(async e => {
    const files = Array.from(e.target.files || []); e.target.value = '';
    if (busy) throw new Error('请先停止当前任务');
    const p = active(); const draft = {...p,files:AGENT.cleanFiles(p.files)};
    for (const file of files) {
      if (file.size > AGENT.MAX_FILE) throw new Error(file.name + ' 超过 256 KiB');
      const text = await file.text();
      if (text.includes('\u0000') || text.includes('\ufffd')) throw new Error(file.name + ' 不是 UTF-8 文本');
      AGENT.putFile(draft,file.name,text);
    }
    p.files = draft.files; save(); renderWorkspace(); toast('文件已添加，启用本地工具后可供 Agent 读取');
  });
  $('#wc-directory').onclick = guard(async () => {
    if (busy) throw new Error('请先停止当前任务');
    const p = active();
    try {
      await AGENT.connect(p);
      renderWorkspace();
    } catch (e) {
      if (e && e.name === 'AbortError') return;
      throw e;
    }
  });
  $('#wc-disconnect').onclick = () => { if (busy) return toast('请先停止当前任务','warning'); AGENT.disconnect(active()); renderWorkspace(); };
}

/* ---------------------------------------------------------------- 初始化 */

function applyMode() {
  const scope = $('#wc-scope');
  const fetchBtn = $('#wc-token-fetch');
  const tokenInput = $('#wc-token');
  if (MODE === 'user') {
    // 用户侧没有「后台令牌」这个概念：按钮换成取自己的 Key，并提示计费口径
    if (fetchBtn) {
      fetchBtn.textContent = '取我的 Key';
      fetchBtn.title = '签发/复用你名下用于测试的 Key';
    }
    if (tokenInput) {
      tokenInput.placeholder = '你的 API Key（自动获取）';
      tokenInput.readOnly = true;   // 只由服务端签发，避免手填或回填出错
    }
    if (scope) scope.textContent = '调用计入余额与限流';
  } else {
    if (scope) scope.textContent = '';
  }
}

function bindUI() {
  const list = $('#wc-projects');
  list.addEventListener('click', e => {
    const item = e.target.closest('.proj');
    if (!item) return;
    const id = item.dataset.id;
    const opBtn = e.target.closest('[data-op]');
    if (opBtn) {
      e.stopPropagation();
      const op = opBtn.dataset.op;
      if (op === 'rename') guard(renameProject)(id);
      else if (op === 'delete') guard(deleteProject)(id);
      else if (op === 'export') exportOne(id);
      return;
    }
    switchProject(id);
  });

  $('#wc-new').onclick = guard(createProject);
  $('#wc-clear').onclick = guard(clearChat);
  $('#wc-send').onclick = guard(send);
  $('#wc-stop').onclick = stop;
  $('#wc-export-all').onclick = guard(exportAll);
  $('#wc-export-one').onclick = () => exportOne(state.active);
  $('#wc-import').onclick = () => $('#wc-import-file').click();

  const fileInput = $('#wc-import-file');
  fileInput.addEventListener('change', e => {
    const f = e.target.files && e.target.files[0];
    e.target.value = '';            // 先清空，同一文件才能重复选
    guard(handleImport)(f);
  });

  $('#wc-reload').onclick = guard(async () => {
    tokenCache = null;
    await ensureToken(true);
    await loadModels(active() && active().model);
    toast('模型已刷新');
  });

  $('#wc-token-fetch').onclick = guard(async () => {
    tokenCache = null;
    await ensureToken(true);
    if (tokenValue()) toast(MODE === 'user' ? '已取到你的 Key' : '已取后台令牌');
    else toast(MODE === 'user' ? '取 Key 失败，检查登录状态' : '后台未配置令牌', 'warning');
  });

  $('#wc-token-reveal').onclick = () => {
    const i = $('#wc-token');
    const show = i.type === 'password';
    i.type = show ? 'text' : 'password';
    $('#wc-token-reveal').firstChild.className = show ? 'bi bi-eye-slash' : 'bi bi-eye';
  };

  $('#wc-token').addEventListener('input', () => {
    tokenCache = tokenValue() || null;
    tokenHint();
  });

  $('#wc-system').addEventListener('input', () => {
    const p = active();
    if (!p) return;
    p.system = $('#wc-system').value;
    p.updated_at = Date.now();
    renderContext();
    scheduleSave();
  });

  $('#wc-stream').addEventListener('change', () => {
    const p = active();
    if (!p) return;
    p.stream = $('#wc-stream').checked;
    p.updated_at = Date.now();
    scheduleSave();
  });

  $('#wc-effort').addEventListener('change', () => {
    const p = active();
    if (!p) return;
    p.effort = $('#wc-effort').value;
    p.updated_at = Date.now();
    syncEffortUI();
    scheduleSave();
  });

  $('#wc-context').onclick = () => togglePop('#wc-context-pop');
  $('#wc-effort-btn').onclick = () => togglePop('#wc-effort-pop');
  $('#wc-effort-range').oninput = () => setEffort(EFFORTS[Number($('#wc-effort-range').value) || 0]);
  document.addEventListener('click', e => {
    if (!e.target.closest('.wc-pop') && !e.target.closest('#wc-context') && !e.target.closest('#wc-effort-btn')) closePops();
  });
  document.addEventListener('keydown', e => {
    if ((e.ctrlKey || e.metaKey) && !e.altKey && e.key.toLowerCase() === 't') {
      e.preventDefault();
      cycleEffort();
    }
  });

  $('#gh-bind').onclick = guard(ghBind);
  $('#gh-unbind').onclick = guard(ghUnbind);
  $('#gh-refresh-runs').onclick = guard(ghRefreshRuns);
  $('#gh-allow').addEventListener('change', () => {
    const p = active();
    if (!p) return;
    p.github = p.github || {repo:'',branch:'main',token:'',allowWrite:false};
    p.github.allowWrite = $('#gh-allow').checked;
    p.updated_at = Date.now();
    scheduleSave();
  });

  $('#wc-model').addEventListener('change', () => {
    const p = active();
    if (!p) return;
    p.model = $('#wc-model').value;
    p.updated_at = Date.now();
    renderModelMeta();
    scheduleSave();
  });

  $('#wc-input').addEventListener('keydown', e => {
    if (e.key !== 'Enter' || e.shiftKey) return;
    if (e.isComposing || e.keyCode === 229) return;   // 中文输入法选词中，不发送
    e.preventDefault();
    guard(send)();
  });

  // 另一个标签页改了存储：先把本页待写的内容落定，再采用对方的版本
  window.addEventListener('storage', e => {
    if (e.key !== LS_KEY || busy) return;
    if (saveTimer) { clearTimeout(saveTimer); saveTimer = null; }
    state = loadState();
    renderAll();
  });

  window.addEventListener('beforeunload', flushSave);
}

async function init() {
  state = loadState();
  applyMode();
  bindUI();
  bindWorkspace();
  renderAll();
  tokenHint();
  await ensureToken(false);
  await loadModels(active() && active().model);
  const requested = window.location ? new URLSearchParams(window.location.search).get('model') : '';
  if (requested) {
    const available = Array.from($('#wc-model').options).some(o => o.value === requested);
    if (available && state.projects.length < MAX_PROJECTS) {
      const project = blankProject(requested.slice(0, MAX_NAME));
      project.model = requested;
      state.projects.push(project); state.active = project.id;
      save(); renderAll();
    } else toast(available ? '项目数量已达上限，请先删除或导出项目' : '该模型暂不可用，请重新选择', 'warning');
  }
  syncInputs();
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', () => { guard(init)(); });
} else {
  guard(init)();
}

window.NGW_CLAW = {parseTextToolCalls, stripToolMarkup, toolResultText, writePreview};
})();

