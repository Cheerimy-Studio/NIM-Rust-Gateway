function bindLoginTabs() {
  $('#go-register').onclick = () => showLoginTab('register');
  $('#go-forgot').onclick = () => showLoginTab('forgot');
  $('#back-login-1').onclick = () => showLoginTab('main');
  $('#back-login-2').onclick = () => showLoginTab('main');
  if (invFromUrl()) showLoginTab('register');
  $('#reg-go').onclick = async () => {
    const email = $('#reg-email').value.trim();
    const pass = $('#reg-pass').value;
    if (!email || pass.length < 6) { lgErr('请输入邮箱和至少 6 位密码'); return; }
    const body = {email: email, password: pass};
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
      showLoginTab('main');
      lgErr('');
      toast(d.message || '重置邮件已发送');
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
    box.innerHTML = '<div class="hint text-center py-5">暂无进行中的活动</div>';
    return;
  }
  for (const ev of evs) {
    box.appendChild(promoCard(ev, d));
  }
}

function promoCard(ev, d) {
  const card = document.createElement('div');
  card.className = 'promo-hero mb-4';
  const st = ev.joined ? ev : null;
  const leftDays = Math.max(0, Math.ceil((ev.expires_at - Date.now() / 1000) / 86400));
  if (!st || !st.joined) {
    card.innerHTML = '<div class="d-flex justify-content-between align-items-center mb-2">'
      + '<div class="fw-bold fs-5">' + esc(ev.name || '拉人活动') + '</div>'
      + (ev.trial ? '<span class="badge bg-warning text-dark">试玩模式</span>' : '')
      + '</div>'
      + '<div class="mb-2">邀请好友注册，赢 <b>¥' + ev.amount + '</b> 余额！</div>'
      + '<div class="small mb-3" style="opacity:.85">活动目标：邀请 ' + ev.target + ' 位好友 · 剩余 ' + leftDays + ' 天</div>'
      + '<button class="pbtn primary" id="promo-join">立即参与</button>';
    card.querySelector('#promo-join').onclick = () => run(async () => {
      await api('promo/join', {method: 'POST', json: {id: ev.id}});
      toast('已加入活动');
      loadPromo();
    });
    return card;
  }
  const pct = ev.amount > 0 ? Math.min(100, Math.round(st.collected / ev.amount * 100)) : 0;
  const paid = st.paid;
  const stages = [];
  const stageRow = (done, ok, label, btnId) =>
    '<div class="promo-stage' + (done ? ' done' : '') + '"><div class="ic">' + (done ? '✓' : (ok ? '→' : '·')) + '</div>'
    + '<div class="tx">' + label + '</div>'
    + (btnId && ok && !done ? '<button class="btn btn-sm btn-warning fw-bold" id="' + btnId + '">领取</button>' : '')
    + '</div>';
  stages.push(stageRow(st.p1, st.eff_invited >= st.t1,
    '邀请 ' + st.t1 + ' 位好友，集齐 20 颗钻石兑换 ¥0.01（钻石 ' + st.diamonds + '/20）', 'p-claim1'));
  stages.push(stageRow(st.p2, st.p1 && st.eff_invited >= st.t1 * 2,
    '再邀请 ' + st.t1 + ' 位好友，集齐 20 枚金币再兑 ¥0.01（金币 ' + st.golds + '/20）', 'p-claim2'));
  stages.push(stageRow(st.p3, st.p2 && st.eff_invited >= st.total,
    '继续邀请满 ' + st.total + ' 位好友，提现审核加速中', null));
  stages.push(stageRow(paid, paid,
    paid ? '¥' + ev.amount + ' 已到账充值余额！' : '拉满 ' + st.total + ' 人，' + ev.amount + ' 元立即到账', null));
  const doublerChip = st.doubler > 0 ? '<span class="promo-chip"><i class="bi bi-stack"></i>翻倍卡 × ' + st.doubler + '</span>' : '';
  const creditChip = st.draw_credits > 0 ? '<span class="promo-chip"><i class="bi bi-ticket-perforated"></i>抽奖次数 × ' + st.draw_credits + '</span>' : '';
  const friends = (st.friends || []).slice(0, 8).map(f =>
    '<div class="small text-muted">' + esc(f.invitee) + '</div>').join('');
  card.innerHTML = '<div class="d-flex justify-content-between align-items-center mb-1">'
    + '<div class="fw-bold fs-5">' + esc(ev.name || '拉人活动') + '</div>'
    + (ev.trial ? '<span class="badge bg-warning text-dark">试玩模式</span>' : '')
    + '</div>'
    + '<div class="small mb-2" style="opacity:.9">剩余 ' + leftDays + ' 天 · 已邀请 <b>' + st.invited + '</b> / ' + ev.target + ' 人</div>'
    + '<div class="promo-amount">¥' + st.collected.toFixed(2) + '<small> / ' + ev.amount.toFixed(2) + '</small></div>'
    + '<div class="promo-bar my-2"><div style="width:' + pct + '%"></div></div>'
    + '<div class="promo-gap">' + (paid ? '已成功提现，余额已到账！' : '还差 <b>¥' + st.remain.toFixed(2) + '</b> 即可提现') + '</div>'
    + '<div class="d-flex gap-2 my-3">' + creditChip + doublerChip + '</div>'
    + stages.join('')
    + '<div class="mt-3 mb-2 small" style="opacity:.9">你的专属邀请链接（好友注册即算你拉新）：</div>'
    + '<div class="promo-link"><input readonly id="promo-link-input" value="' + esc(location.origin + '/user?' + st.link) + '">'
    + '<button class="btn btn-light btn-sm fw-bold" id="promo-copy">复制</button></div>'
    + (friends ? '<div class="mt-3"><div class="small fw-bold mb-1">已拉好友</div>' + friends + '</div>' : '')
    + '<div class="mt-2"><a href="javascript:void(0)" class="small" style="color:#ffe9c9" id="promo-goto-wheel">去大转盘用次数抽奖 →</a></div>';
  card.querySelector('#promo-copy').onclick = () => copyText(card.querySelector('#promo-link-input').value)
    .then(ok => toast(ok ? '邀请链接已复制' : '复制失败'));
  card.querySelector('#promo-goto-wheel').onclick = () => {
    document.querySelector('a[data-p="wheel"]').click();
  };
  const bindClaim = (id, step) => {
    const b = card.querySelector(id);
    if (b) b.onclick = () => run(async () => {
      await api('promo/claim', {method: 'POST', json: {id: ev.id, step: step}});
      toast('领取成功');
      loadPromo();
    });
  };
  bindClaim('#p-claim1', 1);
  bindClaim('#p-claim2', 2);
  return card;
}
