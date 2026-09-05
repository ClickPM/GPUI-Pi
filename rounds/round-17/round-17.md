# Round 17 — Windows 绿色包（免安装）

> 执行方：Windows · 状态：进行中

## 目标

打出一份解压即用的 Windows x64 绿色目录 + zip：含 `gpui-pi.exe`、`vendor/pi/`、内建子代理内核；首启能发现缺件；关于对话框展示版本并指向 Releases。不做自动更新，不强制 NSIS/MSI。

## 前置

- R0–R16、R19–R27 功能代码已在 `main`。
- 新 round 启动门禁已过：本 worktree 内 `fetch-pi` / `fetch-pi-source` / `fetch-pi-web` / `fetch-pi-subagents-lite` / `check-pins` 全绿。
- 立项原文写 NSIS/MSI；本轮按「空机器双击可用」做成**免安装目录拷贝**（无注册表、无服务、无 WebView2）。zip 即分发形态。

## 交付物

- `crates/pi-runtime/src/install.rs` —— 安装根解析（exe 旁 vendor → 仓库根）、文件层自检、`pi --version` 探针
- `crates/app/src/about.rs` —— 关于 / 版本 / 缺件提示；工具栏 `v*` 入口
- `scripts/package.ps1` —— release 构建 + 组装 `dist/gpui-pi-<ver>-windows-x64/` + zip
- `docs/立项文档.md` § 七 R17 行与本轮口径对齐

## 验收

| 级别 | 检查 | 命令 / 期望 |
|---|---|---|
| T1 | 钉版本 | `.\scripts\check-pins.ps1` 全绿 |
| T1 | 逻辑 + 全量 | `.\scripts\validate.ps1 -Logic` 与 `.\scripts\validate.ps1` 全绿 |
| T2 | 绿色包布局 | `.\scripts\package.ps1`：目录含 `gpui-pi.exe` + `vendor\pi\pi.exe` + 内核 `package.json`；**不含** `runtime_fake_child.exe`、`vendor\upstream`；`pi --version` 为 `0.84.2`；目录与 zip 均 ≤ 220MB |
| T2 | 安装根 | 开发态仍能从仓库 `vendor/` 找到 pi；把绿色包拷走后按 exe 旁 `vendor/` 解析 |
| T3 | 空机 | 解压到一台没有装过本仓库的 Windows，双击 `gpui-pi.exe`，配 Key 后能对话 |

## 禁止

- 禁止把 `vendor/upstream`、测试 helper `runtime_fake_child.exe` 打进绿色包。
- 禁止改 `Cargo.lock` 上游 package 的 version / source / checksum；不引入打包器 crate（cargo-packager / WiX）。
- 禁止做自动更新（下载/替换正在运行的 exe）。
- 禁止代写用户 `~/.pi`。
- 发现前序问题写 `rounds/BACKLOG.md`，不当场改。

## 失败处理

连续 2 次 validation 不过 → 写 `rounds/round-17/BLOCKED.md`，停下呼人。禁止放宽验收标准自我通过。

## 视觉审查

本轮 diff 触及 `crates/app` 工具栏与关于对话框，触发视觉 review。

- 视觉审查模式：
- 视觉审查结论：
- 截图验证：
- 兜底原因：
- `requested_at`：
- `deadline`：
- 审查报告 / 证据：
- 说明：

## 本轮实测

### 环境

- Worktree：`D:\variFlight_work\GPUI-Pi-round-17`，分支 `WinClaude/round-17`，基线 `main` @ `20f2de9`
- 门禁：`fetch-pi` / `fetch-pi-source` / `fetch-pi-web` / `fetch-pi-subagents-lite` / `check-pins` 全绿（缓存命中）

### T1

- `.\scripts\validate.ps1 -Logic`：VALIDATE OK
- `.\scripts\validate.ps1`：VALIDATE OK（约 15.7 分钟）

### T2 绿色包

```
.\scripts\package.ps1 -SkipBuild
```

| 项 | 实测 |
|---|---|
| 目录 | `dist\gpui-pi-0.1.0-windows-x64\`（`gpui-pi.exe` + `vendor\pi` + `vendor\pi-subagents-lite-1.13.0` + `使用说明.txt`） |
| zip | `dist\gpui-pi-0.1.0-windows-x64.zip` **55.7 MB** |
| 解压后体积 | **151.3 MB**（上限 220 MB） |
| `pi --version` | `0.84.2` |
| 排除 | 无 `runtime_fake_child.exe`、无 `vendor\upstream` |

### 顺带在 Windows 上堵住的 R27 缺口

R27 在 Linux Cloud Agent 合入，本机 clippy / writer 单测才暴露：`\\?\` 路径让租约 key 对不上（单 writer 失效）、若干 clippy `-D warnings`。已在 `writer_isolation` 查找前 canonicalize；`pi-data::windows_path_key` 的根因记 BACKLOG #43。无符号链接特权时目录链接用例直接返回，避免 1314 误红。

