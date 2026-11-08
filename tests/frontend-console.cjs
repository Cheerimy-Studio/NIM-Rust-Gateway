const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

class Element {
  constructor(tag = 'div') {
    this.tagName = tag; this.children = []; this.style = {}; this.attributes = {};
    this.className = ''; this.textContent = ''; this.value = ''; this.isConnected = true;
  }
  set innerHTML(value) {
    this.children = Array.from(String(value).matchAll(/<td\b/g), () => new Element('td'));
  }
  append(...nodes) { this.children.push(...nodes); }
  appendChild(node) { this.children.push(node); }
  setAttribute(key, value) { this.attributes[key] = value; }
  remove() { this.isConnected = false; }
  select() {}
  focus() { document.activeElement = this; }
}
const nodes = new Map();
const $ = selector => {
  if (!nodes.has(selector)) nodes.set(selector, new Element());
  return nodes.get(selector);
};
let confirmation = false;
const requests = [];
const document = {
  body: new Element('body'), activeElement: new Element('button'),
  createElement: tag => new Element(tag), handlers: {},
  addEventListener(name, fn) { this.handlers[name] = fn; },
  removeEventListener(name, fn) { if (this.handlers[name] === fn) delete this.handlers[name]; },
};
const context = vm.createContext({
  document, window: {}, console, $, esc: String, fmtTime: String,
  setTimeout: () => 1,
  uiConfirm: async () => confirmation,
  run: async fn => fn(), copyText: async () => true,
  api: async (path, options) => {
    requests.push([path, options]);
    return path === 'keys' ? {rows:[{id:'k1', name:'测试 Key', key:'sk-test-key', enabled:true, kind:'all'}]} : {ok:true};
  },
});
const source = fs.readFileSync('web/user.js', 'utf8');
vm.runInContext(source.slice(source.indexOf('function toast('), source.indexOf('\nfunction fmtTime(')), context);
vm.runInContext(source.slice(source.indexOf('async function loadKeys()'), source.indexOf('\nlet LOGS_STATE')), context);
vm.runInContext(fs.readFileSync('web/ui.js', 'utf8'), context);
(async () => {
  context.toast('已保存');
  const message = document.body.children.at(-1);
  assert.ok(message.className.split(' ').includes('show'));
  assert.equal(message.attributes.role, 'status');
  console.log('PASS 用户提示消息可见并向辅助技术播报');
  await context.loadKeys();
  let row = $('#key-rows').children[0];
  assert.equal(row.children.length, 6);
  let deleteButton = row.children.at(-1).children.at(-1);
  await deleteButton.onclick();
  assert.equal(requests.filter(([path]) => path === 'keys/op').length, 0);
  confirmation = true;
  await deleteButton.onclick();
  assert.equal(requests.filter(([path]) => path === 'keys/op').length, 1);
  console.log('PASS 删除 Key 等待确认，取消不发送请求，操作列不重复');

  const original = new Element('button'); original.focus();
  const dialog = context.window.NGW_UI.dialog({title:'删除', body:'删除测试项目', danger:true});
  const mask = document.body.children.at(-1), box = mask.children[0], foot = box.children.at(-1);
  const [cancel, ok] = foot.children;
  assert.equal(box.attributes.role, 'dialog');
  assert.equal(document.activeElement, cancel);
  let prevented = false;
  document.handlers.keydown({key:'Tab',shiftKey:true,preventDefault(){prevented=true;}});
  assert.equal(document.activeElement, ok); assert.ok(prevented);
  document.handlers.keydown({key:'Escape',preventDefault(){},stopImmediatePropagation(){}});
  assert.equal(await dialog, false); assert.equal(document.activeElement, original);
  assert.equal(document.handlers.keydown, undefined);
  console.log('PASS 破坏性弹窗默认取消，焦点循环和关闭恢复正常');
})().catch(error => { console.error(error); process.exitCode = 1; });
