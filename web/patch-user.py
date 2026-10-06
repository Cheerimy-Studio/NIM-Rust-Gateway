# -*- coding: utf-8 -*-
# user.js 补丁：登录三态 + 注册/找回/重置 + 拉人活动
import io

p = 'user.js'
s = io.open(p, encoding='utf-8').read()

# 1) showLogin 三态
old = (
"function showLogin() {\n"
"  $('#login-view').style.display = 'flex';\n"
"  $('#panel-view').style.display = 'none';\n"
"}"
)
new = (
"function showLogin() {\n"
"  $('#login-view').style.display = 'flex';\n"
"  $('#panel-view').style.display = 'none';\n"
"  showLoginTab('main');\n"
"}\n"
"\n"
"function showLoginTab(tab) {\n"
"  for (const t of ['main', 'register', 'forgot', 'reset']) {\n"
"    const el = document.getElementById('login-tab-' + t);\n"
"    if (el) el.style.display = t === tab ? 'block' : 'none';\n"
"  }\n"
"  $('#login-err').style.display = 'none';\n"
"}\n"
"\n"
"function lgErr(msg) {\n"
"  const e = document.querySelector('#login-err');\n"
"  e.textContent = msg;\n"
"  e.style.display = 'block';\n"
"}\n"
"\n"
"function resetTokenFromUrl() {\n"
"  return new URLSearchParams(location.search).get('reset') || '';\n"
"}\n"
"function invFromUrl() {\n"
"  return new URLSearchParams(location.search).get('inv') || '';\n"
"}"
)
assert s.count(old) == 1, 'showLogin anchor'
s = s.replace(old, new)

# 2) 启动：?reset 处理 + bindLoginTabs
old = (
"(async () => {\n"
"  try {\n"
"    await loadMe();\n"
"    showPanel();\n"
"    await loadKeys();\n"
"  } catch (e) {\n"
"    showLogin();\n"
"  }\n"
"})();"
)
new = (
"(async () => {\n"
"  bindLoginTabs();\n"
"  const rt = resetTokenFromUrl();\n"
"  if (rt) {\n"
"    window.__resetToken = rt;\n"
"    showLogin();\n"
"    showLoginTab('reset');\n"
"    return;\n"
"  }\n"
"  try {\n"
"    await loadMe();\n"
"    showPanel();\n"
"    await loadKeys();\n"
"  } catch (e) {\n"
"    showLogin();\n"
"  }\n"
"})();"
)
assert s.count(old) == 1, 'boot anchor'
s = s.replace(old, new)

# 3) 处理器绑定（放在 Enter 监听之后）
old = "$('#login-pass').addEventListener('keydown', e => { if (e.key === 'Enter') $('#login-go').click(); });"
assert s.count(old) == 1, 'enter anchor'
new = old + "\n\n" + io.open('patch-user-snippet.js', encoding='utf-8').read()
s = s.replace(old, new)

io.open(p, 'w', encoding='utf-8', newline='').write(s)
print('user.js 补丁 1/2 完成')
