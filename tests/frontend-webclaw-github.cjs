// GitHub tools in the browser agent: API shape, auth header, read/commit/status.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const calls = [];
const json = (body, status = 200) => ({
  ok: status >= 200 && status < 300,
  status,
  async json() { return body; },
});
const route = (url, opts = {}) => {
  calls.push({ url, opts });
  const method = (opts.method || 'GET').toUpperCase();
  if (url.endsWith('/git/ref/heads/main') && method === 'GET') return json({ object: { sha: 'c1' } });
  if (url.endsWith('/git/commits/c1')) return json({ tree: { sha: 't1' } });
  if (url.endsWith('/git/blobs') && method === 'POST') return json({ sha: 'blob1' });
  if (url.endsWith('/git/trees') && method === 'POST') return json({ sha: 't2' });
  if (url.endsWith('/git/commits') && method === 'POST') return json({ sha: 'c2', html_url: 'https://github.com/o/r/commit/c2' });
  if (url.endsWith('/git/refs/heads/main') && method === 'PATCH') return json({});
  if (url.includes('/contents/a.md')) return json({ type: 'file', sha: 'f1', size: 5, content: btoa('hello') });
  if (url.includes('/actions/runs?branch=main')) return json({ workflow_runs: [{ id: 1, name: 'CI', status: 'completed', conclusion: 'success', event: 'push', created_at: '2026-01-01T00:00:00Z', html_url: 'https://github.com/o/r/actions/runs/1' }] });
  if (url.includes('/actions/runs/1/jobs')) return json({ jobs: [{ name: 'build', status: 'completed', conclusion: 'success', steps: [{ name: 'test', status: 'completed', conclusion: 'success' }] }] });
  return json({ message: 'not found' }, 404);
};

const sandbox = {
  window: {}, console,
  TextEncoder, TextDecoder, Uint8Array, Promise, setTimeout,
  btoa: s => Buffer.from(s, 'binary').toString('base64'),
  atob: s => Buffer.from(s, 'base64').toString('binary'),
  fetch: async (url, opts = {}) => route(url, opts),
  DOMException,
};
sandbox.globalThis = sandbox;
vm.createContext(sandbox);
vm.runInContext(fs.readFileSync('web/webclaw-agent.js', 'utf8'), sandbox);
const agent = sandbox.window.NGW_AGENT;

const tool = (name, args) => ({ type: 'function', function: { name, arguments: JSON.stringify(args) } });
const project = { id: 'gh', github: { repo: 'owner/repo', branch: 'main', token: 'secret-token', allowWrite: true } };

(async () => {
  let n = 0;
  const ok = (label, fn) => { fn(); n++; console.log('PASS ' + label); };

  const read = await agent.execute(project, tool('github_read', { path: 'a.md' }));
  ok('读取仓库文件并解码 UTF-8', () => assert.equal(read.content, 'hello'));

  const commit = await agent.execute(project, tool('github_write', { path: 'a.md', content: 'world', message: 'update a' }));
  ok('写入仓库走 Git Data API 并返回提交', () => {
    assert.equal(commit.commit, 'c2');
    assert.equal(JSON.stringify(commit.files), JSON.stringify(['a.md']));
  });

  const status = await agent.execute(project, tool('github_actions_status', {}));
  ok('读取 Actions 运行与步骤状态', () => {
    assert.equal(status.conclusion, 'success');
    assert.equal(status.jobs[0].steps[0].conclusion, 'success');
  });

  ok('请求携带 GitHub Token 且不进入工具结果', () => {
    assert.ok(calls.some(c => c.opts.headers && c.opts.headers.Authorization === 'Bearer secret-token'));
    assert.ok(!JSON.stringify(commit).includes('secret-token'));
  });

  const readOnly = { id: 'ro', github: { repo: 'owner/repo', branch: 'main', token: 'secret-token', allowWrite: false } };
  await assert.rejects(() => agent.execute(readOnly, tool('github_write', { path: 'a.md', content: 'x', message: 'x' })), /允许 Agent/);
  ok('未开启允许提交时拒绝写仓库', () => {});

  console.log(`\n${n} 项通过`);
})().catch(e => { console.error(e); process.exit(1); });
