// WebClaw 项目文件格式与导入导出回归测试。
// 与 tests/frontend-rewards.cjs 同一套路：切出纯逻辑，在 vm 里跑，不依赖浏览器。
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const src = fs.readFileSync('web/webclaw.js', 'utf8');

// 按函数名取整段：用大括号配平定位结束位置，改文件后不会因为行号漂移而失效
function fn(name) {
  const re = new RegExp('^function ' + name + '\\(', 'm');
  const m = re.exec(src);
  assert.ok(m, '找不到函数 ' + name);
  const start = m.index;
  const open = src.indexOf('{', start);
  let depth = 0;
  for (let i = open; i < src.length; i++) {
    const c = src[i];
    if (c === '{') depth++;
    else if (c === '}') { depth--; if (depth === 0) return src.slice(start, i + 1); }
  }
  assert.fail('函数 ' + name + ' 的大括号没有闭合');
}

const uidLine = src.match(/^const uid = .*$/m);
assert.ok(uidLine, '找不到 uid');

// 上限常量与 clip 是 normalizeStored 的依赖，一并切出来
const limitLines = src.match(/^const MAX_[A-Z_]+ = .*$/gm) || [];
assert.ok(limitLines.length >= 5, '上限常量缺失');

const code = [
  uidLine[0],
  ...limitLines,
  fn('clip'),
  fn('blankProject'),
  fn('normalizeStored'),
  fn('projectForExport'),
  fn('slug'),
  fn('stamp'),
  fn('download'),
  fn('exportAll'),
  fn('exportOne'),
  fn('uniqueName'),
  fn('extractProjects'),
  fn('consumeFrames'),
].join('\n');

const captured = [];
const ctx = vm.createContext({
  Date, Math, JSON, String, Number, Array, Object, console,
  state: {version: 1, active: '', projects: []},
  toast: () => {},
  // 导出链路的副作用：只记录，不真写盘
  document: {createElement: () => ({click() {}, remove() {}}), body: {appendChild() {}}},
  URL: {createObjectURL: () => 'blob:stub', revokeObjectURL() {}},
  setTimeout: fn => fn(),
});
// 让 download 把 (filename, obj) 记到 captured，其余照常
const patched = code.replace(
  /function download\(filename, obj\) \{/,
  'function download(filename, obj) { captured.push({filename: filename, obj: JSON.stringify(obj, null, 2)}); return;'
);
ctx.captured = captured;
vm.runInContext = vm.runInContext.bind(vm);
const api = vm.runInContext(
  patched + '\n;({blankProject, normalizeStored, projectForExport, extractProjects, exportAll, exportOne, uniqueName, consumeFrames, state})',
  ctx
);

let n = 0;
const ok = (label, fn) => { fn(); n++; console.log('PASS ' + label); };

// 1) 导出 → 导入 完整往返
ok('导出再导入保留模型/系统提示/流式/消息正文', () => {
  const p = api.blankProject('往返项目');
  p.model = 'qwen/qwen3-32b';
  p.system = '你是助手';
  p.stream = false;
  p.messages = [
    {role: 'user', content: '你好', reasoning: '', err: '', at: 1},
    {role: 'assistant', content: '你好！', reasoning: '先打招呼', err: '', at: 2},
  ];
  const back = api.extractProjects({app: 'webclaw', kind: 'single', projects: [api.projectForExport(p)]});
  assert.equal(back.length, 1);
  const q = back[0];
  assert.equal(q.name, '往返项目');
  assert.equal(q.model, 'qwen/qwen3-32b');
  assert.equal(q.system, '你是助手');
  assert.equal(q.stream, false);
  // 跨 vm realm 的数组原型不同，用序列化比较
  assert.equal(JSON.stringify(q.messages.map(m => [m.role, m.content])),
    JSON.stringify([['user', '你好'], ['assistant', '你好！']]));
  assert.equal(q.messages[1].reasoning, '先打招呼');
});

// 2) 导入必须换新 id，避免覆盖本地同名项目
ok('导入重建 id', () => {
  const p = api.blankProject('同名');
  const back = api.extractProjects({projects: [api.projectForExport(p)]});
  assert.notEqual(back[0].id, p.id);
});

// 3) 脏数据清洗：非法角色、空内容、pending 中间态都要丢掉
ok('清洗非法消息与未完成态', () => {
  const p = api.normalizeStored({
    name: '脏数据',
    messages: [
      {role: 'system', content: '越权角色'},
      {role: 'user', content: '保留'},
      {role: 'assistant', content: '', pending: true},
      {role: 'assistant', content: '', reasoning: '', err: ''},
      {role: 'user', content: 123},
      null,
      'nope',
    ],
  });
  assert.equal(p.messages.length, 1);
  assert.equal(p.messages[0].content, '保留');
  assert.equal(p.name, '脏数据');
});

// 4) 四种输入形态
ok('支持整包 / 单体 / 裸数组 / 无效输入', () => {
  const a = api.blankProject('A');
  const b = api.blankProject('B');
  const one = api.projectForExport(a);
  assert.equal(api.extractProjects({kind: 'bundle', projects: [one, api.projectForExport(b)]}).length, 2);
  assert.equal(api.extractProjects([one]).length, 1);
  assert.equal(api.extractProjects(one).length, 1, '单对象靠 messages 字段识别');
  assert.equal(api.extractProjects({nonsense: true}).length, 0);
  assert.equal(api.extractProjects(null).length, 0);
});

// 5) 缺字段不炸
ok('缺字段回落默认值', () => {
  const p = api.normalizeStored({});
  assert.equal(p.model, '');
  assert.equal(p.system, '');
  assert.equal(p.stream, true);
  assert.equal(JSON.stringify(p.messages), "[]");
  assert.ok(p.name.length > 0);
});

// 6) 名称截断
ok('名称按 60 字符截断', () => {
  assert.equal(api.normalizeStored({name: 'x'.repeat(200), messages: []}).name.length, 60);
});

// 7) 导出结构
ok('导出整包为 bundle、单个为 single', () => {
  api.state.projects = [api.blankProject('P1'), api.blankProject('P2')];
  captured.length = 0;
  api.exportAll();
  assert.equal(captured.length, 1);
  const all = JSON.parse(captured[0].obj);
  assert.equal(all.app, 'webclaw');
  assert.equal(all.version, 1);
  assert.equal(all.kind, 'bundle');
  assert.equal(all.projects.length, 2);

  captured.length = 0;
  api.exportOne(api.state.projects[0].id);
  const one = JSON.parse(captured[0].obj);
  assert.equal(one.kind, 'single');
  assert.equal(one.projects.length, 1);
  assert.match(captured[0].filename, /^webclaw-.+-\d{8}-\d{4}\.json$/);
});

// 8) 自洽性
ok('导出的整包可被自己导入', () => {
  api.state.projects = [api.blankProject('自洽')];
  captured.length = 0;
  api.exportAll();
  const back = api.extractProjects(JSON.parse(captured[0].obj));
  assert.equal(back.length, 1);
  assert.equal(back[0].name, '自洽');
});

// 9) 文件名安全：路径分隔符不能出现在下载名里
ok('项目名里的路径字符被替换', () => {
  api.state.projects = [api.blankProject('a/b\\c:d*e?f"g<h>i|j')];
  captured.length = 0;
  api.exportOne(api.state.projects[0].id);
  assert.ok(!/[/\\:*?"<>|]/.test(captured[0].filename), captured[0].filename);
});

// 10) 导入重名：连续重名要一直加后缀，不能停在「（导入）」上互相覆盖
ok('连续重名依次加后缀', () => {
  const taken = new Set(['P', 'P（导入）']);
  assert.equal(api.uniqueName(taken, 'Q'), 'Q');
  assert.equal(api.uniqueName(taken, 'P'), 'P（导入 2）');
  taken.add('P（导入 2）');
  assert.equal(api.uniqueName(taken, 'P'), 'P（导入 3）');
});

// 11) SSE 分帧：CRLF / LF / CR 三种行尾都要认，且残留尾帧不丢
ok('SSE 分帧兼容三种行尾并保留残留', () => {
  for (const sep of ['\r\n\r\n', '\n\n', '\r\r']) {
    const got = [];
    const rest = api.consumeFrames('data: a' + sep + 'data: b' + sep, f => got.push(f));
    assert.equal(JSON.stringify(got), JSON.stringify(['data: a', 'data: b']), sep);
    assert.equal(rest, '', sep);
  }
  // 上游不发末尾空行：残留必须原样交回，交给调用方处理
  const got = [];
  const rest = api.consumeFrames('data: a\n\ndata: tail', f => got.push(f));
  assert.equal(JSON.stringify(got), JSON.stringify(['data: a']));
  assert.equal(rest, 'data: tail');
});

// 12) 一帧多条 data 行都要在
ok('同一帧内的多行 data 全部保留', () => {
  const got = [];
  api.consumeFrames('event: x\ndata: 1\ndata: 2\n\n', f => got.push(f));
  assert.equal(got.length, 1);
  assert.ok(got[0].includes('data: 1') && got[0].includes('data: 2'));
  assert.ok(got[0].includes('event: x'));
});

// 13) 超长内容与超多消息要被截断，否则一个畸形导入就能撑爆 localStorage
ok('超长正文与超量消息被截断', () => {
  const huge = 'x'.repeat(300000);
  const many = [];
  for (let i = 0; i < 900; i++) many.push({role: 'user', content: 'm' + i});
  const p = api.normalizeStored({name: 'big', messages: [{role: 'user', content: huge}, ...many]});
  assert.ok(p.messages[0].content.length <= 200000, '正文未被截断: ' + p.messages[0].content.length);
  assert.ok(p.messages.length <= 500, '消息数未被截断: ' + p.messages.length);
});

// 14) 导入项目数上限
ok('导入项目数超过上限时被截断', () => {
  const arr = [];
  for (let i = 0; i < 260; i++) arr.push({name: 'p' + i, messages: [{role: 'user', content: 'x'}]});
  assert.ok(api.extractProjects(arr).length <= 200);
});

// 15) 安全：令牌绝不能落进 localStorage
ok('状态序列化里不含令牌字段', () => {
  const blob = JSON.stringify({projects: [api.projectForExport(api.blankProject('x'))]});
  assert.ok(!/token|authorization/i.test(blob), blob.slice(0, 120));
});

// 16) 安全：markdown 必须先过 DOMPurify 再进 innerHTML
ok('渲染路径先 sanitize 再落 DOM', () => {
  const start = src.indexOf('const mdRender');
  assert.ok(start > 0, '找不到 mdRender');
  const body = src.slice(start, start + 600);
  // 右值先求值：innerHTML 的赋值来源必须是 sanitize 的结果
  assert.ok(/node\.innerHTML\s*=\s*DOMPurify\.sanitize\(/.test(body), '未先 sanitize 再赋 innerHTML');
  assert.ok(body.includes("rel = 'noopener noreferrer'"), '外链缺 rel 保护');
});

console.log('\n' + n + ' 项通过');
