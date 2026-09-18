# DeepSeek Harness Desktop

基于 Tauri 2 的 DeepSeek Harness 桌面封装。应用启动时自动启动内置 Harness 服务并在窗口中打开 Web UI；退出应用时终止 Harness 及其子进程。

## 平台支持

| 平台 | 架构 | WebView 宿主 | 安装包 |
|---|---|---|---|
| macOS | arm64（Apple Silicon） | WKWebView（系统内置） | `.dmg`（另附 `.app.zip`） |
| Windows | x64 | **WebView2**（Edge Chromium 内核） | `.msi` 与 `-setup.exe` |

两个平台原生面板的上层控制 API 完全一致，但底层宿主视图不同：

- **macOS**：`WKWebView` 作为窗口的子视图，页面消息经 `WKScriptMessageHandler`（名字固定为 `dshWebReview`）回到壳进程。
- **Windows**：**WebView2** 的 `CoreWebView2Controller` 作为窗口的子控制器，页面消息经 `window.chrome.webview.postMessage` → `WebMessageReceived` 回到壳进程。Windows 10/11 自 2021 年起已内置 WebView2 运行时，缺失时安装包会引导下载。

两端都刻意不走「页面直连环回 HTTP」：HTTPS 页面在 WebKit 下会被拒绝发起不安全请求，在 Chromium 下也可能被混合内容 / Private Network Access 规则拦住。把消息交给壳进程转发即可绕开浏览器自身的传输限制。

## 本地构建

需要 Rust、Node.js 和 pnpm。运行：

```sh
npm install
npm run build
```

构建脚本会下载官方 Node.js 运行时，并安装锁定版本的官方 npm 构建产物 `@deepseek-ai/dsh@0.1.5-rc.1`。客户电脑无需另行安装 Node.js 或 Harness。

产物位置：

- macOS：安装包在 `src-tauri/target/release/bundle/dmg/`，应用包在 `src-tauri/target/release/bundle/macos/`
- Windows：`src-tauri/target/release/bundle/msi/` 与 `src-tauri/target/release/bundle/nsis/`

Windows 上构建同样需要 Rust（MSVC toolchain）、Node.js 和 pnpm。安装包未做代码签名，首次运行会有 SmartScreen 提示；macOS 侧为 ad-hoc 签名，首次打开会有 Gatekeeper 提示。

## 发布

推送 `v*` tag 即触发 `.github/workflows/release.yml`：矩阵在 `macos-14`（arm64）与 `windows-latest`（x64）上分别构建，把各平台安装包作为构件上传，最后统一创建 GitHub Release。

```sh
git tag -a v0.2.0 -m "deepseek-harness-desktop v0.2.0"
git push origin v0.2.0
```

## 壳提供的两项本地能力

这两项都只依赖环回 HTTP，不修改 harness，也不依赖任何具体 Web UI：

### 1. 原生浏览器面板

应用启动时在环回端口开一个小控制 API，并把端点写入
`$DSH_HOME/web-review/native-browser.json`（退出时删除）。任意 Web UI 发现该文件后即可：

| 方法 | 路径 | 作用 |
|---|---|---|
| GET | `/health` | 探活（陈旧描述文件先探活再使用） |
| GET | `/panel/state` | 当前面板是否打开、会话、URL |
| POST | `/panel/open` | `{session,url,x,y,width,height,bootstrap}`：在窗口内创建子 WKWebView 并注入脚本 |
| POST | `/panel/bounds` | `{x,y,width,height,visible}`：移动/缩放/隐藏 |
| POST | `/panel/command` | `{kind:navigate\|reload\|eval\|close,...}` |

面板使用**持久数据仓**（`浏览器面板` 目录），登录态跨重启保留；`bootstrap` 在每个新文档的页面脚本前执行。
dsh-web-review 插件的 `native` 预览模式即基于此（未安装该插件时此能力只是闲置，不影响使用）。

### 2. 全屏时忽略 ESC

macOS 没有受支持的 API 阻止 ESC 退出全屏，因此壳安装了一个本地 AppKit 事件监视器：
**仅当窗口处于全屏时**吞掉 ESC（其余情况照常传给页面，预览拾取器的 ESC 取消仍然有效）。

| 方法 | 路径 | 作用 |
|---|---|---|
| GET | `/window/state` | `{fullscreen, escapesSwallowed}`（后者为已吞掉的 ESC 次数，便于验证） |
| POST | `/window/fullscreen` | `{value:bool}`：进入/退出全屏 |

## 故障排查

白屏、`dsh web authentication required`、旧应用副本和 dsh 依赖版本混用等问题，见 [故障排查记录](docs/troubleshooting.md)。

## 版本更新

升级 Harness 时，同时修改以下两处版本号，然后重新生成锁文件并构建：

- `src-tauri/resources/harness/package.json`
- `scripts/prepare-runtime.mjs`

本项目封装的 DeepSeek Harness 使用其 MIT 许可证。对外分发前请保留上游许可证及第三方声明，并使用 Apple Developer ID 对 macOS 安装包签名、公证。
