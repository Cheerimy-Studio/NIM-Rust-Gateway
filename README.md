# NIM Rust Gateway

免费、免注册的 OpenAI 兼容 AI 网关。聚合 NVIDIA NIM 与任意 OpenAI 兼容上游，单文件二进制，web 控制台内嵌，无运行时依赖。

## 功能

- 账号池调度：LRU 轮换、按「渠道 + 模型」成功率路由、新账号预热、模型不存在负缓存
- 限速与保护：账号 RPM / TPM / 日限额、渠道并发上限、分级冷却、错误分类、模型熔断、失败封禁阶梯
- 请求处理：失败自动换号重试、429 吸收、同号快速重试、无可用账号时排队等待
- 流式：SSE 逐块透传，慢推理模型心跳保活，首字节超时与空闲超时独立控制
- 协议：OpenAI Chat / Completions / Embeddings / Responses 与 Anthropic Messages，含流式事件转换
- 管理后台：账号、渠道、限额、日志、排队、拦截规则、训练资料、配置导入导出
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

## 接口

```
POST /v1/chat/completions      流式 / 非流式
POST /v1/completions
POST /v1/embeddings
POST /v1/responses             含流式事件转换
POST /v1/messages              Anthropic，含流式事件转换
GET  /v1/models、/v1/models/{id}
GET  /admin                    管理后台
GET  /queue                    公开排队页
```

鉴权：后台生成访问令牌，`Authorization: Bearer <令牌>` 调用；令牌留空时不校验。

## 测试

黑盒验收套件（起 mock 上游与网关实例，需要 Python 3.10+ 与 fastapi、httpx、uvicorn）：

```bash
python tests-blackbox/compat.py       # 19 项：接口与协议结构
python tests-blackbox/regression.py   # 158 项：调度、限速、断连回收、更新回滚、协议守护
```

## 许可

[AGPL-3.0](LICENSE)
