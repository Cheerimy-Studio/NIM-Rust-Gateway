# NIM Rust Gateway

免费、免注册的 OpenAI 兼容 AI 网关。聚合 NVIDIA NIM 与任意 OpenAI 兼容上游，单文件二进制，web 控制台内嵌，无运行时依赖。

## 功能

- 账号池调度：LRU 轮换、按「渠道 + 模型」成功率路由、新账号预热、模型不存在负缓存
- 限速与保护：账号 RPM / TPM / 日限额、渠道并发上限、分级冷却、错误分类、模型熔断、失败封禁阶梯
- 请求处理：失败自动换号重试、429 吸收、同号快速重试、无可用账号时排队等待
- 流式：SSE 逐块透传，慢推理模型心跳保活，首字节超时与空闲超时独立控制
- 协议：OpenAI Chat / Completions / Embeddings / Responses 与 Anthropic Messages，含流式事件转换
- 多用户计费：用户与调用 Key（可限定免费 / 付费 / 全部模型）、按渠道模型定价、余额预检、用户级限速、调用日志与模型广场
- 账本：赠金与充值分离记账，计费优先扣赠金；累计调用与累计消费按用户持久累计（不受日志裁剪影响）
- 福利：每日签到（可选开启）、抽奖活动（多活动、奖项概率合计 100%、可设真实权重）
- 奖品：余额 / 赠金直接入账；模型体验卡与专属额度发放独立 Key（单模型锁定、并发上限、次数额度、到期时间）
- 管理后台：账号、渠道、限额、日志、排队、拦截规则、训练资料、用户、模型定价、抽奖、配置导入导出
- 自动更新：GitHub Releases 正式版，手动触发或定时自动检查，带备份与一键回滚

## 构建

使用当前 Rust stable 工具链与仓库中的 Cargo.lock 构建。

```bash
cargo build --locked --release
```

产物：`target/release/nim-gateway`（Windows 为 `nim-gateway.exe`）。

## 运行

```bash
./run.sh          # Linux / macOS
run.bat           # Windows
```

默认端口 8100。首次启动在控制台打印随机生成的管理员密码，访问 `http://127.0.0.1:8100/admin` 登录。

| 环境变量 | 说明 | 默认 |
|---|---|---|
| `NGW_PORT` / `PORT` | 监听端口 | 8100 |
| `NGW_DATA_DIR` | 数据目录（db.json 所在位置） | `./data` |
| `NGW_ADMIN_PASSWORD` | 启动时同步管理员密码；密码变化时使旧会话失效 | 首次启动随机生成 |
| `NGW_UPDATE_URL` | 自建更新源 | GitHub Releases |
| `NGW_UPDATE_REPO` | 更新仓库名 | `Cheerimy-Studio/NIM-Rust-Gateway` |

忘记后台密码：`nim-gateway --reset-password 新密码`。

## 自动更新

更新源是本仓库的 GitHub Releases，只有正式 Release 会触发更新。发布新版本：

```bash
git tag v1.7.1 && git push origin v1.7.1
```

CI 自动构建 Linux（glibc 2.17）与 Windows 二进制并发布 Release。实例侧两种方式：

- 手动：后台「检查更新」按钮
- 自动：设置里将「自动检查更新间隔」设为 N 小时（0 关闭）

更新流程：下载、备份到 `backup/`、原子替换、自动重启。回滚备份保留一份，可在后台一键回退。

## 数据存储

数据按表分文件存放于 `data/db/`，落盘只写有变化的表（账号、日志、训练资料互不影响，
不会因为一条日志重写整个库）：

```
data/db/config.json       配置与渠道预设
data/db/keys.json         账号池
data/db/upstreams.json    渠道
data/db/logs.json         请求日志
data/db/training.json     训练资料
data/db/sessions.json     会话审计
data/db/intercepted.json  拦截记录
data/db/queue.json        排队
data/db/metrics.json      统计、限速窗口、熔断与负缓存
data/db/users.json        用户、调用 Key、调用日志、限速窗口
data/db/wheels.json       抽奖活动、奖品 Key、抽奖记录
data/db/signs/            签到记录（每天一个文件）
```

从旧版单文件升级：首次启动自动拆分迁移，原 `db.json` 改名 `db.json.migrated` 留存。
更新/回滚会整目录备份与还原数据。

## 验证与流式行为

```bash
cargo test --locked --all-targets
```

测试覆盖跨网络分块的 UTF-8 / CRLF、工具调用转换、异常断流、真实 usage、
成功调用计费、客户端断连释放、并发排队上限、签到目录备份与多用户日志迁移。
CI 在 Linux 和 Windows 上运行测试。

流式响应以完整 SSE 事件为单位转发，单事件上限为 1 MiB（不是整个回复的上限）。
真实 usage 优先；缺失时仅对生成内容估算。收到 `[DONE]`，或在所有 choice 都有
`finish_reason` 且没有残缺事件时关闭连接，才视为正常结束。中断、错误事件与残缺
JSON 会返回协议对应的流内错误，记为失败且不扣费。网关不会依据中英混排密度或
正文中的特殊 token 示例判定“乱码”而截断回复。

## 独立选择上游接口

在管理后台「上游」中设置「发送到上游的接口」。`Base URL` 仍只填公共前缀，例如 `https://api.example.com/v1`，不附带具体端点。

| 渠道 `api_endpoint` | 实际上游对话接口 |
|---|---|
| `chat/completions` | `/chat/completions` |
| `responses` | `/responses` |
| `messages` | `/messages` |
| `completions` | `/completions` |
| `auto` 或旧配置缺失此字段 | 保留旧行为：对话走 Chat，文本补全走 Completions |

明确选择接口后，下游可继续调用 Chat、Responses、Messages 或字符串 prompt 的 Completions；网关转换请求、JSON 响应和 SSE 事件。Embeddings 是独立任务，始终调用 `/embeddings`，不会转换成对话；对话、旧版文本补全与向量的负缓存、熔断状态相互隔离。

渠道 `auth_mode` 可选择 `auto`、`bearer`、`x-api-key`。自动模式按**上游**协议选择：Messages 使用 X-API-Key，其余接口使用 Bearer；始终使用渠道账号密钥，不转发下游的鉴权信息。Messages 同时携带 `anthropic-version: 2023-06-01`。不跟随上游重定向。

```json
{
  "name": "Responses 渠道",
  "base": "https://api.example.com/v1",
  "api_endpoint": "responses",
  "auth_mode": "bearer",
  "enabled": true
}
```

固定参数按**选定上游协议**的字段名填写，例如 Responses 的 `max_output_tokens`、Messages 的 `max_tokens`；`model` 与 `stream` 由路由控制。每次重试从原始请求构建，不继承其他渠道的固定参数。修改渠道路由配置会清理旧的负缓存与熔断状态；升级首次启动也会清理旧的未隔离状态。

适配支持常见文本、函数工具、Chat/Responses/Messages 图片输入和 usage。旧版 Completions 使用角色文本拼接 prompt，没有通用的结构化角色隔离，不支持工具或图片。对于要求系统角色隔离的任务，应使用其他三种上游。

这是无状态的协议适配，不是任意原生 API 透传。`previous_response_id`、后台任务、Responses 服务端内置工具、音频/文件内容、带签名的 Messages 思考历史等不能安全转换的功能会明确拒绝。某些协议没有等价参数，例如 Responses 的 stop/seed、Messages 的 JSON 格式或 strict 工具约束，网关会报错，避免静默丢弃要求。Responses/Messages 不支持跨协议 `n>1`。

非流式上游响应（含解压后的错误体）上限 20 MiB；SSE 仍按单事件 1 MiB 限制。API 请求和上游响应必须为有效 UTF-8。

## 接口

```
POST /v1/chat/completions      流式 / 非流式
POST /v1/completions
POST /v1/embeddings
POST /v1/responses             含流式事件转换
POST /v1/messages              Anthropic，含流式事件转换
GET  /v1/models、/v1/models/{id}
GET  /admin                    管理后台
GET  /user                     用户中心
GET  /queue                    公开排队页
```


鉴权：用户在用户中心生成 API Key（`sk-usr-` 前缀），`Authorization: Bearer <Key>` 调用；也可在后台生成访问令牌。奖品 Key（`sk-prz-` 前缀）仅能调用其锁定的模型。

## 许可

[AGPL-3.0](LICENSE)
