const $ = s => document.querySelector(s);
const esc = s => String(s ?? '').replace(/[&<>"']/g, c =>
  ({'&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'}[c]));

async function api(path, opts = {}) {
  const init = {method: opts.method || 'GET', headers: {}};
  if (opts.json !== undefined) { init.headers['Content-Type'] = 'application/json'; init.body = JSON.stringify(opts.json); }
  const res = await fetch('api/user/' + path, init);
  let data = null;
  try { data = await res.json(); } catch (e) { }
  if (!res.ok) throw new Error((data && data.error && data.error.message) || ('HTTP ' + res.status));
  return data;
}

const run = async fn => { try { return await fn(); } catch (e) { toast(e.message, 'danger'); } };

function toast(msg, kind) {
  const d = document.createElement('div');
  d.className = 'toast align-items-center border-0' + (kind === 'danger' ? ' bg-danger' : ' bg-dark');
  d.style.cssText = 'position:fixed;top:18px;right:18px;z-index:9999;color:#fff';
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

function uiConfirm(msg) { return window.confirm(msg); }

let ME = null;
bindLogsKind();

function showLogin() {
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
function invFromUrl() {
  return new URLSearchParams(location.search).get('inv') || '';
}
function showPanel() {
  $('#login-view').style.display = 'none';
  $('#panel-view').style.display = 'block';
}

async function loadMe() {
  ME = await api('me');
  $('#ov-balance').textContent = (ME.balance ?? 0).toFixed(2);
  $('#ov-grant').textContent = '¥' + (ME.grant ?? 0).toFixed(4);
  $('#ov-grant-total').textContent = '¥' + (ME.grant_total ?? 0).toFixed(4);
  $('#ov-recharge').textContent = '¥' + (ME.recharge ?? 0).toFixed(4);
  $('#ov-recharge-total').textContent = '¥' + (ME.recharge_total ?? 0).toFixed(4);
  try {
    const s = await api('stats');
    $('#ov-calls').textContent = s.total_calls ?? 0;
    $('#ov-calls-sub').textContent = '免费 ' + (s.free_calls ?? 0) + ' / 付费 ' + (s.paid_calls ?? 0);
    $('#ov-cost').textContent = '¥' + (s.total_cost ?? 0).toFixed(2);
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
        + '<td class="small text-truncate" style="max-width:160px">' + esc(r.model || '-') + '</td>'
        + '<td>' + st + '</td>'
        + '<td class="text-end small fw-semibold">' + (r.cost > 0 ? '¥' + r.cost.toFixed(4) : '免费') + '</td>';
      tb.appendChild(tr);
    }
  } catch (e) { }
  loadSign();
}

async function loadSign() {
  try {
    const d = await api('sign');
    if (!d.enabled) return;
    $('#sign-box').style.removeProperty('display');
    const btn = $('#btn-sign');
    const hint = $('#sign-hint');
    if (d.signed_today) {
      btn.disabled = true;
      btn.textContent = '已签到';
      hint.textContent = '今日已领 ¥' + (d.today_amount ?? 0).toFixed(4);
    } else {
      btn.disabled = false;
      btn.textContent = '签到';
      hint.textContent = '每日签到领赠金';
    }
  } catch (e) { }
}

$('#btn-sign').onclick = () => run(async () => {
  const d = await api('sign', {method: 'POST'});
  toast('签到成功，赠金 +¥' + (d.amount ?? 0).toFixed(4));
  ME = await api('me');
  $('#ov-balance').textContent = (ME.balance ?? 0).toFixed(2);
  $('#ov-grant').textContent = '¥' + (ME.grant ?? 0).toFixed(4);
  $('#ov-grant-total').textContent = '¥' + (ME.grant_total ?? 0).toFixed(4);
  loadSign();
});

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
      + '<td class="small text-muted">' + fmtTime(k.last_used_at) + '</td>'
      + '<td class="text-end"></td>';
    const op = document.createElement('td');
    op.className = 'text-end';
    const copy = document.createElement('button');
    copy.className = 'btn btn-sm btn-outline-secondary me-1';
    copy.textContent = '复制';
    copy.onclick = () => copyText(k.key);
    const tog = document.createElement('button');
    tog.className = 'btn btn-sm btn-outline-secondary me-1';
    tog.textContent = k.enabled ? '停用' : '启用';
    tog.onclick = () => run(async () => { await api('keys/op', {method: 'POST', json: {id: k.id, op: k.enabled ? 'disable' : 'enable'}}); toast('已' + tog.textContent); loadKeys(); });
    const del = document.createElement('button');
    del.className = 'btn btn-sm btn-outline-danger';
    del.textContent = '删除';
    del.onclick = () => run(async () => {
      if (!uiConfirm('删除该 Key？')) return;
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
      + '<td>' + esc(r.model || '-') + '</td>'
      + '<td>' + st + '</td>'
      + '<td class="small">' + r.ms + 'ms</td>'
      + '<td class="small">' + (r.in_tok || 0) + '</td>'
      + '<td class="small">' + (r.out_tok || 0) + '</td>'
      + '<td class="small fw-semibold">' + (r.cost > 0 ? '¥' + r.cost.toFixed(4) : '免费') + '</td>';
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

async function loadModels() {
  const rows = (await api('models')).rows || [];
  const box = $('#model-cards');
  box.innerHTML = '';
  for (const m of rows) {
    const col = document.createElement('div');
    col.className = 'col-md-4 col-lg-3';
    const badge = m.free
      ? '<span class="badge badge-free">免费</span>'
      : '<span class="badge badge-paid">¥' + m.price + ' / 次</span>';
    let health = '';
    if (m.health !== null && m.health !== undefined) {
      const h = m.health;
      const cls = h >= 80 ? 'text-success' : h >= 50 ? 'text-warning' : 'text-danger';
      const icon = h >= 80 ? 'bi-heart-pulse-fill' : h >= 50 ? 'bi-heart-pulse' : 'bi-heartbreak-fill';
      health = '<div class="small mt-1"><i class="bi ' + icon + ' ' + cls + '"></i> 健康度 ' + h + '%</div>';
    }
    col.innerHTML = '<div class="card model-card h-100"><div class="card-body">'
      + '<div class="d-flex justify-content-between align-items-start mb-1">'
      + '<span class="mono fw-semibold" style="font-size:12.5px;word-break:break-all">' + esc(m.model) + '</span>' + badge
      + '</div>' + health + '</div></div>';
    box.appendChild(col);
  }
}

document.querySelectorAll('.sidebar nav a').forEach(a => {
  a.addEventListener('click', e => {
    e.preventDefault();
    document.querySelectorAll('.sidebar nav a').forEach(x => x.classList.remove('active'));
    a.classList.add('active');
    document.querySelectorAll('.pane').forEach(p => p.classList.remove('active'));
    $('#pane-' + a.dataset.p).classList.add('active');
    if (a.dataset.p === 'logs') run(() => loadLogs());
    if (a.dataset.p === 'models') run(() => loadModels());
    if (a.dataset.p === 'test') run(() => loadTestModels());
    if (a.dataset.p === 'promo') run(() => loadPromo());
    if (a.dataset.p === 'wheel') run(() => loadWheels());
    if (a.dataset.p === 'prizes') run(() => loadPrizes());
  });
});

// ---------------- 对话测试 ----------------
let tMsgs = [], tKey = null, tBusy = false;

async function ensureTestKey() {
  if (tKey) return tKey;
  const d = await api('test-key', {method: 'POST'});
  tKey = d.key;
  return tKey;
}

async function loadTestModels() {
  const rows = (await api('models')).rows || [];
  const sel = $('#t-model');
  const cur = sel.value;
  sel.innerHTML = '';
  for (const m of rows) {
    const o = document.createElement('option');
    o.value = m.model;
    o.textContent = m.free ? m.model + ' · 免费' : m.model + ' · ¥' + m.price + ' / 次';
    sel.appendChild(o);
  }
  if (cur && [...sel.options].some(o => o.value === cur)) sel.value = cur;
}

function tRender() {
  const box = $('#t-chat');
  box.innerHTML = '';
  if (!tMsgs.length) { box.innerHTML = '<span class="hint">选择模型后输入消息开始测试。</span>'; return; }
  for (const m of tMsgs) {
    const d = document.createElement('div');
    d.className = 'tmsg ' + (m.role === 'user' ? 'me' : 'ai') + (m.pending ? ' pending' : '');
    if (m.role === 'assistant' && m.reasoning) {
      const r = document.createElement('div');
      r.className = 'reason';
      const hd = document.createElement('div');
      hd.className = 'reason-hd';
      hd.textContent = '思考';
      const body = document.createElement('div');
      body.textContent = m.reasoning;
      r.append(hd, body);
      d.appendChild(r);
    }
    const txt = document.createElement('span');
    txt.textContent = m.content || (m.role === 'assistant' && m.pending && !m.reasoning ? '…' : '');
    d.appendChild(txt);
    box.appendChild(d);
  }
  box.scrollTop = box.scrollHeight;
}

async function tSend() {
  if (tBusy) return;
  const model = $('#t-model').value;
  const input = $('#t-input');
  const text = input.value.trim();
  if (!model || !text) return;
  tBusy = true;
  $('#t-send').disabled = true;
  tMsgs.push({role: 'user', content: text});
  input.value = '';
  const reply = {role: 'assistant', content: '', reasoning: '', pending: true};
  tMsgs.push(reply);
  tRender();

  let key = '';
  try { key = await ensureTestKey(); } catch (e) {
    reply.pending = false; tRender();
    tBusy = false; $('#t-send').disabled = false;
    toast(e.message || '未取得测试 Key', 'danger');
    return;
  }

  const stream = $('#t-stream').checked;
  const t0 = performance.now();
  let ttfb = null, inTok = 0, outTok = 0, err = '', status = 0;
  try {
    const r = await fetch('/v1/chat/completions', {
      method: 'POST',
      headers: {'Content-Type': 'application/json', Authorization: 'Bearer ' + key, 'X-NGW-Skip-Training': '1'},
      body: JSON.stringify({model, messages: tMsgs.filter(m => !m.pending), stream}),
    });
    status = r.status;
    if (!stream || !r.body) {
      const txt = await r.text();
      let j = null;
      try { j = JSON.parse(txt); } catch (e) { }
      ttfb = performance.now() - t0;
      if (j && j.error) err = (j.error.message || JSON.stringify(j.error)).slice(0, 200);
      if (j) {
        reply.content = (((j.choices || [{}])[0]).message || {}).content || '';
        reply.reasoning = (((j.choices || [{}])[0]).message || {}).reasoning_content || '';
        inTok = (j.usage || {}).prompt_tokens || 0;
        outTok = (j.usage || {}).completion_tokens || 0;
      } else { err = err || txt.slice(0, 200); }
    } else {
      const reader = r.body.getReader();
      const dec = new TextDecoder();
      let buf = '';
      for (;;) {
        const {done, value} = await reader.read();
        if (done) break;
        buf += dec.decode(value, {stream: true});
        let i;
        while ((i = buf.indexOf('\n\n')) >= 0) {
          const frame = buf.slice(0, i);
          buf = buf.slice(i + 2);
          for (const line of frame.split('\n')) {
            if (!line.startsWith('data:')) continue;
            const payload = line.slice(5).trim();
            if (payload === '[DONE]') continue;
            if (ttfb === null) ttfb = performance.now() - t0;
            let j = null;
            try { j = JSON.parse(payload); } catch (e) { continue; }
            if (j.error) { err = (j.error.message || JSON.stringify(j.error)).slice(0, 200); continue; }
            const delta = ((j.choices || [{}])[0] || {}).delta || {};
            if (typeof delta.content === 'string') reply.content += delta.content;
            if (typeof delta.reasoning_content === 'string') reply.reasoning += delta.reasoning_content;
            if (j.usage) {
              inTok = j.usage.prompt_tokens || inTok;
              outTok = j.usage.completion_tokens || outTok;
            }
            tRender();
          }
        }
      }
    }
  } catch (e) {
    err = String(e && e.message || e).slice(0, 200);
  }
  reply.pending = false;
  tRender();
  const total = ((performance.now() - t0) / 1000).toFixed(1);
  const bits = ['HTTP ' + status,
    '首字 ' + (ttfb ? (ttfb / 1000).toFixed(1) + 's' : '-'),
    '共 ' + total + 's',
    inTok + ' + ' + outTok + ' tk'];
  if (err) bits.push(err);
  $('#t-diag').textContent = bits.join(' · ');
  if (err) toast(err, 'danger');
  tBusy = false;
  $('#t-send').disabled = false;
  if (status === 200) { loadMe().catch(() => {}); }
}

$('#t-send').onclick = () => run(() => tSend());
$('#t-clear').onclick = () => { tMsgs = []; tRender(); $('#t-diag').textContent = ''; };
$('#t-input').addEventListener('keydown', e => {
  if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); run(() => tSend()); }
});

$('#key-add').onclick = () => run(async () => {
  await api('keys', {method: 'POST', json: {name: $('#key-name').value, kind: $('#key-kind').value}});
  $('#key-name').value = '';
  toast('新 Key 已生成');
  loadKeys();
});

$('#btn-logout').onclick = () => run(async () => { await api('logout', {method: 'POST'}); showLogin(); });

$('#promo-refresh').onclick = () => run(() => loadPromo());

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
    showPanel();
    await loadMe();
    await loadKeys();
  } catch (e) {
    err.textContent = e.message || '登录失败';
    err.style.display = 'block';
  }
};
$('#login-pass').addEventListener('keydown', e => { if (e.key === 'Enter') $('#login-go').click(); });

function bindLoginTabs() {
  $('#go-register').onclick = () => showLoginTab('register');
  $('#go-forgot').onclick = () => showLoginTab('forgot');
  $('#back-login-1').onclick = () => showLoginTab('main');
  $('#back-login-2').onclick = () => showLoginTab('main');
  if (invFromUrl()) showLoginTab('register');
  let codeLeft = 0;
  const codeBtn = $('#reg-send');
  setInterval(() => {
    if (codeLeft > 0) { codeLeft--; codeBtn.textContent = codeLeft + 's'; codeBtn.disabled = true; }
    else { codeBtn.textContent = '发送验证码'; codeBtn.disabled = false; }
  }, 1000);
  codeBtn.onclick = async () => {
    const email = $('#reg-email').value.trim();
    if (!email) { lgErr('请先输入邮箱'); return; }
    codeBtn.disabled = true;
    try {
      const res = await fetch('api/user/register/send-code', {method: 'POST',
        headers: {'Content-Type': 'application/json'}, body: JSON.stringify({email: email})});
      const d = await res.json();
      if (!res.ok) { codeBtn.disabled = false; lgErr((d.error && d.error.message) || '发送失败'); return; }
      codeLeft = 60;
      lgErr('');
      toast(d.message || '验证码已发送');
    } catch (e) { codeBtn.disabled = false; lgErr('发送失败'); }
  };
  $('#reg-go').onclick = async () => {
    const email = $('#reg-email').value.trim();
    const pass = $('#reg-pass').value;
    const code = $('#reg-code').value.trim();
    if (!email || pass.length < 6) { lgErr('请输入邮箱和至少 6 位密码'); return; }
    if (!code) { lgErr('请先获取并填写邮箱验证码'); return; }
    const body = {email: email, password: pass, code: code};
    const inv = invFromUrl();
    if (inv) body.inv = inv;
    try {
      const res = await fetch('api/user/register', {method: 'POST',
        headers: {'Content-Type': 'application/json'}, body: JSON.stringify(body)});
      const d = await res.json();
      if (!res.ok) { lgErr((d.error && d.error.message) || '注册失败'); return; }
      toast('注册成功，已自动登录');
      showPanel();
      await loadMe();
      await loadKeys();
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
      showLoginTab('main');
    } catch (e) { lgErr('重置失败'); }
  };
}

// ---------------- 拉人活动 ----------------
let PROMO = null;

async function loadPromo() {
  const d = await api('promo');
  PROMO = d;
  const box = $('#promo-body');
  const evs = d.events || [];
  box.innerHTML = '';
  if (!evs.length) {
    box.innerHTML = '<div class="promo-none text-center py-5"><div class="promo-none-ic">🧧</div>'
      + '<div class="fw-bold mb-1">暂无进行中的活动</div>'
      + '<div class="small text-muted">有新活动时这里会第一时间出现</div></div>';
    promoStopTicker();
    return;
  }
  for (const ev of evs) {
    box.appendChild(promoCard(ev, d));
  }
  // 领取成功后的「揭晓」动画：新解锁的那一步弹出 + 从卡面撒彩带
  const rev = window.__promoReveal;
  window.__promoReveal = null;
  if (rev && rev.id) {
    const hero = box.querySelector('.promo-hero[data-evh="' + rev.id + '"]');
    if (hero) {
      const el = hero.querySelector('.promo-task') || hero.querySelector('.promo-success');
      if (el) el.classList.add('revealed');
      burstInto(hero, 26);
    }
  }
  promoStartTicker();
}

// 剩余时间文案：永远不出现光秃秃的「0 天」（也是倒计时 span 的首帧兜底）
function promoLeftText(exp) {
  exp = exp || 0;
  if (exp <= 0) return '长期有效';
  const left = exp - Date.now() / 1000;
  if (left <= 0) return '活动已结束';
  if (left < 86400) return '今天结束';
  return '剩余 ' + Math.ceil(left / 86400) + ' 天';
}

// 到秒倒计时单元格：promoStartTicker 每秒刷新 [data-cd]
function promoCdHtml(exp) {
  exp = exp || 0;
  return '<span class="promo-cd" data-cd="' + exp + '">' + promoLeftText(exp) + '</span>';
}
// 无期限的活动不显示「距结束」，只显示「长期有效」
function promoCdPart(exp) {
  exp = exp || 0;
  return exp > 0 ? '距结束 ' + promoCdHtml(exp) : '<span class="promo-cd">长期有效</span>';
}

let PROMO_TIMER = null;
function promoStopTicker() {
  if (PROMO_TIMER) { clearInterval(PROMO_TIMER); PROMO_TIMER = null; }
}
function promoStartTicker() {
  promoStopTicker();
  const pad = n => String(n).padStart(2, '0');
  let reloading = false;
  const tick = () => {
    const els = document.querySelectorAll('[data-cd]');
    if (!els.length) { promoStopTicker(); return; }
    const nowS = Date.now() / 1000;
    let anyOver = false;
    els.forEach(el => {
      const exp = +el.dataset.cd || 0;
      if (exp <= 0) return; // 长期有效：不走秒
      let left = Math.floor(exp - nowS);
      if (left <= 0) { left = 0; anyOver = true; }
      const dd = Math.floor(left / 86400);
      const hms = pad(Math.floor(left % 86400 / 3600)) + ':' + pad(Math.floor(left % 3600 / 60)) + ':' + pad(left % 60);
      el.textContent = (dd > 0 ? dd + ' 天 ' : '') + hms;
      el.classList.toggle('urgent', left > 0 && left < 86400);
      el.classList.toggle('over', left <= 0);
    });
    // 活动到点：稍候刷一次，让过期活动从列表里消失
    if (anyOver && !reloading) {
      reloading = true;
      setTimeout(() => { reloading = false; if (document.querySelector('[data-cd]')) loadPromo(); }, 1500);
    }
  };
  tick();
  PROMO_TIMER = setInterval(tick, 1000);
}

// 通用彩带爆开：复用 .promo-confetti 的 pconf 位移动画
function burstInto(el, n) {
  const colors = ['#f59e0b', '#3b82f6', '#10b981', '#f97316', '#a78bfa', '#ef4444', '#fbbf24', '#f472b6'];
  for (let i = 0; i < n; i++) {
    const c = document.createElement('span');
    c.className = 'promo-confetti';
    c.style.background = colors[i % colors.length];
    const ang = (Math.random() * 2 - 1) * Math.PI;
    const dist = 70 + Math.random() * 150;
    c.style.setProperty('--dx', Math.cos(ang) * dist + 'px');
    c.style.setProperty('--dy', (Math.abs(Math.sin(ang)) * -140 - 30) + 'px');
    c.style.left = (20 + Math.random() * 60) + '%';
    c.style.top = (35 + Math.random() * 45) + '%';
    el.appendChild(c);
    setTimeout(() => c.remove(), 2200);
  }
}

// 四个阶段按钮的固定 id（promo_funnel.py 断言依赖这些字面量）
const PROMO_CLAIM_IDS = ['p-claim1', 'p-claim2', 'p-claim3', 'p-claim4'];

// 「带文案复制」用的邀请语（链接拼在其后）
const PROMO_SHARE_TEXT = '我在用「言灵中转」，注册就送 AI 额度～用我的专属链接注册，帮我解锁好友红包：';

// 好友头像底色（按邮箱哈希取色，稳定不闪变）
const PROMO_AV_COLORS = ['#f59e0b', '#3b82f6', '#10b981', '#f97316', '#a78bfa', '#ef4444', '#14b8a6', '#eab308'];
function promoAvColor(s) {
  let h = 0;
  for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) >>> 0;
  return PROMO_AV_COLORS[h % PROMO_AV_COLORS.length];
}

// 金额数字滚动：领取后 collected 变化时从旧值滚到新值
function promoCountUp(el, from, to) {
  if (Math.abs(to - from) < 0.005) { el.textContent = to.toFixed(2); return; }
  const t0 = performance.now(), dur = 750;
  const frame = now => {
    const k = Math.min(1, (now - t0) / dur);
    const e = 1 - Math.pow(1 - k, 3);
    el.textContent = (from + (to - from) * e).toFixed(2);
    if (k < 1) requestAnimationFrame(frame);
  };
  requestAnimationFrame(frame);
}

// 好友加入多久了
function promoAgo(t) {
  const s = Math.max(0, Math.floor(Date.now() / 1000 - (t || 0)));
  if (s < 60) return '刚刚';
  if (s < 3600) return Math.floor(s / 60) + ' 分钟前';
  if (s < 86400) return Math.floor(s / 3600) + ' 小时前';
  return Math.floor(s / 86400) + ' 天前';
}

// 复制成功的按钮反馈：短暂变绿 + 换成「已复制 ✓」
function promoCopied(btn) {
  if (!btn || btn.dataset.busy) return;
  btn.dataset.busy = '1';
  const old = btn.innerHTML;
  btn.innerHTML = '已复制 ✓';
  btn.classList.add('promo-copy-ok');
  setTimeout(() => {
    btn.innerHTML = old;
    delete btn.dataset.busy;
    btn.classList.remove('promo-copy-ok');
  }, 1600);
}

function promoCard(ev, d) {
  const card = document.createElement('div');
  card.className = 'promo-hero mb-4';
  card.setAttribute('data-evh', ev.id || '');
  const st = ev.joined ? ev : null;
  const exp = (st ? st.expires_at : ev.expires_at) || 0;
  if (!st || !st.joined) {
    card.innerHTML = '<div class="d-flex justify-content-between align-items-center mb-2">'
      + '<div class="fw-bold fs-5">' + esc(ev.name || '拉人活动') + '</div>'
      + (ev.trial ? '<span class="badge bg-warning text-dark">试玩模式</span>' : '')
      + '</div>'
      + '<div class="mb-2">邀请好友注册，赢 <b>¥' + ev.amount + '</b> 余额！</div>'
      + '<div class="small mb-3" style="opacity:.9">活动目标：邀请 ??? 位好友 · ' + promoCdPart(exp) + '</div>'
      + '<button class="pbtn primary" id="promo-join">立即参与</button>';
    const jb = card.querySelector('#promo-join');
    jb.onclick = () => {
      if (jb.dataset.busy) return;
      jb.dataset.busy = '1';
      const old = jb.textContent;
      jb.textContent = '加入中…';
      run(async () => {
        try {
          await api('promo/join', {method: 'POST', json: {id: ev.id}});
          toast('已加入活动');
        } catch (err) {
          delete jb.dataset.busy;
          jb.textContent = old;
          throw err;
        }
        loadPromo();
      });
    };
    return card;
  }

  const pct = ev.amount > 0 ? Math.min(100, Math.round(st.collected / ev.amount * 100)) : 0;
  const paid = !!st.paid;
  const eff = st.eff_invited || 0;
  const money = ev.amount || 0;
  const t1 = Math.max(1, st.t1 || 1);
  const total = Math.max(1, st.total || ev.target || 1);
  const step2 = Math.max(t1, Math.min(st.step2_need || Math.min(3, total), total));
  const step3 = Math.max(step2, Math.min(st.step3_need || Math.min(7, total), total));
  // 波次叠加曲线的累计门槛（1、3、7、T）：第 4 步提现也要拉满最后一波
  const needs = [t1, step2, step3, total];
  const flags = [!!st.p1, !!st.p2, !!st.p3, paid];
  const can = [
    eff >= needs[0],
    flags[0] && eff >= needs[1],
    flags[0] && flags[1] && eff >= needs[2],
    flags[0] && flags[1] && flags[2],
  ];
  // 拼多多式分阶段解锁：只展示「当前这一步」，后面的一律打码
  const cur = flags.indexOf(false);
  const at = i => (money * (i + 1) / 4).toFixed(2);
  // target=1 时 t1=step2_need=total=1，四阶段人数门槛全相等，
  // 直接算差值会得到「再邀 0 位好友」——门槛没涨就改用累计说法
  const d12 = needs[1] - needs[0];
  const TASKS = [
    {badge: '第 1 步', name: '初见礼', title: '邀请 ' + needs[0] + ' 位好友',
     sub: '好友注册即计入 · 完成后进度到 ¥' + at(0)},
    {badge: '第 2 步', name: '进阶礼',
     title: d12 > 0 ? '再邀 ' + d12 + ' 位好友' : '累计邀满 ' + needs[1] + ' 位好友',
     sub: '好友注册即计入 · 完成后进度到 ¥' + at(1)},
    {badge: '第 3 步', name: '冲刺礼', title: '累计邀满 ??? 位好友',
     sub: '总人数保密 · 完成最后冲刺解锁提现资格'},
    {badge: '最后一步', name: '提现', title: '提现 ¥' + money.toFixed(2),
     sub: st.trial ? '试玩模式 · 余额立刻可用' : '最后一波 · 达成即刻提现到账'},
  ];
  const stepProg = i => {
    const prev = i === 0 ? 0 : needs[i - 1];
    const span = needs[i] - prev;
    if (span <= 0) return eff >= needs[i] ? 1 : 0;
    return Math.max(0, Math.min(1, (eff - prev) / span));
  };

  // 邀请链接以「用户当前访问的地址」生成为准：走 127.0.0.1 就是 127.0.0.1、
  // 走域名就是域名、协议跟浏览器一致；服务器拼的 link_full 在反代转发头
  // 缺失时会失真（http/内网地址），因此只作兜底
  const inviteLink = st.link
    ? (location.origin + '/user' + st.link)
    : (st.link_full || '');

  let html = '';
  const doneN = paid ? 4 : cur;
  for (let i = 0; i < doneN; i++) {
    // 已完成的冲刺步直接亮出真实总人数（这时候公布反而有成就感）
    const doneTitle = i === 2 ? '累计邀满 ' + needs[2] + ' 位好友' : TASKS[i].title;
    html += '<div class="promo-stage done"><div class="ic">✓</div>'
      + '<div class="tx">' + TASKS[i].badge + ' · ' + doneTitle + '</div>'
      + '<span class="promo-tag">已完成</span></div>';
  }
  if (paid) {
    html += '<div class="promo-success" data-ev="' + esc(ev.id) + '">'
      + '<div class="promo-success-ic">🎉</div>'
      + '<div><b>¥' + money.toFixed(2) + ' 已到账！</b>'
      + '<div>充值余额已入账，可在「概览 · 我的钱包」查看</div></div></div>';
  } else if (cur >= 0) {
    const p = Math.round(stepProg(cur) * 100);
    // 冲刺/提现阶段的差值和门槛不外露：eff + 差值 = 总人数，会把保密的 ??? 算出来
    const waitTxt = cur < 2
      ? '还差 ' + Math.max(0, needs[cur] - eff) + ' 位好友'
      : '继续邀请好友，解锁提现资格';
    const foot = cur < 2
      ? '<span>已邀请 <b>' + eff + '</b> / ' + needs[cur] + ' 人</span>'
      : '<span>已邀请 <b>' + eff + '</b> 人</span>';
    html += '<div class="promo-task promo-in" data-ev="' + esc(ev.id) + '">'
      + '<div class="promo-task-top"><span class="promo-badge">' + TASKS[cur].badge + '</span>'
      + '<span class="promo-task-name">' + TASKS[cur].name + '</span></div>'
      + '<div class="promo-task-title">' + TASKS[cur].title + '</div>'
      + '<div class="promo-task-sub">' + TASKS[cur].sub + '</div>'
      + '<div class="promo-taskbar"><div style="width:' + p + '%"></div></div>'
      + '<div class="promo-task-foot">' + foot + '<b>' + p + '%</b></div>'
      + '<div class="promo-task-btn">'
      + (can[cur]
        ? '<button class="btn btn-warning promo-claim" id="' + PROMO_CLAIM_IDS[cur] + '">🎉 立即领取</button>'
        : '<button class="btn promo-claim is-wait" disabled>' + waitTxt + '</button>')
      + '</div></div>';
  }

  html += '<div class="small mb-1 mt-3" style="opacity:.85">你的专属邀请链接（好友注册即算你拉新）：</div>'
    + '<div class="promo-link"><input readonly id="promo-link-input" value="' + esc(inviteLink) + '">'
    + '<button class="btn btn-light btn-sm fw-bold text-nowrap" id="promo-copy">复制链接</button>'
    + '<button class="btn btn-warning btn-sm fw-bold text-nowrap" id="promo-copy-msg">带文案复制</button></div>'
    + '<div class="promo-how">'
    + '<div class="promo-how-step"><span class="n">1</span><div><b>复制链接</b><span>点上方按钮</span></div></div>'
    + '<div class="promo-how-arrow"><i class="bi bi-chevron-right"></i></div>'
    + '<div class="promo-how-step"><span class="n">2</span><div><b>发给好友</b><span>微信 / QQ 都行</span></div></div>'
    + '<div class="promo-how-arrow"><i class="bi bi-chevron-right"></i></div>'
    + '<div class="promo-how-step"><span class="n">3</span><div><b>好友注册</b><span>进度秒到账</span></div></div>'
    + '</div>';

  // 引导条与神秘奖励之间加一条带标签的分割线
  if (cur >= 0) {
    html += '<div class="promo-sep"><span><i class="bi bi-gift"></i> 神秘奖励 · 一波比一波大</span></div>';
    for (let i = cur + 1; i < 4; i++) {
      html += '<div class="promo-lock promo-in" style="animation-delay:' + (0.08 * (i - cur)) + 's">'
        + '<div class="promo-lock-ic"><i class="bi bi-lock-fill"></i></div>'
        + '<div class="promo-lock-tx"><b>神秘奖励</b><span>完成当前任务后自动解锁</span></div>'
        + '<div class="promo-lock-q">???</div></div>';
    }
  }

  const chips = [];
  if (st.draw_credits > 0) chips.push('<span class="promo-chip"><i class="bi bi-ticket-perforated"></i>抽奖次数 × ' + st.draw_credits + '</span>');
  if (st.doubler > 0) chips.push('<span class="promo-chip"><i class="bi bi-stack"></i>翻倍卡 × ' + st.doubler + '</span>');
  if (st.diamonds > 0) chips.push('<span class="promo-chip"><i class="bi bi-gem"></i>钻石 ' + st.diamonds + '/' + (st.gem_need || 20) + '</span>');
  if (st.golds > 0) chips.push('<span class="promo-chip"><i class="bi bi-coin"></i>金币 ' + st.golds + '/' + (st.gold_need || 20) + '</span>');
  if (chips.length) html += '<div class="d-flex gap-2 flex-wrap my-3">' + chips.join('') + '</div>';

  const friends = st.friends || [];
  if (friends.length) {
    html += '<div class="mt-3 mb-1 small" style="opacity:.85">已有 <b>' + friends.length + '</b> 位好友通过你的链接加入</div>'
      + friends.slice(0, 8).map(f => {
        const who = f.invitee || '?';
        return '<div class="promo-friend"><span class="av" style="background:' + promoAvColor(who) + '">'
          + esc(who.charAt(0).toUpperCase()) + '</span><span class="nm">' + esc(who)
          + '</span><span class="tm">' + promoAgo(f.t) + '</span></div>';
      }).join('');
  } else if (!paid) {
    html += '<div class="promo-friend promo-friend-empty mt-3">还没有好友加入——把上面的链接发给朋友，注册成功就计入进度～</div>';
  }

  html += '<div class="mt-2 mb-1"><a href="javascript:void(0)" class="small" style="color:#ffe9c9" id="promo-goto-wheel">去大转盘用次数抽奖 →</a></div>'
    + '<details class="promo-rules"><summary><i class="bi bi-info-circle"></i> 活动规则</summary><ul>'
    + '<li>活动自创建起 7 天有效，到期即止、不可重开；多个活动可同时参加。</li>'
    + '<li>好友通过你的链接注册成功即计入进度；每拉 1 人另 +1 次抽奖机会。</li>'
    + '<li>邀请要求逐波翻倍（如 1、2、4、8），越往后越接近提现。</li>'
    + '<li>总共需要邀请的人数保密（???），随阶段推进逐步揭晓。</li>'
    + '<li>四个阶段全部完成后，奖励以「充值余额」一次性到账。</li>'
    + '</ul></details>';

  card.innerHTML = '<div class="d-flex justify-content-between align-items-center mb-1">'
    + '<div class="fw-bold fs-5">' + esc(ev.name || '拉人活动') + '</div>'
    + (ev.trial ? '<span class="badge bg-warning text-dark">试玩模式</span>' : '')
    + '</div>'
    + '<div class="small mb-2" style="opacity:.9">' + promoCdPart(exp)
    + ' · 已邀请 <b>' + st.invited + '</b> 人</div>'
    + '<div class="promo-amount">¥<b class="promo-amt">' + st.collected.toFixed(2) + '</b><small> / ' + money.toFixed(2) + '</small></div>'
    + '<div class="promo-bar my-2"><div style="width:' + pct + '%"></div></div>'
    + '<div class="promo-gap">' + (paid ? '已成功提现，余额已到账！' : '还差 <b>¥' + st.remain.toFixed(2) + '</b> 即可提现') + '</div>'
    + html;

  // 奖金数字滚动：只在上次渲染值与本次不同时播放（首次渲染不吵）
  const amtEl = card.querySelector('.promo-amt');
  if (amtEl) {
    window.__promoAmt = window.__promoAmt || {};
    const prev = Object.prototype.hasOwnProperty.call(window.__promoAmt, ev.id)
      ? window.__promoAmt[ev.id] : st.collected;
    window.__promoAmt[ev.id] = st.collected;
    promoCountUp(amtEl, prev, st.collected);
  }

  const inp = card.querySelector('#promo-link-input');
  const cp = card.querySelector('#promo-copy');
  const cpm = card.querySelector('#promo-copy-msg');
  if (inp) inp.onclick = () => inp.select();
  if (cp) cp.onclick = () => copyText(inviteLink).then(ok => {
    if (ok) { promoCopied(cp); toast('邀请链接已复制，发给好友吧'); }
    else toast('复制失败，请手动选择链接复制', 'danger');
  });
  if (cpm) cpm.onclick = () => copyText(PROMO_SHARE_TEXT + inviteLink).then(ok => {
    if (ok) { promoCopied(cpm); toast('邀请文案已复制'); }
    else toast('复制失败，请手动复制', 'danger');
  });
  const gw = card.querySelector('#promo-goto-wheel');
  if (gw) gw.onclick = () => document.querySelector('a[data-p="wheel"]').click();
  if (cur >= 0) {
    const b = card.querySelector('#' + PROMO_CLAIM_IDS[cur]);
    if (b) b.onclick = () => {
      if (b.dataset.busy) return;
      b.dataset.busy = '1';
      const old = b.innerHTML;
      b.innerHTML = '领取中…';
      run(async () => {
        try {
          await api('promo/claim', {method: 'POST', json: {id: ev.id, step: cur + 1}});
          window.__promoReveal = {step: cur + 1, id: ev.id};
          toast('领取成功！');
        } catch (err) {
          delete b.dataset.busy;
          b.innerHTML = old;
          throw err;
        }
        loadPromo();
      });
    };
  }
  return card;
}


(async () => {
  bindLoginTabs();
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
    await loadKeys();
  } catch (e) {
    showLogin();
  }
})();


// ---------------- 抽奖活动 ----------------

const WHEEL_COLORS = {
  gold: {ring: '#f59e0b', chip: 'bg-warning-subtle text-warning-emphasis'},
  blue: {ring: '#3b82f6', chip: 'bg-primary-subtle text-primary'},
  orange: {ring: '#f97316', chip: 'bg-orange-subtle text-orange'},
  green: {ring: '#10b981', chip: 'bg-success-subtle text-success'},
  purple: {ring: '#8b5cf6', chip: 'bg-purple-subtle text-purple'},
  gray: {ring: '#64748b', chip: 'bg-secondary-subtle text-secondary'},
};
const PRIZE_TYPE_NAMES = {
  recharge: '充值余额', grant: '赠金', model_unlimited: '体验卡',
  model_quota: '专属额度', none: '谢谢参与',
};

let WHEELS = [];

function prizeDesc(p) {
  const amt = (p.amount ?? 0).toFixed(4);
  if (p.type === 'recharge') return '充值余额 ¥' + amt;
  if (p.type === 'grant') return '赠金 ¥' + amt;
  if (p.type === 'model_unlimited') {
    const parts = [esc(p.model || '')];
    parts.push(p.duration_hours > 0 ? p.duration_hours + ' 小时' : '长期有效');
    return parts.join(' · ');
  }
  if (p.type === 'model_quota') {
    const parts = [esc(p.model || ''), (p.quota ?? 0) + ' 次'];
    if (p.duration_hours > 0) parts.push(p.duration_hours + ' 小时内有效');
    else parts.push('长期有效');
    return parts.join(' · ');
  }
  return '未中奖';
}

async function loadWheels() {
  const d = await api('wheels');
  WHEELS = d.rows || [];
  const box = $('#wheel-cards');
  box.innerHTML = '';
  if (!WHEELS.length) {
    box.innerHTML = '<div class="col-12"><div class="hint text-center py-5">暂无可参与的抽奖活动</div></div>';
    return;
  }
  for (const w of WHEELS) {
    const col = document.createElement('div');
    col.className = 'col-md-6 col-lg-4';
    const top = (w.prizes || []).slice(0, 3).map(p => {
      const c = WHEEL_COLORS[p.color] || WHEEL_COLORS.gray;
      return '<span class="type-chip ' + c.chip + ' me-1">' + esc(p.label) + '</span>';
    }).join('');
    col.innerHTML = '<div class="card h-100 wheel-activity" data-w="' + esc(w.id) + '" role="button">'
      + '<div class="card-body">'
      + '<div class="d-flex justify-content-between align-items-center mb-2">'
      + '<span class="fw-semibold fs-6">' + esc(w.name) + '</span>'
      + '<span class="badge bg-dark-subtle text-dark">' + (w.cost > 0 ? '¥' + w.cost + ' / 次' : '免费') + '</span></div>'
      + '<div class="small text-muted mb-2">奖品 ' + (w.prizes?.length ?? 0) + ' 项 · 概率合计 100%</div>'
      + '<div>' + top + ((w.prizes?.length ?? 0) > 3 ? '<span class="small text-muted">等 ' + w.prizes.length + ' 项</span>' : '') + '</div>'
      + '<div class="small text-primary mt-2">进入活动 →</div>'
      + '</div></div>';
    col.querySelector('.wheel-activity').onclick = () => openWheelDetail(w);
    box.appendChild(col);
  }
}

function openWheelDetail(w) {
  $('#wheel-list-view').style.display = 'none';
  $('#wheel-detail-view').style.display = 'block';
  $('#wd-name').textContent = w.name;
  $('#wd-cost').textContent = w.cost > 0 ? '¥' + w.cost + ' / 次' : '免费';
  // 转盘盘面按展示概率分段
  const disc = $('#wd-disc');
  const colors = (w.prizes || []).map(p => WHEEL_COLORS[p.color] || WHEEL_COLORS.gray);
  let acc = 0;
  const segs = (w.prizes || []).map((p, i) => {
    const from = acc, to = acc + (p.percent ?? 0);
    acc = to;
    return `${colors[i].ring} ${from}% ${to}%`;
  });
  disc.style.background = 'conic-gradient(' + segs.join(',') + ')';
  disc.style.transition = 'none';
  disc.style.transform = 'rotate(0deg)';
  $('#wd-center').textContent = 'GO';
  // 盘面分段标签：按每格中心角定位（转盘明确写明奖项）
  disc.querySelectorAll('.wheel-lbl').forEach(n => n.remove());
  let a0 = 0;
  for (const p of (w.prizes || [])) {
    const mid = (a0 + (p.percent ?? 0) / 2) * 3.6;
    a0 += (p.percent ?? 0);
    const lbl = document.createElement('span');
    lbl.className = 'wheel-lbl';
    lbl.textContent = (p.label || '').slice(0, 8);
    // 下半圈的标签旋转 180° 后再正向放置：文字永远保持可读方向
    lbl.style.transform = (mid > 90 && mid < 270)
      ? 'rotate(' + (mid + 180) + 'deg) translateY(99px) translateX(-50%)'
      : 'rotate(' + mid + 'deg) translateY(-99px) translateX(-50%)';
    disc.appendChild(lbl);
  }
  // 奖项表：概率 = 展示权重占比（后端已按占比归一化）
  const tb = $('#wd-prizes');
  tb.innerHTML = '';
  for (const p of (w.prizes || [])) {
    const c = WHEEL_COLORS[p.color] || WHEEL_COLORS.gray;
    const pct = p.percent ?? 0;
    const tr = document.createElement('tr');
    tr.innerHTML = '<td class="text-nowrap"><span class="type-chip ' + c.chip + ' me-2" style="width:12px">&nbsp;</span>' + esc(p.label) + '</td>'
      + '<td class="small text-nowrap">' + esc(PRIZE_TYPE_NAMES[p.type] || p.type) + '</td>'
      + '<td class="small" style="color:var(--mut)">' + prizeDesc(p) + '</td>'
      + '<td class="text-end text-nowrap"><div class="d-flex align-items-center justify-content-end gap-2">'
      + '<div class="prob-bar"><div class="prob-fill" style="width:' + pct + '%"></div></div>'
      + '<span class="fw-semibold" style="min-width:46px;display:inline-block;text-align:right">' + pct + '%</span></div></td>';
    tb.appendChild(tr);
  }
  const btn = $('#wd-draw');
  btn.disabled = false;
  btn.onclick = () => confirmDraw(w, btn);
}

function backToWheelList() {
  $('#wheel-detail-view').style.display = 'none';
  $('#wheel-list-view').style.display = 'block';
}
$('#wheel-back').onclick = backToWheelList;

function wheelModal(title, bodyHtml, okText, onCancel) {
  $('#wheel-modal-title').textContent = title;
  $('#wheel-modal-body').innerHTML = bodyHtml;
  $('#wheel-modal-ok').textContent = okText;
  // 每次打开必须重置 OK 处理器：否则残留上一次的 onclick（如 confirmDraw 的
  // 「关闭并再抽一次」），结果弹窗点「收下」会莫名再抽一次
  $('#wheel-modal-ok').onclick = () => { $('#wheel-modal').style.display = 'none'; };
  const mask = $('#wheel-modal');
  mask.style.display = 'flex';
  const close = () => { mask.style.display = 'none'; };
  $('#wheel-modal-cancel').onclick = () => { close(); if (onCancel) onCancel(); };
  mask.onclick = e => { if (e.target === mask) { close(); if (onCancel) onCancel(); } };
  return {ok: $('#wheel-modal-ok'), close};
}

let drawing = false;

function confirmDraw(w, btn) {
  if (drawing) return;
  const costHtml = w.cost > 0
    ? '本次抽奖将消耗 <b>¥' + w.cost + '</b>。'
    : '本次抽奖免费。';
  const body = '<div>' + costHtml + '</div>'
    + '<div class="mt-2 small" style="color:#dbe5f5">中奖结果以本次抽奖为准，余额奖品即时到账。</div>';
  const m = wheelModal('确认抽奖 · ' + w.name, body, '开始抽奖');
  m.ok.onclick = () => {
    m.close();
    doDraw(w, btn);
  };
}

async function doDraw(w, btn) {
  drawing = true;
  btn.disabled = true;
  const disc = $('#wd-disc');
  const center = $('#wd-center');
  center.textContent = '…';
  // 抽奖前刷新拉人活动状态：邀请次数以服务端为准，避免页面快照过期
  PROMO = await api('promo').catch(() => PROMO);
  const useCredit = !!(PROMO && (PROMO.events || []).some(e => e.joined && e.draw_credits > 0));
  api('wheels/draw', {method: 'POST', json: {id: w.id, use_credit: useCredit}}).then(d => {
    // 落点算法：指针在正上方(0°)，把中奖格中心转到指针下（含格内 ±30% 抖动）
    const prizes = w.prizes || [];
    let idx = prizes.findIndex(p => p.id === (d.prize && d.prize.id));
    if (idx < 0) idx = 0;
    let acc = 0, start = 0, pct = 0;
    for (let i = 0; i < prizes.length; i++) {
      const pp = prizes[i].percent ?? 0;
      if (i === idx) { start = acc; pct = pp; break; }
      acc += pp;
    }
    const jitter = (Math.random() - 0.5) * 0.6 * pct;
    const finalDeg = 5 * 360 + (360 - (start + pct / 2 + jitter) * 3.6);
    disc.style.transition = 'none';
    disc.style.transform = 'rotate(0deg)';
    void disc.offsetWidth;
    disc.style.transition = 'transform 2.8s cubic-bezier(.15,.6,.2,1)';
    disc.style.transform = 'rotate(' + finalDeg + 'deg)';
    setTimeout(() => showDrawResult(d, w, btn), 2850);
  }).catch(e => {
    disc.style.transition = '';
    disc.style.transform = '';
    center.textContent = 'GO';
    drawing = false;
    btn.disabled = false;
    wheelModal('抽奖失败', esc(e.message || '请求失败'), '知道了');
  });
}

function showDrawResult(d, w, btn) {
  const disc = $('#wd-disc');
  disc.style.transition = '';
  disc.style.transform = '';
  $('#wd-center').textContent = 'GO';
  drawing = false;
  btn.disabled = false;
  showPrizeModal(d, w, btn);
  // 刷新钱包 + 奖品列表
  run(async () => {
    ME = await api('me');
    $('#ov-balance').textContent = (ME.balance ?? 0).toFixed(2);
    $('#ov-grant').textContent = '¥' + (ME.grant ?? 0).toFixed(4);
    $('#ov-grant-total').textContent = '¥' + (ME.grant_total ?? 0).toFixed(4);
    $('#ov-recharge').textContent = '¥' + (ME.recharge ?? 0).toFixed(4);
    $('#ov-recharge-total').textContent = '¥' + (ME.recharge_total ?? 0).toFixed(4);
  });
  loadPrizes();
}

// 星空票据卡结果弹窗（仿 ZCode Trust Build 风）：星空底 + 飞光 + 白色票卡弹入
let prizeStarsSeeded = false;

function seedPrizeStars() {
  if (prizeStarsSeeded) return;
  prizeStarsSeeded = true;
  const box = $('#prize-stars');
  for (let i = 0; i < 70; i++) {
    const s = document.createElement('i');
    s.style.left = Math.random() * 100 + '%';
    s.style.top = Math.random() * 100 + '%';
    const sz = 1.5 + Math.random() * 2.5;
    s.style.width = s.style.height = sz + 'px';
    s.style.animationDelay = (Math.random() * 3).toFixed(2) + 's';
    s.style.animationDuration = (2.2 + Math.random() * 2.4).toFixed(2) + 's';
    box.appendChild(s);
  }
  for (let i = 0; i < 6; i++) {
    const st = document.createElement('div');
    st.className = 'prize-streak';
    st.style.top = 6 + Math.random() * 55 + '%';
    st.style.left = '-10%';
    st.style.animationDelay = (Math.random() * 3).toFixed(2) + 's';
    st.style.animationDuration = (2.6 + Math.random() * 2.2).toFixed(2) + 's';
    box.appendChild(st);
  }
}

function burstPrizeConfetti() {
  const colors = ['#f59e0b', '#3b82f6', '#10b981', '#f97316', '#a78bfa', '#ef4444'];
  const wrap = $('#prize-card');
  for (let i = 0; i < 16; i++) {
    const c = document.createElement('span');
    c.className = 'prize-confetti';
    c.style.background = colors[i % colors.length];
    const ang = (Math.random() * 2 - 1) * Math.PI; // 左右半圆向上喷
    const dist = 110 + Math.random() * 150;
    c.style.setProperty('--dx', Math.cos(ang) * dist + 'px');
    c.style.setProperty('--dy', (Math.abs(Math.sin(ang)) * -120 - 30) + 'px');
    c.style.left = 50 + '%';
    wrap.appendChild(c);
    setTimeout(() => c.remove(), 1800);
  }
}

function showPrizeModal(d, w, btn) {
  const p = d.prize || {};
  const cost = d.cost || {};
  const won = !!(p.type && p.type !== 'none');
  seedPrizeStars();
  const ov = $('#prize-overlay');
  const card = $('#prize-card');
  card.classList.toggle('lost', !won);
  // 重置入场动画
  card.style.animation = 'none';
  void card.offsetWidth;
  card.style.animation = '';

  const amt = $('#prize-amount');
  const line = $('#prize-line');
  const valid = $('#prize-valid');
  amt.style.fontSize = '';
  if (p.type === 'recharge' || p.type === 'grant') {
    const n = (p.amount ?? 0).toFixed(4).replace(/0+$/, '').replace(/\.$/, '');
    amt.innerHTML = '¥' + n;
    line.innerHTML = '<i class="bi bi-wallet2"></i>已存入' + (p.type === 'recharge' ? '充值' : '赠金') + '账本';
    valid.textContent = '资金即时到账 · 可在「概览」查看明细';
  } else if (p.type === 'model_unlimited') {
    amt.innerHTML = esc(p.model || '-');
    line.innerHTML = '<i class="bi bi-crosshair"></i>限定模型：' + esc(p.model || '-');
    valid.textContent = p.duration_hours > 0
      ? '体验卡 · 不限次数，限时 ' + p.duration_hours + ' 小时'
      : '体验卡 · 不限次数，长期有效';
  } else if (p.type === 'model_quota') {
    amt.innerHTML = (p.quota ?? 0) + ' <small>次</small>';
    line.innerHTML = '<i class="bi bi-crosshair"></i>限定模型：' + esc(p.model || '-');
    valid.textContent = p.duration_hours > 0
      ? '共计 ' + (p.quota ?? 0) + ' 次体验机会，限时 ' + p.duration_hours + ' 小时'
      : '共计 ' + (p.quota ?? 0) + ' 次体验机会，长期有效';
  } else {
    amt.innerHTML = '谢谢参与';
    amt.style.fontSize = '30px';
    line.innerHTML = '<i class="bi bi-emoji-smile"></i>好运在下一抽';
    valid.textContent = '感谢参与，再接再厉！';
  }
  $('#prize-title').textContent = won ? '「' + (p.label || '奖品') + '」领取成功' : '谢谢参与';
  const subTail = (p.type === 'model_unlimited' || p.type === 'model_quota')
    ? 'Key 已发放至「奖品中心」。'
    : (won ? '已可使用。' : '');
  $('#prize-sub').innerHTML = (won ? '<b>' + esc(p.label || '') + '</b> ' + subTail : '');
  // 中奖撒花
  card.querySelectorAll('.prize-confetti').forEach(x => x.remove());
  if (won) burstPrizeConfetti();
  // 按钮：收下（关闭）；再来一次（关闭并立即再抽）
  $('#prize-ok').onclick = () => { ov.style.display = 'none'; };
  $('#prize-again').onclick = () => {
    ov.style.display = 'none';
    if (w && btn) doDraw(w, btn);
  };
  ov.style.display = 'flex';
}

async function loadPrizes() {
  const [kd, ld] = await Promise.all([api('prize-keys'), api('draw-logs')]);
  const tb = $('#prize-rows');
  tb.innerHTML = '';
  const rows = kd.rows || [];
  if (!rows.length) {
    tb.innerHTML = '<tr><td colspan="7" class="hint text-center py-4">还没有奖品 Key，去抽奖活动试试手气</td></tr>';
  }
  const now = Math.floor(Date.now() / 1000);
  for (const k of rows) {
    const tr = document.createElement('tr');
    const expired = k.expires_at > 0 && now >= k.expires_at;
    const exhausted = k.quota > 0 && (k.used ?? 0) >= k.quota;
    const st = !k.enabled ? '<span class="badge bg-secondary">停用</span>'
      : expired ? '<span class="badge bg-danger">已过期</span>'
      : exhausted ? '<span class="badge bg-danger">已用尽</span>'
      : '<span class="badge bg-success">可用</span>';
    tr.innerHTML = '<td class="key-mono small">' + esc(k.key.slice(0, 14)) + '…</td>'
      + '<td class="small">' + (k.type === 'model_unlimited' ? '体验卡' : '专属额度') + '</td>'
      + '<td class="small mono">' + esc(k.model || '-') + '</td>'
      + '<td class="small">' + (k.quota > 0 ? (k.used ?? 0) + ' / ' + k.quota : '不限') + '</td>'
      + '<td class="small text-muted">' + (k.expires_at > 0 ? new Date(k.expires_at * 1000).toLocaleString() : '不限') + '</td>'
      + '<td>' + st + '</td>'
      + '<td class="text-end"></td>';
    const td = tr.lastElementChild;
    const copy = document.createElement('button');
    copy.className = 'btn btn-sm btn-outline-secondary';
    copy.textContent = '复制 Key';
    copy.onclick = () => copyText(k.key);
    td.appendChild(copy);
    tb.appendChild(tr);
  }
  const tb2 = $('#drawlog-rows');
  tb2.innerHTML = '';
  const logs = ld.rows || [];
  if (!logs.length) {
    tb2.innerHTML = '<tr><td colspan="4" class="hint text-center py-3">暂无抽奖记录</td></tr>';
  }
  for (const r of logs) {
    const tr = document.createElement('tr');
    tr.innerHTML = '<td class="small text-muted">' + fmtTime(r.t) + '</td>'
      + '<td class="small">' + esc(r.wheel_name || '-') + '</td>'
      + '<td class="small">' + esc(r.label || '-') + '</td>'
      + '<td class="text-end small">' + (r.cost > 0 ? '¥' + r.cost.toFixed(4) : '免费') + '</td>';
    tb2.appendChild(tr);
  }
}
