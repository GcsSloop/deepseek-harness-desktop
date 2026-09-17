# 故障排查记录

## macOS 启动后白屏或提示 `dsh web authentication required`

### 现象

- 应用窗口显示白屏。
- dsh 子进程可能已经监听本地端口，但访问根地址返回 `401`：
  `dsh web authentication required; reopen the URL printed by dsh web.`
- 直接访问 dsh 输出的 `/?token=...` 地址时，会先返回 `303`，再跳转到 `/`。

### 根因

桌面封装使用 Tauri WebView 从 `tauri://localhost` 进入 dsh 的本地 `http://127.0.0.1:<port>` 服务。dsh 通过 token URL 设置 `HttpOnly` 认证 cookie；在 macOS WebView 中，`SameSite=Strict` cookie 不会在 token URL 的跨站 303 重定向后的根请求中携带，因此最终根页面再次被判定为未认证。

另一个容易混淆的问题是 dsh 的 npm 发布包曾出现入口版本与内部模块版本不一致。`0.1.5-rc.2` 在本项目中会因缺少导出而启动失败；目前使用依赖一致的 `0.1.5-rc.1`。

### 当前修复

1. `src-tauri/src/lib.rs` 捕获 dsh stdout 和 stderr 中的 `dsh web: http://.../?token=...`，并让 WebView 导航到带 token 的 URL。
2. `scripts/prepare-runtime.mjs` 在依赖准备完成后，将内置 `dsh-client-connection` 的认证 cookie 属性改为 `SameSite=Lax`，以兼容 Tauri WebView 的本地重定向。
3. `src-tauri/resources/harness/package.json`、锁文件和准备脚本统一固定 `@deepseek-ai/dsh@0.1.5-rc.1`，避免传递依赖混用 rc.2。
4. 准备脚本清理 `node_modules/.bin` 中的悬空符号链接，避免 Tauri 资源扫描失败。

### 验证方法

```sh
npm run prepare:runtime
npm run build

# 验证打包内置 dsh 的认证重定向
APP="src-tauri/target/release/bundle/macos/DeepSeek Harness.app"
NODE="$APP/Contents/Resources/resources/node/bin/node"
ENTRY="$APP/Contents/Resources/resources/harness/node_modules/@deepseek-ai/dsh/lib/bin.js"
WORK="$APP/Contents/Resources/resources/harness"
PORT=45693

(cd "$WORK" && "$NODE" "$ENTRY" web --no-open --host 127.0.0.1 --port "$PORT") &
# 从 stdout 读取 `dsh web: .../?token=...` 后执行：
curl -L -c /tmp/dsh-cookies -b /tmp/dsh-cookies \
  "http://127.0.0.1:$PORT/?token=<启动输出中的 token>"
```

期望结果：首次响应为 `303 See Other` 并带 `Set-Cookie: ... SameSite=Lax`，跟随重定向后返回 `200`，页面标题为 `DeepSeek Harness`。

### 发布前检查

- 安装测试必须确认 `/Applications/DeepSeek Harness.app/Contents/Info.plist` 的版本号与构建产物一致；macOS 可能继续启动旧的同名应用。
- 对新 `.app` 执行 `codesign --verify --deep --strict`。
- 直接启动构建目录中的 `.app`，确认其子进程命令行路径也来自同一个 `.app`，再交付 DMG。
- rc.2 或其他版本升级前，先在临时目录执行完整 `pnpm install`，检查 `dsh-llm`、`dsh-session`、`dsh-attachment` 和 `dsh-session-query` 的版本是否与入口一致，并实际启动 `dsh web`。
