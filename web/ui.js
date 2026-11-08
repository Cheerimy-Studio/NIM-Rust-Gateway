(() => {
'use strict';
let dialogCount = 0;
window.NGW_UI = {
  dialog({title = '确认操作', body = '', input = null, danger = false, okText = '确定'}) {
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
      (field || (danger ? cancel : ok)).focus();
      if (field) field.select();
    });
  }
};
})();
