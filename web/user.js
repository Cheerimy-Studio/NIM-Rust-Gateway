const $ = s => document.querySelector(s);
const esc = s => String(s ?? '').replace(/[&<>"']/g, c =>
  ({'&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'}[c]));

let userCsrfToken = '';
let userCsrfRefresh = null;
async function getUserCsrfToken() {
  if (userCsrfToken) return userCsrfToken;
  if (!userCsrfRefresh) {
    userCsrfRefresh = fetch('api/user/session', {credentials: 'same-origin'})
      .then(async res => {
        const data = await res.json();
        if (!res.ok || !data.csrf) throw new Error('会话已过期，请重新登录');
        userCsrfToken = data.csrf;
        return userCsrfToken;
      }).finally(() => { userCsrfRefresh = null; });
  }
  return userCsrfRefresh;
}

async function api(path, opts = {}) {
  const init = {method: opts.method || 'GET', headers: {}};
  if (!['GET', 'HEAD', 'OPTIONS'].includes(init.method.toUpperCase())) {
    init.headers['X-CSRF'] = await getUserCsrfToken();
  }
  if (opts.json !== undefined) { init.headers['Content-Type'] = 'application/json'; init.body = JSON.stringify(opts.json); }
  const res = await fetch('api/user/' + path, init);
  let data = null;
  try { data = await res.json(); } catch (e) { }
  if (!res.ok) {
    const error = new Error((data && data.error && data.error.message) || ('HTTP ' + res.status));
    error.status = res.status;
    throw error;
  }
  if (data === null) throw new Error('服务返回无效数据，请重试');
  if (path === 'logout') { userCsrfToken = ''; clearTimeout(signTimer); }
  return data;
}

const run = async fn => { try { return await fn(); } catch (e) { toast(e.message, 'danger'); } };

function toast(msg, kind) {
  const d = document.createElement('div');
  d.className = 'toast show align-items-center border-0' + (kind === 'danger' ? ' bg-danger' : ' bg-dark');
  d.style.cssText = 'position:fixed;top:18px;right:18px;z-index:9999;color:#fff';
  d.setAttribute('role', kind === 'danger' ? 'alert' : 'status');
  d.setAttribute('aria-live', kind === 'danger' ? 'assertive' : 'polite');
  const inner = document.createElement('div');
  inner.className = 'd-flex';
  const body = document.createElement('div');
  body.className = 'toast-body';
  body.textContent = msg;
  inner.appendChild(body);
  d.appendChild(inner);
  document.body.appendChild(d);
  setTimeout(() => d.remove(), 3200);
}

function fmtTime(t) {
  if (!t) return '-';
  const d = new Date(t * 1000);
  const p = n => String(n).padStart(2, '0');
  return `${d.getMonth() + 1}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

function copyText(text) {
  if (navigator.clipboard && navigator.clipboard.writeText) {
    return navigator.clipboard.writeText(text).then(() => true).catch(() => fallbackCopy(text));
  }
  return Promise.resolve(fallbackCopy(text));
}
function fallbackCopy(text) {
  const ta = document.createElement('textarea');
  ta.value = text;
  ta.style.cssText = 'position:fixed;opacity:0;top:0;left:0';
  document.body.appendChild(ta);
  ta.select();
  let ok = false;
  try { ok = document.execCommand('copy'); } catch (e) { ok = false; }
  document.body.removeChild(ta);
  return ok;
}

function uiConfirm(msg) { return window.NGW_UI.dialog({title: '删除 Key', body: msg, danger: true, okText: '删除'}); }

let ME = null;
bindLogsKind();

function showLogin() {
  ME = null;
  userCsrfToken = '';
  clearTimeout(signTimer);
  $('#login-view').style.display = 'flex';
  $('#panel-view').style.display = 'none';
  showLoginTab('main');
}

function showLoginTab(tab) {
  for (const t of ['main', 'register', 'forgot', 'reset']) {
    const el = document.getElementById('login-tab-' + t);
    if (el) el.style.display = t === tab ? 'block' : 'none';
  }
  $('#login-err').style.display = 'none';
}

function lgErr(msg) {
  const e = document.querySelector('#login-err');
  if (!msg) { e.style.display = 'none'; return; }
  e.textContent = msg;
  e.style.display = 'block';
}

function resetTokenFromUrl() {
  return new URLSearchParams(location.search).get('reset') || '';
}
function showPanel() {
  const deviceCode = new URLSearchParams(location.search).get('device');
  if (deviceCode) { location.replace('/user/device?code=' + encodeURIComponent(deviceCode)); return; }
  $('#login-view').style.display = 'none';
  $('#panel-view').style.display = 'block';
}

async function loadMe() {
  ME = await api('me');
  const endpoint = $('#api-base');
  if (endpoint) endpoint.value = location.origin + '/v1';
  $('#ov-balance').textContent = rewardMoney(ME.balance);
  $('#ov-grant').textContent = '¥' + rewardMoney(ME.grant);
  $('#ov-grant-total').textContent = '¥' + rewardMoney(ME.grant_total);
  $('#ov-recharge').textContent = '¥' + rewardMoney(ME.recharge);
  $('#ov-recharge-total').textContent = '¥' + rewardMoney(ME.recharge_total);
  try {
    const s = await api('stats');
    $('#ov-calls').textContent = s.total_calls ?? 0;
    $('#ov-calls-sub').textContent = '免费 ' + (s.free_calls ?? 0) + ' / 付费 ' + (s.paid_calls ?? 0);
    $('#ov-cost').textContent = '¥' + rewardMoney(s.total_cost);
    $('#ov-keys').textContent = s.key_count ?? 0;
    const tb = $('#ov-recent');
    tb.innerHTML = '';
    const recent = s.recent || [];
    if (!recent.length) {
      tb.innerHTML = '<tr><td colspan="4" class="hint text-center py-4">还没有调用记录，去「模型广场」挑一个模型试试</td></tr>';
    }
    for (const r of recent) {
      const tr = document.createElement('tr');
      const st = r.st >= 200 && r.st < 400
        ? '<span class="badge bg-success">OK</span>'
        : '<span class="badge bg-danger">' + r.st + '</span>';
      tr.innerHTML = '<td class="small text-muted">' + fmtTime(r.t) + '</td>'
        + '<td class="small text-truncate" style="max-width:160px">' + esc(r.model || '-') + (r.billing ? '<div class="small text-muted">' + esc(r.billing.mode === 'token' ? '按 token · 输入 ' + r.billing.input + ' / 输出 ' + r.billing.output + ' 元/百万' : '按次') + '</div>' : '') + '</td>'
        + '<td>' + st + '</td>'
        + '<td class="text-end small fw-semibold">' + (r.cost > 0 ? '¥' + r.cost.toFixed(6) : '免费') + '</td>';
      tb.appendChild(tr);
    }
  } catch (e) { }
  loadSign();
}

function rewardMoney(amount) {
  return Number(amount || 0).toFixed(6).replace(/0+$/, '').replace(/\.$/, '');
}
let signing = false;
let signTimer = null;
async function loadSign() {
  clearTimeout(signTimer);
  const box = $('#sign-box');
  try {
    const d = await api('sign');
    box.style.display = d.enabled ? '' : 'none';
    if (d.enabled) signTimer = setTimeout(() => loadSign(), Math.min(86401, Math.max(1, d.reset_in || 86400)) * 1000 + 200);
    const btn = $('#btn-sign');
    btn.disabled = signing || d.signed_today;
    btn.textContent = d.signed_today ? '已签到' : '签到';
    $('#sign-hint').textContent = d.signed_today
      ? '今日已领 ¥' + rewardMoney(d.today_amount) : '每日签到领赠金';
  } catch (e) { box.style.display = 'none'; }
}

$('#btn-sign').onclick = () => run(async () => {
  if (signing) return;
  signing = true;
  $('#btn-sign').disabled = true;
  try {
    const d = await api('sign', {method: 'POST'});
    toast('签到成功，赠金 +¥' + rewardMoney(d.amount));
    await refreshWallet();
  } finally {
    signing = false;
    await loadSign();
  }
});

async function refreshWallet() {
  ME = await api('me');
  $('#ov-balance').textContent = rewardMoney(ME.balance);
  for (const name of ['grant','grant_total','recharge','recharge_total']) {
    $('#ov-' + name.replace('_','-')).textContent = '¥' + rewardMoney(ME[name]);
  }
}

async function loadKeys() {
  const rows = (await api('keys')).rows || [];
  const count = $('#key-count');
  if (count) count.textContent = rows.length + ' 个';
  const tb = $('#key-rows');
  tb.innerHTML = '';
  if (!rows.length) { tb.innerHTML = '<tr><td colspan="6" class="text-muted text-center py-4">还没有 Key，先在上方生成一个</td></tr>'; return; }
  const kindBadge = k => k === 'free'
    ? '<span class="badge bg-success-subtle text-success">仅免费</span>'
    : k === 'paid'
      ? '<span class="badge bg-warning-subtle text-warning-emphasis">仅付费</span>'
      : '<span class="badge bg-secondary-subtle text-secondary">全部</span>';
  for (const k of rows) {
    const tr = document.createElement('tr');
    tr.innerHTML = '<td>' + esc(k.name || '未命名') + '</td>'
      + '<td class="key-mono">' + esc(k.key.slice(0, 14)) + '…</td>'
      + '<td>' + kindBadge(k.kind || 'all') + '</td>'
      + '<td>' + (k.enabled ? '<span class="badge bg-success">启用</span>' : '<span class="badge bg-secondary">停用</span>') + '</td>'
      + '<td class="small text-muted">' + fmtTime(k.last_used_at) + '</td>';
    const op = document.createElement('td');
    op.className = 'text-end';
    const copy = document.createElement('button');
    copy.className = 'btn btn-sm btn-outline-secondary me-1';
    copy.textContent = '复制';
    copy.onclick = () => run(async () => { if (!await copyText(k.key)) throw new Error('复制失败，请检查浏览器权限'); toast('Key 已复制'); });
    const tog = document.createElement('button');
    tog.className = 'btn btn-sm btn-outline-secondary me-1';
    tog.textContent = k.enabled ? '停用' : '启用';
    tog.onclick = () => run(async () => { await api('keys/op', {method: 'POST', json: {id: k.id, op: k.enabled ? 'disable' : 'enable'}}); toast('已' + tog.textContent); loadKeys(); });
    const del = document.createElement('button');
    del.className = 'btn btn-sm btn-outline-danger';
    del.textContent = '删除';
    del.onclick = () => run(async () => {
      if (!await uiConfirm('删除该 Key 后，使用它的应用将无法继续调用。')) return;
      await api('keys/op', {method: 'POST', json: {id: k.id, op: 'delete'}});
      toast('已删除'); loadKeys();
    });
    op.append(copy, tog, del);
    tr.appendChild(op);
    tb.appendChild(tr);
  }
}

let LOGS_STATE = {kind: 'all', page: 1};
const LOGS_PER = 50;

function logsKindBtns() {
  return [
    ['all', $('#logs-kind-all')],
    ['paid', $('#logs-kind-paid')],
    ['free', $('#logs-kind-free')],
  ];
}

function bindLogsKind() {
  for (const [kind, btn] of logsKindBtns()) {
    btn.onclick = () => {
      LOGS_STATE = {kind, page: 1};
      logsKindBtns().forEach(([k, b]) => b.classList.toggle('active', k === kind));
      run(() => loadLogs());
    };
  }
}

async function loadLogs() {
  const d = await api('logs?kind=' + LOGS_STATE.kind + '&page=' + LOGS_STATE.page + '&per=' + LOGS_PER);
  const rows = d.rows || [];
  const total = d.total || 0;
  const per = d.per || LOGS_PER;
  const pages = Math.max(1, Math.ceil(total / per));
  LOGS_STATE.page = Math.min(LOGS_STATE.page, pages);
  const tb = $('#log-rows');
  tb.innerHTML = '';
  if (!rows.length) {
    tb.innerHTML = '<tr><td colspan="7" class="hint text-center py-4">'
      + (LOGS_STATE.kind === 'paid' ? '暂无付费模型调用记录' : LOGS_STATE.kind === 'free' ? '暂无免费模型调用记录' : '暂无调用记录')
      + '</td></tr>';
  }
  for (const r of rows) {
    const tr = document.createElement('tr');
    const st = r.st >= 200 && r.st < 400
      ? '<span class="badge bg-success">' + r.st + '</span>'
      : '<span class="badge bg-danger">' + r.st + '</span>';
    tr.innerHTML = '<td class="small text-muted">' + fmtTime(r.t) + '</td>'
      + '<td>' + esc(r.model || '-') + (r.billing ? '<div class="small text-muted">' + esc(r.billing.mode === 'token' ? '按 token · 输入 ' + r.billing.input + ' / 输出 ' + r.billing.output + ' 元/百万' : '按次') + '</div>' : '') + '</td>'
      + '<td>' + st + '</td>'
      + '<td class="small">' + r.ms + 'ms</td>'
      + '<td class="small">' + (r.in_tok || 0) + '</td>'
      + '<td class="small">' + (r.out_tok || 0) + '</td>'
      + '<td class="small fw-semibold">' + (r.cost > 0 ? '¥' + r.cost.toFixed(6) : '免费') + '</td>';
    tb.appendChild(tr);
  }
  $('#logs-meta').textContent = '共 ' + total + ' 条 · 每页 ' + per + ' 条';
  const nav = $('#logs-pages');
  nav.innerHTML = '';
  const mk = (label, page, disabled, active) => {
    const li = document.createElement('li');
    li.className = 'page-item' + (disabled ? ' disabled' : '') + (active ? ' active' : '');
    const a = document.createElement('a');
    a.className = 'page-link';
    a.href = 'javascript:void(0)';
    a.textContent = label;
    a.onclick = () => {
      if (disabled || active) return;
      LOGS_STATE.page = page;
      run(() => loadLogs());
    };
    li.appendChild(a);
    nav.appendChild(li);
  };
  mk('‹', LOGS_STATE.page - 1, LOGS_STATE.page <= 1, false);
  mk(LOGS_STATE.page + ' / ' + pages, LOGS_STATE.page, true, true);
  mk('›', LOGS_STATE.page + 1, LOGS_STATE.page >= pages, false);
}

let MODEL_ROWS = [];
const MODEL_FILTER = {kind:'all', provider:'', group:'', onlyStats:false, unit:'1m', sort:'name', view:'grid'};
const PROVIDER_COLORS = {
  Anthropic:'#d97757', OpenAI:'#10a37f', Google:'#4285f4', DeepSeek:'#4d6bfe',
  Moonshot:'#111827', Qwen:'#615ced', Zhipu:'#3859ff', Meta:'#0866ff', Mistral:'#f97316'
};
function providerOf(name) {
  const m = String(name || '').toLowerCase();
  if (m.includes('claude')) return 'Anthropic';
  if (m.startsWith('gpt') || m.includes('openai') || m.includes('o1') || m.includes('o3')) return 'OpenAI';
  if (m.includes('gemini')) return 'Google';
  if (m.includes('deepseek')) return 'DeepSeek';
  if (m.includes('kimi') || m.includes('moonshot')) return 'Moonshot';
  if (m.includes('qwen') || m.includes('qwq')) return 'Qwen';
  if (m.includes('glm') || m.includes('zhipu')) return 'Zhipu';
  if (m.includes('llama') || m.includes('meta')) return 'Meta';
  if (m.includes('mistral') || m.includes('mixtral')) return 'Mistral';
  const head = String(name || '').split('/')[0].trim();
  return head ? head.charAt(0).toUpperCase() + head.slice(1) : '其他';
}
function providerColor(name) {
  if (PROVIDER_COLORS[name]) return PROVIDER_COLORS[name];
  let h = 0;
  for (const ch of name) h = (h * 31 + ch.charCodeAt(0)) >>> 0;
  return ['#64748b','#0ea5e9','#8b5cf6','#ef4444','#14b8a6'][h % 5];
}
function modelGroup(m) { return m.chain ? 'route' : 'direct'; }
function groupLabel(g) { return g === 'route' ? '模型路由' : '直连模型'; }
function modelPrice(m) {
  const rules = (m.pricing || []).map(p => p.rule).filter(r => r !== undefined && r !== null);
  const tokens = rules.filter(r => r && typeof r === 'object' && r.mode === 'token');
  const requests = rules.filter(r => typeof r === 'number');
  if (tokens.length) {
    const pick = key => Math.min(...tokens.map(r => Number(r[key] != null ? r[key] : (key === 'cache' ? r.input : 0)) || 0));
    const input = pick('input'), output = pick('output'), cache = pick('cache');
    const tiers = tokens.some(r => (r.tiers || []).length);
    const paidTier = tokens.some(r => (r.tiers || []).some(t =>
      Number(t.input) > 0 || Number(t.output) > 0 || Number(t.cache) > 0));
    const free = input === 0 && output === 0 && cache === 0 && !paidTier;
    return {mode: free ? 'free' : 'token', input, output, cache, tiers, hasRequest: requests.length > 0};
  }
  if (requests.length) {
    const price = Math.min(...requests);
    return {mode: price > 0 ? 'request' : 'free', price};
  }
  return {mode: m.free ? 'free' : 'token', input: 0, output: 0, cache: 0, tiers: false};
}
function fmtMoney(n) {
  if (!Number.isFinite(n)) return '—';
  if (n === 0) return '0';
  if (n >= 1) return String(Math.round(n * 1000) / 1000);
  if (n >= 0.001) return String(Math.round(n * 1000000) / 1000000);
  return n.toFixed(8).replace(/0+$/, '').replace(/\.$/, '');
}
function fmtK(n) {
  n = Number(n) || 0;
  return n >= 1000 ? (Math.round(n / 100) / 10) + 'k' : String(n);
}
function fmtLatency(ms) {
  ms = Number(ms) || 0;
  if (!ms) return '—';
  return ms >= 1000 ? (ms / 1000).toFixed(ms >= 10000 ? 0 : 1) + 's' : Math.round(ms) + 'ms';
}
function fmtTps(n) {
  n = Math.round(Number(n) || 0);
  return n ? n + ' t/s' : '—';
}
function priceHtml(p) {
  if (p.mode === 'free') return '<div class="price-free">免费使用</div>';
  if (p.mode === 'request') {
    return '<div class="price-grid"><div class="price-cell"><span class="k">单次调用</span><span class="v">¥' +
      esc(fmtMoney(p.price)) + ' <small>/ 次</small></span></div><div class="price-cell"><span class="k">计费</span><span class="v">按次</span></div>' +
      '<div class="price-cell"><span class="k">阶梯</span><span class="v">无</span></div></div>';
  }
  const unit = MODEL_FILTER.unit === '1k' ? '1K' : '1M';
  const div = MODEL_FILTER.unit === '1k' ? 1000 : 1;
  const cell = (label, value) => '<div class="price-cell"><span class="k">' + label + '</span><span class="v">¥' +
    esc(fmtMoney(value / div)) + ' <small>/ ' + unit + '</small></span></div>';
  return '<div class="price-grid">' + cell('输入', p.input) + cell('输出', p.output) + cell('缓存', p.cache) + '</div>';
}
function renderModelSide() {
  const groups = [
    {id:'', label:'全部模型', count:MODEL_ROWS.length},
    {id:'route', label:'模型路由', count:MODEL_ROWS.filter(m => m.chain).length},
    {id:'direct', label:'直连模型', count:MODEL_ROWS.filter(m => !m.chain).length},
  ];
  const gbox = $('#model-groups');
  if (gbox) gbox.innerHTML = groups.map(g =>
    '<button type="button" class="group-btn' + (MODEL_FILTER.group === g.id ? ' active' : '') + '" data-group="' + esc(g.id) + '">' +
    '<span>' + esc(g.label) + '</span><span class="cnt">' + g.count + '</span></button>').join('');
  const counts = new Map();
  for (const m of MODEL_ROWS) {
    const p = providerOf(m.model);
    counts.set(p, (counts.get(p) || 0) + 1);
  }
  const providers = [...counts.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
  const pbox = $('#model-providers');
  if (pbox) pbox.innerHTML = '<button type="button" class="group-btn' + (!MODEL_FILTER.provider ? ' active' : '') +
    '" data-provider=""><span>全部供应商</span><span class="cnt">' + MODEL_ROWS.length + '</span></button>' +
    providers.map(([name, count]) =>
      '<button type="button" class="group-btn' + (MODEL_FILTER.provider === name ? ' active' : '') + '" data-provider="' + esc(name) + '">' +
      '<span><i class="bi bi-circle-fill me-2" style="font-size:8px;color:' + providerColor(name) + '"></i>' + esc(name) + '</span>' +
      '<span class="cnt">' + count + '</span></button>').join('');
}
function modelCard(m) {
  const p = modelPrice(m);
  const provider = providerOf(m.model);
  const color = providerColor(provider);
  const initial = provider.charAt(0).toUpperCase();
  const health = m.health == null ? '—' : m.health + '%';
  const healthCls = m.health == null ? '' : m.health >= 80 ? 'good' : m.health >= 50 ? 'warn' : 'bad';
  const heat = window.NGW_UI.heatSquares(m.recent || [], 20);
  const routes = Math.max(1, Number(m.routes) || (m.pricing || []).length || 1);
  const group = m.chain ? '模型路由' : '直连模型';
  const bill = p.mode === 'free' ? '<span class="bill-tag free">免费模型</span>'
    : p.mode === 'request' ? '<span class="bill-tag request">按次计费</span>'
    : '<span class="bill-tag">按量计费' + (p.tiers ? ' · 阶梯' : '') + '</span>';
  const rules = [...new Set((m.pricing || []).map(x => window.NGW_UI.pricingText(x.rule)))];
  const detailRules = rules.length ? rules.map(x => '<div>' + esc(x) + '</div>').join('') : '<div>未设置价格（免费）</div>';
  return '<article class="model-card" data-model="' + esc(m.model) + '">' +
    '<div class="tile-head"><span class="provider-mark" style="background:' + color + '">' + esc(initial) + '</span>' +
    '<div class="tile-title"><div class="name">' + esc(m.model) + '</div><div class="provider">' + esc(provider) + '</div></div>' +
    '<button type="button" class="copy-btn" title="复制模型名称" aria-label="复制模型名称"><i class="bi bi-copy"></i></button></div>' +
    '<div class="tile-desc">暂无描述</div>' + bill + priceHtml(p) +
    '<div class="tile-meta"><span>分组 ' + esc(group) + (routes > 1 ? ' <em>+' + (routes - 1) + '</em>' : '') + '</span>' +
    '<span>端点 openai, anthropic <em>+2</em></span></div>' +
    '<div class="tile-stats">' +
      '<div class="stat-cell"><span class="k">状态</span><span class="v ' + healthCls + '">' + health + '</span></div>' +
      '<div class="stat-cell"><span class="k">延迟</span><span class="v">' + fmtLatency(m.latency_ms) + '</span></div>' +
      '<div class="stat-cell"><span class="k">吞吐</span><span class="v">' + fmtTps(m.measured_tps || m.tps) + '</span></div>' +
      '<div class="tile-heat" title="最近 20 次请求，左旧右新">' + heat + '</div>' +
      '<button type="button" class="detail-btn">详情 <i class="bi bi-chevron-right"></i></button></div>' +
    '<div class="tile-details" hidden><div class="detail-grid">' +
      '<div><span class="k">上下文</span><span class="v">' + (m.ctx ? fmtK(m.ctx) : '未标注') + '</span></div>' +
      '<div><span class="k">输出上限</span><span class="v">' + (m.out ? fmtK(m.out) : '未标注') + '</span></div>' +
      '<div><span class="k">可用线路</span><span class="v">' + routes + '</span></div></div>' +
      '<div class="detail-rules"><b>计费规则</b>' + detailRules + '</div></div>' +
    '<div class="model-actions"><a class="btn btn-sm btn-primary" href="/webclaw?model=' + encodeURIComponent(m.model) +
      '" target="_blank" rel="noopener">打开 Agent</a><button type="button" class="btn btn-sm btn-outline-secondary copy-name">复制名称</button></div>' +
    '</article>';
}
async function loadModels() {
  MODEL_ROWS = (await api('models')).rows || [];
  renderModels();
}
function renderModels() {
  const query = ($('#model-search').value || '').trim().toLowerCase();
  let rows = MODEL_ROWS.filter(m => m.model.toLowerCase().includes(query));
  if (MODEL_FILTER.kind === 'free') rows = rows.filter(m => m.free && !m.paid);
  if (MODEL_FILTER.kind === 'paid') rows = rows.filter(m => m.paid);
  if (MODEL_FILTER.provider) rows = rows.filter(m => providerOf(m.model) === MODEL_FILTER.provider);
  if (MODEL_FILTER.group) rows = rows.filter(m => modelGroup(m) === MODEL_FILTER.group);
  if (MODEL_FILTER.onlyStats) rows = rows.filter(m => m.health != null || (m.recent || []).length);
  rows = rows.slice().sort((a, b) => {
    if (MODEL_FILTER.sort === 'price') return (Number(a.price) || 0) - (Number(b.price) || 0);
    if (MODEL_FILTER.sort === 'health') return (b.health == null ? -1 : b.health) - (a.health == null ? -1 : a.health);
    return a.model.localeCompare(b.model, 'zh-CN');
  });
  const box = $('#model-cards');
  box.classList.toggle('list', MODEL_FILTER.view === 'list');
  box.innerHTML = '';
  $('#model-count').textContent = rows.length;
  $('#model-total').textContent = '/ ' + MODEL_ROWS.length;
  renderModelSide();
  if (!rows.length) {
    const empty = document.createElement('div');
    empty.className = 'empty-state';
    empty.textContent = MODEL_ROWS.length ? '没有匹配的模型，请调整筛选条件。' : '暂无可用模型，请联系管理员接入。';
    box.append(empty);
    return;
  }
  box.innerHTML = rows.map(modelCard).join('');
  box.querySelectorAll('.model-card').forEach(card => {
    const name = card.dataset.model;
    card.querySelectorAll('.copy-btn, .copy-name').forEach(btn => {
      btn.onclick = () => run(async () => {
        if (!await copyText(name)) throw new Error('复制失败，请检查浏览器权限');
        toast('模型名称已复制');
      });
    });
    const detail = card.querySelector('.detail-btn');
    const panel = card.querySelector('.tile-details');
    detail.onclick = () => {
      panel.hidden = !panel.hidden;
      card.classList.toggle('open', !panel.hidden);
    };
  });
}
$('#model-search').addEventListener('input', renderModels);
$('#model-kind').addEventListener('click', e => {
  const btn = e.target.closest('[data-kind]');
  if (!btn) return;
  MODEL_FILTER.kind = btn.dataset.kind;
  document.querySelectorAll('#model-kind [data-kind]').forEach(b => b.classList.toggle('active', b === btn));
  renderModels();
});
$('#model-groups').addEventListener('click', e => {
  const btn = e.target.closest('[data-group]');
  if (!btn) return;
  MODEL_FILTER.group = btn.dataset.group;
  renderModels();
});
$('#model-providers').addEventListener('click', e => {
  const btn = e.target.closest('[data-provider]');
  if (!btn) return;
  MODEL_FILTER.provider = btn.dataset.provider;
  renderModels();
});
$('#model-only-stats').addEventListener('change', e => {
  MODEL_FILTER.onlyStats = e.target.checked;
  renderModels();
});
$('#model-unit').addEventListener('click', e => {
  const btn = e.target.closest('[data-unit]');
  if (!btn) return;
  MODEL_FILTER.unit = btn.dataset.unit;
  document.querySelectorAll('#model-unit [data-unit]').forEach(b => b.classList.toggle('active', b === btn));
  renderModels();
});
$('#model-view').addEventListener('click', e => {
  const btn = e.target.closest('[data-view]');
  if (!btn) return;
  MODEL_FILTER.view = btn.dataset.view;
  document.querySelectorAll('#model-view [data-view]').forEach(b => b.classList.toggle('active', b === btn));
  renderModels();
});
$('#model-sort').addEventListener('change', e => {
  MODEL_FILTER.sort = e.target.value;
  renderModels();
});
$('#model-reset').addEventListener('click', () => {
  MODEL_FILTER.kind = 'all'; MODEL_FILTER.provider = ''; MODEL_FILTER.group = '';
  MODEL_FILTER.onlyStats = false; MODEL_FILTER.unit = '1m'; MODEL_FILTER.sort = 'name'; MODEL_FILTER.view = 'grid';
  $('#model-search').value = '';
  $('#model-only-stats').checked = false;
  $('#model-sort').value = 'name';
  document.querySelectorAll('#model-kind [data-kind]').forEach(b => b.classList.toggle('active', b.dataset.kind === 'all'));
  document.querySelectorAll('#model-unit [data-unit]').forEach(b => b.classList.toggle('active', b.dataset.unit === '1m'));
  document.querySelectorAll('#model-view [data-view]').forEach(b => b.classList.toggle('active', b.dataset.view === 'grid'));
  renderModels();
});
$('#model-refresh').onclick = () => run(() => loadModels());
$('#api-base-copy').onclick = () => run(async () => { if (!await copyText($('#api-base').value)) throw new Error('复制失败'); toast('API 地址已复制'); });
$('#overview-keys').onclick = () => activateUser('keys');
$('#overview-models').onclick = () => activateUser('models');

function activateUser(name, loaded = false) {
  const pane = document.getElementById('pane-' + name);
  if (!pane || !pane.classList.contains('pane')) name = 'overview';
  document.querySelectorAll('.sidebar nav a[data-p]').forEach(a => {
    const selected = a.dataset.p === name;
    a.classList.toggle('active', selected);
    if (selected) a.setAttribute('aria-current', 'page'); else a.removeAttribute('aria-current');
  });
  document.querySelectorAll('.pane').forEach(p => p.classList.toggle('active', p.id === 'pane-' + name));
  if (location.hash !== '#' + name) history.replaceState(null, '', '#' + name);
  const loaders = {overview: loadMe, keys: loadKeys, devices: loadDevices, logs: loadLogs, models: loadModels};
  if (Object.hasOwn(loaders, name) && !(loaded && name === 'overview')) run(loaders[name]);
}
document.querySelectorAll('.sidebar nav a[data-p]').forEach(a => {
  a.addEventListener('click', e => { e.preventDefault(); activateUser(a.dataset.p); });
});
window.addEventListener('hashchange', () => { if (ME) activateUser(location.hash.slice(1)); });

$('#key-add').onclick = () => run(async () => {
  await api('keys', {method: 'POST', json: {name: $('#key-name').value, kind: $('#key-kind').value}});
  $('#key-name').value = '';
  toast('新 Key 已生成');
  loadKeys();
});

$('#btn-logout').onclick = () => run(async () => { await api('logout', {method: 'POST'}); showLogin(); });


$('#login-go').onclick = async () => {
  const err = $('#login-err');
  err.style.display = 'none';
  try {
    const r = await fetch('api/user/login', {method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({username: $('#login-user').value.trim(), password: $('#login-pass').value})});
    if (!r.ok) {
      const d = await r.json().catch(() => null);
      err.textContent = (d && d.error && d.error.message) || '登录失败';
      err.style.display = 'block';
      return;
    }
    const session = await r.json();
    userCsrfToken = session.csrf || '';
    await loadMe();
    showPanel();
    activateUser(location.hash.slice(1) || 'overview', true);
    await run(() => loadKeys());
  } catch (e) {
    err.textContent = e.message || '登录失败';
    err.style.display = 'block';
  }
};
$('#login-pass').addEventListener('keydown', e => { if (e.key === 'Enter' && !e.isComposing) $('#login-go').click(); });

function bindLoginTabs() {
  $('#go-register').onclick = () => showLoginTab('register');
  $('#go-forgot').onclick = () => showLoginTab('forgot');
  $('#back-login-1').onclick = () => showLoginTab('main');
  $('#back-login-2').onclick = () => showLoginTab('main');
  let codeLeft = 0;
  let codeSending = false;
  const codeBtn = $('#reg-send');
  setInterval(() => {
    if (codeLeft > 0) { codeLeft--; codeBtn.textContent = codeLeft + 's'; codeBtn.disabled = true; }
    else if (!codeSending) { codeBtn.textContent = '发送验证码'; codeBtn.disabled = false; }
  }, 1000);
  codeBtn.onclick = async () => {
    if (codeSending || codeLeft > 0) return;
    const email = $('#reg-email').value.trim();
    if (!email) { lgErr('请先输入邮箱'); return; }
    codeBtn.disabled = true;
    codeSending = true;
    try {
      const res = await fetch('api/user/register/send-code', {method: 'POST',
        headers: {'Content-Type': 'application/json'}, body: JSON.stringify({email: email})});
      const d = await res.json();
      if (!res.ok) { codeBtn.disabled = false; lgErr((d.error && d.error.message) || '发送失败'); return; }
      codeLeft = 60;
      lgErr('');
      toast(d.message || '验证码已发送');
    } catch (e) { lgErr('发送失败，请稍后重试'); }
    finally { codeSending = false; codeBtn.disabled = codeLeft > 0; }
  };
  $('#reg-go').onclick = async () => {
    const email = $('#reg-email').value.trim();
    const pass = $('#reg-pass').value;
    const code = $('#reg-code').value.trim();
    if (!email || pass.length < 6) { lgErr('请输入邮箱和至少 6 位密码'); return; }
    if (!code) { lgErr('请先获取并填写邮箱验证码'); return; }
    const body = {email: email, password: pass, code: code};
    try {
      const res = await fetch('api/user/register', {method: 'POST',
        headers: {'Content-Type': 'application/json'}, body: JSON.stringify(body)});
      const d = await res.json();
      if (!res.ok) { lgErr((d.error && d.error.message) || '注册失败'); return; }
      userCsrfToken = d.csrf || '';
      toast('注册成功');
      await loadMe();
      showPanel();
      activateUser('overview', true);
      await run(() => loadKeys());
    } catch (e) { lgErr('注册失败'); }
  };
  $('#forgot-go').onclick = async () => {
    const email = $('#forgot-email').value.trim();
    if (!email) { lgErr('请输入注册邮箱'); return; }
    try {
      const res = await fetch('api/user/forgot', {method: 'POST',
        headers: {'Content-Type': 'application/json'}, body: JSON.stringify({email: email})});
      const d = await res.json();
      if (!res.ok) { lgErr((d.error && d.error.message) || '发送失败'); return; }
      lgErr('');
      toast(d.message || '重置邮件已发送，请查收邮箱');
    } catch (e) { lgErr('发送失败'); }
  };
  $('#reset-go').onclick = async () => {
    const pass = $('#reset-pass').value;
    if (pass.length < 6) { lgErr('密码至少 6 位'); return; }
    try {
      const res = await fetch('api/user/reset', {method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({token: window.__resetToken || '', password: pass})});
      const d = await res.json();
      if (!res.ok) { lgErr((d.error && d.error.message) || '重置失败'); return; }
      toast('密码已重置，请登录');
      window.__resetToken = '';
      history.replaceState(null, '', location.pathname);
      showLoginTab('main');
    } catch (e) { lgErr('重置失败'); }
  };
}

(async () => {
  bindLoginTabs();
  for (const id of ['login-go', 'reg-go', 'forgot-go', 'reset-go', 'key-add']) {
    const button = document.getElementById(id), action = button.onclick;
    button.onclick = async (...args) => {
      if (button.disabled) return;
      const text = button.textContent; button.disabled = true; button.textContent = '处理中…';
      try { await action(...args); } finally { button.disabled = false; button.textContent = text; }
    };
  }
  const rt = resetTokenFromUrl();
  if (rt) {
    window.__resetToken = rt;
    showLogin();
    showLoginTab('reset');
    return;
  }
  try {
    await loadMe();
    showPanel();
    activateUser(location.hash.slice(1) || 'overview', true);
    await run(() => loadKeys());
  } catch (e) {
    showLogin();
    if (e.status !== 401) lgErr(e.message || '暂时无法连接服务，请稍后重试');
  }
})();

async function loadDevices() {
  const r = await api('keys');
  const box = document.getElementById('device-list');
  if (!box) return;
  const rows = (r.rows || []).filter(t => t.device_client === '01code');
  box.replaceChildren();
  if (!rows.length) {
    const empty = document.createElement('div');
    empty.className = 'empty-state';
    empty.textContent = '暂无已授权设备。在 01Code 中登录后，这里会出现授权记录。';
    box.append(empty);
    return;
  }
  for (const t of rows) {
    const card = document.createElement('div');
    card.className = 'device-card' + (t.enabled ? '' : ' revoked');
    const created = t.created_at ? fmtTime(t.created_at) : '—';
    const last = t.last_used_at ? fmtTime(t.last_used_at) : '—';
    card.innerHTML = '<div class="device-head"><span class="device-icon"><i class="bi bi-laptop"></i></span>' +
      '<div class="device-title"><div class="device-name">' + esc(t.name || '01Code-设备') + '</div>' +
      '<div class="device-sub">01Code 桌面客户端</div></div>' +
      (t.enabled ? '<span class="device-badge ok">已授权</span>' : '<span class="device-badge off">已撤销</span>') + '</div>' +
      '<div class="device-meta"><span>来源 IP</span><b>' + esc(t.device_ip || '—') + '</b></div>' +
      '<div class="device-meta"><span>授权时间</span><b>' + esc(created) + '</b></div>' +
      '<div class="device-meta"><span>最近使用</span><b>' + esc(last) + '</b></div>' +
      '<div class="device-actions"></div>';
    const btn = document.createElement('button');
    btn.className = 'btn btn-sm btn-outline-danger';
    btn.textContent = t.enabled ? '撤销授权' : '已撤销';
    btn.disabled = !t.enabled;
    btn.onclick = () => run(async () => {
      if (!await window.NGW_UI.dialog({title:'撤销设备', body:'撤销后该设备需要重新登录。', danger:true, okText:'撤销'})) return;
      await api('keys/op', {method:'POST', json:{id:t.id, op:'disable'}});
      toast('已撤销设备');
      await loadDevices();
    });
    card.querySelector('.device-actions').append(btn);
    box.append(card);
  }
}
const deviceRefresh = document.getElementById('device-refresh');
if (deviceRefresh) deviceRefresh.onclick = () => run(() => loadDevices());

