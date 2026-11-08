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
  const tb = $('#key-rows');
  tb.innerHTML = '';
  if (!rows.length) { tb.innerHTML = '<tr><td colspan="6" class="hint">还没有 Key</td></tr>'; return; }
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
async function loadModels() {
  MODEL_ROWS = (await api('models')).rows || [];
  renderModels();
}
function renderModels() {
  const query = ($('#model-search').value || '').trim().toLowerCase();
  const kind = $('#model-kind').value;
  const rows = MODEL_ROWS.filter(m => m.model.toLowerCase().includes(query)
    && (kind === 'all' || (kind === 'free' ? m.free : (m.paid ?? !m.free))));
  const box = $('#model-cards');
  box.innerHTML = '';
  $('#model-count').textContent = rows.length + ' / ' + MODEL_ROWS.length + ' 个模型';
  if (!rows.length) {
    const empty = document.createElement('div'); empty.className = 'col-12';
    const message = document.createElement('div'); message.className = 'empty-state';
    message.textContent = MODEL_ROWS.length ? '没有匹配的模型，请调整搜索条件。' : '暂无可用模型，请联系管理员配置渠道。';
    empty.append(message); box.append(empty); return;
  }
  for (const m of rows) {
    const col = document.createElement('div'); col.className = 'col-md-6 col-xl-4';
    const rules = [...new Set((m.pricing || [{rule:m.price}]).map(p => window.NGW_UI.pricingText(p.rule)))];
    const badge = rules.map(text => '<div class="small mt-1">' + esc(text) + '</div>').join('');
    col.innerHTML = '<div class="card model-card h-100"><div class="card-body">'
      + '<div class="model-name">' + esc(m.model) + '</div><div class="mt-2">' + badge + '</div>'
      + '<div class="small mt-3 text-muted">'
      + (m.ctx ? '上下文 ' + esc(m.ctx) : '上下文未标注')
      + (m.tps ? ' · 约 ' + esc(m.tps) + ' tok/s' : '') + '</div>'
      + '<div class="small mt-1 text-muted">'
      + (m.health == null ? '暂无调用统计' : '近期成功率 ' + esc(m.health) + '%') + '</div>'
      + '<div class="model-actions"></div></div></div>';
    const actions = col.querySelector('.model-actions');
    const test = document.createElement('a'); test.className = 'btn btn-sm btn-primary';
    test.textContent = '打开 Agent'; test.href = '/webclaw?model=' + encodeURIComponent(m.model);
    test.target = '_blank'; test.rel = 'noopener';
    const copy = document.createElement('button'); copy.className = 'btn btn-sm btn-outline-secondary'; copy.textContent = '复制名称';
    copy.onclick = () => run(async () => { if (!await copyText(m.model)) throw new Error('复制失败，请检查浏览器权限'); toast('模型名称已复制'); });
    actions.append(test, copy); box.append(col);
  }
}
$('#model-search').addEventListener('input', renderModels);
$('#model-kind').addEventListener('change', renderModels);
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

async function loadDevices(){const r=await api('keys');const box=document.getElementById('device-list');if(!box)return;box.replaceChildren();for(const t of (r.rows||[]).filter(t=>t.device_client==='01code')){const row=document.createElement('div');row.className='d-flex align-items-center gap-3 mb-3';const text=document.createElement('span');text.textContent=t.name+' · '+(t.device_ip||'')+' · '+fmtTime(t.created_at)+(t.enabled?'':' · 已撤销');const btn=document.createElement('button');btn.className='btn btn-sm btn-outline-danger';btn.textContent='撤销';btn.disabled=!t.enabled;btn.onclick=()=>run(async()=>{await api('keys/op',{method:'POST',json:{id:t.id,op:'disable'}});await loadDevices();});row.append(text,btn);box.append(row);}if(!box.children.length)box.textContent='暂无已授权设备';}

