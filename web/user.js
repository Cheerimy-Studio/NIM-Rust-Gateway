const $ = s => document.querySelector(s);

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
  d.innerHTML = '<div class="d-flex"><div class="toast-body">' + msg + '</div></div>';
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

function showLogin() {
  $('#login-view').style.display = 'flex';
  $('#panel-view').style.display = 'none';
}
function showPanel() {
  $('#login-view').style.display = 'none';
  $('#panel-view').style.display = 'block';
}

async function loadMe() {
  ME = await api('me');
  $('#ov-balance').textContent = (ME.balance ?? 0).toFixed(2);
  $('#ov-frpm').textContent = ME.free_rpm > 0 ? ME.free_rpm + ' 次 / 分' : '不限';
  $('#ov-prpm').textContent = ME.paid_rpm > 0 ? ME.paid_rpm + ' 次 / 分' : '关闭';
  try {
    const s = await api('stats');
    $('#ov-calls').textContent = s.total_calls ?? 0;
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
        + '<td class="small text-truncate" style="max-width:160px">' + (r.model || '-') + '</td>'
        + '<td>' + st + '</td>'
        + '<td class="text-end small fw-semibold">' + (r.cost > 0 ? '¥' + r.cost.toFixed(4) : '免费') + '</td>';
      tb.appendChild(tr);
    }
  } catch (e) { }
}

async function loadKeys() {
  const rows = (await api('keys')).rows || [];
  const tb = $('#key-rows');
  tb.innerHTML = '';
  if (!rows.length) { tb.innerHTML = '<tr><td colspan="5" class="hint">还没有 Key</td></tr>'; return; }
  for (const k of rows) {
    const tr = document.createElement('tr');
    tr.innerHTML = '<td>' + (k.name || '未命名') + '</td>'
      + '<td class="key-mono">' + k.key.slice(0, 14) + '…</td>'
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

async function loadLogs() {
  const rows = (await api('logs?n=200')).rows || [];
  const tb = $('#log-rows');
  tb.innerHTML = '';
  if (!rows.length) { tb.innerHTML = '<tr><td colspan="7" class="hint">暂无调用记录</td></tr>'; return; }
  for (const r of rows) {
    const tr = document.createElement('tr');
    const st = r.st >= 200 && r.st < 400
      ? '<span class="badge bg-success">' + r.st + '</span>'
      : '<span class="badge bg-danger">' + r.st + '</span>';
    tr.innerHTML = '<td class="small text-muted">' + fmtTime(r.t) + '</td>'
      + '<td>' + (r.model || '-') + '</td>'
      + '<td>' + st + '</td>'
      + '<td class="small">' + r.ms + 'ms</td>'
      + '<td class="small">' + (r.in_tok || 0) + '</td>'
      + '<td class="small">' + (r.out_tok || 0) + '</td>'
      + '<td class="small fw-semibold">' + (r.cost > 0 ? '¥' + r.cost.toFixed(4) : '免费') + '</td>';
    tb.appendChild(tr);
  }
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
      + '<span class="mono fw-semibold" style="font-size:12.5px;word-break:break-all">' + m.model + '</span>' + badge
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
    o.textContent = m.free ? m.model + '（免费）' : m.model + '（¥' + m.price + ' / 次）';
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
  await api('keys', {method: 'POST', json: {name: $('#key-name').value}});
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
    showPanel();
    await loadMe();
    await loadKeys();
  } catch (e) {
    err.textContent = e.message || '登录失败';
    err.style.display = 'block';
  }
};
$('#login-pass').addEventListener('keydown', e => { if (e.key === 'Enter') $('#login-go').click(); });

(async () => {
  try {
    await loadMe();
    showPanel();
    await loadKeys();
  } catch (e) {
    showLogin();
  }
})();
