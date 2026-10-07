# WinTerminalP

[![CI](https://github.com/raincfhnj/winterminalp/actions/workflows/ci.yml/badge.svg)](https://github.com/raincfhnj/winterminalp/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#许可证)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org)

**给原生 Windows Terminal 加上 tmux 风格的键盘控制，而不是替代它。**

WinTerminalP 是一个常驻、无界面的 Rust 控制器，为你正在使用的 Windows Terminal
增加 tmux 式的两段式 Prefix（先按 `Ctrl+B`，再按第二个键）。它不绘制任何窗口、不内嵌
终端、也不管理 PTY。Windows Terminal 仍然是唯一的界面、渲染器、窗格树、标签页和 Shell
所有者；WinTerminalP 只负责把 Prefix 组合翻译成 Windows Terminal 原生动作。

```text
Ctrl+B, Shift+Right   →  向右分屏
Ctrl+B, ←/→/↑/↓       →  移动焦点
Ctrl+B, Ctrl+→        →  调整窗格尺寸
Ctrl+B, C / N / P     →  新建 / 下一个 / 上一个标签页
Ctrl+B, X / Z / ,     →  关闭窗格 / 放大 / 重命名标签页
```

## 为什么

Windows Terminal 没有 tmux 式的 Prefix 模式，其 keybinding 也只能表达“修饰键 + 一个非
修饰键”。WinTerminalP 运行一个很小的低级键盘 Hook，仅在 Windows Terminal 位于前台时
识别 Prefix，然后注入一个隐藏的单组合桥接键，该键绑定到 `User.WinTerminalP.*` 动作。
你的现有 Profile、主题、字体、Shell 和快捷键都不受影响。

## 功能

- **两段式 Prefix**：默认 `Ctrl+B`，支持自定义与超时。
- **窗格**：四向分屏、聚焦与调整尺寸。
- **标签页**：新建、上/下一个、索引激活（`0`–`9`）和原生重命名。
- **鼠标拖动分隔线**：拖动原生窗格分隔线；几何信息来自可丢弃的 UI Automation 矩形，
  并换算为 Terminal 原生 `resizePane` 步长。
- **目录继承**：安装受管的 `OSC 9;9` PowerShell 提示符包装，使复制的窗格继承当前目录。
- **安全可逆安装**：无损 JSONC 编辑，保留注释、顺序、缩进与尾逗号；写入前保存原始字节、
  使用 SHA-256 CAS，卸载只保留仍属于产品的部分，你改过的内容会被保留并报告。
- **默认透明**：非 Terminal 前台应用的按键全部透传，注入输入永不激活 Prefix。
- **无遥测**：不访问网络，不记录终端内容或按键。

## 环境要求

- Windows 10/11 x64
- Windows Terminal 1.21 或更高
- 构建需要 Rust stable（1.85+）与 MSVC 工具链

控制器需要提权，才能同时向普通与管理员权限的 Windows Terminal 注入输入。
`winter run`、`winter launch` 和 `winterd.exe` 在需要时通过 UAC 自重启；
只读/配置类命令（`config`、`plan`、`install`、`uninstall`、`doctor`）不触发 UAC。

## 快速开始

```powershell
git clone https://github.com/raincfhnj/winterminalp.git
cd winterminalp
.\install.ps1   # 构建、把 winter 命令装进 PATH、并安装集成
winter          # 启动控制器（会弹出 UAC 提权）
```

`install.ps1` 只需执行一次：它用 `cargo install` 构建 Release 二进制，把
`winter.exe` 放进已在 PATH 上的 Cargo bin 目录，并安装 Windows Terminal 集成。
之后在任意 shell 里输入 `winter` 即可使用，关机重启后依然有效——重启后再次输入
`winter` 就行。如果集成缺失，`winter` 会在首次启动时自动重新安装。

按 `Ctrl+B` 后再按第二个键执行动作。按 `Ctrl+B`、`Q` 停止控制器，但不关闭
Windows Terminal。

### 手动构建

```powershell
cargo build --release --bins
```

`target\release` 下会生成三个二进制：

| 二进制 | 用途 |
|---|---|
| `winter.exe` | 推荐命令入口 |
| `winterminalp.exe` | 兼容别名，共用同一 CLI |
| `winterd.exe` | 隐藏的后台控制器（可双击） |

可直接运行 `target\release` 下的文件，或用
`cargo install --path . --bins --locked` 安装到 PATH。

## 默认快捷键

所有快捷键仅在 Windows Terminal 位于前台时生效。先按下并释放 `Ctrl+B`，再按第二个键。

| 第二键 | 功能 |
|---|---|
| `←` / `→` / `↑` / `↓` | 聚焦对应方向的窗格 |
| `Shift` + 方向键 | 向对应方向创建窗格 |
| `Ctrl` + 方向键 | 向对应方向调整活动窗格尺寸 |
| `C` | 新建标签页 |
| `N` / `P` | 下一个 / 上一个标签页 |
| `0`–`9` | 激活零基索引标签页 |
| `X` | 关闭活动窗格 |
| `Z` | 放大 / 恢复活动窗格 |
| `,` | 重命名当前标签页 |
| `B` | 发送原始 Prefix（默认 `Ctrl+B`；自定义 Prefix 时发送对应组合） |
| `Q` | 退出控制器（不关闭 Windows Terminal） |
| `Escape` | 取消 Prefix |

## 鼠标调整窗格

把鼠标移到 Windows Terminal 原生分隔线上，光标会变成横向或纵向缩放样式。按住左键拖动即可
调整相邻窗格比例，尺寸按 Terminal 原生约父分栏 5% 的步长变化。

```toml
[mouse_resize]
enabled = true
divider_hit_slop_px = 8
geometry_poll_interval_ms = 100
```

## 配置

```powershell
winter config          # 输出完整生效配置和文件路径
winter config --path   # 只输出路径
winter config --edit   # 用记事本打开
```

配置文件位于 `%LOCALAPPDATA%\WinTerminalP\config.toml`。只需写想覆盖的动作，其余继承
默认值。例如改成 Vim 风格：

```toml
prefix = "ctrl+a"

[shortcuts]
focus_left = "h"
focus_down = "j"
focus_up = "k"
focus_right = "l"
new_tab = "t"
shutdown = "q"
```

Prefix 必须包含 `Ctrl` 或 `Alt`。`Escape` 与系统保留组合（`Alt+Tab`、`Ctrl+Tab`、
`Ctrl+Shift+Tab`、`Alt+F4`、`Alt+Escape`、`Ctrl+Escape`、`Ctrl+Shift+T`、
`Ctrl+Shift+W`、Windows 键组合）不能绑定。两个动作不能使用同一 chord。可选动作可以设为
`"disabled"`。修改快捷键无需重新执行 `winter install`。

所有配置键、默认值与允许范围：

| 键 | 默认值 | 范围 / 说明 |
|---|---|---|
| `schema_version` | `2` | 配置 schema；旧版 `1` 文件可加载并在内存中迁移 |
| `prefix_timeout_ms` | `1500` | `250`–`5000`；Prefix 超时时间 |
| `launch_terminal_on_start` | `true` | 布尔值；控制器启动时打开原生 Windows Terminal 窗口 |
| `prefix` | `"ctrl+b"` | 必须包含 `ctrl` 或 `alt` |
| `shortcuts` | 见上文 | 动作名 → chord 或 `"disabled"` 的映射 |
| `mouse_resize.enabled` | `true` | 布尔值；启用原生分隔线拖动 |
| `mouse_resize.divider_hit_slop_px` | `8` | `0`–`32` |
| `mouse_resize.geometry_poll_interval_ms` | `100` | `50`–`1000` |

完整参考见 [`docs/SHORTCUTS.md`](docs/SHORTCUTS.md)。

## 退出码

`plan`、`install`、`uninstall` 和 `doctor` 会在 stdout 输出 JSON 报告；
`winter doctor` 返回 `{ schema_version, healthy, config: { path, ok, error },
integration }`。

| 码 | 含义 |
|---|---|
| `0` | 成功；命令完成且结果健康 |
| `1` | 硬失败（I/O 错误、不支持的配置等） |
| `2` | 需要处理：`winter plan` 无法安装桥接时；`winter doctor` 配置无效或桥接未就绪时 |

## 安全与隐私

- 键盘 Hook 不记录、不存储、不传输按键。
- 鼠标 Hook 只判断光标是否靠近缓存的分隔线，不存储或发送轨迹。
- 注入（合成）输入始终透传，永不重新进入 Prefix 状态机。
- 控制器不开放网络端口，也不读取终端缓冲区。
- 安装前备份原始字节并使用 CAS；卸载只删除仍与托管 manifest 一致的项，并报告你改过的内容。

## 卸载

```powershell
target\release\winter.exe uninstall
```

只移除仍属于 WinTerminalP 的 fragment、隐藏键位和 Shell 受管块。

## 工作原理

项目是一个小型模块化 Rust 单体。纯状态机（`prefix`、`pane_layout`）不依赖 Win32；
`platform/windows` 适配层负责低级 Hook、UI Automation、前台身份识别与 `SendInput`；
`integration` 负责无损 JSONC 事务与备份。完整设计与失败语义见
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)。

## 文档

- [`docs/PRD.md`](docs/PRD.md) — 产品需求
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — 架构与失败语义
- [`docs/SHORTCUTS.md`](docs/SHORTCUTS.md) — 快捷键参考
- [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) — 构建与开发指南
- [`docs/TESTING.md`](docs/TESTING.md) — 测试策略与人工验证
- [`docs/VERIFICATION.md`](docs/VERIFICATION.md) — 真实环境验证记录
- [`docs/IMPLEMENTATION_PLAN.md`](docs/IMPLEMENTATION_PLAN.md) — 实施计划
- [`docs/fault-reviews/`](docs/fault-reviews) — 故障复盘

## 已知限制

- Windows Terminal 不公开 pane tree 或 Action 执行回执，因此窗格几何只能从可见的
  `TermControl` 矩形推导，`SendInput` 成功只证明事件已插入。
- 调整尺寸遵循 Windows Terminal 原生约 5% 的步长，而非任意像素。
- 项目仅支持 Windows。

## 参与贡献

欢迎贡献。请先阅读 [`CONTRIBUTING.md`](CONTRIBUTING.md)，并在提交 PR 前运行质量门：

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

## 许可证

采用以下任一许可证：

- Apache License, Version 2.0（[LICENSE-APACHE](LICENSE-APACHE)）
- MIT license（[LICENSE-MIT](LICENSE-MIT)）

由你选择。

除非你明确声明，否则任何有意提交以纳入本作品的贡献，均按 Apache-2.0 定义双重授权，
不附加任何额外条款或条件。
