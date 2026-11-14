(() => {
'use strict';
let dialogCount = 0;
window.NGW_UI = {
  paid(p) {
    if (typeof p === 'number') return p > 0;
    if (!p || p.mode !== 'token') return false;
    return Number(p.input) > 0 || Number(p.output) > 0 || Number(p.cache) > 0
      || (p.tiers || []).some(t => Number(t.input) > 0 || Number(t.output) > 0 || Number(t.cache) > 0);
  },
  // 最近 10 次请求热力图：1=成功（绿），2=重试后成功（黄），0=失败（红），空=无数据。
  // 入参按新→旧传入，展示时翻转成旧→新（左旧右新）。
  heatSquares(flags) {
    const vals = Array.isArray(flags) ? flags.map(Number).filter(v => v === 0 || v === 1 || v === 2).slice(0, 10) : [];
    const slots = new Array(10).fill(null);
    for (let i = 0; i < vals.length; i++) slots[10 - vals.length + i] = vals[i];
    const cls = {1: 'ok', 2: 'warn', 0: 'bad'};
    const name = {1: '成功', 2: '重试后成功', 0: '失败'};
    return '<span class="heat" role="img" aria-label="最近 10 次请求，绿=成功，黄=重试后成功，红=失败">' +
      slots.map((v, i) => '<i class="heat-sq ' + (v === null ? 'none' : cls[v]) + '" title="第 ' +
        (i + 1) + ' 格：' + (v === null ? '暂无数据' : name[v]) + '"></i>').join('') + '</span>';
  },
  heatLegend() {
    return '<span class="heat-legend"><i class="heat-sq ok"></i>成功<i class="heat-sq warn"></i>重试成功<i class="heat-sq bad"></i>失败<i class="heat-sq none"></i>无数据</span>';
  },
  pricingText(p) {
    if (!p || typeof p === 'number') return p > 0 ? '¥' + p + ' / 次' : '免费';
    if (p.mode !== 'token') return '未配置';
    let text = '输入 ¥' + p.input + ' · 输出 ¥' + p.output + (p.cache != null ? ' · 缓存 ¥' + p.cache : '') + ' / 百万 token';
    for (const t of p.tiers || []) text += '；输入 ≥ ' + t.min_input + '：¥' + t.input + ' / ¥' + t.output + (t.cache != null ? ' / 缓存 ¥' + t.cache : '');
    return text;
  },
  dialog({title = '确认操作', body = '', input = null, danger = false, okText = '确定', signal = null}) {
    return new Promise(resolve => {
      const previous = document.activeElement;
      const mask = document.createElement('div'); mask.className = 'ui-mask';
      const box = document.createElement('div'); box.className = 'ui-dialog';
      box.setAttribute('role', 'dialog'); box.setAttribute('aria-modal', 'true');
      const heading = document.createElement('div'); heading.className = 'ui-title';
      heading.id = 'ui-dialog-' + (++dialogCount); heading.textContent = title;
      box.setAttribute('aria-labelledby', heading.id);
      const text = document.createElement('div'); text.className = 'ui-msg'; text.textContent = body;
      box.append(heading, text);
      let field = null;
      if (input !== null) {
        field = document.createElement('input'); field.className = 'form-control mt-3';
        field.value = input.value || ''; field.placeholder = input.placeholder || '';
        field.setAttribute('aria-label', title); box.append(field);
      }
      const foot = document.createElement('div'); foot.className = 'ui-foot';
      const cancel = document.createElement('button'); cancel.className = 'btn btn-outline-secondary'; cancel.textContent = '取消';
      const ok = document.createElement('button'); ok.className = 'btn ' + (danger ? 'btn-danger' : 'btn-primary'); ok.textContent = okText;
      cancel.type = ok.type = 'button';
      foot.append(cancel, ok); box.append(foot); mask.append(box); document.body.append(mask);
      let closed = false;
      const finish = value => {
        if (closed) return; closed = true;
        mask.remove(); document.removeEventListener('keydown', onKey, true);
        if (signal) signal.removeEventListener('abort', dismiss);
        if (previous && previous.isConnected) previous.focus();
        resolve(value);
      };
      const dismiss = () => finish(input === null ? false : null);
      const onKey = e => {
        if (e.key === 'Escape') { e.preventDefault(); e.stopImmediatePropagation(); dismiss(); }
        if (e.key === 'Enter' && e.target === field && !e.isComposing) { e.preventDefault(); finish(field.value.trim()); }
        if (e.key === 'Tab') {
          const controls = field ? [field, cancel, ok] : [cancel, ok];
          const first = controls[0], last = controls[controls.length - 1];
          if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); }
          else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
        }
      };
      cancel.onclick = dismiss; ok.onclick = () => finish(field ? field.value.trim() : true);
      mask.onclick = e => { if (e.target === mask) dismiss(); };
      document.addEventListener('keydown', onKey, true);
      if (signal) { signal.addEventListener('abort', dismiss, {once:true}); if (signal.aborted) { dismiss(); return; } }
      (field || (danger ? cancel : ok)).focus();
      if (field) field.select();
    });
  }
};
})();
