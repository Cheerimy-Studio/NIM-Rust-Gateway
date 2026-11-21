
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

const parseCapacity = (value, label) => {
  const raw = String(value == null ? '' : value).trim().replace(/,/g, '').toLowerCase();
  if (!raw) return 0;
  const m = raw.match(/^(\d+(?:\.\d+)?)\s*([kmg]?)$/);
  if (!m) throw new Error((label || '容量') + ' 支持数字或 128k / 1M 格式');
  const unit = { '': 1, k: 1000, m: 1000000, g: 1000000000 }[m[2]];
  const n = Number(m[1]) * unit;
  if (!Number.isFinite(n) || n < 0 || n > 1000000000) throw new Error((label || '容量') + ' 超出范围');
  return Math.round(n);
};
const fmtCapacity = n => {
  n = Number(n) || 0;
  if (n <= 0) return '0';
  for (const [unit, size] of [['G', 1000000000], ['M', 1000000], ['k', 1000]]) {
    if (n >= size && n % size === 0) return String(n / size) + unit;
  }
  return String(n);
};

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
  if (!res.ok) {
    if (res.status === 401) { location.href = B + 'admin'; }
    throw new Error((data && data.error && data.error.message) || ('HTTP ' + res.status));
  }
  if (data === null) throw new Error('服务返回无效数据，请重试');
  return data;
}


const run = async fn => { try { return await fn(); } catch (e) { toast(e.message, 'danger'); } };

const guard = fn => async (...args) => { try { return await fn(...args); } catch (e) { toast(e.message, 'danger'); } };


function uiDialog(options) { return window.NGW_UI.dialog(options); }
const uiConfirm = (msg, opts = {}) => uiDialog({ title: opts.title || '确认操作', body: msg, danger: !!opts.danger, okText: opts.okText || '确定' });
const uiPrompt = (msg, val = '', opts = {}) => uiDialog({ title: opts.title || '输入', body: msg, input: { value: val, placeholder: opts.placeholder || '' }, okText: opts.okText || '确定' });


const LOADERS = {
  dash: () => loadOverview(),
  analytics: () => loadAnalytics(),
  keys: () => loadKeys(),
  upstreams: () => loadUpstreams(),
  import: () => loadUpstreams(),
  catalog: () => loadCatalog(),
  routing: () => loadRouting(),
  client01: () => loadClient01(),
  tokens: () => loadTokens(),
  intercept: () => loadIntercept(),
  logs: () => loadLogs(),
  queue: () => loadQueue(),
  training: () => loadTraining(),
  sessions: () => loadSessions(),
  settings: () => loadSettings(),
  docs: () => fillDocs(),
  users: loadUsers,
};
function activate(name) {
  if (name === 'auto') name = 'routing';
  if (!Object.hasOwn(LOADERS, name)) name = 'dash';
  if (location.hash !== '#' + name) history.replaceState(null, '', '#' + name);
  $$('.sidebar nav a').forEach(a => { a.classList.toggle('active', a.dataset.pane === name); if (a.dataset.pane === name) a.setAttribute('aria-current', 'page'); else a.removeAttribute('aria-current'); });
  $$('.pane').forEach(p => p.classList.toggle('active', p.id === 'pane-' + name));
  if (LOADERS[name]) run(LOADERS[name]);
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
      o.pool && o.pool.inflight >= o.pool.max_connections * 0.8 ? '连接使用率较高' : '连接容量充足',
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


// ---------------------------------------------------------------- 调用看板
let anDays = 7;
const AN_COLORS = ['#3b82f6', '#10b981', '#f59e0b', '#8b5cf6', '#ef4444', '#06b6d4', '#ec4899', '#84cc16'];

const anFmtNum = n => n >= 1e8 ? (n / 1e8).toFixed(1).replace(/\.0$/, '') + '亿'
  : n >= 1e4 ? (n / 1e4).toFixed(1).replace(/\.0$/, '') + '万' : String(n);
const anFmtCost = v => {
  if (!v) return '¥0';
  if (v >= 100) return '¥' + v.toFixed(2);
  if (v >= 1) return '¥' + v.toFixed(3).replace(/0+$/, '').replace(/\.$/, '');
  return '¥' + v.toFixed(4).replace(/0+$/, '').replace(/\.$/, '');
};
const anPct = (n, total) => {
  if (!total || !n) return '0%';
  const p = n / total * 100;
  if (p < 1) return '<1%';
  return (p >= 10 ? Math.round(p) : p.toFixed(1)) + '%';
};

function anDonut(box, rows, centerLabel, emptyText) {
  box.innerHTML = '';
  const total = rows.reduce((s, r) => s + r.count, 0);
  if (!total) { box.appendChild(el('div', 'an-empty', emptyText || '暂无调用')); return; }
  const sorted = rows.slice().sort((a, b) => b.count - a.count);
  let items = sorted;
  if (sorted.length > 6) {
    const rest = sorted.slice(6).reduce((s, r) => s + r.count, 0);
    items = sorted.slice(0, 6).concat([{name: '其他', count: rest}]);
  }
  const wrap = el('div', 'an-donut-wrap');
  const NS = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(NS, 'svg');
  svg.setAttribute('viewBox', '0 0 150 150');
  svg.classList.add('an-donut');
  const radius = 58, C = 2 * Math.PI * radius;
  const mk = (tag, attrs) => {
    const n = document.createElementNS(NS, tag);
    for (const [k, v] of Object.entries(attrs)) n.setAttribute(k, String(v));
    return n;
  };
  svg.appendChild(mk('circle', {cx: 75, cy: 75, r: radius, fill: 'none', stroke: '#eef2f8', 'stroke-width': 16}));
  let offset = 0;
  items.forEach((it, i) => {
    const len = it.count / total * C;
    const seg = mk('circle', {
      cx: 75, cy: 75, r: radius, fill: 'none', stroke: it.color || AN_COLORS[i % AN_COLORS.length],
      'stroke-width': 16, 'stroke-dasharray': `${Math.max(0, len - 1.5)} ${C - Math.max(0, len - 1.5)}`,
      'stroke-dashoffset': -offset, transform: 'rotate(-90 75 75)',
    });
    const title = document.createElementNS(NS, 'title');
    title.textContent = `${it.name}：${it.count} 次（${anPct(it.count, total)}）`;
    seg.appendChild(title);
    svg.appendChild(seg);
    offset += len;
  });
  const num = mk('text', {x: 75, y: 73, 'text-anchor': 'middle', 'font-size': 20, 'font-weight': 700, fill: '#1c2536'});
  num.textContent = anFmtNum(total);
  const label = mk('text', {x: 75, y: 90, 'text-anchor': 'middle', 'font-size': 10, fill: '#98a3b8'});
  label.textContent = centerLabel;
  svg.append(num, label);
  const legend = el('div', 'an-legend');
  items.forEach((it, i) => {
    const row = el('div', 'an-legend-row');
    const dot = el('span', 'an-dot');
    dot.style.background = it.color || AN_COLORS[i % AN_COLORS.length];
    const name = el('span', 'an-name', it.name);
    name.title = it.name;
    row.append(dot, name, el('span', 'an-count', String(it.count)), el('span', 'an-pct', anPct(it.count, total)));
    legend.appendChild(row);
  });
  wrap.append(svg, legend);
  box.appendChild(wrap);
}

function anTrend(rows) {
  const box = $('#an-trend');
  box.innerHTML = '';
  if (!rows.length || rows.every(r => !r.calls)) {
    box.appendChild(el('div', 'an-empty', '暂无调用'));
    $('#an-trend-sum').textContent = '';
    return;
  }
  const max = Math.max(1, ...rows.map(r => r.calls));
  for (const r of rows) {
    const col = el('div', 'an-col');
    col.title = `${r.day}：${r.calls} 次 · 成功 ${r.success} · 失败 ${r.fail} · ${anFmtCost(r.cost)}`;
    const bars = el('div', 'an-bars');
    if (r.fail > 0) {
      const bar = el('div', 'an-bar fail');
      bar.style.height = (r.fail / max * 100) + '%';
      bars.appendChild(bar);
    }
    if (r.success > 0) {
      const bar = el('div', 'an-bar ok');
      bar.style.height = (r.success / max * 100) + '%';
      bars.appendChild(bar);
    }
    col.append(bars, el('div', 'an-day', r.day));
    box.appendChild(col);
  }
  $('#an-trend-sum').textContent = `合计 ${rows.reduce((s, r) => s + r.calls, 0)} 次 · ${anFmtCost(rows.reduce((s, r) => s + r.cost, 0))}`;
}

async function loadAnalytics() {
  const d = await run(() => api('analytics?days=' + anDays));
  if (!d) return;
  const t = d.totals || {};
  const cards = [
    ['调用次数', anFmtNum(t.calls || 0), `成功 ${t.success || 0} / 失败 ${t.fail || 0}`, 'accent', 'bi-activity'],
    ['成功率', t.rate == null ? '—' : t.rate + '%', `计费 ${t.paid || 0} / 免费 ${t.free || 0}`, 'green', 'bi-graph-up'],
    ['Token 用量', anFmtNum((t.in_tok || 0) + (t.out_tok || 0)), `输入 ${anFmtNum(t.in_tok || 0)} · 输出 ${anFmtNum(t.out_tok || 0)}`, 'amber', 'bi-coin'],
    ['计费金额', anFmtCost(t.cost || 0), `缓存命中 ${anFmtNum(t.cached_tok || 0)} tokens`, 'accent', 'bi-wallet2'],
  ];
  const box = $('#an-cards');
  box.innerHTML = '';
  for (const [label, val, sub, tone, icon] of cards) {
    const col = el('div', 'col-6 col-xl-3');
    const stat = el('div', 'card stat h-100 ' + tone);
    const lbl = el('div', 'd-flex align-items-center gap-2 stat-lbl');
    lbl.append(el('i', 'bi ' + icon), el('span', 'lbl', label));
    stat.append(lbl, el('div', 'num', String(val)), el('div', 'sub', sub));
    col.appendChild(stat);
    box.appendChild(col);
  }
  anDonut($('#an-models'), (d.models || []).map(m => ({name: m.model, count: m.count})), '次调用', '暂无调用');
  anDonut($('#an-channels'), (d.channels || []).map(c => ({name: c.name || c.id, count: c.count})), '次调用', '暂无渠道调用');
  anDonut($('#an-paid'), [
    {name: '付费', count: t.paid || 0, color: AN_COLORS[0]},
    {name: '免费', count: t.free || 0, color: '#cbd5e1'},
  ], '次计费调用', '暂无计费调用');
  anTrend(d.trend || []);
  const tb = $('#an-model-rows');
  tb.innerHTML = '';
  const total = (d.models || []).reduce((s, m) => s + m.count, 0);
  if (!(d.models || []).length) {
    const tr = el('tr');
    const td = el('td', 'text-muted text-center', '暂无调用');
    td.colSpan = 4;
    tr.appendChild(td);
    tb.appendChild(tr);
  }
  for (const m of (d.models || []).slice(0, 30)) {
    const tr = el('tr');
    const name = el('td', 'text-truncate', m.model);
    name.style.maxWidth = '280px';
    name.title = m.model;
    const share = el('td');
    const wrap = el('div', 'an-share');
    const bar = el('div', 'an-share-bar');
    const fill = el('i');
    fill.style.width = total ? Math.max(1, m.count / total * 100) + '%' : '0';
    bar.appendChild(fill);
    wrap.append(bar, el('span', '', anPct(m.count, total)));
    share.appendChild(wrap);
    tr.append(name, el('td', 'text-end', String(m.count)), share, el('td', 'text-end', anFmtCost(m.cost || 0)));
    tb.appendChild(tr);
  }
}

function bindAnalytics() {
  $$('#an-range button').forEach(b => b.addEventListener('click', () => {
    anDays = parseInt(b.dataset.days, 10) || 7;
    $$('#an-range button').forEach(x => x.classList.toggle('active', x === b));
    loadAnalytics();
  }));
  const refresh = $('#an-refresh');
  if (refresh) refresh.onclick = loadAnalytics;
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
    td.colSpan = 15; tr.appendChild(td); tb.appendChild(tr);
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
      el('td', 'small', ({auto: '自动', 'chat/completions': 'Chat Completions', responses: 'Responses', messages: 'Messages', completions: 'Completions'})[u.api_endpoint || 'auto'] || '未识别'),
      (() => { const td = el('td'); td.appendChild(el('span', scoreCls, u.feasibility + '%'));
        td.title = '近 20 次成功率' + (u.recent ? `，样本 ${u.recent}` : '，暂无样本'); return td; })(),
      (() => { const td = el('td');
        td.innerHTML = (u.recent_flags && u.recent_flags.length)
          ? window.NGW_UI.heatSquares(u.recent_flags)
          : '<span class="text-muted small">—</span>';
        td.title = '最近 10 次请求（左旧右新）：绿=成功，黄=重试后成功，红=失败';
        return td; })(),
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
      $('#up-id').dataset.prev = u.id;
      $('#up-name').value = u.name;
      $('#up-base').value = u.base;
      $('#up-endpoint').value = u.api_endpoint || 'auto';
      $('#up-auth-mode').value = u.auth_mode || 'auto';
      $('#up-weight').value = u.weight;
      $('#up-rpm').value = u.rpm_cap;
      $('#up-daily').value = u.daily_cap;
      $('#up-models').value = (u.models || []).join('\n');
      $('#up-map').value = Object.entries(u.model_map || {}).map(([k, v]) => `${k}=${v}`).join('\n');
      $('#up-internal').value = (u.internal_models || []).join('\n');
      for (const [id, key] of UP_FIELDS) $('#' + id).value = u[key] || 0;
      $('#up-pua').checked = !!u.pass_client_ua;
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
  ['up-hourcap', 'hourly_request_cap'], ['up-fivehourcap', 'five_hour_request_cap'],
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
  const effort = $('#tdef-effort').value.trim().toLowerCase();
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

function resetUpstreamForm() {
    $('#up-id').value = '';
    delete $('#up-id').dataset.prev;
    $('#up-name').value = '';
    $('#up-base').value = '';
    $('#up-endpoint').value = 'auto';
    $('#up-auth-mode').value = 'auto';
    $('#up-weight').value = '10';
    $('#up-rpm').value = '0';
    $('#up-daily').value = '0';
    $('#up-models').value = '';
    $('#up-map').value = '';
    $('#up-internal').value = '';
    $('#up-pua').checked = false;
    $('#up-herr').value = '0';
    $('#up-hname').value = '0';
    tdefClear();
    poverClear();
    for (const [id] of UP_FIELDS) $('#' + id).value = '0';
    $('#up-enabled').checked = true;
    $('#up-form-title').textContent = '添加上游';
    $('#up-cancel').classList.add('d-none');
}

function upstreamPayload() {
  const payload = {
    id: $('#up-id').value.trim() || undefined,
    prev_id: $('#up-id').dataset.prev || undefined,
    name: $('#up-name').value.trim(),
    base: $('#up-base').value.trim(),
    api_endpoint: $('#up-endpoint').value,
    auth_mode: $('#up-auth-mode').value,
    weight: $('#up-weight').value,
    rpm_cap: $('#up-rpm').value,
    daily_cap: $('#up-daily').value,
    models: $('#up-models').value,
    model_map: $('#up-map').value,
    internal_models: $('#up-internal').value,
    pass_client_ua: $('#up-pua').checked,
    hide_errors: $('#up-herr').value,
    hide_mapped: $('#up-hname').value,
    thinking_defaults: tdefDump(),
    param_overrides: poverDump(),
    enabled: $('#up-enabled').checked,
  };
  for (const [id, key] of UP_FIELDS) payload[key] = $('#' + id).value;
  return payload;
}

async function saveUpstreamForm(reset = true) {
  const r = await api('upstreams', {method: 'POST', json: upstreamPayload()});
  if (r && r.upstream && r.upstream.id) {
    $('#up-id').value = r.upstream.id;
    $('#up-id').dataset.prev = r.upstream.id;
  }
  if (reset) {
    resetUpstreamForm();
    toast('已保存');
  }
  loadUpstreams(); refreshBatchUps(); loadPresets();
  return r;
}

function bindUpstreams() {
  $('#up-save').onclick = guard(async () => { await saveUpstreamForm(true); });
  const fetchBtn = $('#up-fetch');
  if (fetchBtn) fetchBtn.onclick = guard(async () => {
    const saved = await saveUpstreamForm(false);
    const id = saved && saved.upstream && saved.upstream.id;
    if (!id) throw new Error('保存后未取得渠道 ID');
    const fetched = await api('upstream-models/fetch', {method: 'POST', json: {channel_id: id}});
    toast('已拉取 ' + (fetched.count || '') + ' 个模型，可勾选、标注和测速');
    activate('catalog');
    const search = $('#cat-search');
    if (search) search.value = '';
    await loadCatalog();
    const sel = $('#cat-channel');
    if (sel && [...sel.options].some(o => o.value === id)) { sel.value = id; renderCatalog(); }
  });
  const probeBtn = $('#up-probe');
  if (probeBtn) probeBtn.onclick = guard(async () => {
    const base = $('#up-base').value.trim();
    if (!base) throw new Error('先填写 Base URL');
    probeBtn.disabled = true;
    probeBtn.textContent = '检测中…';
    try {
      const channel_id = $('#up-id').dataset.prev || $('#up-id').value.trim();
      const d = await api('upstreams/probe', {method: 'POST', json: {base, channel_id}});
      if (d.ok && d.base) {
        $('#up-base').value = d.base;
        toast('已找到可用接口：' + d.base);
      } else if (d.base) {
        $('#up-base').value = d.base;
        toast('接口路径存在，但返回 HTTP ' + d.status + '；请检查账号或鉴权', 'warning');
      } else {
        const detail = (d.tried || []).map(x => x.base + ' → ' + (x.status || '不可达')).join('；');
        throw new Error('没有找到可用接口：' + detail);
      }
    } finally {
      probeBtn.disabled = false;
      probeBtn.textContent = '自动检测';
    }
  });
  $('#up-cancel').onclick = resetUpstreamForm;

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
      if (p.api_endpoint) $('#up-endpoint').value = p.api_endpoint;
      if (p.auth_mode) $('#up-auth-mode').value = p.auth_mode;
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
  ['set-origin', 'public_origin'], ['set-trusted', 'trusted_proxies'],
  ['set-allowed-origins', 'allowed_origins'],
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

// ---------------- 模型路由（链路）编辑器 ----------------
let CHAIN_CANDS = []; // [{name, routable, chain}]，来自 /api/routable-models

async function chainLoadCandidates() {
  try {
    const d = await api('routable-models');
    CHAIN_CANDS = (d.models || []).filter(m => m && m.name);
  } catch (e) { CHAIN_CANDS = []; }
  const dl = $('#chain-cands');
  if (!dl) return;
  dl.textContent = '';
  for (const m of CHAIN_CANDS) {
    const o = document.createElement('option');
    o.value = m.name;
    dl.appendChild(o);
  }
}

function splitNames(text) {
  return String(text || '').split(/[,\n，]/).map(x => x.trim()).filter(Boolean);
}

function openModelPicker(title, options, initial, onSave, opts = {}) {
  const single = !!opts.single;
  const picked = [];
  for (const name of initial || []) {
    if (name && !picked.includes(name)) picked.push(name);
  }
  const all = [];
  const seen = new Set();
  for (const item of options || []) {
    if (!item || !item.name || seen.has(item.name)) continue;
    seen.add(item.name);
    all.push(item);
  }
  for (const name of picked) {
    if (!seen.has(name)) {
      seen.add(name);
      all.unshift({name, routable: true});
    }
  }
  const mask = el('div', 'ui-mask');
  const box = el('div', 'ui-dialog mp-dialog');
  const head = el('div', 'ui-title-row');
  head.append(el('div', 't', title));
  const close = el('button', 'ui-close', '×');
  close.type = 'button';
  head.appendChild(close);
  const body = el('div', 'mp-body');
  const search = document.createElement('input');
  search.className = 'form-control form-control-sm';
  search.placeholder = '搜索模型 / 路由';
  const list = el('div', 'mp-list');
  const foot = el('div', 'mp-foot');
  const count = el('span', 'mp-count');
  const cancel = el('button', 'btn btn-sm btn-outline-secondary', '取消');
  const save = el('button', 'btn btn-sm btn-primary', single ? '选择' : '确定');
  cancel.type = save.type = 'button';
  foot.append(count, cancel, save);
  body.append(search, list, foot);
  box.append(head, body);
  mask.appendChild(box);
  document.body.appendChild(mask);
  let closed = false;
  const closeAll = () => {
    if (closed) return;
    closed = true;
    mask.remove();
    document.removeEventListener('keydown', onKey, true);
  };
  const onKey = e => {
    if (e.key === 'Escape') { e.preventDefault(); closeAll(); }
  };
  document.addEventListener('keydown', onKey, true);
  close.onclick = cancel.onclick = closeAll;
  mask.onclick = e => { if (e.target === mask) closeAll(); };
  const render = () => {
    const q = search.value.trim().toLowerCase();
    list.innerHTML = '';
    const items = all.filter(item => !q || item.name.toLowerCase().includes(q));
    if (!items.length) {
      list.appendChild(el('div', 'text-muted small text-center py-4', '没有匹配的模型'));
    }
    for (const item of items) {
      const index = picked.indexOf(item.name);
      const row = el('button', 'mp-row' + (index >= 0 ? ' on' : ''));
      row.type = 'button';
      row.appendChild(el('span', 'mp-check', index >= 0 ? String(index + 1) : ''));
      row.appendChild(el('span', 'mp-name', item.name));
      const badges = el('span', 'mp-badges');
      if (item.chain) badges.appendChild(el('span', 'mp-badge chain', '路由'));
      if (item.routable === false) badges.appendChild(el('span', 'mp-badge warn', '不可路由'));
      row.appendChild(badges);
      row.onclick = () => {
        if (single) {
          picked.length = 0;
          picked.push(item.name);
        } else {
          const at = picked.indexOf(item.name);
          if (at >= 0) picked.splice(at, 1);
          else picked.push(item.name);
        }
        render();
      };
      list.appendChild(row);
    }
    count.textContent = picked.length ? '已选 ' + picked.length + ' 个' : '未选择';
    save.disabled = picked.length === 0;
  };
  search.oninput = render;
  save.onclick = () => {
    if (!picked.length) return;
    onSave(single ? picked.slice(0, 1) : picked.slice());
    closeAll();
  };
  render();
  search.focus();
}

function chainAddRow(v) {
  v = v || {};
  const tb = $('#chain-rows');
  if (!tb) return;
  const tr = document.createElement('tr');
  tr.innerHTML =
    '<td><input class="form-control form-control-sm font-monospace chain-name" list="chain-cands" placeholder="如 auto-best"></td>' +
    '<td><div class="d-flex gap-1"><input class="form-control form-control-sm font-monospace chain-members flex-grow-1" list="chain-cands" placeholder="model-a, model-b"><button class="btn btn-sm btn-outline-secondary chain-pick" type="button" title="从模型列表选择">选择</button></div></td>' +
    '<td><select class="form-select form-select-sm chain-mode">' +
    '<option value="fb">故障回退</option><option value="rr">轮询分配</option></select></td>' +
    '<td><input type="number" min="1" max="100" class="form-control form-control-sm chain-sticky" value="3" disabled title="同一起点保持 N 轮"></td>' +
    '<td><button class="btn btn-sm btn-outline-danger chain-del" type="button" title="删除此路由">×</button></td>';
  tr.querySelector('.chain-name').value = v.name || '';
  tr.querySelector('.chain-members').value = (v.members || []).join(', ');
  tr.querySelector('.chain-mode').value = v.rr ? 'rr' : 'fb';
  tr.querySelector('.chain-pick').onclick = () => openModelPicker(
    '选择路由成员',
    CHAIN_CANDS.filter(item => !item.chain),
    splitNames(tr.querySelector('.chain-members').value),
    names => {
      const input = tr.querySelector('.chain-members');
      input.value = names.join(', ');
      input.dispatchEvent(new Event('input', {bubbles: true}));
    }
  );
  const sticky = tr.querySelector('.chain-sticky');
  sticky.value = v.rr ? (v.sticky || 3) : 3;
  sticky.disabled = !v.rr;
  tr.querySelector('.chain-mode').onchange = e => {
    sticky.disabled = e.target.value !== 'rr';
    chainMapRender();
  };
  tr.querySelector('.chain-del').onclick = () => { tr.remove(); chainMapRender(); renderRouteStats(); };
  tb.appendChild(tr);
}

// 解析单行 → 行数据（与服务端 parse_model_chains 同语法）
function chainParseLine(line) {
  line = line.trim();
  if (!line || line.startsWith('#')) return null;
  const eq = line.indexOf('=');
  if (eq < 0) return null;
  const name = line.slice(0, eq).trim();
  let rest = line.slice(eq + 1).trim();
  let rr = false, sticky = 3;
  const bar = rest.lastIndexOf('|');
  if (bar >= 0) {
    const flag = rest.slice(bar + 1).trim().toLowerCase();
    const head = rest.slice(0, bar).trim();
    if (flag === 'fb' || flag === 'fallback') { rest = head; rr = false; }
    else if (flag.startsWith('rr')) {
      rest = head; rr = true;
      const n = parseInt(flag.slice(2), 10);
      if (Number.isFinite(n)) sticky = Math.min(100, Math.max(1, n));
    }
  }
  const members = rest.split(',').map(s => s.trim()).filter(Boolean);
  return { name, members, rr, sticky };
}

function chainLoad(text) {
  const tb = $('#chain-rows');
  if (!tb) return;
  tb.textContent = '';
  (text || '').split('\n').forEach(line => {
    const row = chainParseLine(line);
    if (row && row.members.length >= 2) chainAddRow(row);
  });
}

// 收集 + 校验，返回序列化文本；有硬伤抛 Error（guard 统一 toast），可疑项落到 #chain-err
// 收集 + 校验（纯函数）：错误=必须改才能保存；警告=能存但链可能不对外展示
// （成员没接入时，链会按口径从 /v1/models 与模型广场消失）
function chainCollect() {
  const warns = [], errors = [];
  const rows = [];
  for (const tr of $('#chain-rows').querySelectorAll('tr')) {
    const name = tr.querySelector('.chain-name').value.trim();
    const raw = tr.querySelector('.chain-members').value;
    const rr = tr.querySelector('.chain-mode').value === 'rr';
    const sticky = Math.min(100, Math.max(1, parseInt(tr.querySelector('.chain-sticky').value, 10) || 3));
    if (!name && !raw.trim()) continue; // 全空行 = 删除
    const members = [...new Set(raw.split(',').map(s => s.trim()).filter(Boolean))];
    rows.push({ name, members, rr, sticky });
  }
  const chainNames = new Set(rows.map(r => r.name));
  for (const m of CHAIN_CANDS) if (m.chain) chainNames.add(m.name);
  const autoNames = new Set(AUTO_ROUTES.map(r => r.name.toLowerCase()));
  const seenNames = new Set();
  for (const r of rows) {
    const tag = r.name || '(空路由名)';
    if (!r.name) { errors.push('存在没有路由名的路由'); continue; }
    if (r.name.length > 80) errors.push(`${tag}：路由名过长（>80 字符）`);
    if (/[=|#]/.test(r.name)) errors.push(`${tag}：路由名不能包含 = | # 字符`);
    if (seenNames.has(r.name)) errors.push(`${tag}：路由名重复`);
    if (autoNames.has(r.name.toLowerCase())) errors.push(`${tag}：与 Auto 路由重名`);
    seenNames.add(r.name);
    const shadow = CHAIN_CANDS.find(m => m.name === r.name && !m.chain);
    if (shadow) errors.push(`${tag}：与现有模型/别名重名，会遮蔽原模型`);
    if (r.members.length < 2) { errors.push(`${tag}：至少需要 2 个成员`); continue; }
    for (const mem of r.members) {
      if (mem.length > 160) { errors.push(`${tag}：成员名过长（${mem.slice(0, 40)}…）`); continue; }
      if (/[=|#]/.test(mem)) { errors.push(`${tag}：成员不能包含 = | # 字符`); continue; }
      if (chainNames.has(mem)) { errors.push(`${tag}：成员 ${mem} 是链名，不支持链嵌套`); continue; }
      const cand = CHAIN_CANDS.find(m => m.name === mem);
      if (cand && cand.routable === false) warns.push(`${tag} 的成员 ${mem} 当前不可路由（隐藏原名或未列入渠道模型），请求到它会跳过`);
    }
  }
  return { rows, errors, warns };
}

function chainDump() {
  const errBox = $('#chain-err');
  const { rows, errors, warns } = chainCollect();
  if (errBox) errBox.textContent = warns.join('；');
  if (errors.length) throw new Error('模型路由：' + errors[0] + (errors.length > 1 ? `（共 ${errors.length} 处问题）` : ''));
  return rows.map(r => {
    const suffix = r.rr ? (r.sticky === 3 ? ' | rr' : ` | rr${r.sticky}`) : '';
    return `${r.name} = ${r.members.join(', ')}${suffix}`;
  }).join('\n');
}

// 只读校验：编辑与加载时把问题摆到明面——尤其是「成员没接入导致链不对外」
let chainValT = null;
function chainValidateDisplay() {
  const errBox = $('#chain-err');
  if (!errBox || !$('#chain-rows')) return;
  if (!$('#chain-rows').querySelector('.chain-name')) { errBox.textContent = ''; return; }
  const { errors, warns } = chainCollect();
  errBox.textContent = errors.concat(warns).join('；');
}

// ---------------- 模型路由：链路 + 多个 Auto ----------------
let AUTO_ROUTES = [];

function autoParse(raw) {
  let list = [];
  try { list = JSON.parse(raw || '[]'); } catch (e) { list = []; }
  if (!Array.isArray(list)) list = [];
  return list.filter(r => r && typeof r === 'object').slice(0, 20).map(r => ({
    name: String(r.name || '').trim(),
    enabled: r.enabled !== false,
    price: r.price == null ? 0 : r.price,
    rules: Array.isArray(r.rules) ? r.rules.filter(x => x && typeof x === 'object').map(x => ({
      name: String(x.name || '').trim(),
      enabled: x.enabled !== false,
      image: ['any', 'yes', 'no'].includes(x.image) ? x.image : 'any',
      keywords: Array.isArray(x.keywords) ? x.keywords.map(k => String(k).trim()).filter(Boolean).slice(0, 30) : [],
      target: String(x.target || '').trim(),
    })) : [],
  }));
}

function chainRowsSafe() {
  try { return chainCollect().rows; } catch (e) { return []; }
}

function autoMapHtml(route) {
  const rules = (route.rules || []).map((r, ri) => ({ r, ri })).filter(x => x.r.enabled !== false);
  if (!rules.length) return '<span class="auto-empty">还没有启用的规则，这个 Auto 无法选择分支</span>';
  const chains = chainRowsSafe();
  return '<div class="auto-root">' + esc(route.name || 'Auto') + '</div><div class="auto-branches">' + rules.map(({ r, ri }) => {
    const cond = [];
    if (r.image === 'yes') cond.push('有图片');
    else if (r.image === 'no') cond.push('无图片');
    if (r.keywords && r.keywords.length) cond.push('含「' + r.keywords.slice(0, 2).join(' / ') + '」');
    const chain = chains.find(c => c.name === r.target);
    return '<div class="auto-branch" data-rule="' + ri + '"><span class="auto-cond">' +
      esc(cond.join(' · ') || '默认') + '</span><span class="auto-arrow">→</span><span class="auto-target">' +
      esc(r.target || '未设置') + '</span>' + (chain ? '<span class="chain-mode">链路</span>' : '') + '</div>';
  }).join('') + '</div>';
}

function bindAutoMap(card, route) {
  const box = card.querySelector('.auto-map');
  if (!box) return;
  box.innerHTML = autoMapHtml(route);
  box.querySelectorAll('.auto-branch').forEach(el => {
    el.onclick = () => {
      const tr = card.querySelector('.auto-rule-rows tr[data-rule="' + el.dataset.rule + '"]');
      if (tr) tr.scrollIntoView({ block: 'center' });
    };
  });
}

function chainMapRender() {
  const box = $('#chain-map');
  if (!box) return;
  const rows = chainRowsSafe().filter(r => r.name && r.members.length >= 2);
  if (!rows.length) {
    box.innerHTML = '<div class="auto-empty">还没有路由链路。保存后这里会显示成员顺序。</div>';
    renderRouteStats();
    return;
  }
  box.innerHTML = rows.map(r =>
    '<div class="chain-card"><div class="chain-card-head"><span>' + esc(r.name) + '</span><span class="chain-mode">' +
    (r.rr ? '轮询 ' + (r.sticky || 3) : '故障回退') + '</span></div><div class="chain-flow">' +
    r.members.map((m, i) => '<span class="chain-node">' + esc(m) + '</span>' +
      (i < r.members.length - 1 ? '<span class="chain-arrow">→</span>' : '')).join('') + '</div></div>').join('');
  renderRouteStats();
}

function renderRouteStats() {
  const box = $('#route-stats');
  if (!box) return;
  const chains = chainRowsSafe().filter(r => r.name && r.members.length >= 2).length;
  const autos = AUTO_ROUTES.filter(r => r.enabled).length;
  const branches = AUTO_ROUTES.filter(r => r.enabled)
    .reduce((sum, r) => sum + (r.rules || []).filter(x => x.enabled !== false).length, 0);
  box.innerHTML = [['路由链路', chains], ['Auto 模型', autos], ['启用分支', branches]]
    .map(([label, value]) => '<div class="route-stat"><span>' + label + '</span><b>' + value + '</b></div>').join('');
}

function autoSyncCard(card, route) {
  const name = card.querySelector('.auto-route-name');
  if (name) route.name = name.value.trim();
  const on = card.querySelector('.auto-route-on');
  if (on) route.enabled = on.checked;
  try { route.price = readPrice(card.querySelector('.auto-price')); } catch (e) { /* 保存时再报 */ }
}

function autoSyncAll() {
  document.querySelectorAll('#auto-route-list .auto-route-card').forEach((card, idx) => {
    if (AUTO_ROUTES[idx]) autoSyncCard(card, AUTO_ROUTES[idx]);
  });
}

function autoRuleRow(route, ri) {
  const r = route.rules[ri];
  const tr = document.createElement('tr');
  tr.dataset.rule = ri;
  tr.innerHTML =
    '<td><input type="checkbox" class="form-check-input auto-rule-on"></td>' +
    '<td><input class="form-control form-control-sm auto-rule-name" placeholder="如图片请求"></td>' +
    '<td><select class="form-select form-select-sm auto-rule-image"><option value="any">任意</option><option value="yes">有图片</option><option value="no">无图片</option></select></td>' +
    '<td><input class="form-control form-control-sm auto-rule-keywords" placeholder="画图, 截图（逗号分隔）"></td>' +
    '<td><div class="d-flex gap-1"><input class="form-control form-control-sm font-monospace auto-rule-target flex-grow-1" list="chain-cands" placeholder="目标模型/链路"><button class="btn btn-sm btn-outline-secondary auto-rule-pick" type="button" title="从模型/路由列表选择">选择</button></div></td>' +
    '<td class="text-nowrap"><button class="btn btn-sm btn-outline-secondary auto-rule-up" type="button" title="上移">↑</button> ' +
    '<button class="btn btn-sm btn-outline-secondary auto-rule-down" type="button" title="下移">↓</button> ' +
    '<button class="btn btn-sm btn-outline-danger auto-rule-del" type="button" title="删除">×</button></td>';
  const on = tr.querySelector('.auto-rule-on');
  const name = tr.querySelector('.auto-rule-name');
  const image = tr.querySelector('.auto-rule-image');
  const keywords = tr.querySelector('.auto-rule-keywords');
  const target = tr.querySelector('.auto-rule-target');
  on.checked = r.enabled !== false;
  name.value = r.name || '';
  image.value = r.image || 'any';
  keywords.value = (r.keywords || []).join(', ');
  target.value = r.target || '';
  const refresh = () => bindAutoMap(tr.closest('.auto-route-card'), route);
  on.onchange = () => { r.enabled = on.checked; refresh(); renderRouteStats(); };
  name.oninput = () => { r.name = name.value.trim(); refresh(); };
  image.onchange = () => { r.image = image.value; refresh(); };
  keywords.oninput = () => {
    r.keywords = keywords.value.split(/[,，]/).map(x => x.trim()).filter(Boolean).slice(0, 30);
    refresh();
  };
  target.oninput = () => { r.target = target.value.trim(); refresh(); };
  tr.querySelector('.auto-rule-pick').onclick = () => openModelPicker(
    '选择目标',
    CHAIN_CANDS,
    splitNames(target.value),
    names => {
      target.value = names[0] || '';
      target.dispatchEvent(new Event('input', {bubbles: true}));
    },
    {single: true}
  );
  const card = tr.closest('.auto-route-card');
  tr.querySelector('.auto-rule-up').onclick = () => {
    autoSyncCard(card, route);
    if (ri > 0) { [route.rules[ri - 1], route.rules[ri]] = [route.rules[ri], route.rules[ri - 1]]; autoRender(); }
  };
  tr.querySelector('.auto-rule-down').onclick = () => {
    autoSyncCard(card, route);
    if (ri < route.rules.length - 1) { [route.rules[ri + 1], route.rules[ri]] = [route.rules[ri], route.rules[ri + 1]]; autoRender(); }
  };
  tr.querySelector('.auto-rule-del').onclick = () => {
    autoSyncCard(card, route);
    route.rules.splice(ri, 1);
    autoRender();
  };
  return tr;
}

function autoCard(route, idx) {
  const card = el('div', 'auto-route-card');
  card.dataset.index = idx;
  const head = el('div', 'auto-route-head');
  const name = document.createElement('input');
  name.className = 'form-control form-control-sm font-monospace auto-route-name';
  name.placeholder = 'Auto 模型名，如 auto、auto-vision';
  name.value = route.name || '';
  const toggle = el('div', 'form-check form-switch');
  const on = document.createElement('input');
  on.type = 'checkbox'; on.className = 'form-check-input auto-route-on'; on.checked = route.enabled !== false;
  on.id = 'auto-on-' + idx;
  const label = el('label', 'form-check-label small', '启用');
  label.htmlFor = on.id;
  toggle.append(on, label);
  const del = el('button', 'btn btn-sm btn-outline-danger auto-route-del', '删除');
  del.type = 'button';
  head.append(name, toggle, del);
  const body = el('div', 'auto-route-body');
  const priceWrap = el('div', 'mb-2');
  priceWrap.appendChild(el('div', 'side-label mb-1', '计费'));
  const price = el('div', 'auto-price');
  price.innerHTML = priceFields(route.price == null ? 0 : route.price);
  bindPriceFields(price);
  priceWrap.appendChild(price);
  const map = el('div', 'auto-map');
  const ruleBox = el('div', 'table-responsive');
  const table = document.createElement('table');
  table.className = 'table table-sm align-middle mb-2';
  table.innerHTML = '<thead><tr><th style="width:48px">启用</th><th style="width:150px">条件名称</th>' +
    '<th style="width:104px">图片</th><th>关键词（任一命中）</th><th style="width:220px">目标模型/链路</th><th style="width:92px"></th></tr></thead>';
  const tbody = el('tbody', 'auto-rule-rows');
  table.appendChild(tbody);
  ruleBox.appendChild(table);
  const addRule = el('button', 'btn btn-sm btn-outline-primary auto-rule-add', '加一条规则');
  addRule.type = 'button';
  const test = el('div', 'rule-test mt-2');
  test.innerHTML = '<div class="side-label mb-2">分支测试</div><div class="d-flex gap-2 flex-wrap align-items-center">' +
    '<input class="form-control form-control-sm auto-test-text" style="width:260px" placeholder="示例文本，如 帮我画图">' +
    '<label class="form-check m-0"><input class="form-check-input auto-test-image" type="checkbox"><span class="form-check-label small ms-1">带图片</span></label>' +
    '<button class="btn btn-sm btn-outline-secondary auto-test-run" type="button">测试</button></div>' +
    '<div class="small mt-2 auto-test-result"></div>';
  body.append(priceWrap, map, ruleBox, addRule, test);
  card.append(head, body);

  name.oninput = () => { route.name = name.value.trim(); bindAutoMap(card, route); renderRouteStats(); };
  on.onchange = () => { route.enabled = on.checked; card.style.opacity = on.checked ? '' : '.62'; renderRouteStats(); };
  card.style.opacity = route.enabled === false ? '.62' : '';
  del.onclick = () => { autoSyncAll(); AUTO_ROUTES.splice(idx, 1); autoRender(); };
  addRule.onclick = () => {
    autoSyncCard(card, route);
    route.rules.push({ name: '', enabled: true, image: 'any', keywords: [], target: '' });
    const ri = route.rules.length - 1;
    tbody.appendChild(autoRuleRow(route, ri));
    bindAutoMap(card, route);
    renderRouteStats();
  };
  (route.rules || []).forEach((_, ri) => tbody.appendChild(autoRuleRow(route, ri)));
  bindAutoMap(card, route);
  test.querySelector('.auto-test-run').onclick = () => {
    const text = test.querySelector('.auto-test-text').value.toLowerCase();
    const image = test.querySelector('.auto-test-image').checked;
    const out = test.querySelector('.auto-test-result');
    for (const r of route.rules) {
      if (r.enabled === false) continue;
      if (r.image === 'yes' && !image) continue;
      if (r.image === 'no' && image) continue;
      if (r.keywords.length && !r.keywords.some(k => text.includes(k.toLowerCase()))) continue;
      out.className = 'small mt-2 text-success auto-test-result';
      out.textContent = '命中「' + (r.name || '未命名') + '」→ ' + (r.target || '未设置');
      return;
    }
    out.className = 'small mt-2 text-muted auto-test-result';
    out.textContent = '没有命中分支';
  };
  return card;
}

function autoRender() {
  const box = $('#auto-route-list');
  if (!box) return;
  box.innerHTML = '';
  if (!AUTO_ROUTES.length) {
    box.innerHTML = '<div class="auto-empty">还没有 Auto 路由。新增一个后，它会作为一个独立模型出现在模型列表和模型广场。</div>';
    renderRouteStats();
    return;
  }
  AUTO_ROUTES.forEach((route, idx) => box.appendChild(autoCard(route, idx)));
  renderRouteStats();
}

function autoCollect() {
  const routes = [];
  const names = new Set();
  const allNames = new Set();
  document.querySelectorAll('#auto-route-list .auto-route-card').forEach((card, idx) => {
    const name = card.querySelector('.auto-route-name').value.trim();
    const tag = name || ('第 ' + (idx + 1) + ' 个 Auto');
    if (!/^[A-Za-z0-9._-]{1,64}$/.test(name)) throw new Error(tag + '：名称只能含字母、数字、点、下划线、连字符，最长 64');
    if (names.has(name.toLowerCase())) throw new Error(tag + '：名称重复');
    names.add(name.toLowerCase());
    const cand = CHAIN_CANDS.find(m => m.name.toLowerCase() === name.toLowerCase());
    if (cand || chainRowsSafe().some(r => r.name.toLowerCase() === name.toLowerCase())) {
      throw new Error(tag + '：与现有模型/链路重名');
    }
    const rules = [];
    card.querySelectorAll('.auto-rule-rows tr').forEach((tr, ri) => {
      const target = tr.querySelector('.auto-rule-target').value.trim();
      const rname = tr.querySelector('.auto-rule-name').value.trim();
      const keywords = tr.querySelector('.auto-rule-keywords').value.split(/[,，]/).map(x => x.trim()).filter(Boolean);
      if (!target && !rname && !keywords.length) return;
      if (!target) throw new Error(tag + ' 第 ' + (ri + 1) + ' 条规则缺少目标');
      if (keywords.length > 30) throw new Error(tag + ' 第 ' + (ri + 1) + ' 条规则最多 30 个关键词');
      rules.push({
        name: rname || ('规则 ' + (rules.length + 1)),
        enabled: tr.querySelector('.auto-rule-on').checked,
        image: tr.querySelector('.auto-rule-image').value,
        keywords,
        target,
      });
    });
    routes.push({
      name,
      enabled: card.querySelector('.auto-route-on').checked,
      price: readPrice(card.querySelector('.auto-price')),
      rules,
    });
  });
  routes.forEach(r => allNames.add(r.name.toLowerCase()));
  for (const r of routes) {
    if (r.enabled && !r.rules.some(x => x.enabled)) throw new Error(r.name + '：启用后至少需要一条启用的规则');
    for (const rule of r.rules) {
      if (allNames.has(rule.target.toLowerCase())) throw new Error(r.name + '：目标不能是另一个 Auto 路由');
    }
  }
  return routes;
}

async function autoSaveAll() {
  const err = $('#auto-err');
  if (err) err.textContent = '';
  try {
    const routes = autoCollect();
    await api('settings', { method: 'POST', json: { config: { auto_routes: JSON.stringify(routes) } } });
    AUTO_ROUTES = routes;
    autoRender();
    chainMapRender();
    toast('Auto 路由已保存');
  } catch (e) {
    if (err) err.textContent = e.message;
    throw e;
  }
}

function autoAdd() {
  autoSyncAll();
  let n = AUTO_ROUTES.length + 1;
  let name = n === 1 ? 'auto' : 'auto-' + n;
  while (AUTO_ROUTES.some(r => r.name.toLowerCase() === name.toLowerCase())) {
    n += 1;
    name = 'auto-' + n;
  }
  AUTO_ROUTES.push({
    name,
    enabled: true,
    price: 0,
    rules: [{ name: '默认', enabled: true, image: 'any', keywords: [], target: '' }],
  });
  autoRender();
}

async function loadRouting() {
  await chainLoadCandidates();
  const c = await run(() => api('settings'));
  if (!c) return;
  chainLoad(c.model_chains || '');
  AUTO_ROUTES = autoParse(c.auto_routes || '[]');
  autoRender();
  chainMapRender();
}

function bindRouting() {
  const add = $('#chain-add');
  if (add) add.onclick = () => { chainAddRow({}); chainMapRender(); };
  const rows = $('#chain-rows');
  if (rows) rows.addEventListener('input', () => {
    clearTimeout(chainValT);
    chainValT = setTimeout(() => { chainValidateDisplay(); chainMapRender(); renderRouteStats(); }, 350);
  });
  const save = $('#chain-save');
  if (save) save.onclick = guard(async () => {
    const text = chainDump();
    await api('settings', { method: 'POST', json: { config: { model_chains: text } } });
    toast('路由已保存');
    await chainLoadCandidates();
    chainMapRender();
    autoRender();
  });
  const autoAddBtn = $('#auto-add');
  if (autoAddBtn) autoAddBtn.onclick = () => autoAdd();
  const autoSave = $('#auto-save-all');
  if (autoSave) autoSave.onclick = guard(autoSaveAll);
}

// ---------------- 模型接入（上游模型 / 别名 / 上限） ----------------
let CAT_ROWS = [];

async function loadCatalog() {
  const d = await run(() => api('upstream-models'));
  if (!d) return;
  CAT_ROWS = d.rows || [];
  const sel = $('#cat-channel');
  const chans = [...new Set(CAT_ROWS.map(r => r.channel_id))];
  const prev = sel.value;
  sel.innerHTML = '<option value="">全部渠道</option>' + chans.map(id => {
    const r = CAT_ROWS.find(x => x.channel_id === id);
    return '<option value="' + esc(id) + '">' + esc(r.channel_name || id) + '</option>';
  }).join('');
  if ([...sel.options].some(o => o.value === prev)) sel.value = prev;
  renderCatalog();
  chainLoadCandidates();
  // 定价接口失败不能拖垮接入表/定价区的加载
  await run(() => loadPrices());
}

function openModelTest(row, inlineOut, inlineBtn) {
  const wrap = el('div');
  const info = el('div', 'model-test-grid');
  const cell = (k, v) => {
    const box = el('div', 'model-test-cell');
    box.append(el('span', 'k', k), el('div', 'v', v || '—'));
    return box;
  };
  info.append(cell('渠道', row.channel_name || row.channel_id), cell('上游模型', row.upstream),
    cell('调用名称', row.alias || '未接入'), cell('说明', '使用已保存的渠道配置与账号'));
  const run = el('button', 'btn btn-sm btn-primary mt-2', '开始测试');
  run.type = 'button';
  const result = el('div', 'mt-2');
  wrap.append(info, run, result);
  const render = d => {
    result.innerHTML = '';
    if (!d) {
      result.appendChild(el('div', 'text-muted small', '等待测试…'));
      return;
    }
    const grid = el('div', 'model-test-grid');
    const status = el('div', 'model-test-cell');
    status.append(el('span', 'k', '结果'));
    status.appendChild(el('div', 'v ' + (d.ok ? 'model-test-ok' : 'model-test-bad'), d.ok ? '可用' : '失败'));
    const http = el('div', 'model-test-cell');
    http.append(el('span', 'k', 'HTTP / 耗时'));
    http.appendChild(el('div', 'v', String(d.status || 0) + ' · ' + (d.ms || 0) + 'ms'));
    grid.append(status, http);
    result.appendChild(grid);
    if (d.error) result.appendChild(el('div', 'model-test-err', d.error));
  };
  const runTest = async () => {
    run.disabled = true;
    run.textContent = '测试中…';
    render(null);
    if (inlineBtn) inlineBtn.disabled = true;
    if (inlineOut) {
      inlineOut.className = 'cat-test-result small text-muted';
      inlineOut.textContent = '测试中…';
    }
    try {
      const d = await api('upstream-models/test', {method: 'POST', json: {
        channel_id: row.channel_id, model: row.upstream,
      }});
      render(d);
      if (inlineOut) {
        inlineOut.className = 'cat-test-result small ' + (d.ok ? 'text-success' : 'text-danger');
        inlineOut.textContent = d.ok ? (d.ms + 'ms') : '失败';
        inlineOut.title = d.ok ? ('HTTP ' + d.status + ' · ' + d.ms + 'ms') : (d.error || ('HTTP ' + d.status));
      }
    } catch (e) {
      render({ok: false, status: 0, ms: 0, error: e.message || String(e)});
      if (inlineOut) {
        inlineOut.className = 'cat-test-result small text-danger';
        inlineOut.textContent = '失败';
        inlineOut.title = e.message || String(e);
      }
    } finally {
      run.disabled = false;
      run.textContent = '重新测试';
      if (inlineBtn) inlineBtn.disabled = false;
    }
  };
  run.onclick = runTest;
  uiPanel('模型测试 · ' + row.upstream, wrap);
  runTest();
}

function renderCatalog() {
  const tb = $('#cat-rows');
  tb.innerHTML = '';
  const sel = $('#cat-channel').value;
  const query = ($('#cat-search').value || '').trim().toLowerCase();
  const rows = CAT_ROWS.filter(r => (!sel || r.channel_id === sel)
    && (!query || [r.channel_id, r.channel_name, r.upstream, r.alias].some(v => String(v || '').toLowerCase().includes(query))));
  // 概览条跟随当前渠道筛选：已接入/待接入/免费模型/近 10 次失败红格
  const exposed = new Set(), pending = new Set(), free = new Set();
  let fails = 0;
  for (const r of rows) {
    const name = r.alias || r.upstream;
    if (r.exposed) {
      exposed.add(name);
      if (!window.NGW_UI.paid(r.price)) free.add(name);
      for (const v of r.recent || []) if (Number(v) === 0) fails++;
    } else {
      pending.add(r.channel_id + '/' + r.upstream);
    }
  }
  $('#cat-sum-exposed').textContent = exposed.size;
  $('#cat-sum-pending').textContent = pending.size;
  $('#cat-sum-free').textContent = free.size;
  $('#cat-sum-fail').textContent = fails;
  if (!rows.length) {
    tb.innerHTML = '<tr><td colspan="12" class="text-muted small text-center py-4">' +
      (query ? '没有匹配的模型，换个关键词试试。' : '暂无模型。选择渠道后点击「拉取模型清单」，也可在「上游」中手动添加。') + '</td></tr>';
    return;
  }
  for (const r of rows) {
    const tr = document.createElement('tr');
    const st = r.exposed
      ? '<span class="badge text-bg-success">已接入</span>'
      : '<span class="badge text-bg-secondary">待接入</span>';
    tr.innerHTML =
      '<td><input type="checkbox" class="cat-sel" data-key="' + esc(r.channel_id + '\u0000' + r.upstream) + '"></td>' +
      '<td class="small text-muted">' + esc(r.channel_name || r.channel_id) + '</td>' +
      '<td class="font-monospace small" style="max-width:260px;word-break:break-all">' + esc(r.channel_id + '/' + r.upstream) + '</td>' +
      '<td><input class="form-control form-control-sm font-monospace cat-alias" value="' + esc(r.alias || '') + '" placeholder="空=隐藏"></td>' +
      '<td><input class="form-control form-control-sm cat-ctx" inputmode="numeric" value="' + esc(fmtCapacity(r.ctx)) + '" placeholder="128k / 1M"></td>' +
      '<td><input class="form-control form-control-sm cat-out" inputmode="numeric" value="' + esc(fmtCapacity(r.out)) + '" placeholder="128k / 1M"></td>' +
      '<td><input type="number" min="0" class="form-control form-control-sm cat-tps" value="' + (r.tps || 0) + '"></td>' +
      '<td class="small"><button class="btn btn-sm btn-link p-0 text-start cat-price" title="' + esc(window.NGW_UI.pricingText(r.price)) + '；点击跳到下方定价编辑">' + esc(window.NGW_UI.pricingText(r.price)) + '</button></td>' +
      '<td>' + st + '</td>' +
      '<td title="最近 10 次请求（左旧右新）">' + ((r.recent && r.recent.length) ? window.NGW_UI.heatSquares(r.recent) : '<span class="text-muted small">—</span>') + '</td>' +
      '<td class="cat-test-cell"><button type="button" class="btn btn-sm btn-outline-secondary cat-test">测速</button> <span class="cat-test-result small"></span></td>' +
      '<td class="text-end text-nowrap"><button class="btn btn-sm btn-primary cat-save">保存</button> ' +
      (r.exposed
        ? '<button class="btn btn-sm btn-outline-danger cat-toggle">取消接入</button>'
        : '<button class="btn btn-sm btn-outline-success cat-toggle">接入</button>') + '</td>';
    tr.querySelector('.cat-price').onclick = () => {
      const target = Array.from(document.querySelectorAll('#price-rows tr')).find(x => x.dataset.ch === r.channel_id && x.dataset.model === (r.alias || r.upstream));
      if (!target) { toast('该模型还没出现在定价列表，先保存接入', 'warning'); return; }
      target.scrollIntoView({block:'center'});
      target.classList.add('price-flash');
      setTimeout(() => target.classList.remove('price-flash'), 1400);
    };
    tr.querySelector('.cat-toggle').onclick = guard(async () => {
      const willExpose = !r.exposed;
      if (!willExpose && !await uiConfirm('取消接入「' + (r.alias || r.upstream) + '」？该模型的定价会一并移除。', {danger:true, okText:'取消接入'})) return;
      const alias = willExpose ? (tr.querySelector('.cat-alias').value.trim() || r.upstream) : '';
      const ctx = willExpose ? parseCapacity(tr.querySelector('.cat-ctx').value, '上下文') : (r.ctx || 0);
      const out = willExpose ? parseCapacity(tr.querySelector('.cat-out').value, '输出上限') : (r.out || 0);
      const tps = willExpose ? (parseInt(tr.querySelector('.cat-tps').value, 10) || 0) : (r.tps || 0);
      if (alias && /[=|#]/.test(alias)) throw new Error('别名不能包含 = | # 字符');
      await api('upstream-models/op', {method:'POST', json:{channel_id:r.channel_id, upstream:r.upstream, alias, ctx, out, tps}});
      toast(willExpose ? '已接入：' + alias : '已取消接入');
      loadCatalog();
    });
    const testBtn = tr.querySelector('.cat-test');
    const testOut = tr.querySelector('.cat-test-result');
    testBtn.onclick = () => openModelTest(r, testOut, testBtn);
    tr.querySelector('.cat-save').onclick = guard(async () => {
      const alias = tr.querySelector('.cat-alias').value.trim();
      const ctx = parseCapacity(tr.querySelector('.cat-ctx').value, '上下文');
      const out = parseCapacity(tr.querySelector('.cat-out').value, '输出上限');
      const tps = parseInt(tr.querySelector('.cat-tps').value, 10) || 0;
      if (alias && /[=|#]/.test(alias)) throw new Error('别名不能包含 = | # 字符');
      if (alias && CHAIN_CANDS.some(m => m.chain && m.name === alias)) throw new Error('别名与模型路由重名，会让其中一个不可达');
      await api('upstream-models/op', {method: 'POST', json: {channel_id: r.channel_id, upstream: r.upstream, alias, ctx, out, tps}});
      toast(alias ? '已接入：' + alias : '已隐藏');
      loadCatalog();
    });
    tb.appendChild(tr);
  }
  updateSelInfo();
}

function selectedRows() {
  const picked = [];
  for (const tr of $('#cat-rows').querySelectorAll('tr')) {
    const box = tr.querySelector('.cat-sel');
    if (!box || !box.checked) continue;
    const [ch, up] = box.dataset.key.split('\u0000');
    const r = CAT_ROWS.find(x => x.channel_id === ch && x.upstream === up);
    if (r) picked.push({r, tr});
  }
  return picked;
}

function updateSelInfo() {
  const n = selectedRows().length;
  const el = $('#cat-selinfo');
  if (el) el.textContent = n ? '已选 ' + n + ' 行' : '';
}

async function applyBatch() {
  const picked = selectedRows();
  if (!picked.length) throw new Error('先勾选要标注的行');
  const bat = {
    ctx: $('#bat-ctx').value.trim(),
    out: $('#bat-out').value.trim(),
    tps: $('#bat-tps').value.trim(),
  };
  if (bat.ctx === '' && bat.out === '' && bat.tps === '') throw new Error('三项都留空，没有可应用的值');
  for (const {r} of picked) {
    await api('upstream-models/op', {method: 'POST', json: {
      channel_id: r.channel_id, upstream: r.upstream, alias: r.alias || '',
      ctx: bat.ctx === '' ? (r.ctx || 0) : parseCapacity(bat.ctx, '上下文'),
      out: bat.out === '' ? (r.out || 0) : parseCapacity(bat.out, '输出上限'),
      tps: bat.tps === '' ? (r.tps || 0) : (parseInt(bat.tps, 10) || 0),
    }});
  }
  toast('已批量标注 ' + picked.length + ' 行');
  loadCatalog();
}

async function enableSelectedModels() {
  const picked = selectedRows().filter(({r}) => !r.exposed);
  if (!picked.length) throw new Error('没有待接入的选中模型');
  for (const {r} of picked) {
    await api('upstream-models/op', {method: 'POST', json: {
      channel_id: r.channel_id, upstream: r.upstream, alias: r.alias || r.upstream,
      ctx: r.ctx || 0, out: r.out || 0, tps: r.tps || 0,
    }});
  }
  toast('已接入 ' + picked.length + ' 个模型');
  loadCatalog();
}

async function testSelectedModels() {
  const picked = selectedRows();
  if (!picked.length) throw new Error('先勾选要测速的模型');
  for (const {tr} of picked) {
    const btn = tr.querySelector('.cat-test');
    if (btn) btn.click();
    await new Promise(resolve => setTimeout(resolve, 120));
  }
  toast('已提交 ' + picked.length + ' 个模型的测速请求');
}

function bindCatalog() {
  $('#cat-channel').onchange = renderCatalog;
  $('#cat-search').addEventListener('input', renderCatalog);
  $('#cat-test').onclick = guard(testSelectedModels);
  $('#cat-enable').onclick = guard(enableSelectedModels);
  $('#cat-selall').onchange = (e) => {
    for (const box of $('#cat-rows').querySelectorAll('.cat-sel')) box.checked = e.target.checked;
    updateSelInfo();
  };
  $('#cat-apply').onclick = guard(applyBatch);
  $('#cat-fetch').onclick = guard(async () => {
    const ch = $('#cat-channel').value;
    if (!ch) throw new Error('先在上面选一个渠道');
    const d = await api('upstream-models/fetch', {method: 'POST', json: {channel_id: ch}});
    toast('已获取 ' + d.count + ' 个模型');
    loadCatalog();
  });
}

async function loadSettings() {
  await chainLoadCandidates();
  const c = await run(() => api('settings'));
  if (!c) return;
  for (const [id, key] of SET_FIELDS) $('#' + id).value = c[key] != null ? c[key] : '';
  $('#set-logen').checked = !!c.log_enabled;
  $('#set-queue').checked = !!c.queue_enabled;
  $('#set-update').checked = !!c.update_enabled;
  $('#set-breaker').checked = !!c.breaker_enabled;
  $('#set-herr').checked = !!c.hide_upstream_errors;
  $('#set-mhide').checked = !!c.hide_mapped_names;
  $('#set-deviceon').checked = c.device_login_enabled !== false;
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
  const pick = (button, textarea, title) => {
    const btn = $(button), area = $(textarea);
    if (!btn || !area) return;
    btn.onclick = () => openModelPicker(title, CHAIN_CANDS, splitNames(area.value), names => {
      area.value = names.join(String.fromCharCode(10));
    });
  };
  pick('#set-mwl-pick', '#set-mwl', '选择白名单模型');
  pick('#set-mbl-pick', '#set-mbl', '选择黑名单模型');
  $('#settings-save').onclick = guard(async () => {
    const config = {};
    for (const [id, key] of SET_FIELDS) config[key] = $('#' + id).value;
    config.log_enabled = $('#set-logen').checked;
    config.queue_enabled = $('#set-queue').checked;
    config.update_enabled = $('#set-update').checked;
    config.breaker_enabled = $('#set-breaker').checked;
    config.hide_upstream_errors = $('#set-herr').checked;
    config.hide_mapped_names = $('#set-mhide').checked;
    config.device_login_enabled = $('#set-deviceon').checked;
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
  return { close };
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


// ---------------------------------------------------------------- 01Code 客户端
const C01_PLACEMENTS = {modal: '居中弹窗', corner: '右下角', inline: '对话内', tool: '工具调用下'};
let C01 = {ads: [], config: {}, stats: {totals: {}, ads: []}};

function c01Pill(text, cls) {
  return '<span class="c01-pill ' + (cls || '') + '">' + esc(text) + '</span>';
}
function c01AdState(ad) {
  const now = Math.floor(Date.now() / 1000);
  if (!ad.enabled) return ['已停用', 'off'];
  if (ad.starts_at && now < ad.starts_at) return ['未开始', 'warn'];
  if (ad.ends_at && now > ad.ends_at) return ['已结束', 'off'];
  return ['生效中', 'on'];
}
function c01ShortTime(ts) {
  if (!ts) return '';
  const d = new Date(ts * 1000);
  return d.toLocaleString('zh-CN', {month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit'});
}
function c01WindowText(ad) {
  if (!ad.starts_at && !ad.ends_at) return '不限';
  return (ad.starts_at ? c01ShortTime(ad.starts_at) : '现在') + ' → ' + (ad.ends_at ? c01ShortTime(ad.ends_at) : '不限');
}
function c01FreqText(ad) {
  const cd = Number(ad.cooldown_secs) || 0;
  const cdText = cd >= 86400 ? (cd / 86400) + ' 天' : cd >= 3600 ? (cd / 3600) + ' 小时' : cd + ' 秒';
  return '权重 ' + (ad.weight || 1) + ' · 最多 ' + (ad.max_displays || 1) + ' 次 · 冷却 ' + cdText;
}
function c01FreqShort(ad) {
  const cd = Number(ad.cooldown_secs) || 0;
  const cdText = cd >= 86400 ? (cd / 86400) + '天' : cd >= 3600 ? (cd / 3600) + '时' : cd + '秒';
  return '×' + (ad.weight || 1) + ' · ' + (ad.max_displays || 1) + ' 次 · ' + cdText;
}
function c01TargetText(ad) {
  const models = ad.models || [];
  if (ad.free_only && models.length) return '免费 · ' + models.length + ' 个';
  if (ad.free_only) return '仅免费';
  if (models.length) return models.length + ' 个模型';
  return '全部';
}
function c01DateInput(ts) {
  if (!ts) return '';
  const d = new Date(ts * 1000);
  const p = n => String(n).padStart(2, '0');
  return d.getFullYear() + '-' + p(d.getMonth() + 1) + '-' + p(d.getDate()) + 'T' + p(d.getHours()) + ':' + p(d.getMinutes());
}
function c01DateTs(v) {
  if (!v) return 0;
  const t = new Date(v).getTime();
  return Number.isFinite(t) ? Math.max(0, Math.floor(t / 1000)) : 0;
}

async function loadClient01() {
  await chainLoadCandidates();
  const d = await run(() => api('client01'));
  if (!d) return;
  C01 = {ads: d.ads || [], config: d.config || {}, stats: d.stats || {totals: {}, ads: []}};
  renderC01Stats();
  renderC01Analytics();
  renderC01Ads();
  fillC01Config();
}

function renderC01Stats() {
  const box = $('#c01-stats');
  if (!box) return;
  const c = C01.config || {};
  const now = Math.floor(Date.now() / 1000);
  const ads = C01.ads || [];
  const active = ads.filter(a => a.enabled && (!a.starts_at || a.starts_at <= now) && (!a.ends_at || a.ends_at >= now)).length;
  const freeCount = splitNames(c.free_models || '').length;
  const pay = Math.round((Number(c.discount_ratio) || 1) * 100);
  const off = Math.max(0, 100 - pay);
  const benefitOn = !!c.attest_enabled;
  const cards = [
    ['广告接口', c.ads_enabled ? '已启用' : '已关闭', ads.length + ' 条 · ' + ((C01.stats && C01.stats.totals && C01.stats.totals.impressions) || 0) + ' 次曝光', c.ads_enabled ? 'accent' : '', 'bi-badge-ad'],
    ['防伪验证', c.attest_enabled ? '已开启' : '未开启', c.secret_set ? '共享密钥已配置' : '未配置共享密钥', c.attest_enabled ? 'green' : 'amber', 'bi-shield-check'],
    ['设备登录', c.device_login_enabled ? '已开启' : '已关闭', c.public_origin || '未配置公网入口', c.device_login_enabled ? 'accent' : '', 'bi-phone'],
    ['官方客户端优惠', benefitOn && off > 0 ? off + '%' : (benefitOn ? '原价' : '未启用'),
      benefitOn ? (pay < 100 ? '支付 ' + pay + '% · ' + freeCount + ' 个限免' : '未开启折扣') : '需开启设备签名与 attest',
      benefitOn && off > 0 ? 'green' : '', 'bi-percent'],
  ];
  box.innerHTML = '';
  for (const [label, val, sub, tone, icon] of cards) {
    const col = el('div', 'col-6 col-xl-3');
    const stat = el('div', 'card stat h-100 ' + tone);
    const lbl = el('div', 'd-flex align-items-center gap-2 stat-lbl');
    lbl.append(el('i', 'bi ' + icon), el('span', 'lbl', label));
    stat.append(lbl, el('div', 'num', String(val)), el('div', 'sub', sub));
    col.appendChild(stat);
    box.appendChild(col);
  }
}

function renderC01Analytics() {
  const stats = C01.stats || {totals: {}, ads: []};
  const rows = stats.ads || [];
  const total = Number(stats.totals && stats.totals.impressions) || 0;
  const pie = rows.filter(r => Number(r.impressions) > 0).map(r => ({name: r.title || r.id, count: Number(r.impressions)}));
  if ($('#c01-ad-pie')) anDonut($('#c01-ad-pie'), pie, '次曝光', '暂无曝光');
  const tb = $('#c01-stat-rows');
  if (!tb) return;
  tb.innerHTML = '';
  $('#c01-stat-empty').classList.toggle('d-none', rows.length > 0);
  for (const r of rows) {
    const tr = el('tr');
    const name = el('td');
    name.innerHTML = '<div class="c01-ad-title">' + esc(r.title || r.id) + '</div>' +
      '<div class="c01-ad-sub">' + esc(r.id || '') + '</div>';
    const imp = el('td', 'text-end');
    const share = total > 0 ? (Number(r.impressions) / total * 100) : 0;
    imp.innerHTML = '<div>' + Number(r.impressions || 0).toLocaleString('zh-CN') + '</div>' +
      '<div class="c01-mini-bar"><i style="width:' + Math.max(0, Math.min(100, share)).toFixed(1) + '%"></i></div>';
    const uniq = el('td', 'text-end', Number(r.unique || 0).toLocaleString('zh-CN'));
    const clicks = el('td', 'text-end', Number(r.clicks || 0).toLocaleString('zh-CN'));
    const ctr = el('td', 'text-end', (Number(r.ctr) || 0).toFixed(2) + '%');
    const closes = el('td', 'text-end text-muted', Number(r.closes || 0).toLocaleString('zh-CN'));
    tr.append(name, imp, uniq, clicks, ctr, closes);
    tb.appendChild(tr);
  }
}

function renderC01Ads() {
  const tb = $('#c01-ad-rows');
  if (!tb) return;
  tb.innerHTML = '';
  const ads = C01.ads || [];
  $('#c01-ad-empty').classList.toggle('d-none', ads.length > 0);
  ads.forEach((ad, index) => {
    const [state, tone] = c01AdState(ad);
    const tr = el('tr');
    const st = el('td');
    st.innerHTML = c01Pill(state, tone);
    const info = el('td');
    info.innerHTML = '<div class="c01-ad-title">' + esc(ad.title || '(未命名)') + '</div>' +
      '<div class="c01-ad-sub">' + esc(ad.id || '') + '</div>';
    const place = el('td');
    place.innerHTML = c01Pill(C01_PLACEMENTS[ad.placement] || ad.placement || '居中弹窗', 'info');
    const target = el('td');
    target.innerHTML = '<span class="small">' + esc(c01TargetText(ad)) + '</span>';
    const freq = el('td');
    freq.title = c01FreqText(ad);
    freq.innerHTML = '<span class="small text-muted">' + esc(c01FreqShort(ad)) + '</span>';
    const win = el('td');
    win.innerHTML = '<span class="small text-muted">' + esc(c01WindowText(ad)) + '</span>';
    const ops = el('td', 'text-end');
    const opsRow = el('div', 'd-flex justify-content-end gap-1');
    const edit = el('button', 'btn btn-sm btn-outline-secondary', '编辑');
    const copy = el('button', 'btn btn-sm btn-outline-secondary', '复制');
    const toggle = el('button', 'btn btn-sm btn-outline-secondary', ad.enabled ? '停用' : '启用');
    const del = el('button', 'btn btn-sm btn-outline-danger', '删除');
    edit.type = copy.type = toggle.type = del.type = 'button';
    edit.onclick = () => c01OpenAdEditor(index);
    copy.onclick = guard(async () => {
      const clone = JSON.parse(JSON.stringify(ad));
      let id = (ad.id || 'ad') + '-copy';
      let n = 2;
      while ((C01.ads || []).some(a => a.id === id)) id = (ad.id || 'ad') + '-copy' + (n++);
      clone.id = id;
      clone.title = (ad.title || '') + '（副本）';
      await c01SaveAds((C01.ads || []).concat([clone]));
      toast('已复制广告');
    });
    toggle.onclick = guard(async () => {
      const next = (C01.ads || []).map((a, i) => i === index ? Object.assign({}, a, {enabled: !a.enabled}) : a);
      await c01SaveAds(next);
    });
    del.onclick = guard(async () => {
      if (!await uiConfirm('删除广告「' + (ad.title || ad.id) + '」？客户端将不再收到它。', {danger: true, okText: '删除'})) return;
      await c01SaveAds((C01.ads || []).filter((_, i) => i !== index));
      toast('广告已删除');
    });
    opsRow.append(edit, copy, toggle, del);
    ops.appendChild(opsRow);
    tr.append(st, info, place, target, freq, win, ops);
    tb.appendChild(tr);
  });
}

async function c01SaveAds(next) {
  const d = await api('client01/ads', {method: 'POST', json: {ads: next}});
  C01 = {ads: d.ads || [], config: d.config || C01.config, stats: d.stats || C01.stats};
  renderC01Ads();
  renderC01Stats();
  renderC01Analytics();
}

function renderC01FreeChips() {
  const box = $('#c01-free-chips');
  const area = $('#c01-free-models');
  if (!box || !area) return;
  const names = splitNames(area.value);
  box.innerHTML = '';
  for (const name of names) {
    const chip = el('span', 'c01-model-chip');
    chip.appendChild(el('span', '', name));
    const x = el('button', '', '×');
    x.type = 'button';
    x.title = '移除 ' + name;
    x.onclick = () => {
      area.value = splitNames(area.value).filter(n => n !== name).join(String.fromCharCode(10));
      renderC01FreeChips();
      updateC01BenefitPreview();
    };
    chip.appendChild(x);
    box.appendChild(chip);
  }
}

function updateC01BenefitPreview() {
  const pay = Number($('#c01-discount').value);
  const off = Math.max(0, Math.round(100 - (Number.isFinite(pay) ? pay : 100)));
  const names = splitNames($('#c01-free-models').value);
  const gold = $('#c01-benefit-gold');
  const green = $('#c01-benefit-green');
  const note = $('#c01-benefit-note');
  if (gold) gold.style.display = names.length ? '' : 'none';
  if (green) {
    green.style.display = off > 0 ? '' : 'none';
    green.textContent = off + '%';
  }
  if (note) {
    const parts = [];
    if (names.length) parts.push(names.length + ' 个模型显示金色「限免」');
    if (off > 0) parts.push('其他付费模型显示绿色「' + off + '%」折扣');
    const enabled = $('#c01-attest-enabled') && $('#c01-attest-enabled').checked;
    const body = parts.length ? '客户端：' + parts.join('；') + '。' : '未开启 01Code 优惠。';
    note.textContent = enabled ? body : '开启设备签名与 attest 后生效。' + body;
  }
}

function fillC01Config() {
  const c = C01.config || {};
  const set = (id, val) => { const n = $(id); if (n) n.value = val; };
  const check = (id, val) => { const n = $(id); if (n) n.checked = !!val; };
  check('#c01-ads-enabled', c.ads_enabled);
  check('#c01-free-only', c.ads_free_only);
  check('#c01-attest-enabled', c.attest_enabled);
  set('#c01-discount', Math.round((Number(c.discount_ratio) || 1) * 100));
  set('#c01-minver', c.discount_min_version || '');
  set('#c01-free-models', c.free_models || '');
  set('#c01-secret', '');
  check('#c01-secret-clear', false);
  $('#c01-secret-state').textContent = c.secret_set ? '已配置（留空保持不变）' : '未配置';
  $('#c01-secret-state').className = 'form-text mb-0 ' + (c.secret_set ? 'text-success' : 'text-muted');
  $('#c01-device-state').textContent = c.device_login_enabled ? '已开启' : '已关闭';
  $('#c01-origin').textContent = c.public_origin || '未配置';
  $('#c01-save-state').textContent = '';
  renderC01FreeChips();
  updateC01BenefitPreview();
}

function c01OpenAdEditor(index) {
  const editing = Number.isInteger(index) && index >= 0;
  const src = editing ? (C01.ads[index] || {}) : {};
  const ad = Object.assign({
    id: '', title: '', body: '', badge: '推荐', cta: '了解详情', url: 'https://ai.gzmath.cn',
    accent: '#315FE9', weight: 1, max_displays: 1, cooldown_secs: 86400, dismiss_secs: 0,
    starts_at: 0, ends_at: 0, min_version: '', placement: 'modal', free_only: false,
    models: [], image: '', layout: 'card', image_ratio: 'wide', enabled: true,
  }, src);
  const wrap = el('div');
  wrap.innerHTML =
    '<div class="c01-preview-wrap"><div class="c01-preview-lbl">客户端预览 · ' + (C01_PLACEMENTS[ad.placement] || '居中弹窗') + '</div>' +
      '<div class="c01-preview" id="c01-preview"><div class="bar"></div><div class="in">' +
        '<img id="c01p-image" class="c01-preview-img" alt="" hidden>' +
        '<span class="badge" id="c01p-badge"></span><div class="ttl" id="c01p-title"></div><div class="bd" id="c01p-body"></div>' +
        '<div class="foot"><span class="cta" id="c01p-cta"></span><span class="from">来自言灵中转</span></div>' +
      '</div></div></div>' +
    '<div class="c01-form-grid">' +
      '<div class="c01-form-sec">基础信息</div>' +
      '<div><label for="c01f-id">广告 ID</label><input class="form-control form-control-sm font-monospace" id="c01f-id" maxlength="64" placeholder="01code-discount-2026-11"></div>' +
      '<div><label for="c01f-title">标题</label><input class="form-control form-control-sm" id="c01f-title" maxlength="40" placeholder="不超过 28 个汉字"></div>' +
      '<div class="full"><label for="c01f-body">正文</label><textarea class="form-control form-control-sm" id="c01f-body" rows="3" maxlength="240" placeholder="纯文本，不使用 HTML"></textarea></div>' +
      '<div><label for="c01f-badge">小标签</label><input class="form-control form-control-sm" id="c01f-badge" maxlength="16" placeholder="推荐"></div>' +
      '<div><label for="c01f-cta">按钮文案</label><input class="form-control form-control-sm" id="c01f-cta" maxlength="12" placeholder="了解详情"></div>' +
      '<div class="full"><label for="c01f-url">跳转链接</label><input class="form-control form-control-sm font-monospace" id="c01f-url" placeholder="https://ai.gzmath.cn，留空则按钮为「知道了」"></div>' +
      '<div class="c01-form-sec">展示样式</div>' +
      '<div class="full"><label for="c01f-image">广告图片</label><input class="form-control form-control-sm font-monospace" id="c01f-image" placeholder="https://cdn.example.com/ad.png，留空为纯文本卡片"></div>' +
      '<div><label for="c01f-layout">版式</label><select class="form-select form-select-sm" id="c01f-layout"><option value="card">卡片 · 图片在上</option><option value="compact">紧凑 · 小图</option><option value="text">纯文本</option></select></div>' +
      '<div><label for="c01f-ratio">图片比例</label><select class="form-select form-select-sm" id="c01f-ratio"><option value="wide">宽图 16:9</option><option value="square">方图 1:1</option><option value="banner">横幅 3:1</option></select></div>' +
      '<div><label for="c01f-accent">主色</label><div class="d-flex gap-2"><input type="color" class="form-control form-control-color" id="c01f-accent-color" value="#315FE9" title="选择主色"><input class="form-control form-control-sm font-monospace" id="c01f-accent" maxlength="7"></div></div>' +
      '<div><label for="c01f-placement">展示位置</label><select class="form-select form-select-sm" id="c01f-placement">' +
        Object.entries(C01_PLACEMENTS).map(([k, v]) => '<option value="' + k + '">' + v + '</option>').join('') + '</select></div>' +
      '<div class="c01-form-sec">投放规则</div>' +
      '<div><label for="c01f-weight">权重</label><input type="number" min="1" max="100" class="form-control form-control-sm" id="c01f-weight"></div>' +
      '<div><label for="c01f-max">每设备最多次数</label><input type="number" min="1" max="1000" class="form-control form-control-sm" id="c01f-max"></div>' +
      '<div><label for="c01f-cooldown">冷却 · 秒</label><input type="number" min="60" max="2592000" class="form-control form-control-sm" id="c01f-cooldown"></div>' +
      '<div><label for="c01f-dismiss">右下角自动收起 · 秒</label><input type="number" min="0" max="300" class="form-control form-control-sm" id="c01f-dismiss" placeholder="0 = 手动关闭"></div>' +
      '<div><label for="c01f-start">开始时间</label><input type="datetime-local" class="form-control form-control-sm" id="c01f-start"></div>' +
      '<div><label for="c01f-end">结束时间</label><input type="datetime-local" class="form-control form-control-sm" id="c01f-end"></div>' +
      '<div><label for="c01f-minver">最低客户端版本</label><input class="form-control form-control-sm font-monospace" id="c01f-minver" placeholder="留空不限"></div>' +
      '<div><label>投放模型</label><div class="d-flex gap-1"><input class="form-control form-control-sm font-monospace" id="c01f-models" placeholder="留空 = 全部模型"><button class="btn btn-sm btn-outline-secondary" id="c01f-models-pick" type="button">选择</button></div></div>' +
      '<div class="full d-flex flex-wrap gap-4 pt-1">' +
        '<div class="form-check form-switch"><input class="form-check-input" type="checkbox" id="c01f-free"><label class="form-check-label small" for="c01f-free">仅免费模型展示</label></div>' +
        '<div class="form-check form-switch"><input class="form-check-input" type="checkbox" id="c01f-enabled"><label class="form-check-label small" for="c01f-enabled">启用这条广告</label></div>' +
      '</div>' +
    '</div>' +
    '<div class="c01-form-actions"><button class="btn btn-sm btn-outline-secondary" id="c01f-cancel" type="button">取消</button><button class="btn btn-sm btn-primary" id="c01f-save" type="button">' + (editing ? '保存广告' : '创建广告') + '</button></div>';

  const panel = uiPanel((editing ? '编辑广告 · ' : '新建广告 · ') + (ad.title || '未命名'), wrap);
  const val = id => $(id).value;
  const setVal = (id, v) => { $(id).value = v == null ? '' : v; };
  setVal('#c01f-id', ad.id);
  setVal('#c01f-title', ad.title);
  setVal('#c01f-body', ad.body);
  setVal('#c01f-badge', ad.badge);
  setVal('#c01f-cta', ad.cta);
  setVal('#c01f-url', ad.url);
  setVal('#c01f-image', ad.image || '');
  setVal('#c01f-layout', ad.layout || 'card');
  setVal('#c01f-ratio', ad.image_ratio || 'wide');
  setVal('#c01f-accent', ad.accent || '#315FE9');
  $('#c01f-accent-color').value = /^#[0-9a-fA-F]{6}$/.test(ad.accent || '') ? ad.accent : '#315FE9';
  setVal('#c01f-placement', ad.placement || 'modal');
  setVal('#c01f-weight', ad.weight || 1);
  setVal('#c01f-max', ad.max_displays || 1);
  setVal('#c01f-cooldown', ad.cooldown_secs || 86400);
  setVal('#c01f-dismiss', ad.dismiss_secs || 0);
  setVal('#c01f-start', c01DateInput(ad.starts_at));
  setVal('#c01f-end', c01DateInput(ad.ends_at));
  setVal('#c01f-minver', ad.min_version || '');
  setVal('#c01f-models', (ad.models || []).join(', '));
  $('#c01f-free').checked = !!ad.free_only;
  $('#c01f-enabled').checked = ad.enabled !== false;

  const syncAccent = () => {
    const a = val('#c01f-accent').trim();
    if (/^#[0-9a-fA-F]{6}$/.test(a)) $('#c01f-accent-color').value = a;
  };
  $('#c01f-accent').addEventListener('input', syncAccent);
  $('#c01f-accent-color').addEventListener('input', e => { setVal('#c01f-accent', e.target.value.toUpperCase()); updatePreview(); });
  const updatePreview = () => {
    const accent = /^#[0-9a-fA-F]{6}$/.test(val('#c01f-accent').trim()) ? val('#c01f-accent').trim() : '#315FE9';
    const card = $('#c01-preview');
    card.style.setProperty('--c01-accent', accent);
    const img = $('#c01p-image');
    const imgUrl = val('#c01f-image').trim();
    const layout = val('#c01f-layout');
    if (imgUrl && layout !== 'text') {
      img.onerror = () => { img.hidden = true; };
      img.src = imgUrl;
      img.hidden = false;
      img.style.maxHeight = layout === 'compact' ? '72px' : (val('#c01f-ratio') === 'banner' ? '70px' : '150px');
    } else {
      img.hidden = true;
      img.removeAttribute('src');
    }
    $('#c01p-badge').textContent = val('#c01f-badge').trim() || '推荐';
    $('#c01p-title').textContent = val('#c01f-title').trim() || '广告标题';
    $('#c01p-body').textContent = val('#c01f-body').trim() || '这里展示广告正文。';
    $('#c01p-cta').textContent = val('#c01f-url').trim() ? (val('#c01f-cta').trim() || '了解详情') : '知道了';
  };
  wrap.querySelectorAll('input, textarea, select').forEach(n => n.addEventListener('input', updatePreview));
  updatePreview();

  $('#c01f-models-pick').onclick = () => openModelPicker('选择投放模型', CHAIN_CANDS, splitNames(val('#c01f-models')), names => {
    setVal('#c01f-models', names.join(', '));
    updatePreview();
  });
  $('#c01f-cancel').onclick = () => panel.close();
  $('#c01f-save').onclick = guard(async () => {
    const next = {
      id: val('#c01f-id').trim(),
      title: val('#c01f-title').trim(),
      body: val('#c01f-body').trim(),
      badge: val('#c01f-badge').trim() || '推荐',
      cta: val('#c01f-cta').trim() || '了解详情',
      url: val('#c01f-url').trim(),
      image: val('#c01f-image').trim(),
      layout: val('#c01f-layout'),
      image_ratio: val('#c01f-ratio'),
      accent: val('#c01f-accent').trim(),
      weight: Number(val('#c01f-weight')) || 1,
      max_displays: Number(val('#c01f-max')) || 1,
      cooldown_secs: Number(val('#c01f-cooldown')) || 86400,
      dismiss_secs: Number(val('#c01f-dismiss')) || 0,
      starts_at: c01DateTs(val('#c01f-start')),
      ends_at: c01DateTs(val('#c01f-end')),
      min_version: val('#c01f-minver').trim(),
      placement: val('#c01f-placement'),
      free_only: $('#c01f-free').checked,
      models: splitNames(val('#c01f-models')),
      enabled: $('#c01f-enabled').checked,
    };
    if (!next.id || !next.title) throw new Error('广告 ID 和标题不能为空');
    const ads = (C01.ads || []).slice();
    if (editing) ads[index] = next;
    else ads.push(next);
    await c01SaveAds(ads);
    panel.close();
    toast(editing ? '广告已保存' : '广告已创建');
  });
}

function c01OpenImport() {
  const wrap = el('div');
  wrap.innerHTML = '<div class="form-text mb-2">粘贴广告 JSON（数组或 <code>{"ads":[...]}</code>），导入后会整体替换现有广告。</div>' +
    '<textarea class="form-control form-control-sm font-monospace" id="c01-import-text" rows="12" placeholder=\'{"ads":[{"id":"demo","title":"...","body":"..."}]}\'></textarea>' +
    '<div class="d-flex justify-content-end gap-2 mt-3"><button class="btn btn-sm btn-outline-secondary" id="c01-import-cancel" type="button">取消</button><button class="btn btn-sm btn-primary" id="c01-import-ok" type="button">导入并保存</button></div>';
  const panel = uiPanel('导入广告 JSON', wrap);
  $('#c01-import-cancel').onclick = () => panel.close();
  $('#c01-import-ok').onclick = guard(async () => {
    let text = $('#c01-import-text').value.trim();
    if (!text) throw new Error('请先粘贴 JSON');
    text = text.replace(/^```(?:json)?\s*/i, '').replace(/\s*```$/, '');
    let parsed;
    try { parsed = JSON.parse(text); } catch (e) { throw new Error('JSON 解析失败：' + e.message); }
    const ads = Array.isArray(parsed) ? parsed : parsed.ads;
    if (!Array.isArray(ads)) throw new Error('JSON 必须是数组或包含 ads 数组');
    await c01SaveAds(ads);
    panel.close();
    toast('已导入 ' + ads.length + ' 条广告');
  });
}

const C01_PROMPT = `你是言灵中转的运营配置助手。请为 01Code 桌面客户端生成一条广告弹窗 JSON。
必须严格遵守：
1. 只输出 JSON 对象，不要 markdown、不要解释。
2. 顶层结构为 {"ads": [ ... ]}，只生成 1 条广告。
3. 字段：id, title, body, badge, cta, url, accent, weight, max_displays, cooldown_secs, starts_at, ends_at, min_version, placement, free_only, models, image, layout, image_ratio。
4. title 不超过 28 个汉字；body 不超过 120 个汉字，纯文本，不用 HTML；badge 不超过 8 个汉字；cta 不超过 8 个汉字。
5. url 必须是 https:// 开头的言灵中转站内地址；accent 必须是 6 位十六进制颜色。
6. max_displays 默认 1，cooldown_secs 默认 86400；活动时间用 Unix 秒，0 表示不限。
7. placement 可选 modal（居中弹窗）、corner（右下角）、inline（对话内）、tool（工具调用下）。
8. image 可选，必须是 https 图片直链；layout 可选 card（图片在上）/ compact（小图）/ text（纯文本）；image_ratio 可选 wide / square / banner。
9. 文案必须真实、克制，不能承诺未配置的折扣，不能使用“点击领取”“限时秒杀”等诱导性表述。
活动信息：
- 活动名称：01Code 官方客户端专属优惠
- 优惠内容：使用 01Code 官方客户端调用模型，按网关实际配置享受折扣/减免
- 活动地址：https://ai.gzmath.cn
- 主色：#315FE9
请生成 JSON。`;

function bindClient01() {
  const save = $('#c01-save');
  if (!save) return;
  $('#c01-ad-new').onclick = () => c01OpenAdEditor(null);
  $('#c01-stat-refresh').onclick = loadClient01;
  $('#c01-stat-reset').onclick = guard(async () => {
    if (!await uiConfirm('清空所有广告曝光、点击与独立设备统计？广告配置不会受影响。', {danger: true, okText: '清空统计'})) return;
    const d = await api('client01/stats/reset', {method: 'POST', json: {}});
    C01 = {ads: d.ads || C01.ads, config: d.config || C01.config, stats: d.stats || {totals: {}, ads: []}};
    renderC01Stats();
    renderC01Analytics();
    toast('广告统计已清空');
  });
  $('#c01-ad-import').onclick = c01OpenImport;
  $('#c01-ad-prompt').onclick = guard(async () => { await copyText(C01_PROMPT); toast('生成提示词已复制'); });
  $('#c01-free-pick').onclick = () => openModelPicker('选择 01Code 限免模型', CHAIN_CANDS, splitNames($('#c01-free-models').value), names => {
    $('#c01-free-models').value = names.join(String.fromCharCode(10));
    renderC01FreeChips();
    updateC01BenefitPreview();
  });
  $('#c01-free-models').addEventListener('input', () => { renderC01FreeChips(); updateC01BenefitPreview(); });
  $('#c01-discount').addEventListener('input', updateC01BenefitPreview);
  $('#c01-attest-enabled').addEventListener('change', updateC01BenefitPreview);
  $('#c01-goto-device').onclick = () => {
    activate('settings');
    setTimeout(() => { const n = document.getElementById('sec-device'); if (n) n.scrollIntoView({behavior: 'smooth', block: 'start'}); }, 120);
  };
  save.onclick = guard(async () => {
    const percent = Number($('#c01-discount').value);
    if (!Number.isFinite(percent) || percent < 10 || percent > 100) throw new Error('优惠比例必须在 10% 到 100% 之间');
    const d = await api('client01/config', {method: 'POST', json: {
      config: {
        ads_enabled: $('#c01-ads-enabled').checked,
        ads_free_only: $('#c01-free-only').checked,
        attest_enabled: $('#c01-attest-enabled').checked,
        discount_ratio: percent / 100,
        discount_min_version: $('#c01-minver').value.trim(),
        free_models: $('#c01-free-models').value,
      },
      secret: $('#c01-secret').value,
      clear_secret: $('#c01-secret-clear').checked,
    }});
    C01 = {ads: d.ads || [], config: d.config || {}, stats: d.stats || C01.stats};
    renderC01Stats();
    renderC01Ads();
    fillC01Config();
    $('#c01-save-state').textContent = '已保存 ' + new Date().toLocaleTimeString('zh-CN', {hour: '2-digit', minute: '2-digit'});
    toast('01Code 设置已保存');
  });
}

document.addEventListener('DOMContentLoaded', () => {
  $$('.sidebar nav a').forEach(a => {
    if (!a.dataset.pane) return;   // 外链（WebClaw 测试台）交给浏览器原生跳转
    a.addEventListener('click', e => { e.preventDefault(); activate(a.dataset.pane); });
  });
  $('#btn-logout').onclick = guard(async () => { await api('logout', {method: 'POST'}); location.href = B + 'admin'; });
  bindImport(); bindSettings(); bindUpstreams(); bindBatch(); bindCatalog(); bindRouting(); bindAnalytics(); bindClient01();
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

  activate(location.hash.slice(1) || 'dash');
  window.addEventListener('hashchange', () => activate(location.hash.slice(1)));
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


// ---------------- 模型定价（只列已接入模型，行内编辑） ----------------
let _priceRows = [];
async function loadPrices() {
  const data = await api('prices');
  _priceRows = data.rows || [];
  renderPrices();
}
function priceFields(price = 0) {
  const token = price && price.mode === 'token';
  const tiers = token ? (price.tiers || []).map(t => [t.min_input, t.input, t.output, t.cache != null ? t.cache : ''].join(' ').trim()).join('\n') : '';
  return '<div class="price-fields"><select class="form-select form-select-sm price-mode" aria-label="计费方式"><option value="request">按次 · 元/次</option><option value="token"' + (token ? ' selected' : '') + '>按 token · 元/百万</option></select>'
    + '<div class="d-flex gap-2 mt-2"><label>输入 / 单次<input type="number" min="0" max="100000" step="0.000001" class="form-control form-control-sm price-input" value="' + esc(token ? price.input : price) + '"></label>'
    + '<label class="price-token"' + (token ? '' : ' hidden') + '>输出<input type="number" min="0" max="100000" step="0.000001" class="form-control form-control-sm price-output" value="' + esc(token ? price.output : 0) + '"></label>'
    + '<label class="price-token"' + (token ? '' : ' hidden') + ' title="缓存命中价，留空=按输入价">缓存<input type="number" min="0" max="100000" step="0.000001" class="form-control form-control-sm price-cache" value="' + esc(token && price.cache != null ? price.cache : '') + '" placeholder="同输入价"></label></div>'
    + '<label class="price-token small mt-2 w-100"' + (token ? '' : ' hidden') + '>长度档位（可选，每行：输入起点 输入单价 输出单价 [缓存单价]）<textarea class="form-control form-control-sm price-tiers" rows="2" placeholder="128001 4 16">' + esc(tiers) + '</textarea></label></div>';
}
function bindPriceFields(node) {
  node.querySelector('.price-mode').onchange = e => {
    node.querySelectorAll('.price-token').forEach(n => n.hidden = e.target.value !== 'token');
  };
}
function readPrice(node) {
  const number = field => {
    const raw = field.value.trim(), n = Number(raw);
    if (!raw || !Number.isFinite(n) || n < 0 || n > 100000) throw new Error('价格应为 0 到 100000 之间的数字');
    return n;
  };
  const input = number(node.querySelector('.price-input'));
  if (node.querySelector('.price-mode').value !== 'token') return input;
  const price = {mode:'token', input, output:number(node.querySelector('.price-output')), tiers:[]};
  const cacheNode = node.querySelector('.price-cache');
  if (cacheNode && cacheNode.value.trim() !== '') {
    const n = Number(cacheNode.value);
    if (!Number.isFinite(n) || n < 0 || n > 100000) throw new Error('缓存价应为 0 到 100000 之间的数字');
    price.cache = n;
  }
  let previous = 0;
  for (const line of node.querySelector('.price-tiers').value.split('\n').filter(l => l.trim())) {
    const parts = line.trim().split(/\s+/).map(Number);
    if ((parts.length !== 3 && parts.length !== 4) || !parts.every(Number.isFinite) || !Number.isSafeInteger(parts[0]) || parts[0] <= previous || parts[0] > 100000000 || parts.slice(1).some(n => n < 0 || n > 100000)) throw new Error('档位应为递增起点与非负单价，例如 128001 4 16 或 128001 4 16 1');
    previous = parts[0];
    const tier = {min_input:parts[0], input:parts[1], output:parts[2]};
    if (parts.length === 4) tier.cache = parts[3];
    price.tiers.push(tier);
  }
  if (price.tiers.length > 16) throw new Error('最多设置 16 个长度档位');
  return price;
}
function collectPrices() {
  document.querySelectorAll('#price-rows tr[data-index]').forEach(tr => { _priceRows[Number(tr.dataset.index)].price = readPrice(tr); });
}
function renderPrices() {
  const tb = $('#price-rows'); tb.innerHTML = '';
  _priceRows.forEach((r, index) => {
    const tr = document.createElement('tr'); tr.dataset.index = index;
    tr.dataset.ch = r.channel_id; tr.dataset.model = r.model;
    tr.innerHTML = '<td>' + esc(r.channel_name || r.channel_id) + '</td><td class="font-monospace">' + esc(r.model) + '</td><td>' + priceFields(r.price) + '</td><td>' + (r.free ? '<span class="badge badge-free">免费</span>' : '<span class="badge badge-paid">付费</span>') + '</td><td class="small text-muted">' + (r.enabled ? '已启用' : '渠道停用') + '</td><td><button class="btn btn-sm btn-outline-secondary price-reset">设为免费</button></td>';
    bindPriceFields(tr);
    tr.querySelector('.price-reset').onclick = guard(() => { collectPrices(); r.price = 0; renderPrices(); });
    tb.appendChild(tr);
  });
}
$('#prices-save').onclick = guard(async () => {
  collectPrices();
  const byChannel = new Map();
  for (const r of _priceRows) {
    if (!byChannel.has(r.channel_id)) byChannel.set(r.channel_id, Object.create(null));
    byChannel.get(r.channel_id)[r.model] = r.price;
  }
  $('#prices-save').disabled = true;
  let saved = 0;
  try {
    for (const [channel_id, prices] of byChannel) { await api('prices', {method:'POST', json:{channel_id, prices}}); saved++; }
    toast('定价已保存'); await loadPrices();
  } catch (e) { throw new Error('已保存 ' + saved + ' 个渠道；' + e.message + '。未保存内容仍保留，可重试。'); }
  finally { $('#prices-save').disabled = false; }
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
      + '<td class="small text-nowrap"><div>¥' + (u.grant ?? 0).toFixed(6) + '</div><div class="text-muted">累计 ¥' + (u.grant_total ?? 0).toFixed(6) + '</div></td>'
      + '<td class="small text-nowrap"><div>¥' + (u.balance ?? 0).toFixed(6) + '</div><div class="text-muted">累计 ¥' + (u.recharge_total ?? 0).toFixed(6) + '</div></td>'
      + '<td class="small text-nowrap"><div>免费 ' + (u.free_calls ?? 0) + '</div><div class="text-muted">付费 ' + (u.paid_calls ?? 0) + '</div></td>'
      + '<td>' + (u.key_count ?? 0) + '</td>'
      + '<td class="small">' + (u.free_rpm > 0 ? u.free_rpm + ' 次/分' : '不限') + '</td>'
      + '<td class="small">' + (u.paid_rpm > 0 ? u.paid_rpm + ' 次/分' : '不限') + '</td>'
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
      const p = await uiPrompt('付费模型每分钟上限', String(u.paid_rpm ?? 0), {title: '限速 · 0 为不限', placeholder: '0 为不限'});
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
      const fd = await api('user-funds?id=' + encodeURIComponent(u.id) + '&page=1&per=100');
      showUserFinance(u, fd);
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
      + '<td class="text-end small fw-semibold">' + (r.cost > 0 ? '¥' + r.cost.toFixed(6) : '免费') + '</td>';
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
  sign: '签到', draw: '历史支出', prize: '历史入账', call: '调用计费',
};

function showUserFinance(u, fd) {
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
      + '<td class="small ' + cls(dg) + '">' + (dg > 0 ? '+' : '') + dg.toFixed(6) + '</td>'
      + '<td class="small ' + cls(dr) + '">' + (dr > 0 ? '+' : '') + dr.toFixed(6) + '</td>'
      + '<td class="small text-muted">' + esc(r.note || '-') + '</td>';
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


})();
