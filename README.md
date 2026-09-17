# DeepSeek Harness Desktop

基于 Tauri 2 的 DeepSeek Harness 桌面封装。应用启动时自动启动内置 Harness 服务并在窗口中打开 Web UI；退出应用时终止 Harness 及其子进程。

## 本地构建

需要 Rust、Node.js 和 pnpm。运行：

```sh
npm install
npm run build
```

构建脚本会下载官方 Node.js 运行时，并安装锁定版本的官方 npm 构建产物 `@deepseek-ai/dsh@0.1.5-rc.1`。客户电脑无需另行安装 Node.js 或 Harness。

macOS 安装包位于 `src-tauri/target/release/bundle/dmg/`，应用包位于 `src-tauri/target/release/bundle/macos/`。

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
