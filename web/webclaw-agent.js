(() => {
'use strict';
const MAX_FILE = 256 * 1024, MAX_TOTAL = 2 * 1024 * 1024, MAX_FILES = 64;
const roots = new Map();
function pathOf(value) {
  if (typeof value !== 'string' || !value || value.length > 240 || value.startsWith('/') || /[\\:\u0000-\u001f]/.test(value)) throw new Error('文件路径无效');
  const parts = value.split('/');
  if (parts.some(p => !p || p === '.' || p === '..' || ['__proto__','prototype','constructor'].includes(p))) throw new Error('文件路径无效');
  return parts.join('/');
}
const bytes = text => new TextEncoder().encode(text).length;
function cleanFiles(raw) {
  const files = Object.create(null); let total = 0;
  for (const [key,value] of Object.entries(raw && typeof raw === 'object' ? raw : {})) {
    try {
      const path = pathOf(key);
      if (typeof value !== 'string' || bytes(value) > MAX_FILE || total + bytes(value) > MAX_TOTAL || Object.keys(files).length >= MAX_FILES) continue;
      total += bytes(value); files[path] = value;
    } catch (_) { /* Ignore malformed imported entries. */ }
  }
  return files;
}
function putFile(project, path, content) {
  path = pathOf(path);
  if (typeof content !== 'string' || bytes(content) > MAX_FILE) throw new Error('单个文本文件最多 256 KiB');
  const files = cleanFiles(project.files);
  if (!Object.hasOwn(files,path) && Object.keys(files).length >= MAX_FILES) throw new Error('每个工作区最多 64 个文件');
  const total = Object.entries(files).filter(([p]) => p !== path).reduce((n,[,text]) => n + bytes(text), bytes(content));
  if (total > MAX_TOTAL) throw new Error('工作区文件总量最多 2 MiB');
  files[path] = content; project.files = files;
}
function checkStop(signal) { if (signal?.aborted) throw new DOMException('已停止', 'AbortError'); }
async function directory(project, path, create = false) {
  let parent = roots.get(project.id);
  if (!parent) return null;
  const parts = pathOf(path).split('/'); const name = parts.pop();
  for (const part of parts) parent = await parent.getDirectoryHandle(part, {create});
  return {parent, name};
}
const def = (name,description,properties,required=[]) => ({type:'function',function:{name,description,parameters:{type:'object',properties,required,additionalProperties:false}}});
const pathProp = {type:'string',description:'工作区内的相对文件路径'};
const systemPrompt = `你是 WebClaw，一个在用户浏览器里工作的文件与仓库助手。
规则：
1. 需要查看、搜索、创建、修改、删除或移动文件时，必须调用提供的工具；不要凭空猜测文件内容，也不要把工具调用写成正文或 XML/DSML 标签。
2. 先调用工具拿到结果，再决定下一步；不要编造工具结果。一次可以并行调用多个互不依赖的工具。
3. 路径必须是工作区或仓库内的相对路径（例如 notes/a.md），不要使用绝对路径、盘符、反斜杠或 ..。
4. 写入、删除、移动文件前，先用一句话说明将要做什么；本机目录操作会由用户逐次确认。
5. 读取大文件前先列出目录或搜索；单个文件最多 256 KiB，工作区最多 64 个文件 / 2 MiB。长内容分块读写。
6. 工具返回 error 时，解释原因并尝试可行替代；不要重复完全相同的失败调用。
7. 任务完成后用简短中文总结改了哪些文件、下一步可做什么。
8. 已绑定 GitHub 仓库时，优先用 github_* 工具读取/修改仓库、提交、触发 Actions 并检查结果；提交信息写清目的，不要输出 Token。
9. GitHub 写入、建分支、提 PR 和触发 Actions 需要用户开启「允许 Agent 直接提交」；被拒绝时说明原因，不要绕过。`;
const meta = {
  workspace_list:{label:'列出文件',icon:'bi-folder2-open'},
  workspace_read:{label:'读取',icon:'bi-file-earmark-text'},
  workspace_write:{label:'写入',icon:'bi-pencil-square'},
  workspace_search:{label:'搜索',icon:'bi-search'},
  workspace_delete:{label:'删除',icon:'bi-trash3'},
  workspace_move:{label:'移动',icon:'bi-arrow-left-right'},
  github_tree:{label:'仓库文件',icon:'bi-github'},
  github_read:{label:'读取仓库',icon:'bi-file-earmark-code'},
  github_commit:{label:'提交仓库',icon:'bi-cloud-upload'},
  github_write:{label:'写入仓库',icon:'bi-pencil-square'},
  github_delete:{label:'删除仓库文件',icon:'bi-trash3'},
  github_branch:{label:'创建分支',icon:'bi-bezier2'},
  github_actions_run:{label:'触发 Actions',icon:'bi-play-circle'},
  github_actions_status:{label:'Actions 状态',icon:'bi-activity'},
  github_pr:{label:'创建 PR',icon:'bi-git'},
};
const tools = [
  def('workspace_list','列出当前浏览器工作区文件。连接本机目录时只列出用户所选目录，不读取文件正文。',{}),
  def('workspace_read','读取工作区中的 UTF-8 文本文件。', {path:pathProp}, ['path']),
  def('workspace_write','创建或替换 UTF-8 文本文件。浏览器工作区内直接保存；本机目录写入需用户确认。', {path:pathProp,content:{type:'string'}}, ['path','content']),
  def('workspace_search','在工作区或本机目录中搜索文本（正则，非法时按子串），返回文件、行号与片段。', {pattern:{type:'string',description:'要搜索的文本或正则'},path:{type:'string',description:'限定目录前缀，可省略'}}, ['pattern']),
  def('workspace_delete','删除工作区中的文件。删除本机目录文件前需用户确认；不支持删除目录。', {path:pathProp}, ['path']),
  def('workspace_move','移动或重命名文件。本机目录操作写入目标前需用户确认。', {from:pathProp,to:pathProp}, ['from','to']),
  def('github_tree','列出已绑定 GitHub 仓库的文件树（只读）。', {path:{type:'string',description:'限定目录前缀，可省略'}}),
  def('github_read','读取已绑定 GitHub 仓库中的 UTF-8 文本文件。', {path:pathProp}, ['path']),
  def('github_commit','一次提交多个文本文件到绑定分支（创建/更新）。需要用户开启允许直接提交。', {
    message:{type:'string',description:'提交信息'},
    files:{type:'array',items:{type:'object',properties:{path:{type:'string'},content:{type:'string'}},required:['path','content'],additionalProperties:false}}
  }, ['message','files']),
  def('github_write','创建或替换 GitHub 仓库中的单个文本文件并提交。', {path:pathProp,content:{type:'string'},message:{type:'string'}}, ['path','content','message']),
  def('github_delete','删除 GitHub 仓库中的文件并提交。', {path:pathProp,message:{type:'string'}}, ['path','message']),
  def('github_branch','从当前分支创建新分支。', {name:{type:'string',description:'新分支名'}}, ['name']),
  def('github_actions_run','触发 GitHub Actions 的 workflow_dispatch。', {workflow:{type:'string',description:'workflow 文件名或 ID'},ref:{type:'string',description:'分支，默认绑定分支'}}, ['workflow']),
  def('github_actions_status','查看最近一次或指定 Actions 运行的状态、任务与步骤。', {run_id:{type:'number',description:'可选，指定 run id'}}),
  def('github_pr','创建 Pull Request。', {title:{type:'string'},head:{type:'string',description:'源分支'},base:{type:'string',description:'目标分支'},body:{type:'string'}}, ['title','head','base']),
];

async function readText(project, path, signal) {
  const dir = await directory(project,path);
  if (dir) {
    const handle = await dir.parent.getFileHandle(dir.name); const file = await handle.getFile();
    if (file.size > MAX_FILE) throw new Error('文件超过 256 KiB，请选取较小的文本文件');
    const content = await file.text();
    if (content.includes('\u0000') || content.includes('\ufffd')) throw new Error('仅支持 UTF-8 文本文件');
    checkStop(signal); return content;
  }
  if (!Object.hasOwn(project.files || {},path)) throw new Error('文件不存在');
  return project.files[path];
}
async function deleteFile(project, path, signal, confirmDelete) {
  if (roots.has(project.id)) {
    const dir = await directory(project,path);
    if (!dir) throw new Error('本机目录模式请使用目录中的相对路径');
    const handle = await dir.parent.getFileHandle(dir.name); // 目录会在这里抛错
    if (handle.kind !== 'file') throw new Error('只能删除文件');
    if (!confirmDelete || !await confirmDelete(path)) throw new Error('用户取消了删除');
    checkStop(signal);
    await dir.parent.removeEntry(dir.name);
    return {path,deleted:'local-directory'};
  }
  if (!Object.hasOwn(project.files || {},path)) throw new Error('文件不存在');
  if (!confirmDelete || !await confirmDelete(path)) throw new Error('用户取消了删除');
  checkStop(signal);
  const files = Object.assign(Object.create(null), project.files); delete files[path]; project.files = files;
  return {path,deleted:'browser'};
}
async function moveFile(project, from, to, signal, confirmMove) {
  const target = pathOf(to);
  if (roots.has(project.id)) {
    const content = await readText(project, from, signal);
    if (!confirmMove || !await confirmMove(from, target)) throw new Error('用户取消了移动');
    checkStop(signal);
    const dir = await directory(project,target,true);
    const handle = await dir.parent.getFileHandle(dir.name,{create:true});
    const writer = await handle.createWritable();
    try { checkStop(signal); await writer.write(content); checkStop(signal); await writer.close(); }
    catch (e) { await writer.abort().catch(() => {}); throw e; }
    const src = await directory(project,from);
    if (src) await src.parent.removeEntry(src.name);
    return {from,to:target,moved:'local-directory'};
  }
  const content = await readText(project, from, signal);
  if (!confirmMove || !await confirmMove(from, target)) throw new Error('用户取消了移动');
  checkStop(signal); putFile(project,target,content);
  const files = Object.assign(Object.create(null), project.files); delete files[from]; project.files = files;
  return {from,to:target,moved:'browser'};
}
async function search(project, pattern, prefix, signal) {
  let re = null; try { re = new RegExp(pattern, 'i'); } catch (_) { re = null; }
  const scope = prefix ? pathOf(prefix).replace(/\/?$/, '/') : '';
  const matches = []; let scanned = 0, truncated = false;
  const scan = (path, text) => {
    scanned++;
    const lines = text.split('\n');
    for (let i = 0; i < lines.length; i++) {
      const hit = re ? re.test(lines[i]) : lines[i].toLowerCase().includes(pattern.toLowerCase());
      if (hit) { matches.push({path,line:i+1,text:lines[i].slice(0,200)}); if (matches.length >= 100) { truncated = true; return; } }
    }
  };
  if (roots.has(project.id)) {
    async function walk(handle, base, depth) {
      if (depth > 6 || truncated) return;
      for await (const [key,entry] of handle.entries()) {
        checkStop(signal);
        if (scanned >= 200 || truncated) { truncated = true; return; }
        if (key.startsWith('.') || key === 'node_modules' || key === 'target') continue;
        const path = base + key;
        if (scope && !path.startsWith(scope)) continue;
        if (entry.kind === 'directory') { await walk(entry,path + '/',depth + 1); }
        else {
          const file = await entry.getFile();
          if (file.size > MAX_FILE) continue;
          const text = await file.text();
          if (text.includes('\u0000')) continue;
          scan(path, text);
        }
      }
    }
    await walk(roots.get(project.id),'',0);
    return {matches, scanned, truncated, location:'local-directory'};
  }
  for (const [path, text] of Object.entries(project.files || {})) {
    if (truncated) break;
    if (scope && !path.startsWith(scope)) continue;
    checkStop(signal); scan(path, text);
  }
  return {matches, scanned, truncated, location:'browser'};
}
/* ---------------------------------------------------------------- GitHub */
const GH_MAX_FILE = 256 * 1024, GH_MAX_TOTAL = 2 * 1024 * 1024;

function ghCfg(project) {
  const g = project.github || {};
  const repo = String(g.repo || '').trim();
  const m = repo.match(/^([A-Za-z0-9_.-]+)\/([A-Za-z0-9_.-]+)$/);
  if (!m) throw new Error('先在右侧绑定 GitHub 仓库（owner/repo）');
  const token = String(g.token || '').trim();
  if (!token) throw new Error('缺少 GitHub Token');
  return {
    owner: m[1], repo: m[2],
    branch: String(g.branch || 'main').trim() || 'main',
    token, allowWrite: g.allowWrite === true,
  };
}
const ghPath = p => String(p || '').split('/').map(encodeURIComponent).join('/');
const ghBranch = b => String(b || '').split('/').map(encodeURIComponent).join('/');

function b64e(text) {
  const bytes = new TextEncoder().encode(text);
  let bin = '';
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin);
}
function b64d(text) {
  const bin = atob(String(text || '').replace(/\s+/g, ''));
  const bytes = Uint8Array.from(bin, c => c.charCodeAt(0));
  return new TextDecoder().decode(bytes);
}
async function ghFetch(project, path, opts = {}) {
  const g = ghCfg(project);
  const headers = {
    Accept: 'application/vnd.github+json',
    Authorization: 'Bearer ' + g.token,
    'X-GitHub-Api-Version': '2022-11-28',
    ...(opts.headers || {}),
  };
  if (opts.body && !headers['Content-Type']) headers['Content-Type'] = 'application/json';
  const res = await fetch('https://api.github.com/repos/' + g.owner + '/' + g.repo + path, { ...opts, headers });
  if (!res.ok) {
    let detail = '';
    try { const j = await res.json(); detail = j.message || ''; } catch (_) { /* 非 JSON 错误体 */ }
    throw new Error('GitHub ' + res.status + (detail ? ': ' + detail : ''));
  }
  return res;
}
async function ghJson(project, path, opts = {}) {
  const res = await ghFetch(project, path, opts);
  if (res.status === 204) return null;
  return await res.json();
}
async function ghHead(project) {
  const g = ghCfg(project);
  const ref = await ghJson(project, '/git/ref/heads/' + ghBranch(g.branch));
  return ref.object.sha;
}
async function githubRepo(project) {
  return await ghJson(project, '');
}
async function githubTree(project, prefix) {
  const g = ghCfg(project);
  const head = await ghHead(project);
  const commit = await ghJson(project, '/git/commits/' + head);
  const tree = await ghJson(project, '/git/trees/' + commit.tree.sha + '?recursive=1');
  const scope = String(prefix || '').replace(/^\/+|\/+$/g, '');
  const all = (tree.tree || []).filter(x => x.type === 'blob');
  const files = all
    .filter(x => !scope || x.path === scope || x.path.startsWith(scope + '/'))
    .slice(0, 200)
    .map(x => ({ path: x.path, size: x.size || 0 }));
  return { branch: g.branch, files, truncated: !!tree.truncated || files.length >= 200 };
}
async function githubRead(project, path) {
  const g = ghCfg(project);
  const clean = pathOf(path);
  const data = await ghJson(project, '/contents/' + ghPath(clean) + '?ref=' + encodeURIComponent(g.branch));
  if (Array.isArray(data)) {
    return { path: clean, entries: data.slice(0, 200).map(x => ({ path: x.path, type: x.type, size: x.size || 0 })) };
  }
  if (!data || data.type !== 'file' || typeof data.content !== 'string') throw new Error('不是可读取的文件');
  const content = b64d(data.content);
  if (bytes(content) > GH_MAX_FILE || content.includes('\u0000')) throw new Error('只支持 256 KiB 以内的 UTF-8 文本文件');
  return { path: clean, sha: data.sha, size: data.size, content };
}
async function githubCommit(project, message, files) {
  const g = ghCfg(project);
  if (!g.allowWrite) throw new Error('请先在右侧开启「允许 Agent 直接提交」');
  message = String(message || '').trim();
  if (!message || message.length > 500) throw new Error('提交信息不能为空且不超过 500 字符');
  if (!Array.isArray(files) || !files.length || files.length > 50) throw new Error('一次提交 1 到 50 个文件');
  const head = await ghHead(project);
  const base = await ghJson(project, '/git/commits/' + head);
  const entries = [];
  let total = 0;
  for (const f of files) {
    const path = pathOf(f && f.path);
    if (f.content === null) {
      entries.push({ path, mode: '100644', type: 'blob', sha: null });
      continue;
    }
    if (typeof f.content !== 'string') throw new Error('文件内容必须是字符串');
    total += bytes(f.content);
    if (bytes(f.content) > GH_MAX_FILE || total > GH_MAX_TOTAL) throw new Error('单文件 256 KiB / 总量 2 MiB 以内');
    const blob = await ghJson(project, '/git/blobs', {
      method: 'POST', body: JSON.stringify({ content: b64e(f.content), encoding: 'base64' }),
    });
    entries.push({ path, mode: '100644', type: 'blob', sha: blob.sha });
  }
  const tree = await ghJson(project, '/git/trees', {
    method: 'POST', body: JSON.stringify({ base_tree: base.tree.sha, tree: entries }),
  });
  const commit = await ghJson(project, '/git/commits', {
    method: 'POST', body: JSON.stringify({ message, tree: tree.sha, parents: [head] }),
  });
  await ghJson(project, '/git/refs/heads/' + ghBranch(g.branch), {
    method: 'PATCH', body: JSON.stringify({ sha: commit.sha, force: false }),
  });
  return { commit: commit.sha, url: commit.html_url, branch: g.branch, files: entries.map(e => e.path) };
}
async function githubBranch(project, name) {
  const g = ghCfg(project);
  if (!g.allowWrite) throw new Error('请先在右侧开启「允许 Agent 直接提交」');
  name = String(name || '').trim();
  if (!/^[A-Za-z0-9._/-]{1,200}$/.test(name) || name.startsWith('/') || name.endsWith('/') || name.includes('..')) {
    throw new Error('分支名无效');
  }
  const head = await ghHead(project);
  await ghJson(project, '/git/refs', {
    method: 'POST', body: JSON.stringify({ ref: 'refs/heads/' + name, sha: head }),
  });
  return { branch: name, from: g.branch, sha: head };
}
async function githubActionsRun(project, workflow, ref) {
  const g = ghCfg(project);
  if (!g.allowWrite) throw new Error('请先在右侧开启「允许 Agent 直接提交」');
  workflow = String(workflow || '').trim();
  if (!workflow) throw new Error('workflow 不能为空');
  ref = String(ref || g.branch).trim();
  await ghJson(project, '/actions/workflows/' + encodeURIComponent(workflow) + '/dispatches', {
    method: 'POST', body: JSON.stringify({ ref }),
  });
  return { triggered: true, workflow, ref };
}
async function githubActionsStatus(project, runId) {
  const g = ghCfg(project);
  let run = null;
  if (runId) {
    run = await ghJson(project, '/actions/runs/' + encodeURIComponent(String(runId)));
  } else {
    const data = await ghJson(project, '/actions/runs?branch=' + encodeURIComponent(g.branch) + '&per_page=5');
    run = (data.workflow_runs || [])[0] || null;
  }
  if (!run) return { runs: [], message: '该分支还没有 Actions 运行' };
  const jobs = await ghJson(project, '/actions/runs/' + run.id + '/jobs?per_page=100');
  return {
    id: run.id, name: run.name, status: run.status, conclusion: run.conclusion,
    event: run.event, head_branch: run.head_branch, created_at: run.created_at,
    html_url: run.html_url,
    jobs: (jobs.jobs || []).slice(0, 30).map(j => ({
      name: j.name, status: j.status, conclusion: j.conclusion,
      steps: (j.steps || []).slice(0, 40).map(s => ({ name: s.name, status: s.status, conclusion: s.conclusion })),
    })),
  };
}
async function githubRuns(project) {
  const g = ghCfg(project);
  const data = await ghJson(project, '/actions/runs?branch=' + encodeURIComponent(g.branch) + '&per_page=6');
  return (data.workflow_runs || []).map(r => ({
    id: r.id, name: r.name, status: r.status, conclusion: r.conclusion,
    event: r.event, created_at: r.created_at, html_url: r.html_url,
  }));
}
async function githubPr(project, title, head, base, body) {
  const g = ghCfg(project);
  if (!g.allowWrite) throw new Error('请先在右侧开启「允许 Agent 直接提交」');
  title = String(title || '').trim();
  head = String(head || '').trim();
  base = String(base || g.branch).trim();
  if (!title || !head) throw new Error('PR 标题和源分支不能为空');
  const pr = await ghJson(project, '/pulls', {
    method: 'POST', body: JSON.stringify({ title, head, base, body: String(body || '') }),
  });
  return { number: pr.number, url: pr.html_url, state: pr.state };
}

async function execute(project, call, opts = {}) {
  const {signal} = opts; const confirmWrite = opts.confirmWrite;
  checkStop(signal);
  if (call?.type !== 'function' || !tools.some(t => t.function.name === call.function?.name)) throw new Error('不支持的工具');
  if (typeof call.function.arguments !== 'string' || call.function.arguments.length > MAX_FILE * 2) throw new Error('工具参数过长');
  const args = JSON.parse(call.function.arguments || '{}');
  if (!args || typeof args !== 'object' || Array.isArray(args)) throw new Error('工具参数必须为对象');
  const name = call.function.name;
  if (name.startsWith('github_')) {
    ghCfg(project);
    if (name === 'github_tree') return await githubTree(project, args.path ? String(args.path) : '');
    if (name === 'github_read') return await githubRead(project, args.path);
    if (name === 'github_commit') {
      if (!Array.isArray(args.files)) throw new Error('files 必须是数组');
      return await githubCommit(project, args.message, args.files.map(f => ({ path: f && f.path, content: f && f.content })));
    }
    if (name === 'github_write') {
      const path = pathOf(args.path);
      if (typeof args.content !== 'string') throw new Error('文件内容必须是字符串');
      return await githubCommit(project, args.message, [{ path, content: args.content }]);
    }
    if (name === 'github_delete') return await githubCommit(project, args.message, [{ path: pathOf(args.path), content: null }]);
    if (name === 'github_branch') return await githubBranch(project, args.name);
    if (name === 'github_actions_run') return await githubActionsRun(project, args.workflow, args.ref);
    if (name === 'github_actions_status') return await githubActionsStatus(project, args.run_id);
    if (name === 'github_pr') return await githubPr(project, args.title, args.head, args.base, args.body);
    throw new Error('不支持的 GitHub 工具');
  }
  if (name === 'workspace_list') {
    if (!roots.has(project.id)) return {files:Object.keys(project.files || {}), location:'browser'};
    const paths = [];
    async function walk(handle, prefix, depth) {
      if (depth > 6) return;
      for await (const [key,entry] of handle.entries()) {
        checkStop(signal);
        if (paths.length >= MAX_FILES) return;
        if (key.startsWith('.') || key === 'node_modules' || key === 'target') continue;
        const path = prefix + key;
        if (entry.kind === 'directory') { paths.push(path + '/'); await walk(entry,path + '/',depth + 1); }
        else paths.push(path);
      }
    }
    await walk(roots.get(project.id),'',0);
    return {files:paths, location:'local-directory', truncated:paths.length >= MAX_FILES};
  }
  if (name === 'workspace_search') {
    const pattern = String(args.pattern || '').slice(0, 200);
    if (!pattern) throw new Error('搜索内容不能为空');
    return await search(project, pattern, args.path ? String(args.path) : '', signal);
  }
  if (name === 'workspace_delete') {
    return await deleteFile(project, pathOf(args.path), signal, opts.confirmDelete);
  }
  if (name === 'workspace_move') {
    return await moveFile(project, pathOf(args.from), args.to, signal, opts.confirmMove);
  }
  const path = pathOf(args.path);
  if (name === 'workspace_read') {
    const dir = await directory(project,path);
    if (dir) {
      const handle = await dir.parent.getFileHandle(dir.name); const file = await handle.getFile();
      if (file.size > MAX_FILE) throw new Error('文件超过 256 KiB，请选取较小的文本文件');
      const content = await file.text();
      if (content.includes('\u0000') || content.includes('\ufffd')) throw new Error('仅支持 UTF-8 文本文件');
      checkStop(signal); return {path,content};
    }
    if (!Object.hasOwn(project.files || {},path)) throw new Error('文件不存在');
    return {path,content:project.files[path]};
  }
  if (typeof args.content !== 'string' || bytes(args.content) > MAX_FILE) throw new Error('单个文本文件最多 256 KiB');
  if (roots.has(project.id)) {
    if (!confirmWrite || !await confirmWrite(path,args.content)) throw new Error('用户取消了文件写入');
    checkStop(signal);
    const dir = await directory(project,path,true);
    const handle = await dir.parent.getFileHandle(dir.name,{create:true});
    const writer = await handle.createWritable();
    try { checkStop(signal); await writer.write(args.content); checkStop(signal); await writer.close(); }
    catch (e) { await writer.abort().catch(() => {}); throw e; }
    return {path,bytes:bytes(args.content),saved:'local-directory'};
  }
  checkStop(signal); putFile(project,path,args.content);
  return {path,bytes:bytes(args.content),saved:'browser'};
}
window.NGW_AGENT = {tools, meta, systemPrompt, cleanFiles, putFile, execute, MAX_FILE,
  github: {repo: githubRepo, runs: githubRuns, status: githubActionsStatus},
  connected: project => roots.has(project.id),
  disconnect: project => roots.delete(project.id),
  async connect(project) {
    if (!window.showDirectoryPicker) throw new Error('此浏览器不支持目录访问，请用添加文件与下载文件');
    const handle = await window.showDirectoryPicker({mode:'readwrite'}); roots.set(project.id,handle); return handle.name;
  },
};
})();
