# 前端依赖

以下文件随二进制内嵌分发，不从浏览器连接第三方 CDN。各项目的许可文件保留在本目录。

| 依赖 | 版本 | 来源 |
| --- | --- | --- |
| Bootstrap | 5.3.3 | https://github.com/twbs/bootstrap/tree/v5.3.3 |
| Bootstrap Icons | 1.11.3 | https://github.com/twbs/icons/tree/v1.11.3 |
| marked | 4.3.0 | https://github.com/markedjs/marked/tree/v4.3.0 |
| DOMPurify | 3.4.17 | https://github.com/cure53/DOMPurify/releases/tag/3.4.17 |

DOMPurify 由原来的 3.0.9 更新至 3.4.17。保留原有 sanitize 后写入 DOM 的调用方式。
其他依赖保持原页面使用的版本，减少此次样式修复对现有行为的影响。
