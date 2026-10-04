const $ = s => document.querySelector(s);
const B = '';

async function api(path, opts = {}) {
  const init = {method: opts.method || 'GET', headers: {}};
  if (opts.json !== undefined) { init.headers['Content-Type'] = 'application/json'; init.body = JSON.stringify(opts.json); }
  const res = await fetch(B + 'api/user/' + path, init);
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

let ME = null;

async function loadMe() {
  ME = await api('me');
  $('#balance').textContent = (ME.balance ?? 0).toFixed(2);
  $('#chip-free').textContent = '免费模型限速 ' + (ME.free_rpm > 0 ? ME.free_rpm + ' 次/分' : '不限');
  $('#chip-paid').textContent = '付费模型限速 ' + (ME.paid_rpm > 0 ? ME.paid_rpm + ' 次/分' : '不限');
  document.querySelector('.top-sub').textContent = 'USER PORTAL · ' + ME.username;
}

async function loadKeys() {
  const rows = (await api('keys')).rows || [];
  const tb = $('#key-rows');
  tb.innerHTML = '';
  if (!rows.length) { tb.innerHTML = '<tr><td colspan="5" class="hint">还没有 Key，点上方「生成新 Key」创建</td></tr>'; return; }
  for (const k of rows) {
    const tr = document.createElement('tr');
    tr.innerHTML = '<td>' + (k.name || '未命名') + '</td>'
      + '<td class="key-mono">' + k.key.slice(0, 14) + '…' + '</td>'
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
      if (!await uiConfirm('删除该 Key？使用它的调用将立即失效。', {danger: true, okText: '删除'})) return;
      await api('keys/op', {method: 'POST', json: {id: k.id, op: 'delete'}});
      toast('已删除');
      loadKeys();
    });
    op.append(copy, tog, del);
    tr.appendChild(op);
    tb.appendChild(tr);
  }
}

function copyText(text) {
  if (navigator.clipboard && navigator.clipboard.writeText) {
    return navigator.clipboard.writeText(text).then(() => fallbackCopy(text));
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

function uiConfirm(msg, opts = {}) {
  return window.confirm(msg);
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
    col.innerHTML = '<div class="card model-card h-100"><div class="card-body">'
      + '<div class="d-flex justify-content-between align-items-start mb-2">'
      + '<span class="mono fw-semibold" style="font-size:12.5px;word-break:break-all">' + m.model + '</span>' + badge
      + '</div></div></div>';
    box.appendChild(col);
  }
}

document.querySelectorAll('.nav-pills .nav-link').forEach(a => {
  a.addEventListener('click', e => {
    e.preventDefault();
    document.querySelectorAll('.nav-pills .nav-link').forEach(x => x.classList.remove('active'));
    a.classList.add('active');
    document.querySelectorAll('.pane').forEach(p => p.classList.remove('active'));
    $('#pane-' + a.dataset.p).classList.add('active');
    if (a.dataset.p === 'logs') loadLogs();
    if (a.dataset.p === 'models') loadModels();
  });
});

$('#key-add').onclick = () => run(async () => {
  await api('keys', {method: 'POST', json: {name: $('#key-name').value}});
  $('#key-name').value = '';
  toast('新 Key 已生成');
  loadKeys();
});

$('#btn-logout').onclick = () => run(async () => { await api('logout', {method: 'POST'}); location.href = '/user'; });

(async () => {
  try {
    await loadMe();
    await loadKeys();
  } catch (e) { location.href = '/user'; }
})();
