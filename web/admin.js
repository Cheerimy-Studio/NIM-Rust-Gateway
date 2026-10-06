
(() => {
'use strict';

const B = window.NGW.base, CSRF = window.NGW.csrf;

function copyText(text) {
  if (navigator.clipboard && navigator.clipboard.writeText) {
    return navigator.clipboard.writeText(text).catch(() => fallbackCopy(text));
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
const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => Array.from(r.querySelectorAll(s));



const esc = (s) => String(s).replace(/[&<>"']/g, (c) =>
  ({'&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'}[c]));

const el = (tag, cls, text) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
};
const fmtTime = ts => !ts ? '-' : new Date(ts * 1000).toLocaleString('zh-CN', {hour12: false});
const fmtAgo = ts => {
  if (!ts) return '从未';
  const s = Math.max(0, Math.floor(Date.now() / 1000 - ts));
  if (s < 60) return s + 's';
  if (s < 3600) return Math.floor(s / 60) + 'm';
  if (s < 86400) return Math.floor(s / 3600) + 'h';
  return Math.floor(s / 86400) + 'd';
};
const statusText = st => st === 0 ? '网络' : st === 499 ? '断开' : String(st);
const errCls = st => st === 499 ? 'text-muted' : 'text-danger';

const BAN_NAMES = {
  fail_ladder: '失败封禁', auth_fail: '鉴权失败', invalid_key: '密钥失效',
  daily_cap: '当日额度',
};
const keyState = k => {
  const now = Date.now() / 1000;
  if ((k.banned_until || 0) > now) {
    const left = Math.ceil(k.banned_until - now);
    const name = BAN_NAMES[k.ban_reason] || '封禁';
    const leftTxt = left >= 3600 ? Math.ceil(left / 3600) + 'h' : left >= 60 ? Math.ceil(left / 60) + 'm' : left + 's';
    return {t: name + ' ' + leftTxt, c: k.ban_reason === 'invalid_key' || k.ban_reason === 'daily_cap' ? 'danger' : 'warning', banned: true};
  }
  if (!k.enabled) return {t: '已停用', c: 'secondary'};
  if (k.status === 'invalid') return {t: '密钥失效', c: 'danger'};
  return {t: '可用', c: 'success'};
};

let toastBox = null;
const toast = (msg, type = 'success') => {
  if (!toastBox) { toastBox = el('div', 'toast-container position-fixed bottom-0 end-0 p-3'); document.body.appendChild(toastBox); }
  const t = el('div', 'toast align-items-center text-bg-' + type + ' border-0');
  const wrap = el('div', 'd-flex');
  const body = el('div', 'toast-body'); body.textContent = msg;
  const btn = el('button', 'btn-close btn-close-white me-2 m-auto');
  btn.onclick = () => t.remove();
  wrap.append(body, btn); t.appendChild(wrap);
  toastBox.appendChild(t);
  new bootstrap.Toast(t, {delay: 2200}).show();
  t.addEventListener('hidden.bs.toast', () => t.remove());
};

async function api(path, opts = {}) {
  const init = {method: opts.method || 'GET', headers: {'X-CSRF': CSRF}};
  if (opts.json !== undefined) { init.headers['Content-Type'] = 'application/json'; init.body = JSON.stringify(opts.json); }
  if (opts.form) init.body = opts.form;
  const res = await fetch(B + 'api/' + path, init);
  let data = null;
  try { data = await res.json(); } catch (e) {  }
  if (!res.ok) throw new Error((data && data.error && data.error.message) || ('HTTP ' + res.status));
  return data;
}


const run = async fn => { try { return await fn(); } catch (e) { toast(e.message, 'danger'); } };

const guard = fn => async (...args) => { try { return await fn(...args); } catch (e) { toast(e.message, 'danger'); } };


function uiDialog({ title = '确认', body = '', input = null, danger = false, okText = '确定' }) {
  return new Promise(resolve => {
    const mask = el('div', 'ui-mask');
    const box = el('div', 'ui-dialog');
    box.append(el('div', 'ui-title', title));
    const bodyEl = el('div', 'ui-body');
    if (body) bodyEl.appendChild(el('div', 'ui-msg', body));
    let inputEl = null;
    if (input !== null) {
      inputEl = el('input', 'form-control form-control-sm mt-2');
      inputEl.value = input.value || '';
      inputEl.placeholder = input.placeholder || '';
      bodyEl.appendChild(inputEl);
    }
    box.appendChild(bodyEl);
    const foot = el('div', 'ui-foot');
    const cancel = el('button', 'btn btn-sm btn-outline-secondary', '取消');
    const ok = el('button', 'btn btn-sm ' + (danger ? 'btn-danger' : 'btn-primary'), okText);
    foot.append(cancel, ok);
    box.appendChild(foot);
    mask.appendChild(box);
    document.body.appendChild(mask);
    const close = val => { mask.remove(); document.removeEventListener('keydown', onKey); resolve(val); };
    const okFn = () => close(inputEl ? inputEl.value.trim() : true);
    cancel.onclick = () => close(input !== null ? null : false);
    ok.onclick = okFn;
    mask.onclick = e => { if (e.target === mask) close(input !== null ? null : false); };
    const onKey = e => {
      if (e.key === 'Escape') close(input !== null ? null : false);
      if (e.key === 'Enter') okFn();
    };
    document.addEventListener('keydown', onKey);
    if (inputEl) { inputEl.focus(); inputEl.select(); } else { ok.focus(); }
  });
}
const uiConfirm = (msg, opts = {}) => uiDialog({ title: opts.title || '确认操作', body: msg, danger: !!opts.danger, okText: opts.okText || '确定' });
const uiPrompt = (msg, val = '', opts = {}) => uiDialog({ title: opts.title || '输入', body: msg, input: { value: val, placeholder: opts.placeholder || '' }, okText: opts.okText || '确定' });


const LOADERS = {
  dash: () => loadOverview(),
  keys: () => loadKeys(),
  upstreams: () => loadUpstreams(),
  tokens: () => loadTokens(),
  intercept: () => loadIntercept(),
  logs: () => loadLogs(),
  queue: () => loadQueue(),
  test: () => loadTestModels(),
  training: () => loadTraining(),
  sessions: () => loadSessions(),
  settings: () => loadSettings(),
  docs: () => fillDocs(),
  users: loadUsers,
  promo: loadPromoAdmin,
  prices: loadPrices,
  wheels: loadWheels
};
async function loadPromoAdmin() {
  const d = await run(() => api('promo'));
  const tb = $('#promo-rows');
  if (!tb) return;
  tb.innerHTML = '';
  const rows = d.rows || [];
  if (!rows.length) {
    tb.innerHTML = '<tr><td colspan="8" class="text-muted small text-center py-4">还没有活动，先创建一个</td></tr>';
    return;
  }
  for (const e of rows) {
    const tr = document.createElement('tr');
    if (!e.enabled) tr.classList.add('table-light');
    const mode = e.trial ? '<span class="badge bg-warning text-dark">试玩</span>' : '<span class="badge bg-primary-subtle text-primary">正式</span>';
    const st = e.expired ? '<span class="badge bg-secondary">已过期</span>'
      : e.enabled ? '<span class="badge bg-success">进行中</span>' : '<span class="badge bg-secondary">已停用</span>';
    tr.innerHTML = '<td class="fw-semibold">' + esc(e.name || e.id) + '</td>'
      + '<td class="fw-semibold">¥' + (e.amount ?? 0) + '</td>'
      + '<td>' + (e.target ?? 0) + ' 人</td>'
      + '<td class="small">' + (e.members ?? 0) + ' / ' + (e.invites ?? 0) + '</td>'
      + '<td>' + mode + '</td>'
      + '<td class="small">' + (e.expired ? '—' : e.left_days + ' 天') + '</td>'
      + '<td>' + st + '</td>'
      + '<td class="text-end text-nowrap"></td>';
    const td = tr.lastElementChild;
    const mk = (label, op, cls) => {
      const b = document.createElement('button');
      b.className = 'btn btn-sm ' + cls + ' me-1';
      b.textContent = label;
      b.onclick = () => run(async () => {
        await api('promo/op', {method: 'POST', json: {id: e.id, op: op}});
        toast('已' + label);
        loadPromoAdmin();
      });
      return b;
    };
    if (!e.expired) td.appendChild(mk(e.enabled ? '停用' : '启用', e.enabled ? 'disable' : 'enable',
      e.enabled ? 'btn-outline-secondary' : 'btn-outline-success'));
    const del = document.createElement('button');
    del.className = 'btn btn-sm btn-outline-danger';
    del.textContent = '删';
    del.onclick = () => uiConfirm('删除活动「' + esc(e.name || e.id) + '」？参与进度将一并删除。', {danger: true, okText: '删除'})
      .then(ok => { if (ok) run(async () => { await api('promo/op', {method: 'POST', json: {id: e.id, op: 'delete'}}); toast('已删除'); loadPromoAdmin(); }); });
    td.appendChild(del);
    tb.appendChild(tr);
  }
}

$('#promo-add').onclick = guard(async () => {
  const name = $('#promo-name').value.trim();
  if (!name) { toast('请填写活动名称', 'danger'); return; }
  await api('promo/save', {method: 'POST', json: {
    name: name,
    amount: parseFloat($('#promo-amount').value) || 0,
    target: parseInt($('#promo-target').value) || 0,
    trial: $('#promo-trial').checked,
    enabled: $('#promo-enabled').checked,
  }});
  toast('活动已创建');
  loadPromoAdmin();
});

function activate(name) {
  $$('.sidebar nav a').forEach(a => a.classList.toggle('active', a.dataset.pane === name));
  $$('.pane').forEach(p => p.classList.toggle('active', p.id === 'pane-' + name));
  if (LOADERS[name]) LOADERS[name]();
}

async function loadOverview() {
  const o = await run(() => api('overview'));
  if (!o) return;
  const cards = [
    ['账号', o.keys.enabled + ' / ' + o.keys.total, o.keys.banned ? '封禁 ' + o.keys.banned : '全部可用', 'accent', 'bi-key'],
    ['今日请求', o.today.total, '成功 ' + o.today.success + ' / 失败 ' + o.today.fail, 'green', 'bi-activity'],
    ['今日成功率', o.today.rate == null ? '—' : o.today.rate + '%', '账号计 ' + o.daily.requests + ' 次', 'accent', 'bi-graph-up'],
    ['当前 RPM', o.rpm + ' / ' + (o.rpm_limit_total === -1 ? '不限' : o.rpm_limit_total), '今日 tokens ' + o.daily.tokens, 'amber', 'bi-speedometer'],
    ['排队中', o.queue, o.queue ? '等待可用账号' : '无等待', o.queue ? 'red' : 'accent', 'bi-hourglass-split'],
    ['在途 / 连接池', (o.pool ? o.pool.inflight : 0) + ' / ' + (o.pool ? o.pool.max_connections : 0),
      o.pool && o.pool.inflight >= o.pool.max_connections * 0.8 ? '已吃满池容量' : '池余量充足',
      o.pool && o.pool.inflight >= o.pool.max_connections * 0.8 ? 'amber' : 'accent', 'bi-diagram-3'],
  ];
  const box = $('#dash-cards'); box.innerHTML = '';
  for (const [label, val, sub, tone, icon] of cards) {
    const col = el('div', 'col-6 col-md-4 col-xl');
    const stat = el('div', 'card stat h-100 ' + tone);
    const lblRow = el('div', 'd-flex align-items-center gap-2 stat-lbl');
    lblRow.append(el('i', 'bi ' + icon), el('span', 'lbl', label));
    stat.append(lblRow, el('div', 'num', String(val)), el('div', 'sub', sub || ' '));
    col.appendChild(stat); box.appendChild(col);
  }

  const tb = $('#dash-models'); tb.innerHTML = '';
  if (!o.models.length) { const tr = el('tr'); const td = el('td', 'text-muted text-center', '—'); td.colSpan = 2; tr.appendChild(td); tb.appendChild(tr); }
  for (const m of o.models) {
    const tr = el('tr');
    const td1 = el('td', 'text-truncate', m.model); td1.style.maxWidth = '180px'; td1.title = m.model;
    tr.append(td1, el('td', 'text-end', String(m.count)));
    tb.appendChild(tr);
  }

  const te = $('#dash-errors'); te.innerHTML = '';
  if (!o.recent_errors.length) { const tr = el('tr'); const td = el('td', 'text-muted text-center', '—'); td.colSpan = 4; tr.appendChild(td); te.appendChild(tr); }
  for (const r of o.recent_errors) {
    const tr = el('tr');
    tr.append(el('td', 'small', fmtTime(r[0])), el('td', 'small text-truncate', r[2] || '-'),
      el('td', 'small', statusText(r[4])), el('td', 'small err-cell text-truncate ' + errCls(r[4]), r[6] || '-'));
    te.appendChild(tr);
  }

  const tk = $('#dash-risky'); tk.innerHTML = '';
  if (!o.risky.length) { const tr = el('tr'); const td = el('td', 'text-muted text-center', '—'); td.colSpan = 4; tr.appendChild(td); tk.appendChild(tr); }
  for (const k of o.risky) {
    const tr = el('tr');
    tr.append(el('td', 'small', k.email), el('td', 'text-end small' + (k.consecutive ? ' text-danger fw-bold' : ''), String(k.consecutive)),
      el('td', 'text-end small', k.fail_ratio + '%'), el('td', 'small text-danger err-cell text-truncate', k.last_error || '-'));
    tk.appendChild(tr);
  }
  loadPoolMap();
}

async function loadPoolMap() {
  const box = $('#poolmap');
  if (!box) return;
  const d = await run(() => api('poolmap'));
  if (!d || !d.groups) return;
  box.innerHTML = '';
  let tot = {ok: 0, busy: 0, bad: 0, total: 0};
  for (const g of d.groups) {
    const grp = el('div', 'pm-group');
    const head = el('div', 'pm-head');
    const dot = el('span', 'pm-dot');
    dot.style.background = g.bad === g.total ? '#dc2626' : (g.ok === g.total ? '#16a34a' : '#d97706');
    head.append(dot, el('b', '', g.name),
      el('span', 'text-muted', `${g.total} · 可用 ${g.ok} · 繁忙 ${g.busy} · 不可用 ${g.bad}`));
    grp.appendChild(head);
    const cells = el('div', 'pm-cells');
    for (const c of g.cells) {
      const cell = el('span', 'cell s' + c.s);
      cell.title = c.w;
      cells.appendChild(cell);
    }
    grp.appendChild(cells);
    box.appendChild(grp);
    tot.ok += g.ok; tot.busy += g.busy; tot.bad += g.bad; tot.total += g.total;
  }
  const sum = $('#poolmap-sum');
  if (sum) sum.textContent = tot.total + ' 个账号 · ' + tot.ok + ' 可用 · ' + tot.busy + ' 繁忙 · ' + tot.bad + ' 不可用';
}


const keysState = {page: 1};
let revealSet = new Set();
const batchSel = new Set();

function updateBatchBar(rows) {
  const bar = $('#batch-bar');
  const n = batchSel.size;
  bar.classList.toggle('d-none', n === 0);
  $('#batch-count').textContent = String(n);
  const all = rows && rows.length && rows.every(k => batchSel.has(k.id));
  $('#keys-all').checked = !!all;
}

async function runBatch(op) {
  const ids = [...batchSel];
  if (!ids.length) return;
  if (op === 'delete') {
    const ok = await uiConfirm(`删除选中的 ${ids.length} 个账号？此操作不可恢复。`, {danger: true, okText: '删除'});
    if (!ok) return;
  }
  if (op === 'reset') {
    const ok = await uiConfirm(`重置选中的 ${ids.length} 个账号的统计与封禁？`);
    if (!ok) return;
  }
  await run(async () => {
    const payload = {op, ids};
    if (op === 'move') payload.upstream_id = $('#batch-up').value;
    const r = await api('keys/batch', {method: 'POST', json: payload});
    if (op === 'delete' || op === 'move') batchSel.clear();  
    if (op === 'test') {
      const okN = Object.values(r.results).filter(t => t && t.ok).length;
      toast(`测试完成：${okN}/${Object.keys(r.results).length} 可用`);
    } else if (op === 'move') {
      toast(`已转移 ${r.moved} 个账号`);
    } else if (op === 'unban') {
      const okN = Object.values(r.results || {}).filter(Boolean).length;
      toast(`已解封 ${okN} 个账号`);
    } else {
      toast('批量操作完成');
    }
    loadKeys(); loadOverview();
  });
}

function bindBatch() {
  $('#keys-all').onchange = e => {
    $$('#keys-rows tr.krow').forEach(tr => {
      const cb = tr.querySelector('.row-check');
      if (cb) { cb.checked = e.target.checked; e.target.checked ? batchSel.add(tr.dataset.id) : batchSel.delete(tr.dataset.id); }
    });
    updateBatchBar();
  };
  $('#batch-clear').onclick = () => { batchSel.clear(); loadKeys(); };
  $('#batch-unban').onclick = () => runBatch('unban');
  $('#batch-enable').onclick = () => runBatch('enable');
  $('#batch-disable').onclick = () => runBatch('disable');
  $('#batch-test').onclick = () => runBatch('test');
  $('#batch-reset').onclick = () => runBatch('reset');
  $('#batch-delete').onclick = () => runBatch('delete');
  $('#batch-move').onclick = () => runBatch('move');
}

async function loadKeys() {
  const q = $('#keys-q').value.trim();
  const status = $('#keys-filter').value;
  const d = await run(() => api(`keys?q=${encodeURIComponent(q)}&status=${status}&page=${keysState.page}`));
  if (!d) return;
  const tb = $('#keys-rows'); tb.innerHTML = '';
  if (!d.rows.length) {
    const tr = el('tr'); const td = el('td', 'text-muted text-center py-4', '—');
    td.colSpan = 16; tr.appendChild(td); tb.appendChild(tr);
  }
  
  const groups = new Map();
  for (const k of d.rows) {
    const g = k.upstream_name || '-';
    if (!groups.has(g)) groups.set(g, []);
    groups.get(g).push(k);
  }
  for (const [gName, rows] of groups) {
    const grpTr = el('tr', 'grp-row');
    const grpTd = el('td', '', '');
    grpTd.colSpan = 16;
    grpTd.append(
      el('i', 'bi bi-hdd-network me-1 text-muted'),
      document.createTextNode(gName + ' '),
      el('span', 'text-muted fw-normal', `（${rows.filter(k => k.enabled).length}/${rows.length} 可用 · 今日 ${rows.reduce((s, k) => s + (k.today.requests || 0), 0)} 次 · ${rows.reduce((s, k) => s + (k.today.tokens || 0), 0)} tk）`),
    );
    grpTr.appendChild(grpTd);
    tb.appendChild(grpTr);
  }
  for (const k of d.rows) {
    const tr = el('tr', 'krow');
    tr.dataset.id = k.id;
    const st = keyState(k);
    if (!k.enabled || st.c === 'danger') tr.classList.add('table-light');
    else if (st.c === 'warning') tr.classList.add('table-warning');

    const chevTd = el('td', 'text-center');
    const rowCb = el('input', 'form-check-input row-check');
    rowCb.type = 'checkbox';
    rowCb.checked = batchSel.has(k.id);
    rowCb.style.verticalAlign = 'middle';
    rowCb.onclick = e => e.stopPropagation();
    rowCb.onchange = () => { rowCb.checked ? batchSel.add(k.id) : batchSel.delete(k.id); updateBatchBar(); };
    const chev = el('i', 'bi bi-chevron-right chev ms-1');
    chevTd.append(rowCb, chev);
    chevTd.title = '选择 / 展开详情';

    const tdEmail = el('td', 'small', k.email);
    const tdKey = el('td');
    const code = el('code', 'key-mono');
    code.textContent = revealSet.has(k.id) ? k.apikey : k.apikey.slice(0, 11) + '…' + k.apikey.slice(-4);
    code.style.cursor = 'pointer'; code.title = '显示/隐藏';
    code.onclick = () => { revealSet.has(k.id) ? revealSet.delete(k.id) : revealSet.add(k.id); loadKeys(); };
    const copy = el('button', 'btn btn-sm btn-link py-0 ps-1', '复制');
    copy.onclick = () => copyText(k.apikey).then(ok => toast(ok ? '已复制' : '复制失败，请手动复制'));
    tdKey.append(code, copy);

    tr.append(
      chevTd,
      tdEmail, tdKey,
      el('td', 'small text-muted', k.upstream_name || '-'),
      (() => { const td = el('td'); td.appendChild(el('span', 'badge text-bg-' + st.c, st.t));
        if (k.last_error) { const e = el('div', 'small text-danger text-truncate', k.last_error); e.style.maxWidth = '170px'; e.title = k.last_error; td.appendChild(e); }
        return td; })(),
      (() => { const td = el('td', 'text-end small', `${k.today.requests}/${d.daily_cap} · ${k.today.tokens}`); td.title = '今日请求/上限 · tokens'; return td; })(),
      el('td', 'text-end', String(k.total_requests)),
      el('td', 'text-end text-success', String(k.total_success)),
      el('td', 'text-end' + (k.total_fail ? ' text-danger' : ''), String(k.total_fail)),
      el('td', 'text-end' + (k.consecutive_failures ? ' text-danger fw-bold' : ''), String(k.consecutive_failures)),
      el('td', 'text-end' + (k.inflight ? ' text-primary fw-semibold' : ' text-muted'), String(k.inflight || 0)),
      el('td', 'text-end', `${k.rpm_used}/${d.rate_limit}`),
      el('td', 'text-end small', `${k.prompt_tokens}/${k.completion_tokens}`),
      el('td', 'small text-muted', fmtAgo(k.last_used_at)),
    );

    const tdOp = el('td');
    const grp = el('div', 'btn-group btn-group-sm');
    const mk = (label, cls, fn) => {
      const b = el('button', 'btn btn-sm ' + cls, label);
      b.onclick = fn; grp.appendChild(b);
    };
    if (st.banned) {
      mk('解封', 'btn-outline-warning', guard(async () => {
        await api('keys/op', {method: 'POST', json: {op: 'unban', id: k.id}}); loadKeys();
      }));
    }
    mk(k.enabled ? '停用' : '启用', 'btn-outline-' + (k.enabled ? 'secondary' : 'success'),
      guard(async () => { await api('keys/op', {method: 'POST', json: {op: k.enabled ? 'disable' : 'enable', id: k.id}}); loadKeys(); }));
    mk('测试', 'btn-outline-primary', guard(async () => {
      const t = (await api('keys/op', {method: 'POST', json: {op: 'test', id: k.id}})).test;
      t.ok ? toast(`可用 · ${t.models} 模型 · ${t.ms}ms`) : toast(`失败：${t.error || 'HTTP ' + t.status}`, 'danger');
    }));
    mk('重置', 'btn-outline-secondary', guard(async () => {
      if (await uiConfirm('重置该账号统计与封禁？')) { await api('keys/op', {method: 'POST', json: {op: 'reset', id: k.id}}); loadKeys(); }
    }));
    mk('删除', 'btn-outline-danger', guard(async () => {
      if (await uiConfirm(`删除 ${k.email}？`, {danger: true, okText: '删除'})) { await api('keys/op', {method: 'POST', json: {op: 'delete', id: k.id}}); loadKeys(); }
    }));
    tdOp.appendChild(grp);
    tr.appendChild(tdOp);
    tb.appendChild(tr);
    
    tr.addEventListener('click', e => {
      if (e.target.closest('button') || e.target.closest('a') || e.target.closest('code') || e.target.closest('input')) return;
      toggleKeyDetail(k.id, tr);
    });
    if (keysState2.openId === k.id) toggleKeyDetail(k.id, tr, true);
  }
  updateBatchBar(d.rows);

  const pg = $('#keys-pages'); pg.innerHTML = '';
  const addPage = (label, page, active, disabled) => {
    const li = el('li', 'page-item' + (active ? ' active' : '') + (disabled ? ' disabled' : ''));
    const a = el('a', 'page-link', label); a.href = '#';
    a.onclick = e => { e.preventDefault(); if (!active && !disabled) { keysState.page = page; loadKeys(); } };
    li.appendChild(a); pg.appendChild(li);
  };
  addPage('«', Math.max(1, keysState.page - 1), false, keysState.page <= 1);
  for (let p = 1; p <= d.pages; p++) {
    if (p > 1 && p < d.pages && Math.abs(p - keysState.page) > 2) {
      if (p === 2 || p === d.pages - 1) addPage('…', p, false, true);
      continue;
    }
    addPage(String(p), p, p === keysState.page, false);
  }
  addPage('»', Math.min(d.pages, keysState.page + 1), false, keysState.page >= d.pages);
}


const keysState2 = {openId: null};

function toggleKeyDetail(id, tr, force) {
  const existed = tr.nextElementSibling;
  const isOpen = existed && existed.classList.contains('detail-row');
  if (isOpen) {
    if (force === true) return;
    existed.remove();
    tr.classList.remove('open');
    if (keysState2.openId === id) keysState2.openId = null;
    return;
  }
  
  $$('#keys-rows tr.detail-row').forEach(r => r.remove());
  $$('#keys-rows tr.krow.open').forEach(r => r.classList.remove('open'));
  keysState2.openId = id;
  tr.classList.add('open');
  const dtr = el('tr', 'detail-row');
  const td = el('td');
  td.colSpan = tr.children.length;
  const panel = el('div', 'kpanel');
  panel.textContent = '加载中…';
  td.appendChild(panel);
  dtr.appendChild(td);
  tr.after(dtr);
  refreshKeyPanel(id, panel);
}

async function refreshKeyPanel(id, panel) {
  const d = await run(() => api('keydetail?id=' + encodeURIComponent(id)));
  if (!d || !panel.isConnected) return;
  panel.innerHTML = '';
  const k = d.key;
  const st = keyState(k);

  const chips = el('div', 'mini');
  const chip = (label, val) => {
    const c = el('div', 'chip');
    c.append(el('span', '', label), el('b', '', String(val)));
    return c;
  };
  chips.append(
    (() => { const c = chip('状态', st.t); c.appendChild(el('span', 'badge text-bg-' + st.c, ' ')); return c; })(),
    chip('今日', `${k.today.requests} 次 / ${k.today.tokens} tk`),
    chip('RPM', `${k.rpm_used}/${k.rate_limit || '-'}`),
    chip('tokens', `${k.prompt_tokens} / ${k.completion_tokens}`),
    chip('连败', String(k.consecutive_failures)),
    chip('上游', k.upstream_name || '-'),
  );
  panel.appendChild(chips);

  const strip = el('div', 'd-flex align-items-center mb-2 flex-wrap');
  strip.append(el('span', 'small text-muted me-2', '最近 10 次'));
  const recent = d.recent || [];
  for (let i = 0; i < 10; i++) {
    const r = recent[i];
    const dot = el('span', 'dot ' + (r ? ((r[3] >= 200 && r[3] < 400) ? 'ok' : 'fail') : 'empty'));
    if (r) {
      dot.title = `${fmtTime(r[0])} · ${r[2] || r[1]} · HTTP ${statusText(r[3])} · ${r[4]}ms${r[5] ? ' · ' + r[5] : ''}`;
      dot.style.cursor = 'help';
    }
    strip.appendChild(dot);
  }
  const refreshBtn = el('button', 'btn btn-sm btn-link py-0 ms-2', '刷新');
  refreshBtn.onclick = () => { panel.textContent = '加载中…'; refreshKeyPanel(id, panel); };
  strip.appendChild(refreshBtn);
  panel.appendChild(strip);

  const tbl = el('table', 'table table-sm table-bordered mb-0');
  const thead = el('thead', 'table-light');
  const htr = el('tr');
  ['时间', '接口', '模型', '状态', '耗时', '错误'].forEach(h => htr.appendChild(el('th', '', h)));
  thead.appendChild(htr);
  tbl.appendChild(thead);
  const tbody = el('tbody');
  if (!recent.length) {
    const tr = el('tr'); const td = el('td', 'text-muted text-center', '暂无请求'); td.colSpan = 6; tr.appendChild(td); tbody.appendChild(tr);
  }
  for (const r of recent) {
    const tr = el('tr');
    const ok = r[3] >= 200 && r[3] < 400;
    tr.append(
      el('td', 'small', fmtTime(r[0])),
      el('td', 'small', EP_NAMES[r[1]] || r[1]),
      el('td', 'small text-truncate', r[2] || '-'),
      el('td', 'small ' + (ok ? 'text-success' : (r[3] === 499 ? 'text-muted' : 'text-danger fw-bold')), statusText(r[3])),
      el('td', 'text-end small', r[4] + 'ms'),
      el('td', 'small err-cell ' + errCls(r[3]), r[5] || '-'),
    );
    tbody.appendChild(tr);
  }
  tbl.appendChild(tbody);
  panel.appendChild(tbl);
}


async function loadUpstreams() {
  const d = await run(() => api('upstreams'));
  if (!d) return;
  const tb = $('#up-rows'); tb.innerHTML = '';
  if (!d.rows.length) {
    const tr = el('tr'); const td = el('td', 'text-muted text-center py-4', '—');
    td.colSpan = 13; tr.appendChild(td); tb.appendChild(tr);
  }
  for (const u of d.rows) {
    const tr = el('tr');
    if (!u.enabled) tr.classList.add('table-light');
    const modelsTxt = (u.models && u.models.length) ? u.models.length + ' 个' : '全部';
    const modelsCell = el('td', 'small text-muted', modelsTxt + (u.model_map_count ? ' · ' + u.model_map_count + ' 映射' : ''));
    if (u.models && u.models.length) {
      modelsCell.title = '模型：\n' + u.models.join('\n');
      modelsCell.style.cursor = 'help';
    }
    const scoreCls = u.feasibility >= 80 ? 'score-hi' : u.feasibility >= 60 ? 'score-mid' : 'score-lo';
    const flags = [];
    if ((u.hide_errors || 0) === 1 || ((u.hide_errors || 0) === 0 && u.hide_errors_global)) flags.push('隐错');
    if ((u.hide_mapped || 0) === 1 || ((u.hide_mapped || 0) === 0 && u.hide_mapped_global)) flags.push('隐原名');
    if (u.param_overrides && Object.keys(u.param_overrides).length) flags.push('固定参数×' + Object.keys(u.param_overrides).length);
    if (u.thinking_defaults && u.thinking_defaults.trim()) flags.push('思考默认');
    tr.append(
      el('td', 'fw-semibold', u.name),
      (() => { const td = el('td'); const c = el('code', 'key-mono small', u.base); td.appendChild(c); return td; })(),
      (() => { const td = el('td'); td.appendChild(el('span', scoreCls, u.feasibility + '%'));
        td.title = '近 20 次成功率' + (u.recent ? `，样本 ${u.recent}` : '，暂无样本'); return td; })(),
      el('td', 'text-end', String(u.weight)),
      el('td', 'text-end', u.rpm_cap > 0 ? String(u.rpm_cap) : '不限'),
      el('td', 'text-end', u.daily_cap > 0 ? String(u.daily_cap) : '不限'),
      modelsCell,
      (() => { const td = el('td', 'small text-muted', flags.length ? flags.join(' · ') : '—'); return td; })(),
      el('td', 'text-end', `${u.enabled_keys}/${u.keys}`),
      el('td', 'text-end', String(u.today_requests)),
      el('td', 'text-end', String(u.minute_used)),
      (() => { const td = el('td'); td.appendChild(el('span', 'badge text-bg-' + (u.enabled ? 'success' : 'secondary'), u.enabled ? '启用' : '停用')); return td; })(),
    );
    const tdOp = el('td');
    const grp = el('div', 'btn-group btn-group-sm');
    const mk = (label, cls, fn) => {
      const b = el('button', 'btn btn-sm ' + cls, label);
      b.onclick = fn; grp.appendChild(b);
    };
    mk('编辑', 'btn-outline-primary', () => {
      $('#up-id').value = u.id;
      $('#up-name').value = u.name;
      $('#up-base').value = u.base;
      $('#up-weight').value = u.weight;
      $('#up-rpm').value = u.rpm_cap;
      $('#up-daily').value = u.daily_cap;
      $('#up-models').value = (u.models || []).join('\n');
      $('#up-map').value = Object.entries(u.model_map || {}).map(([k, v]) => `${k}=${v}`).join('\n');
      for (const [id, key] of UP_FIELDS) $('#' + id).value = u[key] || 0;
      $('#up-herr').value = String(u.hide_errors || 0);
      $('#up-hname').value = String(u.hide_mapped || 0);
      tdefLoad(u.thinking_defaults || '');
      poverLoad(u.param_overrides || {});
      $('#up-enabled').checked = !!u.enabled;
      $('#up-form-title').textContent = '编辑：' + u.name;
      $('#up-cancel').classList.remove('d-none');
      $('#up-form-title').scrollIntoView({block: 'center'});
    });
    mk(u.enabled ? '停用' : '启用', 'btn-outline-secondary', guard(async () => {
      
      
      
      await api('upstreams', {
        method: 'POST',
        json: {id: u.id, name: u.name, base: u.base, enabled: !u.enabled},
      });
      loadUpstreams();
    }));
    mk('删除', 'btn-outline-danger', guard(async () => {
      if (!(await uiConfirm(`删除上游 ${u.name}？`, {danger: true, okText: '删除'}))) return;
      await api('upstreams/delete', {method: 'POST', json: {id: u.id}});
      loadUpstreams();
    }));
    tdOp.appendChild(grp);
    tr.appendChild(tdOp);
    tb.appendChild(tr);
  }
  
  const sel = $('#import-up');
  const cur = sel.value;
  sel.innerHTML = '';
  for (const u of d.rows.filter(x => x.enabled)) {
    const opt = el('option', '', u.name);
    opt.value = u.id;
    sel.appendChild(opt);
  }
  if ([...sel.options].some(o => o.value === cur)) sel.value = cur;
}


const UP_FIELDS = [
  ['up-rpm-key', 'rpm'], ['up-tpm-key', 'tpm'],
  ['up-coolcd', 'account_cooldown_ms'], ['up-hrl', 'hourly_request_limit'],
  ['up-accon', 'acct_concurrency'], ['up-chcon', 'total_concurrency'],
  ['up-dcap-key', 'daily_request_cap'], ['up-dtok-key', 'daily_token_limit'],
  ['up-retries', 'max_retries'], ['up-bb-base', 'retry_backoff_base_ms'], ['up-bb-max', 'retry_backoff_max_ms'],
  ['up-minwait', 'retry_min_wait_ms'],
  ['up-timeout', 'request_timeout'], ['up-ctimeout', 'connect_timeout'],
  ['up-bstep', 'ban_step_seconds'], ['up-bmax', 'ban_max_seconds'],
  ['up-hfban', 'hard_fail_ban_seconds'], ['up-hfcnt', 'hard_fail_disable_count'],
  ['up-c429k', 'cool_429_seconds'], ['up-c5xxk', 'cool_5xx_seconds'],
  ['up-ctok', 'cool_timeout_seconds'], ['up-ccnk', 'cool_conn_seconds'],
  ['up-bthk', 'breaker_threshold'], ['up-bseck', 'breaker_seconds'],
];


let poverData = {};  
let tdefData = {};   

function poverLoad(obj) {
  poverData = {};
  for (const [scope, params] of Object.entries(obj || {})) {
    poverData[scope] = {};
    for (const [k, v] of Object.entries(params || {})) {
      poverData[scope][k] = v;
    }
  }
  poverRender();
}

function poverClear() {
  poverData = {};
  poverRender();
}

function poverDump() {
  const out = {};
  for (const [scope, params] of Object.entries(poverData)) {
    if (Object.keys(params).length) out[scope] = params;
  }
  return out;
}

function poverRender() {
  const list = $('#pover-list');
  if (!list) return;
  list.innerHTML = '';
  const entries = Object.entries(poverData);
  if (!entries.length) {
    list.innerHTML = '<span class="text-muted small">暂无覆写规则</span>';
    return;
  }
  for (const [scope, params] of entries) {
    for (const [k, v] of Object.entries(params)) {
      const item = el('span', 'pover-item');
      const scopeSpan = el('span', 'pover-scope', scope === '*' ? '全部' : scope);
      const kvSpan = el('span', 'pover-kv', `${k}=${v}`);
      const del = el('span', 'pover-del', '×');
      del.title = '删除';
      del.onclick = () => { delete poverData[scope][k]; if (!Object.keys(poverData[scope]).length) delete poverData[scope]; poverRender(); };
      item.append(scopeSpan, kvSpan, del);
      list.appendChild(item);
    }
  }
}

function tdefLoad(raw) {
  tdefData = {};
  for (const line of String(raw || '').split('\n')) {
    const line2 = line.trim();
    if (!line2 || !line2.includes('=')) continue;
    const [k, ...rest] = line2.split('=');
    const v = rest.join('=').trim();
    if (k.trim() && v) tdefData[k.trim()] = v;
  }
  tdefRender();
}

function tdefDump() {
  return Object.entries(tdefData).map(([k, v]) => `${k}=${v}`).join('\n');
}

function tdefClear() {
  tdefData = {};
  tdefRender();
}

function tdefRender() {
  const list = $('#tdef-list');
  if (!list) return;
  list.innerHTML = '';
  const entries = Object.entries(tdefData);
  if (!entries.length) {
    list.innerHTML = '<span class="text-muted small">暂无默认强度</span>';
    return;
  }
  for (const [model, effort] of entries) {
    const item = el('span', 'pover-item');
    const scopeSpan = el('span', 'pover-scope', model);
    const kvSpan = el('span', 'pover-kv', effort);
    const del = el('span', 'pover-del', '×');
    del.title = '删除';
    del.onclick = () => { delete tdefData[model]; tdefRender(); };
    item.append(scopeSpan, kvSpan, del);
    list.appendChild(item);
  }
}

function poverAdd() {
  const scope = $('#pover-model').value.trim() || '*';
  const key = $('#pover-key').value.trim();
  const val = $('#pover-val').value.trim();
  if (!key || !val) { toast('参数名和值不能为空', 'warning'); return; }
  if (!poverData[scope]) poverData[scope] = {};
  poverData[scope][key] = val;
  $('#pover-key').value = '';
  $('#pover-val').value = '';
  poverRender();
}

function tdefAdd() {
  const model = $('#tdef-model').value.trim();
  const effort = $('#tdef-effort').value;
  if (!model) { toast('请选择模型', 'warning'); return; }
  tdefData[model] = effort;
  tdefRender();
  toast(`已添加 ${model}=${effort}`, 'success');
}
async function fillModelSelects() {
  const sel1 = $('#pover-model');
  const sel2 = $('#tdef-model');
  if (!sel1 && !sel2) return;
  const models = new Set();
  
  try {
    const d = await api('upstreams');
    for (const u of (d.rows || [])) {
      for (const m of (u.models || [])) models.add(m);
      for (const src of Object.keys(u.model_map || {})) models.add(src);
    }
  } catch (e) {  }
  
  const upModels = $('#up-models');
  if (upModels && upModels.value.trim()) {
    upModels.value.split(/[\n,]/).forEach(m => { m = m.trim(); if (m) models.add(m); });
  }
  const upMap = $('#up-map');
  if (upMap && upMap.value.trim()) {
    upMap.value.split('\n').forEach(line => {
      const parts = line.split('=');
      if (parts[0] && parts[0].trim()) models.add(parts[0].trim());
    });
  }
  for (const sel of [sel1, sel2]) {
    if (!sel) continue;
    const cur = sel.value;
    sel.innerHTML = '<option value="">选择模型...</option>';
    for (const m of [...models].sort()) {
      const opt = el('option', '', m);
      opt.value = m;
      sel.appendChild(opt);
    }
    if (cur && [...sel.options].some(o => o.value === cur)) sel.value = cur;
  }
}

function bindUpstreams() {
  $('#up-save').onclick = guard(async () => {
    const payload = {
      id: $('#up-id').value || undefined,
      name: $('#up-name').value.trim(),
      base: $('#up-base').value.trim(),
      weight: $('#up-weight').value,
      rpm_cap: $('#up-rpm').value,
      daily_cap: $('#up-daily').value,
      models: $('#up-models').value,
      model_map: $('#up-map').value,
      hide_errors: $('#up-herr').value,
      hide_mapped: $('#up-hname').value,
      thinking_defaults: tdefDump(),
      param_overrides: poverDump(),
      enabled: $('#up-enabled').checked,
    };
    for (const [id, key] of UP_FIELDS) payload[key] = $('#' + id).value;
    await api('upstreams', {method: 'POST', json: payload});
    $('#up-id').value = '';
    $('#up-name').value = '';
    $('#up-base').value = '';
    $('#up-weight').value = '10';
    $('#up-rpm').value = '0';
    $('#up-daily').value = '0';
    $('#up-models').value = '';
    $('#up-map').value = '';
    $('#up-herr').value = '0';
    $('#up-hname').value = '0';
    tdefClear();
    poverClear();
    for (const [id] of UP_FIELDS) $('#' + id).value = '0';
    $('#up-enabled').checked = true;
    $('#up-form-title').textContent = '添加上游';
    $('#up-cancel').classList.add('d-none');
    toast('已保存');
    loadUpstreams(); refreshBatchUps(); loadPresets();
  });
  $('#up-cancel').onclick = () => {
    $('#up-id').value = '';
    $('#up-form-title').textContent = '添加上游';
    $('#up-cancel').classList.add('d-none');
  };
  
  const poverBtn = $('#pover-add');
  if (poverBtn) poverBtn.onclick = () => { poverAdd(); };
  const tdefBtn = $('#tdef-add');
  if (tdefBtn) tdefBtn.onclick = () => { tdefAdd(); };
  
  const upModels = $('#up-models');
  if (upModels) upModels.addEventListener('input', fillModelSelects);
  const upMap = $('#up-map');
  if (upMap) upMap.addEventListener('input', fillModelSelects);
}

async function loadPresets() {
  try {
    const r = await fetch(B + 'api/presets');
    const d = await r.json();
    const sel = $('#up-preset');
    sel.innerHTML = '<option value="">— 选择预设 —</option>';
    for (const [id, p] of Object.entries(d.presets || {})) {
      sel.innerHTML += `<option value="${esc(id)}">${esc(p.name)}</option>`;
    }
    sel.onchange = () => {
      const p = d.presets[sel.value];
      if (!p) return;
      for (const [id, key] of UP_FIELDS) { $('#' + id).value = p[key] != null ? p[key] : 0; }
    };
  } catch(e) {}
}

async function refreshBatchUps() {
  const d = await run(() => api('upstreams'));
  if (!d) return;
  const sel = $('#batch-up');
  sel.innerHTML = '';
  for (const u of d.rows.filter(x => x.enabled)) {
    const opt = el('option', '', u.name);
    opt.value = u.id;
    sel.appendChild(opt);
  }
}


function bindImport() {
  $('#import-btn').onclick = guard(async () => {
    const text = $('#import-text').value;
    const file = $('#import-file').files[0];
    if (!file && !text.trim()) return toast('无内容', 'warning');
    const fd = new FormData();
    if (file) fd.append('file', file);
    if (text.trim()) fd.append('text', text);
    fd.append('upstream_id', $('#import-up').value);
    const res = await fetch(B + 'api/keys/import', {method: 'POST', headers: {'X-CSRF': CSRF}, body: fd});
    const data = await res.json();
    if (!res.ok) throw new Error((data.error && data.error.message) || '导入失败');
    $('#import-result').textContent = `新增 ${data.added} · 更新 ${data.updated} · 去重 ${data.duplicate} · 无效 ${data.invalid} · 总 ${data.total}`;
    keysState.page = 1;
    loadKeys(); loadOverview();
  });
}


const tokReveal = new Set();

async function loadTokens() {
  const d = await run(() => api('tokens'));
  if (!d) return;
  const tb = $('#tok-rows'); tb.innerHTML = '';
  if (!d.rows.length) {
    const tr = el('tr'); const td = el('td', 'text-muted text-center py-4', '—');
    td.colSpan = 5; tr.appendChild(td); tb.appendChild(tr);
    return;
  }
  for (const t of d.rows) {
    const tr = el('tr');
    const tdT = el('td');
    const code = el('code', 'key-mono');
    code.textContent = tokReveal.has(t.t) ? t.t : t.t.slice(0, 11) + '…' + t.t.slice(-4);
    code.style.cursor = 'pointer'; code.title = '显示/隐藏';
    code.onclick = () => { tokReveal.has(t.t) ? tokReveal.delete(t.t) : tokReveal.add(t.t); loadTokens(); };
    const copy = el('button', 'btn btn-sm btn-link py-0 ps-1', '复制');
    copy.onclick = () => copyText(t.t).then(ok => toast(ok ? '已复制' : '复制失败，请手动复制'));
    tdT.append(code, copy);
    tr.appendChild(tdT);
    const tdM = el('td');
    const mWrap = el('div', 'small');
    if (t.m && t.m.length) {
      t.m.forEach((m, i) => {
        if (i) mWrap.appendChild(document.createTextNode(', '));
        mWrap.appendChild(el('span', 'badge bg-primary-subtle text-primary me-1', m));
      });
    } else {
      mWrap.appendChild(el('span', 'text-muted', '全部模型'));
    }
    tdM.appendChild(mWrap);
    tr.appendChild(tdM);
    tr.appendChild(el('td', 'small text-muted', t.last_at ? fmtAgo(t.last_at) : '从未'));
    tr.appendChild(el('td', 'small key-mono', t.last_ip || '-'));
    const tdOp = el('td', 'text-end');
    const grp = el('div', 'btn-group btn-group-sm');
    const mk = (label, cls, fn) => { const b = el('button', 'btn btn-sm ' + cls, label); b.onclick = fn; grp.appendChild(b); };
    mk('编辑模型', 'btn-outline-primary', guard(async () => {
      const val = await uiPrompt('设置该令牌可用的模型，留空为全部',
        (t.m || []).join(', '), {title: '模型限制', placeholder: 'kimi-k3, glm-5.3'});
      if (val === null) return;
      await api('tokens/update', {method: 'POST', json: {t: t.t, m: val}});
      toast('已更新'); loadTokens();
    }));
    mk('删除', 'btn-outline-danger', guard(async () => {
      if (await uiConfirm(`删除该令牌？使用它的客户端将立即失效。`, {danger: true, okText: '删除'})) {
        await api('tokens/delete', {method: 'POST', json: {t: t.t}});
        loadTokens();
      }
    }));
    tdOp.appendChild(grp);
    tr.appendChild(tdOp);
    tb.appendChild(tr);
  }
}

function bindTokens() {
  $('#tok-add').onclick = guard(async () => {
    const wasCustom = !!$('#tok-name').value.trim();
    const r = await api('tokens', {method: 'POST', json: {
      t: $('#tok-name').value.trim(), m: $('#tok-models').value.trim(),
    }});
    $('#tok-name').value = ''; $('#tok-models').value = '';
    if (wasCustom) {
      toast('已添加');
    } else {
      copyText(r.token);
      toast(`已生成并复制：${r.token.slice(0, 18)}…`);
    }
    loadTokens();
  });
}
bindTokens();


const logsState = {filter: 'all', rows: []};
const EP_NAMES = {chat: 'chat/completions', resp: 'responses', cmpl: 'completions', emb: 'embeddings', models: 'models', test: '测试'};
const EP_BADGE = {chat: 'bg-primary-subtle text-primary', resp: 'bg-info-subtle text-info', cmpl: 'bg-primary-subtle text-primary',
                  emb: 'bg-warning-subtle text-warning', models: 'bg-secondary-subtle text-secondary', test: 'bg-secondary-subtle text-secondary'};

async function loadLogs() {
  const d = await run(() => api('logs'));
  if (!d) return;
  logsState.rows = d.rows || [];
  $('#logs-meta').textContent = `${logsState.rows.length} 条`;
  renderLogs();
}

function renderLogs() {
  const tb = $('#logs-rows'); tb.innerHTML = '';
  const rows = logsState.rows.filter(r => {
    const st = r[4];
    if (logsState.filter === 'ok') return st >= 200 && st < 400;
    if (logsState.filter === 'err') return st === 0 || st >= 400;
    return true;
  });
  if (!rows.length) {
    const tr = el('tr'); const td = el('td', 'text-muted text-center py-4', '—');
    td.colSpan = 9; tr.appendChild(td); tb.appendChild(tr);
    return;
  }
  for (const r of rows) {
    
    const t = r[0], ep = r[1], model = r[2], key = r[3], st = r[4], ms = r[5], err = r[6], ip = r[7], att = r[8];
    const upModel = r[9] || '', isStream = !!r[10], ttfb = r[11] || 0, inTok = r[12] || 0, outTok = r[13] || 0, tok = r[14] || '';
    const stCls = st >= 400 || st === 0 ? 'bg-danger-subtle text-danger' : 'bg-success-subtle text-success';
    const tr = el('tr');
    tr.append(
      el('td', 'small text-muted', fmtTime(t)),
      (() => {
        const td = el('td');
        const div = el('div', 'fw-medium', model || '-');
        td.appendChild(div);
        if (upModel && upModel !== model) {
          td.appendChild(el('div', 'small text-muted', '↳ ' + upModel));
        }
        return td;
      })(),
      (() => {
        const td = el('td');
        const badge = el('span', 'badge ' + (EP_BADGE[ep] || 'bg-secondary-subtle text-secondary'), EP_NAMES[ep] || ep);
        td.appendChild(badge);
        if (isStream) td.appendChild(el('span', 'badge bg-purple-subtle text-purple ms-1', 'SSE'));
        return td;
      })(),
      el('td', 'small', key || '-'),
      el('td', 'small key-mono', tok || '-'),
      (() => {
        const td = el('td');
        td.appendChild(el('span', 'badge ' + stCls, statusText(st)));
        return td;
      })(),
      
      (() => {
        const td = el('td', 'text-end small text-muted');
        if (inTok || outTok) {
          td.textContent = `${inTok}↑ ${outTok}↓`;
        } else {
          td.textContent = '—';
        }
        return td;
      })(),
      
      (() => {
        const td = el('td', 'text-end small');
        if (isStream && ttfb > 0 && ttfb < ms) {
          td.textContent = `${ttfb}ms / ${ms}ms`;
        } else {
          td.textContent = ms + 'ms';
        }
        return td;
      })(),
      
      el('td', 'small text-danger err-cell', err || '-'),
    );
    tr.style.cursor = 'pointer';
    tr.title = '点击查看完整信息';
    tr.onclick = () => showLogDetail(r);
    tb.appendChild(tr);
  }
}

function showLogDetail(r) {
  const [t, ep, model, key, st, ms, err, ip, att] = r;
  const upModel = r[9] || '', isStream = !!r[10], ttfb = r[11] || 0, inTok = r[12] || 0, outTok = r[13] || 0, tok = r[14] || '';
  const wrap = el('div', 'conv');
  const addRow = (label, value, cls) => {
    const row = el('div', 'cmsg');
    row.append(el('span', 'crole r-system', label), el('div', 'ctext' + (cls ? ' ' + cls : ''), String(value)));
    wrap.appendChild(row);
  };
  addRow('时间', fmtTime(t));
  addRow('端点', (EP_NAMES[ep] || ep || '-') + (isStream ? '(流式)' : ''));
  addRow('模型', (model || '-') + (upModel && upModel !== model ? ` ↳ ${upModel}` : ''));
  addRow('账号', key || '-');
  addRow('令牌', tok || '-');
  addRow('状态', statusText(st), st >= 400 || st === 0 ? 'text-danger' : 'text-success');
  addRow('耗时', ms + 'ms' + (isStream && ttfb > 0 && ttfb < ms ? `(首字 ${ttfb}ms)` : ''));
  addRow('重试', att || 1);
  addRow('Token', inTok || outTok ? `${inTok}↑ ${outTok}↓` : '—');
  addRow('来源', ip || '-');
  if (err) addRow('错误', err, 'text-danger');
  uiPanel('请求详情', wrap);
}


async function loadQueue() {
  const d = await run(() => api('queue'));
  if (!d) return;
  $('#queue-meta').textContent = d.rows.length ? `${d.rows.length} 个等待 · 最长 ${d.max_wait}s` : '空';
  const tb = $('#queue-rows'); tb.innerHTML = '';
  if (!d.rows.length) {
    const tr = el('tr'); const td = el('td', 'text-muted text-center py-4', '—');
    td.colSpan = 8; tr.appendChild(td); tb.appendChild(tr);
    return;
  }
  d.rows.forEach((q, i) => {
    const tr = el('tr');
    tr.append(
      el('td', 'small text-muted', String(i + 1)),
      el('td', 'small', fmtTime(q.t)),
      el('td', 'text-end small' + (q.wait > d.max_wait / 2 ? ' text-warning fw-bold' : ''), q.wait + 's'),
      el('td', 'small', q.ip),
      el('td', 'small key-mono', q.tok || '-'),
      el('td', 'small', EP_NAMES[q.ep] || q.ep),
      el('td', 'small text-truncate', q.model || '-'),
      el('td', 'small text-muted text-truncate', q.reason || '—'),
    );
    tb.appendChild(tr);
  });
}


const SET_FIELDS = [
  ['set-rate', 'rate_limit_per_minute'], ['set-tpm', 'tpm_limit'],
  ['set-coolcd', 'account_cooldown_ms'], ['set-hrl', 'hourly_request_limit'],
  ['set-accon', 'acct_concurrency'], ['set-chcon', 'total_concurrency'],
  ['set-prpm', 'pool_rpm_cap'], ['set-pdcap', 'pool_daily_cap'],
  ['set-dcap', 'daily_request_cap'], ['set-dtok', 'daily_token_limit'],
  ['set-warmup', 'warmup_seconds'],
  ['set-retries', 'max_retries'],
  ['set-bb-base', 'retry_backoff_base_ms'], ['set-bb-max', 'retry_backoff_max_ms'],
  ['set-minwait', 'retry_min_wait_ms'],
  ['set-timeout', 'request_timeout'], ['set-ctimeout', 'connect_timeout'],
  ['set-ban-step', 'ban_step_seconds'], ['set-ban-max', 'ban_max_seconds'],
  ['set-hf-ban', 'hard_fail_ban_seconds'], ['set-hf-cnt', 'hard_fail_disable_count'],
  ['set-qwait', 'queue_max_wait'], ['set-qpoll', 'queue_poll_ms'],
  ['set-c429', 'cool_429_seconds'], ['set-c5xx', 'cool_5xx_seconds'],
  ['set-cto', 'cool_timeout_seconds'], ['set-ccn', 'cool_conn_seconds'],
  ['set-bth', 'breaker_threshold'], ['set-bsec', 'breaker_seconds'],
  ['set-ttfb', 'ttfb_timeout'], ['set-sidle', 'sse_idle_timeout'],
  ['set-poolconns', 'pool_max_connections'],
  ['set-logmax', 'log_max'], ['set-tz', 'timezone'],
  ['set-uachk', 'update_auto_check_hours'],
  ['set-user', 'admin_username'], ['set-mwl', 'model_whitelist'], ['set-mbl', 'model_blacklist'],
  ['set-pover', 'param_overrides'],
  ['set-smax', 'session_log_max'],
  ['set-trmax', 'training_log_max'],
  ['set-trmin', 'training_min_chars'],
  ['set-watchmin', 'watchdog_minutes'],
  ['set-updtok', 'update_token'],
  ['set-restart-h', 'restart_interval_hours'],
  ['set-mttl', 'model_missing_ttl'],
  ['set-intmax', 'intercept_log_max'],
  ['set-signmin', 'sign_min'], ['set-signmax', 'sign_max'],
];

async function loadSettings() {
  const c = await run(() => api('settings'));
  if (!c) return;
  for (const [id, key] of SET_FIELDS) $('#' + id).value = c[key] != null ? c[key] : '';
  $('#set-logen').checked = !!c.log_enabled;
  $('#set-verify').checked = !!c.verify_tls;
  $('#set-queue').checked = !!c.queue_enabled;
  $('#set-update').checked = !!c.update_enabled;
  $('#set-breaker').checked = !!c.breaker_enabled;
  $('#set-herr').checked = !!c.hide_upstream_errors;
  $('#set-mhide').checked = !!c.hide_mapped_names;
  $('#set-watchdog').checked = !!c.watchdog_enabled;
  $('#set-signen').checked = !!c.sign_enabled;
  $('#set-smtp-host').value = c.smtp_host || '';
  $('#set-smtp-port').value = c.smtp_port != null ? c.smtp_port : 465;
  $('#set-smtp-user').value = c.smtp_user || '';
  // smtp_pass 不回显（GET 已脱敏）；留空保存=保持不变
  $('#set-smtp-pass').value = '';
  $('#set-smtp-from').value = c.smtp_from || '';
  $('#set-smtp-tls').checked = c.smtp_tls != null ? !!c.smtp_tls : true;
  $('#set-reg-email-enabled').checked = !!c.reg_email_enabled;
  $('#set-reg-email-regex').value = c.reg_email_regex || '';
  $('#set-reg-email-error-msg').value = c.reg_email_error_msg || '';
}

function bindSettings() {
  $('#settings-save').onclick = guard(async () => {
    const config = {};
    for (const [id, key] of SET_FIELDS) config[key] = $('#' + id).value;
    config.log_enabled = $('#set-logen').checked;
    config.verify_tls = $('#set-verify').checked;
    config.queue_enabled = $('#set-queue').checked;
    config.update_enabled = $('#set-update').checked;
    config.breaker_enabled = $('#set-breaker').checked;
    config.hide_upstream_errors = $('#set-herr').checked;
    config.hide_mapped_names = $('#set-mhide').checked;
    config.watchdog_enabled = $('#set-watchdog').checked;
    config.sign_enabled = $('#set-signen').checked;
  config.smtp_host = $('#set-smtp-host').value;
  config.smtp_port = parseInt($('#set-smtp-port').value) || 465;
  config.smtp_user = $('#set-smtp-user').value;
  const _smtpPass = $('#set-smtp-pass').value;
  if (_smtpPass) config.smtp_pass = _smtpPass;
  config.smtp_from = $('#set-smtp-from').value;
  config.smtp_tls = $('#set-smtp-tls').checked;
  config.reg_email_enabled = $('#set-reg-email-enabled').checked;
  config.reg_email_regex = $('#set-reg-email-regex').value;
  config.reg_email_error_msg = $('#set-reg-email-error-msg').value;
    await api('settings', {method: 'POST', json: {config}});
    toast('已保存'); loadSettings(); fillDocs();
  });

  $('#pwd-btn').onclick = guard(async () => {
    const old = $('#pwd-old').value, nw = $('#pwd-new').value;
    if (!old || !nw) return toast('填写完整', 'warning');
    await api('password', {method: 'POST', json: {old, new: nw}});
    $('#pwd-old').value = ''; $('#pwd-new').value = '';
    toast('密码已修改');
  });

  $('#btn-reset-stats').onclick = guard(async () => {
    if (await uiConfirm('重置全部统计与封禁？', {danger: true})) { await api('stats/reset', {method: 'POST'}); toast('已重置'); loadOverview(); }
  });

  $('#cfg-export').href = B + 'api/config/export';
  $('#cfg-import').onclick = guard(async () => {
    const file = $('#cfg-import-file').files[0];
    if (!file) return toast('请先选择配置 JSON 文件', 'warning');
    const text = await file.text();
    let parsed;
    try { parsed = JSON.parse(text); } catch (e) { return toast('文件不是合法 JSON', 'danger'); }
    const res = await fetch(B + 'api/config/import', {method: 'POST', headers: {'X-CSRF': CSRF, 'Content-Type': 'application/json'}, body: JSON.stringify(parsed)});
    const data = await res.json();
    if (!res.ok) throw new Error((data.error && data.error.message) || '导入失败');
    $('#cfg-result').textContent = `已应用 ${data.applied} 项配置`;
    toast(`配置已导入（${data.applied} 项）`);
    loadSettings(); fillDocs();
  });
  $('#btn-clear-logs').onclick = guard(async () => {
    if (await uiConfirm('清空日志？')) { await api('logs/clear', {method: 'POST'}); toast('已清空'); loadLogs(); }
  });
  $('#btn-clear-keys').onclick = guard(async () => {
    if (!(await uiConfirm('删除账号池中全部账号？', {danger: true, okText: '删除'}))) return;
    if ((await uiPrompt('输入 DELETE 确认：', '', {okText: '删除'})) !== 'DELETE') return;
    const r = await api('keys/clear-all', {method: 'POST', json: {confirm: 'yes'}});
    toast(`已删除 ${r.removed}`); loadKeys(); loadOverview();
  });
}


function fillDocs() {
  const base = location.origin + (B === '/' ? '' : B.replace(/\/$/, ''));
  $('#doc-base').textContent = base + '/v1';
  $('#doc-curl1').textContent =
`curl ${base}/v1/chat/completions \\
  -H "Authorization: Bearer <令牌>" \\
  -H "Content-Type: application/json" \\
  -d '{"model":"deepseek-ai/deepseek-v4-flash-0731","messages":[{"role":"user","content":"hi"}],"stream":true}'`;
  $('#doc-curl2').textContent =
`curl ${base}/v1/responses \\
  -H "Authorization: Bearer <令牌>" \\
  -H "Content-Type: application/json" \\
  -d '{"model":"meta/llama-3.3-70b-instruct","input":"hi","stream":true}'`;
  $('#doc-py').textContent =
`from openai import OpenAI

client = OpenAI(api_key="<令牌>", base_url="${base}/v1")

resp = client.chat.completions.create(
    model="meta/llama-3.3-70b-instruct",
    messages=[{"role": "user", "content": "hi"}],
)
print(resp.choices[0].message.content)`;
  $('#doc-anthropic').textContent =
`from anthropic import Anthropic

client = Anthropic(api_key="<令牌>", base_url="${base}/")

msg = client.messages.create(
    model="meta/llama-3.3-70b-instruct",
    max_tokens=1024,
    messages=[{"role": "user", "content": "hi"}],
)
print(msg.content[0].text)`;
}



function uiPanel(title, contentEl) {
  const mask = el('div', 'ui-mask');
  const box = el('div', 'ui-dialog wide');
  const head = el('div', 'ui-title-row');
  head.append(el('div', 't', title));
  const closeBtn = el('button', 'ui-close', '×');
  head.appendChild(closeBtn);
  const body = el('div', 'ui-body-flex');
  body.appendChild(contentEl);
  box.append(head, body);
  mask.appendChild(box);
  document.body.appendChild(mask);
  const close = () => mask.remove();
  closeBtn.onclick = close;
  mask.onclick = e => { if (e.target === mask) close(); };
  const onKey = e => { if (e.key === 'Escape') { close(); document.removeEventListener('keydown', onKey); } };
  document.addEventListener('keydown', onKey);
}


const EP_SHOW = {chat: 'Chat', resp: 'Responses', msg: 'Messages', cmpl: 'Compl', emb: 'Embed', test: 'Test'};

async function loadTraining() {
  const d = await run(() => api('training?n=100'));
  if (!d) return;
  $('#training-meta').textContent = d.total ? `${d.total} 条 / 上限 ${d.max}` : '空';
  const tb = $('#training-rows'); tb.innerHTML = '';
  if (!d.rows.length) {
    const tr = el('tr'); const td = el('td', 'text-muted text-center py-4', '—');
    td.colSpan = 6; tr.appendChild(td); tb.appendChild(tr);
    return;
  }
  for (const e of d.rows) {
    const tr = el('tr');
    const preview = String(e.response || '').replace(/\s+/g, ' ').slice(0, 90);
    tr.append(
      el('td', 'small text-muted', fmtTime(e.t)),
      el('td', 'small', e.model || '-'),
      (() => { const td = el('td'); td.appendChild(el('span', 'badge bg-secondary-subtle text-secondary', EP_SHOW[e.ep] || e.ep || '-')); return td; })(),
      el('td', 'text-end small', String((e.messages || []).length)),
      el('td', 'small text-truncate', preview || '—'),
    );
    const tdOp = el('td');
    const view = el('button', 'btn btn-sm btn-outline-primary', '查看');
    view.onclick = () => showTrainingDetail(e);
    tdOp.appendChild(view);
    tr.appendChild(tdOp);
    tb.appendChild(tr);
  }
}

function showTrainingDetail(e) {
  const wrap = el('div', 'conv');
  for (const m of (e.messages || [])) {
    const row = el('div', 'cmsg');
    row.append(
      el('span', 'crole r-' + (m.role || 'user'), (m.role || 'user') === 'system' ? 'system' : (m.role || 'user')),
      el('div', 'ctext', String(m.content || '')),
    );
    wrap.appendChild(row);
  }
  if (e.reasoning) {
    const row = el('div', 'cmsg');
    row.append(
      el('span', 'crole r-assistant', '思考'),
      el('div', 'ctext cthink', String(e.reasoning)),
    );
    wrap.appendChild(row);
  }
  const row = el('div', 'cmsg');
  row.append(
    el('span', 'crole r-assistant', 'assistant'),
    el('div', 'ctext', String(e.response || '')),
  );
  wrap.appendChild(row);
  const meta = el('div', 'cmeta');
  meta.textContent = `${e.model || '-'} · ${e.usage ? (e.usage.prompt_tokens + '↑ ' + e.usage.completion_tokens + '↓ tk') : ''}`;
  wrap.appendChild(meta);
  uiPanel(`${fmtTime(e.t)} · 对话详情`, wrap);
}
$('#training-refresh').onclick = loadTraining;
$('#training-clear').onclick = guard(async () => {
  if (await uiConfirm('清空全部训练资料？', {danger: true, okText: '清空'})) {
    await api('training/clear', {method: 'POST'}); loadTraining();
  }
});


async function loadSessions() {
  const d = await run(() => api('sessions'));
  if (!d) return;
  $('#sessions-meta').textContent = d.rows.length ? `${d.rows.length} 条 / 上限 ${d.max}` : '空';
  const box = $('#sessions-list');
  box.innerHTML = '';
  if (!d.rows.length) { box.innerHTML = '<div class="text-muted text-center py-4">—</div>'; return; }
  for (const s of d.rows) {
    const card = document.createElement('div');
    card.className = 'card mb-2';
    const reqPreview = s.req ? s.req.substring(0, 200) : '';
    const respPreview = s.resp ? s.resp.substring(0, 200) : '';
    card.innerHTML = `
      <div class="card-header py-1 d-flex justify-content-between align-items-center">
        <span class="small">${new Date(s.t * 1000).toLocaleString('zh-CN', {hour12: false})}</span>
        <span class="badge text-bg-${s.status >= 400 ? 'danger' : 'success'}">${s.status}</span>
      </div>
      <div class="card-body py-2">
        <div class="small text-muted mb-1">模型: ${esc(s.model)} · 密钥: ${esc(s.key)}</div>
        <details><summary class="small text-primary" style="cursor:pointer">请求</summary>
          <pre style="font-size:.75em;margin:4px 0">${esc(reqPreview)}</pre></details>
        <details><summary class="small text-primary" style="cursor:pointer">响应</summary>
          <pre style="font-size:.75em;margin:4px 0">${esc(respPreview)}</pre></details>
      </div>`;
    box.appendChild(card);
  }
}
$('#sessions-refresh').onclick = loadSessions;
$('#sessions-clear').onclick = guard(async () => {
  if (await uiConfirm('清空全部会话日志？')) { await api('sessions/clear', {method: 'POST'}); loadSessions(); }
});


async function loadIntercept() {
  const d = await run(() => api('intercept'));
  if (!d) return;
  $('#int-enabled').checked = !!d.enabled;
  $('#int-meta').textContent = d.logs.length ? `${d.logs.length} / ${d.cap || 100} 条` : '';
  const upRows = (await run(() => api('upstreams')))?.rows || [];
  const upName = new Map(upRows.map(u => [u.id, u.name]));
  const sel = $('#int-ups');
  const keep = new Set(Array.from(sel.selectedOptions).map(o => o.value));
  sel.innerHTML = '';
  for (const u of upRows) {
    const o = document.createElement('option');
    o.value = u.id; o.textContent = u.name;
    if (keep.has(u.id)) o.selected = true;
    sel.appendChild(o);
  }
  const tb = $('#int-rules'); tb.innerHTML = '';
  if (!d.rules.length) {
    const tr = el('tr'); const td = el('td', 'text-muted text-center py-3', '—');
    td.colSpan = 6; tr.appendChild(td); tb.appendChild(tr);
  }
  for (const r of d.rules) {
    const tr = el('tr');
    const upNames = (r.upstreams || []).map(x => upName.get(x) || x);
    const scopeTxt = [(r.models || []).length ? '模型 ' + (r.models || []).join(',') : '',
                      upNames.length ? '渠道 ' + upNames.join(',') : ''].filter(Boolean).join(' · ') || '全部';
    const hits = Number(r.hits || 0);
    tr.append(
      (() => { const td = el('td'); td.appendChild(el('span', 'badge bg-secondary-subtle text-secondary', r.match_mode || '')); return td; })(),
      (() => { const td = el('td', 'small text-end' + (hits ? '' : ' text-muted')); td.textContent = String(hits); td.title = hits ? fmtTime(r.last_hit_at) : '还没命中过'; return td; })(),
      (() => { const td = el('td', 'small text-muted text-truncate', scopeTxt); td.style.maxWidth = '220px'; td.title = scopeTxt; return td; })(),
      (() => { const td = el('td', 'small key-mono text-truncate'); td.style.maxWidth = '340px'; td.textContent = r.pattern || ''; td.title = r.pattern || ''; return td; })(),
      (() => { const td = el('td', 'small text-truncate'); td.style.maxWidth = '280px'; td.textContent = String(r.reply || '').replace(/[\r\n]+/g, ' '); td.title = r.reply || ''; return td; })(),
    );
    const tdOp = el('td', 'text-end');
    const del = el('button', 'btn btn-sm btn-outline-danger', '删除');
    del.onclick = guard(async () => {
      await api('intercept/rules/delete', {method: 'POST', json: {id: r.id}});
      loadIntercept();
    });
    tdOp.appendChild(del);
    tr.appendChild(tdOp);
    tb.appendChild(tr);
  }
  const tb2 = $('#int-logs'); tb2.innerHTML = '';
  if (!d.logs.length) {
    const tr = el('tr'); const td = el('td', 'text-muted text-center py-3', '—');
    td.colSpan = 6; tr.appendChild(td); tb2.appendChild(tr);
  }
  for (const x of d.logs) {
    const tr = el('tr');
    tr.append(
      el('td', 'small text-muted', fmtTime(x.t)),
      el('td', 'small', x.ip || '-'),
      el('td', 'small key-mono', x.tok || '-'),
      el('td', 'small', x.model || '-'),
      el('td', 'small', x.rule || x.pattern || ''),
      (() => { const td = el('td', 'small'); const box = el('div', ''); box.style.maxWidth = '460px'; box.style.overflowWrap = 'anywhere'; box.textContent = x.content || ''; td.title = x.content || ''; td.appendChild(box); return td; })(),
    );
    tb2.appendChild(tr);
  }
}

$('#int-refresh').onclick = loadIntercept;
$('#int-clear').onclick = guard(async () => {
  if (await uiConfirm('清空全部拦截记录？', {danger: true, okText: '清空'})) {
    await api('intercept/clear', {method: 'POST'}); loadIntercept();
  }
});
$('#int-enabled').onchange = guard(async () => {
  await api('intercept/toggle', {method: 'POST', json: {enabled: $('#int-enabled').checked}});
  toast($('#int-enabled').checked ? '拦截已启用' : '拦截已关闭');
});
$('#int-add').onclick = guard(async () => {
  await api('intercept/rules', {method: 'POST', json: {
    match_mode: $('#int-mode').value,
    pattern: $('#int-pattern').value.trim(),
    reply: $('#int-reply').value,
    models: $('#int-models').value.trim(),
    upstreams: Array.from($('#int-ups').selectedOptions).map(o => o.value),
  }});
  $('#int-pattern').value = ''; $('#int-reply').value = ''; $('#int-models').value = '';
  toast('规则已添加'); loadIntercept();
});
$('#int-test').onclick = guard(async () => {
  const d = await api('intercept/test', {method: 'POST', json: {
    match_mode: $('#int-mode').value,
    pattern: $('#int-pattern').value.trim(),
    sample: $('#int-sample').value,
  }});
  if (!d) return;
  $('#int-meta').textContent = d.matched ? '命中' : ('不命中：' + (d.reason || '样本里没有这段内容'));
  if (d.entity_fixed) $('#int-pattern').value = d.pattern;
  toast(d.matched ? '命中' : '不命中', d.matched ? 'success' : 'warning');
});

let testToken = null;   
let testMsgs = [];      
let testBusy = false;

async function testEnsureToken() {
  if (testToken !== null) return testToken;
  try {
    const s = await api('settings');
    const toks = s.gateway_tokens || [];
    const first = toks[0];
    testToken = (first && (first.t || first)) || '';
  } catch (e) { testToken = ''; }
  const hint = $('#test-hint');
  if (hint) {
    hint.textContent = testToken
      ? '令牌 ' + String(testToken).slice(0, 12) + '…'
      : '未配置网关令牌（网关为开放模式）';
  }
  return testToken;
}

async function loadTestModels() {
  const sel = $('#test-model');
  if (!sel) return;
  const keep = sel.value;
  const tok = await testEnsureToken();
  const models = [];
  try {
    const r = await fetch(B + 'v1/models', { headers: tok ? { Authorization: 'Bearer ' + tok } : {} });
    const d = await r.json();
    for (const m of (d.data || [])) if (m && m.id) models.push(m.id);
  } catch (e) {  }
  if (!models.length) {
    try {
      const d = await api('upstreams');
      for (const u of (d.rows || [])) {
        for (const m of (u.models || [])) models.push(m);
        for (const k of Object.keys(u.model_map || {})) models.push(k);
      }
    } catch (e) {  }
  }
  const uniq = [...new Set(models)];
  sel.innerHTML = '';
  if (!uniq.length) { sel.appendChild(el('option', '', '（无可用模型，请先配置渠道）')); return; }
  for (const m of uniq) sel.appendChild(el('option', '', m));   
  sel.value = keep && uniq.includes(keep) ? keep : uniq[0];
}


const mdRender = (text, node) => {
  if (typeof marked !== 'undefined' && typeof DOMPurify !== 'undefined') {
    node.innerHTML = DOMPurify.sanitize(marked.parse(text, {breaks: true, gfm: true}), {ADD_ATTR: ['target']});
    node.querySelectorAll('a').forEach(a => { a.target = '_blank'; a.rel = 'noopener noreferrer'; });
  } else {
    node.textContent = text;
  }
};

function testRender() {
  const box = $('#test-chat');
  if (!box) return;
  box.innerHTML = '';
  if (!testMsgs.length) {
    box.appendChild(el('span', 'text-muted small', '选择模型后输入消息开始测试。'));
    return;
  }
  for (const m of testMsgs) {
    const mine = m.role === 'user';
    const row = el('div', 'mb-2 d-flex ' + (mine ? 'justify-content-end' : 'justify-content-start'));
    const b = el('div', 'tmsg ' + (mine ? 'me' : 'ai'));
    if (mine) {
      b.textContent = m.content || '';   
    } else {
      if (m.reasoning) {
        const th = el('div', 'think');
        th.appendChild(el('div', 'think-hd', '思考'));
        const tb = el('div', 'think-body');
        mdRender(m.reasoning, tb);
        th.appendChild(tb);
        b.appendChild(th);
      }
      const body = el('div', 'md');
      if (m.content) {
        mdRender(m.content, body);
      } else {
        body.textContent = m.pending ? '…' : '（内容为空）';
      }
      b.appendChild(body);
    }
    if (m.pending) b.classList.add('pending');
    row.appendChild(b);
    box.appendChild(row);
  }
  box.scrollTop = box.scrollHeight;
}

function testDiag(d) {
  const box = $('#test-diag');
  const sum = $('#test-diag-sum');
  const wrap = $('#test-raw-wrap');
  if (!box) return;
  box.innerHTML = '';
  const chip = (label, val, cls) => {
    const s = el('span', 'badge me-2 mb-1 ' + (cls || 'text-bg-light'), label + ' ' + val);
    box.appendChild(s);
  };
  const okStatus = d.status >= 200 && d.status < 400;
  chip('状态', d.status, okStatus ? 'text-bg-success' : 'text-bg-danger');
  chip('首字', d.ttfb == null ? '-' : Math.round(d.ttfb) + 'ms');
  chip('总耗时', Math.round(d.total) + 'ms');
  chip('令牌', (d.inTok || 0) + '↑ ' + (d.outTok || 0) + '↓');
  if (d.events != null) chip('事件', d.events + ' 个');
  if (d.up && d.up !== d.model) chip('映射', d.model + ' ↳ ' + d.up, 'text-bg-info');
  if (d.format) chip('Content-Type', d.format, 'text-bg-light');
  if (d.err) {
    const e = el('div', 'mt-1 text-danger', '错误：' + d.err);
    box.appendChild(e);
  }
  if (sum) sum.textContent = d.at;
  const raw = $('#test-raw');
  if (raw && wrap) {
    raw.textContent = d.raw || '';
    wrap.classList.toggle('d-none', !d.raw);
  }
}

async function testSend() {
  if (testBusy) return;
  const model = $('#test-model') && $('#test-model').value;
  const input = $('#test-input');
  const text = input ? input.value.trim() : '';
  if (!model || !text) return;
  testBusy = true;
  const btn = $('#test-send');
  if (btn) btn.disabled = true;

  testMsgs.push({role: 'user', content: text});
  input.value = '';
  const reply = {role: 'assistant', content: '', reasoning: '', pending: true};
  testMsgs.push(reply);
  testRender();

  const stream = $('#test-stream').checked;
  const sys = ($('#test-system') && $('#test-system').value.trim()) || '';
  const messages = sys ? [{role: 'system', content: sys}, ...testMsgs.filter(m => !m.pending)] : testMsgs.filter(m => !m.pending);
  const tok = await testEnsureToken();
  const headers = {'Content-Type': 'application/json', 'X-NGW-Skip-Training': '1'};
  if (tok) headers.Authorization = 'Bearer ' + tok;

  const t0 = performance.now();
  let ttfb = null, events = 0, inTok = 0, outTok = 0, upModel = '', format = '', err = '', raw = [], status = 0;
  try {
    const r = await fetch(B + 'v1/chat/completions', {
      method: 'POST', headers,
      body: JSON.stringify({model, messages, stream}),
    });
    status = r.status;
    format = (r.headers.get('content-type') || '').split(';')[0];
    if (!stream) {
      const txt = await r.text();
      raw.push(txt);
      let j = null;
      try { j = JSON.parse(txt); } catch (e) {  }
      ttfb = performance.now() - t0;
      if (j) {
        if (j.error) err = (j.error.message || JSON.stringify(j.error)).slice(0, 300);
        reply.content = ((j.choices || [{}])[0].message || {}).content || '';
        reply.reasoning = ((j.choices || [{}])[0].message || {}).reasoning_content || '';
        upModel = j.model || '';
        inTok = (j.usage || {}).prompt_tokens || 0;
        outTok = (j.usage || {}).completion_tokens || 0;
      } else {
        err = txt.slice(0, 300);
      }
    } else if (r.body) {
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
            raw.push(payload);
            if (payload === '[DONE]') continue;
            if (ttfb === null) ttfb = performance.now() - t0;
            events++;
            let j = null;
            try { j = JSON.parse(payload); } catch (e) { continue; }
            if (j.error) { err = (j.error.message || JSON.stringify(j.error)).slice(0, 300); continue; }
            const delta = ((j.choices || [{}])[0] || {}).delta || {};
            if (typeof delta.content === 'string') reply.content += delta.content;
            if (typeof delta.reasoning_content === 'string') reply.reasoning += delta.reasoning_content;
            if (j.model) upModel = j.model;
            if (j.usage) {
              inTok = j.usage.prompt_tokens || inTok;
              outTok = j.usage.completion_tokens || outTok;
            }
            testRender();
          }
        }
      }
    } else {
      err = '无响应体（HTTP ' + status + '）';
    }
  } catch (e) {
    err = String(e && e.message || e).slice(0, 300);
  }
  reply.pending = false;
  testRender();
  testDiag({
    status, ttfb, total: performance.now() - t0, events, inTok, outTok,
    model, up: upModel, format, err, raw: raw.join('\n'),
    at: new Date().toLocaleTimeString('zh-CN', {hour12: false}),
  });
  testBusy = false;
  if (btn) btn.disabled = false;
}

$('#test-reload').onclick = () => { testToken = null; loadTestModels(); };
$('#test-clear').onclick = () => { testMsgs = []; testRender(); };
$('#test-send').onclick = testSend;
$('#gen-updtok').onclick = () => {
  const arr = new Uint8Array(24);
  crypto.getRandomValues(arr);
  const tok = 'upd-' + Array.from(arr).map(b => b.toString(16).padStart(2, '0')).join('');
  $('#set-updtok').value = tok;
  copyText(tok).then(ok => toast(ok ? '更新令牌已生成并复制' : '更新令牌已生成，复制失败请手动复制'));
};
$('#btn-update').onclick = guard(async () => {
  if (!await uiConfirm('检查并安装最新版本？安装完成后网关将自动重启。', {okText: '检查更新'})) return;
  const r = await api('update', {method: 'POST'});
  toast(r.note || '已开始更新');
  setTimeout(() => location.reload(), 5000);
});
$('#btn-rollback').onclick = guard(async () => {
  if (!await uiConfirm('回滚到上次更新前的版本？数据也会恢复到更新前的状态，此操作只能执行一次。',
      {danger: true, okText: '回滚'})) return;
  const r = await api('rollback', {method: 'POST'});
  toast(r.note || '已回滚,网关正在重启');
  setTimeout(() => location.reload(), 3000);
});
$('#test-input').addEventListener('keydown', e => {
  if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); testSend(); }
});


document.addEventListener('DOMContentLoaded', () => {
  $$('.sidebar nav a').forEach(a => a.addEventListener('click', e => { e.preventDefault(); activate(a.dataset.pane); }));
  $('#btn-logout').onclick = guard(async () => { await api('logout', {method: 'POST'}); location.href = B + 'admin'; });
  bindImport(); bindSettings(); bindUpstreams(); bindBatch();
  refreshBatchUps();
  fillModelSelects();  

  let qTimer = null;
  $('#keys-q').addEventListener('input', () => {
    clearTimeout(qTimer);
    qTimer = setTimeout(() => { keysState.page = 1; loadKeys(); }, 300);
  });
  $('#keys-filter').onchange = () => { keysState.page = 1; loadKeys(); };
  $('#keys-refresh').onclick = loadKeys;
  $('#keys-export').href = B + 'api/keys/export';

  $('#logs-filter').onchange = renderLogs;
  $('#logs-refresh').onclick = loadLogs;
  $$('.sec-nav a').forEach(a => a.addEventListener('click', () => {
    const t = document.getElementById(a.dataset.sec);
    if (t) t.scrollIntoView({behavior: 'smooth', block: 'start'});
  }));
  $('#logs-clear').onclick = guard(async () => {
    if (await uiConfirm('清空日志？')) { await api('logs/clear', {method: 'POST'}); loadLogs(); }
  });
  $('#queue-refresh').onclick = loadQueue;
  $('#queue-clear').onclick = guard(async () => { await api('queue', {method: 'POST'}); loadQueue(); });

  activate('dash');
  setInterval(() => {
    if (document.hidden) return;
    if ($('#pane-dash').classList.contains('active')) loadOverview();
    if ($('#pane-logs').classList.contains('active') && $('#logs-auto').checked) loadLogs();
    if ($('#pane-queue').classList.contains('active') && $('#queue-auto').checked) loadQueue();
    if ($('#pane-keys').classList.contains('active') && keysState2.openId) {
      const panel = $('#keys-rows tr.detail-row .kpanel');
      if (panel) refreshKeyPanel(keysState2.openId, panel);
    }
  }, 10000);
});


// ---------------- 模型定价 ----------------
let _priceRows = [];

async function loadPrices() {
  const data = await api('prices');
  _priceRows = data.rows || [];
  renderPrices();
  const sel = $('#price-channel');
  const chs = [...new Set(_priceRows.map(r => r.channel_name))];
  sel.innerHTML = '<option value="">选择渠道</option>' + chs.map(c => `<option>${esc(c)}</option>`).join('');
}

function renderPrices() {
  const tb = $('#price-rows');
  tb.innerHTML = '';
  for (const r of _priceRows) {
    const tr = document.createElement('tr');
    tr.innerHTML = '<td>' + esc(r.channel_name) + '</td>'
      + '<td class="mono fw-semibold">' + esc(r.model) + '</td>'
      + '<td><input type="number" step="0.001" min="0" class="form-control form-control-sm price-input" '
      + 'data-ch="' + esc(r.channel_id) + '" data-model="' + esc(r.model) + '" value="' + (r.price || 0) + '" style="width:100px"></td>'
      + '<td>' + (r.free ? '<span class="badge badge-free">免费</span>' : '<span class="badge badge-paid">付费</span>') + '</td>'
      + '<td class="small text-muted">' + (r.enabled ? '' : '<span class="badge bg-secondary">渠道停用</span>') + '</td>';
    tb.appendChild(tr);
  }
}

$('#price-add').onclick = guard(async () => {
  const ch = $('#price-channel').value;
  const model = $('#price-model').value.trim();
  const val = parseFloat($('#price-val').value) || 0;
  if (!ch || !model) { toast('请选择渠道并填写模型名', 'danger'); return; }
  const exist = _priceRows.find(r => r.channel_name === ch && r.model === model);
  if (exist) { exist.price = val; exist.free = val <= 0; renderPrices(); return; }
  _priceRows.push({channel_id: _priceRows.find(r => r.channel_name === ch)?.channel_id || '',
    channel_name: ch, model: model, price: val, free: val <= 0, enabled: true});
  renderPrices();
});

$('#prices-save').onclick = guard(async () => {
  const byChannel = {};
  document.querySelectorAll('.price-input').forEach(inp => {
    const ch = inp.dataset.ch, model = inp.dataset.model, val = parseFloat(inp.value) || 0;
    if (!byChannel[ch]) byChannel[ch] = {};
    byChannel[ch][model] = val;
  });
  for (const [chId, prices] of Object.entries(byChannel)) {
    await api('prices', {method: 'POST', json: {channel_id: chId, prices: prices}});
  }
  toast('定价已保存');
  loadPrices();
});

// ---------------- 用户管理 ----------------
async function loadUsers() {
  const rows = (await api('users')).rows || [];
  const tb = $('#usr-rows');
  tb.innerHTML = '';
  if (!rows.length) {
    const tr = document.createElement('tr');
    tr.innerHTML = '<td colspan="10" class="text-muted small text-center py-4">还没有用户</td>';
    tb.appendChild(tr);
    return;
  }
  for (const u of rows) {
    const tr = document.createElement('tr');
    tr.innerHTML = '<td class="fw-semibold">' + esc(u.username) + '</td>'
      + '<td class="small text-nowrap"><div>¥' + (u.grant ?? 0).toFixed(4) + '</div><div class="text-muted">累计 ¥' + (u.grant_total ?? 0).toFixed(4) + '</div></td>'
      + '<td class="small text-nowrap"><div>¥' + (u.balance ?? 0).toFixed(4) + '</div><div class="text-muted">累计 ¥' + (u.recharge_total ?? 0).toFixed(4) + '</div></td>'
      + '<td class="small text-nowrap"><div>免费 ' + (u.free_calls ?? 0) + '</div><div class="text-muted">付费 ' + (u.paid_calls ?? 0) + '</div></td>'
      + '<td>' + (u.key_count ?? 0) + '</td>'
      + '<td class="small">' + (u.free_rpm > 0 ? u.free_rpm + ' 次/分' : '不限') + '</td>'
      + '<td class="small">' + (u.paid_rpm > 0 ? u.paid_rpm + ' 次/分' : '关闭') + '</td>'
      + '<td>' + (u.enabled ? '<span class="badge bg-success">启用</span>' : '<span class="badge bg-secondary">停用</span>') + '</td>'
      + '<td class="small text-muted">' + new Date(u.created_at * 1000).toLocaleDateString() + '</td>';
    const td = document.createElement('td');
    td.className = 'text-end text-nowrap';
    const bal = document.createElement('button');
    bal.className = 'btn btn-sm btn-outline-secondary me-1';
    bal.textContent = '充值';
    bal.onclick = () => run(async () => {
      const v = await uiPrompt('为 ' + u.username + ' 充值', '0', {title: '充值 · 计入充值累计'});
      if (v === null) return;
      await api('users/op', {method: 'POST', json: {id: u.id, op: 'add-balance', kind: 'recharge', amount: parseFloat(v) || 0}});
      toast('充值完成'); loadUsers();
    });
    const gift = document.createElement('button');
    gift.className = 'btn btn-sm btn-outline-secondary me-1';
    gift.textContent = '赠金';
    gift.onclick = () => run(async () => {
      const v = await uiPrompt('为 ' + u.username + ' 发放赠金', '0', {title: '赠金 · 计入赠金累计 · 计费优先扣'});
      if (v === null) return;
      await api('users/op', {method: 'POST', json: {id: u.id, op: 'add-balance', kind: 'grant', amount: parseFloat(v) || 0}});
      toast('赠金已发放'); loadUsers();
    });
    const lim = document.createElement('button');
    lim.className = 'btn btn-sm btn-outline-secondary me-1'; lim.textContent = '限速';
    lim.onclick = () => run(async () => {
      const f = await uiPrompt('免费模型每分钟上限', String(u.free_rpm ?? 0), {title: '限速 · 0 为不限', placeholder: '0 为不限'});
      if (f === null) return;
      const p = await uiPrompt('付费模型每分钟上限', String(u.paid_rpm ?? 0), {title: '限速 · 0 为关闭', placeholder: '0 为关闭'});
      if (p === null) return;
      await api('users/op', {method: 'POST', json: {id: u.id, op: 'set-limits', free_rpm: parseInt(f) || 0, paid_rpm: parseInt(p) || 0}});
      toast('限速已更新'); loadUsers();
    });
    const pw = document.createElement('button');
    pw.className = 'btn btn-sm btn-outline-secondary me-1'; pw.textContent = '改密';
    pw.onclick = () => run(async () => {
      const v = await uiPrompt('为 ' + u.username + ' 设置新密码', '', {title: '改密 · 至少 6 位', placeholder: '至少 6 位'});
      if (!v) return;
      await api('users/op', {method: 'POST', json: {id: u.id, op: 'reset-password', password: v}});
      toast('密码已重置');
    });
    const lg = document.createElement('button');
    lg.className = 'btn btn-sm btn-outline-secondary me-1'; lg.textContent = '日志';
    lg.onclick = () => run(async () => {
      const d = await api('user-logs?id=' + encodeURIComponent(u.id) + '&kind=all&page=1&per=100');
      showUserLogs(u, d.rows || [], d.total || 0);
    });
    const fin = document.createElement('button');
    fin.className = 'btn btn-sm btn-outline-secondary me-1'; fin.textContent = '资金';
    fin.onclick = () => run(async () => {
      const [fd, dd] = await Promise.all([
        api('user-funds?id=' + encodeURIComponent(u.id) + '&page=1&per=100'),
        api('user-draws?id=' + encodeURIComponent(u.id) + '&page=1&per=100'),
      ]);
      showUserFinance(u, fd, dd);
    });
    const tog = document.createElement('button');
    tog.className = 'btn btn-sm btn-outline-secondary me-1';
    tog.textContent = u.enabled ? '停用' : '启用';
    tog.onclick = () => run(async () => {
      await api('users/op', {method: 'POST', json: {id: u.id, op: u.enabled ? 'disable' : 'enable'}});
      toast('已' + tog.textContent); loadUsers();
    });
    const del = document.createElement('button');
    del.className = 'btn btn-sm btn-outline-danger'; del.textContent = '删除';
    del.onclick = () => run(async () => {
      if (!await uiConfirm('删除用户 ' + u.username + '？其调用 Key 将一并停用。', {danger: true, okText: '删除'})) return;
      await api('users/op', {method: 'POST', json: {id: u.id, op: 'delete'}});
      toast('已删除'); loadUsers();
    });
    td.append(bal, gift, lim, pw, lg, fin, tog, del);
    tr.appendChild(td);
    tb.appendChild(tr);
  }
  if (!rows.length) tb.innerHTML = '<tr><td colspan="8" class="text-muted small">还没有用户</td></tr>';
}

function showUserLogs(u, rows, total) {
  const tb = el('table', 'table table-sm table-hover align-middle');
  tb.innerHTML = '<thead><tr><th>时间</th><th>模型</th><th>状态</th><th class="text-end">耗时</th>'
    + '<th class="text-end">输入</th><th class="text-end">输出</th><th class="text-end">费用</th></tr></thead>';
  const body = el('tbody');
  if (!rows.length) body.innerHTML = '<tr><td colspan="7" class="text-muted small text-center py-3">该用户还没有成功调用记录</td></tr>';
  for (const r of rows) {
    const tr = el('tr');
    const st = r.st >= 200 && r.st < 400
      ? '<span class="badge bg-success-subtle text-success">' + r.st + '</span>'
      : '<span class="badge bg-danger-subtle text-danger">' + r.st + '</span>';
    tr.innerHTML = '<td class="small text-muted">' + fmtTime(r.t) + '</td>'
      + '<td class="small">' + esc(r.model || '-') + '</td>'
      + '<td>' + st + '</td>'
      + '<td class="text-end small">' + r.ms + 'ms</td>'
      + '<td class="text-end small">' + (r.in_tok || 0) + '</td>'
      + '<td class="text-end small">' + (r.out_tok || 0) + '</td>'
      + '<td class="text-end small fw-semibold">' + (r.cost > 0 ? '¥' + r.cost.toFixed(4) : '免费') + '</td>';
    body.appendChild(tr);
  }
  tb.appendChild(body);
  const note = el('div', 'small text-muted mt-2', '合并视图最近 ' + rows.length + ' 条' + (total > rows.length ? ' · 共 ' + total + ' 条' : ''));
  const wrap = el('div');
  wrap.append(tb, note);
  uiPanel('调用日志 · ' + u.username, wrap);
}

const FUND_KINDS = {
  signup: '建户', adjust: '余额调整', grant: '赠金发放', recharge: '充值',
  sign: '签到', draw: '抽奖消耗', prize: '抽奖中奖', call: '调用计费',
};

function showUserFinance(u, fd, dd) {
  const wrap = el('div');
  const mk = (title, rows, cols, render) => {
    const box = el('div', 'mb-3');
    box.appendChild(el('div', 'fw-semibold mb-1', title));
    const tb = el('table', 'table table-sm table-hover align-middle');
    tb.innerHTML = '<thead><tr>' + cols.map(c => '<th>' + c + '</th>').join('') + '</tr></thead>';
    const body = el('tbody');
    if (!rows.length) {
      body.innerHTML = '<tr><td colspan="' + cols.length + '" class="text-muted small text-center py-3">暂无记录</td></tr>';
    }
    for (const r of rows) body.appendChild(render(r));
    tb.appendChild(body);
    box.appendChild(tb);
    return box;
  };
  const fundRows = fd.rows || [];
  const fundNote = el('div', 'small text-muted mb-1',
    '共 ' + (fd.total ?? fundRows.length) + ' 条 · 每条为一次资金变动（+入账 / -扣减）');
  wrap.appendChild(fundNote);
  wrap.appendChild(mk('资金变动', fundRows, ['时间', '类型', '赠金', '充值', '备注'], r => {
    const tr = el('tr');
    const dg = r.dg ?? 0, dr = r.dr ?? 0;
    const cls = v => v > 0 ? 'text-success fw-semibold' : v < 0 ? 'text-danger fw-semibold' : 'text-muted';
    tr.innerHTML = '<td class="small text-muted">' + fmtTime(r.t) + '</td>'
      + '<td class="small">' + esc(FUND_KINDS[r.kind] || r.kind) + '</td>'
      + '<td class="small ' + cls(dg) + '">' + (dg > 0 ? '+' : '') + dg.toFixed(4) + '</td>'
      + '<td class="small ' + cls(dr) + '">' + (dr > 0 ? '+' : '') + dr.toFixed(4) + '</td>'
      + '<td class="small text-muted">' + esc(r.note || '-') + '</td>';
    return tr;
  }));
  const drawRows = dd.rows || [];
  wrap.appendChild(mk('抽奖记录', drawRows, ['时间', '转盘', '奖品', '消耗赠金', '消耗充值'], r => {
    const tr = el('tr');
    tr.innerHTML = '<td class="small text-muted">' + fmtTime(r.t) + '</td>'
      + '<td class="small">' + esc(r.wheel_name || '-') + '</td>'
      + '<td class="small">' + esc(r.label || '-') + '</td>'
      + '<td class="small text-danger">' + (-(r.cost_grant ?? 0)).toFixed(4) + '</td>'
      + '<td class="small text-danger">' + (-(r.cost_recharge ?? 0)).toFixed(4) + '</td>';
    return tr;
  }));
  uiPanel('资金明细 · ' + u.username, wrap);
}

$('#usr-add').onclick = guard(async () => {
  const name = $('#usr-name').value.trim();
  const pw = $('#usr-pw').value;
  if (!name || pw.length < 6) { toast('用户名必填，密码至少 6 位', 'danger'); return; }
  await api('users', {method: 'POST', json: {
    username: name, password: pw,
    balance: parseFloat($('#usr-bal').value) || 0,
    free_rpm: parseInt($('#usr-frpm').value) || 0, paid_rpm: 0,
  }});
  $('#usr-name').value = ''; $('#usr-pw').value = ''; $('#usr-bal').value = '0';
  toast('用户已添加'); loadUsers();
});

// ---------------- 大转盘 ----------------

const PRIZE_TYPES = [
  ['recharge', '充值余额'],
  ['grant', '赠金'],
  ['model_unlimited', '体验卡'],
  ['model_quota', '专属额度'],
  ['none', '谢谢参与'],
];
const PRIZE_COLORS = [
  ['gold', '金色'], ['blue', '蓝色'], ['orange', '橙色'],
  ['green', '绿色'], ['purple', '紫色'], ['gray', '灰色'],
];

function prizeRowHtml(p = {}) {
  const tr = document.createElement('tr');
  const typeOpts = PRIZE_TYPES.map(([v, t]) =>
    `<option value="${v}"${p.type === v ? ' selected' : ''}>${t}</option>`).join('');
  const colorOpts = PRIZE_COLORS.map(([v, t]) =>
    `<option value="${v}"${p.color === v ? ' selected' : ''}>${t}</option>`).join('');
  // 金额/模型共用一个输入框：余额类回显金额、Key 类回显模型名。
  // 不能用 amount ?? model —— 模型类奖品 amount 恒为 0（非 null），?? 短路回显成 0，
  // 再点保存模型名就被写成 "0"。
  const isAmt = p.type === 'recharge' || p.type === 'grant';
  const amVal = isAmt ? (p.amount ? String(p.amount) : '') : (p.model ?? '');
  tr.innerHTML = '<td><input class="form-control form-control-sm wz-label" value="' + esc(p.label || '') + '"></td>'
    + '<td><select class="form-select form-select-sm wz-type">' + typeOpts + '</select></td>'
    + '<td><select class="form-select form-select-sm wz-color">' + colorOpts + '</select></td>'
    + '<td><input type="number" step="0.01" min="0.01" class="form-control form-control-sm wz-weight" value="' + (p.weight ?? 25) + '"></td>'
    + '<td><input type="number" step="0.01" min="0" class="form-control form-control-sm wz-real" value="' + (p.real_weight ?? '') + '" placeholder="同展示"></td>'
    + '<td><input class="form-control form-control-sm wz-amount-model" value="' + esc(amVal) + '" placeholder="' + (isAmt ? '金额' : '模型名') + '"></td>'
    + '<td class="text-nowrap"><input type="number" step="0.1" min="0" class="form-control form-control-sm wz-hours d-inline-block" style="width:74px" value="' + (p.duration_hours ?? '') + '" placeholder="小时"> '
    + '<input type="number" min="1" max="64" class="form-control form-control-sm wz-conc d-inline-block" style="width:66px" value="' + (p.concurrency ?? '') + '" placeholder=" "> '
    + '<input type="number" step="1" min="0" class="form-control form-control-sm wz-quota d-inline-block" style="width:70px" value="' + (p.quota ?? '') + '" placeholder="次数"></td>'
    + '<td><button class="btn btn-sm btn-outline-danger wz-del">删</button></td>';
  tr._prize = p;
  tr.querySelector('.wz-del').onclick = () => { tr.remove(); wheelSum(); };
  tr.querySelector('.wz-weight').addEventListener('input', wheelSum);
  // 切换类型时按原始奖品数据切换回显（金额 ↔ 模型名），两个语义共用输入框不串值
  tr.querySelector('.wz-type').addEventListener('change', () => {
    const t = tr.querySelector('.wz-type').value;
    const inp = tr.querySelector('.wz-amount-model');
    const amt = t === 'recharge' || t === 'grant';
    const src = tr._prize || {};
    inp.value = amt ? (src.amount ? String(src.amount) : '') : (src.model ?? '');
    inp.placeholder = amt ? '金额' : '模型名';
  });
  return tr;
}

function wheelSum() {
  let sum = 0;
  for (const tr of document.querySelectorAll('#wh-prize-rows tr')) {
    sum += parseFloat(tr.querySelector('.wz-weight').value) || 0;
  }
  const el = $('#wh-sum');
  if (el) {
    el.textContent = Math.round(sum * 100) / 100;
    el.className = Math.abs(sum - 100) < 0.01 ? 'fw-semibold text-success' : 'fw-semibold text-danger';
  }
  return sum;
}

function openWheelEditor(w) {
  $('#wh-editor').style.display = 'block';
  $('#wh-id').value = w?.id || '';
  $('#wh-name').value = w?.name || '';
  $('#wh-cost').value = w?.cost ?? '0.01';
  $('#wh-enabled').checked = w ? !!w.enabled : true;
  const tb = $('#wh-prize-rows');
  tb.innerHTML = '';
  for (const p of (w?.prizes || [{label: '', type: 'grant', color: 'blue', weight: 100}])) tb.appendChild(prizeRowHtml(p));
  wheelSum();
  $('#wh-editor').scrollIntoView({behavior: 'smooth', block: 'nearest'});
}

function closeWheelEditor() {
  $('#wh-editor').style.display = 'none';
}

function collectWheel() {
  const prizes = [];
  for (const tr of document.querySelectorAll('#wh-prize-rows tr')) {
    const q = c => tr.querySelector(c);
    const amountModel = q('.wz-amount-model').value.trim();
    const type = q('.wz-type').value;
    const p = {
      label: q('.wz-label').value.trim(),
      type,
      color: q('.wz-color').value,
      weight: parseFloat(q('.wz-weight').value) || 0,
      amount: (type === 'recharge' || type === 'grant') ? (parseFloat(amountModel) || 0) : 0,
      model: (type === 'model_unlimited' || type === 'model_quota') ? amountModel : '',
      duration_hours: parseFloat(q('.wz-hours').value) || 0,
      concurrency: parseInt(q('.wz-conc').value) || 1,
      quota: parseFloat(q('.wz-quota').value) || 0,
    };
    if (q('.wz-real').value.trim() !== '') p.real_weight = parseFloat(q('.wz-real').value) || 0;
    // 保留原奖品 id：编辑保存不换 id，抽中结果定位与历史流水引用才稳定
    if (tr._prize && tr._prize.id) p.id = tr._prize.id;
    prizes.push(p);
  }
  return {
    id: $('#wh-id').value,
    name: $('#wh-name').value.trim(),
    cost: parseFloat($('#wh-cost').value) || 0,
    enabled: $('#wh-enabled').checked,
    prizes,
  };
}

async function loadWheels() {
  const d = await run(() => api('wheels'));
  if (!d) return;
  const rows = d.rows || [];
  const tb = $('#wh-rows');
  tb.innerHTML = '';
  if (!rows.length) {
    tb.innerHTML = '<tr><td colspan="5" class="text-muted small text-center py-4">还没有转盘，点右上「新建转盘」</td></tr>';
  }
  for (const w of rows) {
    const tr = document.createElement('tr');
    tr.innerHTML = '<td class="fw-semibold">' + esc(w.name) + '</td>'
      + '<td>' + (w.cost > 0 ? '¥' + w.cost : '免费') + '</td>'
      + '<td>' + (w.prizes?.length ?? 0) + '</td>'
      + '<td>' + (w.enabled ? '<span class="badge bg-success">启用</span>' : '<span class="badge bg-secondary">停用</span>') + '</td>'
      + '<td class="text-end text-nowrap"></td>';
    const td = tr.lastElementChild;
    const edit = document.createElement('button');
    edit.className = 'btn btn-sm btn-outline-secondary me-1'; edit.textContent = '编辑';
    edit.onclick = () => openWheelEditor(w);
    const del = document.createElement('button');
    del.className = 'btn btn-sm btn-outline-danger';
    del.textContent = '删除';
    del.onclick = () => run(async () => {
      if (!(await uiConfirm('删除转盘「' + w.name + '」？奖品 Key 不受影响', {danger: true}))) return;
      await api('wheels/delete', {method: 'POST', json: {id: w.id}});
      toast('已删除'); loadWheels();
    });
    td.append(edit, del);
    tb.appendChild(tr);
  }
  await loadPrizeKeys();
}

async function loadPrizeKeys() {
  const d = await run(() => api('prize-keys'));
  if (!d) return;
  const rows = d.rows || [];
  const tb = $('#pk-rows');
  tb.innerHTML = '';
  if (!rows.length) {
    tb.innerHTML = '<tr><td colspan="7" class="text-muted small text-center py-4">暂无奖品 Key</td></tr>';
    return;
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
      + '<td class="text-end text-nowrap"></td>';
    const td = tr.lastElementChild;
    const copy = document.createElement('button');
    copy.className = 'btn btn-sm btn-outline-secondary me-1'; copy.textContent = '复制';
    copy.onclick = () => copyText(k.key);
    const tog = document.createElement('button');
    tog.className = 'btn btn-sm btn-outline-secondary';
    tog.textContent = k.enabled ? '停用' : '启用';
    tog.onclick = () => run(async () => {
      await api('prize-keys', {method: 'POST', json: {id: k.id, op: k.enabled ? 'disable' : 'enable'}});
      loadPrizeKeys();
    });
    td.append(copy, tog);
    tb.appendChild(tr);
  }
}

$('#wh-new').onclick = () => openWheelEditor(null);
$('#wh-cancel').onclick = () => closeWheelEditor();
$('#wh-prize-add').onclick = () => { $('#wh-prize-rows').appendChild(prizeRowHtml()); wheelSum(); };
$('#wh-save').onclick = guard(async () => {
  const body = collectWheel();
  if (!body.name) return toast('填写转盘名称', 'danger');
  const sum = body.prizes.reduce((a, p) => a + (p.weight || 0), 0);
  if (Math.abs(sum - 100) > 0.01) return toast('展示权重合计必须为 100，当前 ' + Math.round(sum * 100) / 100, 'danger');
  await api('wheels', {method: 'POST', json: body});
  toast('转盘已保存');
  closeWheelEditor();
  loadWheels();
});
$('#pk-refresh').onclick = () => run(() => loadPrizeKeys());

})();
