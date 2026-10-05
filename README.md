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

需要 Rust 1.75+。

```bash
cargo build --release
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
| `NGW_ADMIN_PASSWORD` | 首次初始化的管理员密码 | 随机生成 |
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
