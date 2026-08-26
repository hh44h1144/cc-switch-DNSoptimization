# CC Switch 项目说明

## 项目定位

CC Switch 是一个基于 Tauri 2 的跨平台桌面应用，用于统一管理和切换 Claude Code、Claude Desktop、Codex、Gemini CLI、Grok Build、OpenCode、OpenClaw、Hermes 等 AI 工具的供应商配置、API 密钥、模型参数和本地代理。

项目由 Rust 后端和 React/TypeScript 前端组成：

- `src-tauri/`：Rust/Tauri 后端、SQLite 数据库、供应商配置读写、代理服务、HTTP 转发和 Tauri commands。
- `src/`：React 前端、供应商卡片和编辑表单、代理设置、查询缓存、API 调用封装和多语言资源。
- `src-tauri/resources/`：随应用分发的运行时资源。
- `dist/`：前端生产构建输出，不手工编辑。

## 主要功能模块

- 供应商管理：新增、编辑、删除、启用、排序、批量操作和按应用类型维护配置。
- 多客户端配置同步：将供应商设置写入各 CLI/桌面工具实际使用的配置文件。
- 统一本地代理：支持 HTTP/SOCKS 出站代理、供应商切换、请求转发、SSE 流式响应和请求错误处理。
- 使用量与订阅：读取本地会话/订阅数据，展示模型使用量和成本。
- MCP、Skills、Profiles、Failover 等扩展能力。
- Cloudflare 优选 IP：在不修改供应商 URL 和系统 Hosts 的前提下，将 Cloudflare 域名的 TCP 连接定向到 CFST 选出的 IP。

## Cloudflare 优选 IP 当前实现

相关文件：

- `src-tauri/src/services/cloudflare.rs`：CFST 扫描、首条结果解析、active IP 状态、TCP/TLS/HTTP 巡检、自动重扫和事件。
- `src-tauri/src/commands/cloudflare.rs`：获取/保存路由、启动/取消扫描、手动巡检、恢复 DNS 的 Tauri commands。
- `src-tauri/src/database/dao/cloudflare.rs`、`src-tauri/src/database/schema.rs`：路由配置、active IP、巡检指标和最近一次完整扫描结果的持久化及迁移。
- `src-tauri/src/proxy/forwarder.rs`、`src-tauri/src/proxy/hyper_client.rs`、`src-tauri/src/proxy/http_client.rs`：统一请求链路中的 IP 定向连接、Host/SNI 保留和连接池淘汰。
- `src/components/providers/ProviderCard.tsx`：供应商 banner 快捷入口及当前优选 IP/TCP 延迟摘要。
- `src/components/providers/forms/CloudflareRoutePanel.tsx`：Cloudflare 路由详细设置、手动扫描、停止扫描、立即巡检和最近扫描结果展示。
- `src/lib/api/cloudflare.ts`：前端 Tauri API 封装。

设计边界：

1. CFST 只负责扫描 Cloudflare IP 池，使用默认参数和默认测速资源，不绑定任何供应商 URL。
2. 只检查 CFST 结果 CSV 的第一条记录：`loss_rate == 0` 且 `download_speed > 0` 才接受；第一条不合格时不尝试第二条，也不切换旧 IP。
3. 日常巡检由 CC Switch 自己完成，核心指标是对 `active_ip:443` 的全新 TCP 建联耗时和失败率，可叠加 TLS 与边缘 HTTP 探测；不使用 API 完整响应时间或业务错误触发切换。
4. 开启全局 HTTP/SOCKS 出站代理时，优选 IP 路由和 CFST 直连扫描暂停。
5. 切换 IP 时淘汰旧解析/连接池；在途连接不主动中断，新请求使用新 IP；HTTP URL、Host、TLS SNI 和证书校验仍使用原始供应商域名。
6. 最近一次扫描结果独立保存并展示，无论切换成功、首条结果无效、无结果、取消还是进程失败，都不能覆盖旧的 active IP。

## CFST 随应用分发要求

CFST 资源位于 `src-tauri/resources/cfst/`，必须包含：

- `ip.txt`；
- `darwin-amd64/cfst`、`darwin-arm64/cfst`；
- `linux-amd64/cfst`、`linux-arm64/cfst`；
- `windows-amd64/cfst.exe`、`windows-arm64/cfst.exe`；
- `LICENSE` 和 `SOURCE.txt` 等许可声明。

`src-tauri/tauri.conf.json` 通过 `bundle.resources` 打包整个 `resources/cfst`。`src-tauri/build.rs` 会在 Cargo 构建期检查所有必需文件，缺少资源时必须直接失败，不能生成不完整安装包。macOS app bundle 中的实际路径是：

```text
Contents/Resources/resources/cfst/<platform>/cfst
```

运行时查找逻辑必须覆盖该路径以及开发环境路径；不要只假设可执行文件旁边存在 `resources/cfst`。

## 常用开发与验证命令

在 `cc-switch/` 目录执行：

```bash
# 前端
pnpm install --registry=https://registry.npmmirror.com
pnpm run typecheck
pnpm run build:renderer

# Rust
cargo check --manifest-path src-tauri/Cargo.toml

# Tauri 打包（已生成 dist 时可跳过前端 beforeBuild）
./node_modules/.bin/tauri build --config '{"build":{"beforeBuildCommand":""}}'
```

打包后至少检查：

```bash
find 'src-tauri/target/release/bundle/macos/CC Switch.app/Contents/Resources/resources/cfst' -type f
test -x 'src-tauri/target/release/bundle/macos/CC Switch.app/Contents/Resources/resources/cfst/darwin-arm64/cfst'
tar -tzf 'src-tauri/target/release/bundle/macos/CC Switch.app.tar.gz' | grep 'Contents/Resources/resources/cfst'
```

Tauri 自动生成的 `bundle_dmg.sh` 在无 Finder/图形环境时可能失败；这不等于 `.app` 或 CFST 资源缺失。需要交付 DMG 时，可使用 `hdiutil create -srcfolder` 生成简单 DMG，再只读挂载确认 CFST 文件存在。

## 修改约束

- 先追踪真实调用链，再修改共享层；不要臆造参数或增加没有调用方的兼容逻辑。
- 供应商原始配置和 Cloudflare 路由数据分开保存，不要把 active IP 写回供应商 URL 或配置文件。
- 不使用系统 Hosts 修改实现优选路由。
- 不要停止、杀掉或重启用户现有的 `127.0.0.1:15721` 实例；验证新客户端时使用其他端口和隔离配置目录。
- 保留工作区中已有的用户修改，不使用破坏性的 `git reset --hard` 或大范围清理。
- 完成功能后运行与改动风险匹配的检查，并在交付说明中明确实际验证结果和未解决的环境问题。

