# Aocker 管理器

把多个 [zcode-api](https://github.com/TriDefender/zcode-api) 容器编排起来：一个容器一个
Z.AI / bigmodel 账号，对外一个 API 入口，**一把 Key 绑定一个账号**，每个账号独享一个
Cloudflare WARP 出口 IP。

```
客户端A ──key a1──┐
客户端B ──key a2──┼─▶ Aocker :9000 ──┬─▶ zc-a1 ──HTTPS_PROXY──▶ warp-a1（WARP IP #1）
客户端C ──key a3──┘   查表反代        ├─▶ zc-a2 ──HTTPS_PROXY──▶ warp-a2（WARP IP #2）
                                     └─▶ ...
              Aocker ──生成/收敛──▶ docker compose（外部网络 zcm-mesh）
```

- 反代只做认证与字节级转发，不解析业务体、不做协议转换、不跨容器重试；
  zcode-api 本身就提供 OpenAI / Anthropic / Responses 标准端点。
- 每账号一个 `caomingjun/warp` 容器，gost 在 1080 端口同收 HTTP 与 SOCKS5，
  zcode 容器经 `HTTPS_PROXY` 走本账号的 WARP。注册数据独立成卷：
  重启不变 IP，「换 IP」删卷重建即随机新 IP。
- 重启后自动拉起全部容器；compose 文件是期望状态，漂移循环每 5 分钟收敛。
- 单进程 Python（FastAPI + httpx），无数据库，注册表为 `data/accounts.yaml`。

> 提醒：zcode-api 是第三方逆向项目，上游存在风控与封号案例
> （其 issue #47/#49/#63）。WARP 分流缓解「同 IP 多号」，不消除风险。自担。

---

## 要求

- Linux x86_64 / arm64（Windows 仅可做开发，WARP 容器依赖 `/dev/net/tun`）
- Python 3.9+，直接用系统 `python3`，无需 venv
- Docker Engine + compose v2 插件
- 放行 9000 端口，建议套 nginx/TLS；账号容器端口只绑 `127.0.0.1`

## 快速开始（python3 + screen）

```bash
python3 -m pip install -r requirements.txt
# Debian 12+ 若提示 externally-managed-environment，改用：
#   python3 -m pip install --user --break-system-packages -r requirements.txt

screen -S aocker
./run.sh                 # 等价 python3 -m zcm，首次启动打印管理密码与脚本令牌
# Ctrl+A D 脱离，screen -r aocker 回来看输出
```

打开 `http://服务器IP:9000/admin`，账号 `admin` + 初始密码登录。
忘密码：`python3 -m zcm --reset-password '新密码'`。

## 添加账号

1. 面板「添加账号」：选上游（z.ai / bigmodel）、套餐、出口（默认 WARP；也可选直连或自定义 SOCKS5，地址支持账号密码认证），可批量。
2. 点「登录」→ 弹窗实时回传授权链接，手机打开完成 OAuth。
3. 点「探测」：确认健康、WARP 出口 IP、配额拉取正常。
4. 客户端用面板复制的 `sk-zc-…` Key 接入（OpenAI / Anthropic 形制都认）。

也可以本地运行 `aocker_local.py`，在 Chrome 无痕窗口里经账号出口完成授权
（代理只作用于该窗口），见面板登录弹窗提示。

## 多实例与端口

每账号占两个 `127.0.0.1` 槽位（21100 代理、21101 WARP），分配前实测绑定，
宿主机上被占的端口自动跳过。多套实例用不同 `ZCM_PROJECT` / `ZCM_MESH` 区分。

## 日常运维

| 操作 | 入口 |
|---|---|
| 换 WARP 出口 IP | 面板「换IP」（删注册卷重建，自动重探） |
| 更新 zcode-api | 面板「拉取镜像更新」 |
| 停用 / 删除 | 面板操作，容器与数据卷可分离处理 |
| 容器 / 管理日志 | 面板「日志」 |
| 忘记密码 | `python3 -m zcm --reset-password '新密码'` |

## 配置

常用环境变量见 [.env.example](.env.example)：监听地址、端口槽位、
镜像覆盖、空闲看门狗、后台循环间隔等。

## 说明

- 授权密钥（配置串）格式 `AOCKER:` 与隧道协议已冻结，新旧版本互通
- 出口三种：WARP 容器 / 直连 / 自定义 SOCKS5（账号行「代理」按钮随时切换，gost 自动桥接）
- 领取被风控拦截（3012）时切换账号出口重试，或等自动重试；不影响其他请求
- 本工具依赖的 zcode-api 为第三方逆向项目，上游存在风控与封号案例，自担风险

## 测试

```bash
python3 tests/test_logic.py    # 28 项，无需 docker
```
