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
const tools = [
  def('workspace_list','列出当前浏览器工作区文件。连接本机目录时只列出用户所选目录，不读取文件正文。',{}),
  def('workspace_read','读取工作区中的 UTF-8 文本文件。', {path:pathProp}, ['path']),
  def('workspace_write','创建或替换 UTF-8 文本文件。浏览器工作区内直接保存；本机目录写入需用户确认。', {path:pathProp,content:{type:'string'}}, ['path','content']),
  def('workspace_search','在工作区或本机目录中搜索文本（正则，非法时按子串），返回文件、行号与片段。', {pattern:{type:'string',description:'要搜索的文本或正则'},path:{type:'string',description:'限定目录前缀，可省略'}}, ['pattern']),
  def('workspace_delete','删除工作区中的文件。删除本机目录文件前需用户确认；不支持删除目录。', {path:pathProp}, ['path']),
  def('workspace_move','移动或重命名文件。本机目录操作写入目标前需用户确认。', {from:pathProp,to:pathProp}, ['from','to']),
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
async function execute(project, call, opts = {}) {
  const {signal} = opts; const confirmWrite = opts.confirmWrite;
  checkStop(signal);
  if (call?.type !== 'function' || !tools.some(t => t.function.name === call.function?.name)) throw new Error('不支持的工具');
  if (typeof call.function.arguments !== 'string' || call.function.arguments.length > MAX_FILE * 2) throw new Error('工具参数过长');
  const args = JSON.parse(call.function.arguments || '{}');
  if (!args || typeof args !== 'object' || Array.isArray(args)) throw new Error('工具参数必须为对象');
  const name = call.function.name;
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
window.NGW_AGENT = {tools, cleanFiles, putFile, execute, MAX_FILE,
  connected: project => roots.has(project.id),
  disconnect: project => roots.delete(project.id),
  async connect(project) {
    if (!window.showDirectoryPicker) throw new Error('此浏览器不支持目录访问，请用添加文件与下载文件');
    const handle = await window.showDirectoryPicker({mode:'readwrite'}); roots.set(project.id,handle); return handle.name;
  },
};
})();
