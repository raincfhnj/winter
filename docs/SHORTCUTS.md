# Winter 快捷键

所有快捷键仅在 Windows Terminal 位于前台时生效。先按下并释放 `Ctrl+B`，再按第二个键。

| 第二键 | 功能 |
|---|---|
| `←` / `→` / `↑` / `↓` | 聚焦对应方向的窗格 |
| `Shift+方向键` | 向对应方向创建窗格 |
| `Ctrl+方向键` | 向对应方向调整活动窗格尺寸 |
| `C` | 新建标签页 |
| `N` | 下一个标签页 |
| `P` | 上一个标签页 |
| `0`–`9` | 激活零基索引标签页 |
| `X` | 关闭活动窗格 |
| `Z` | 放大或恢复活动窗格 |
| `,` | 打开标签页重命名框 |
| `B` | 向 Shell 发送原始 Prefix（默认 Ctrl+B；自定义 Prefix 时发送对应组合） |
| `Q` | 退出 Winter 控制器，不关闭 Terminal |
| `Escape` | 取消 Prefix |

## 鼠标调整窗格

多窗格时，把鼠标移到 Windows Terminal 原生分隔线上，光标会变成横向或纵向缩放样式；按住左键拖动即可调整相邻窗格比例。实现仍调用 Terminal 原生 `resizePane`，因此尺寸按父分栏约 5% 的步长变化，而不是任意像素变化。

鼠标拖动默认开启，可在配置中调整边界命中宽度和几何刷新周期：

```toml
[mouse_resize]
enabled = true
divider_hit_slop_px = 8
geometry_poll_interval_ms = 100
```

鼠标功能在前台 Windows Terminal 的可见窗格上工作。控制器运行时会自动请求 UAC 提权，因此普通和管理员权限启动的 Terminal 都使用同一套快捷键与鼠标 resize；关闭鼠标功能后键盘 `Ctrl+B`、`Ctrl+方向键` 调整尺寸仍然可用。

## 自定义快捷键

查看当前生效配置和文件位置：

```powershell
winter config
```

直接用记事本打开：

```powershell
winter config --edit
```

配置文件位于 `%LOCALAPPDATA%\WinTerminalP\config.toml`。例如改成更接近 Vim/tmux 的 `H/J/K/L`：

```toml
prefix = "ctrl+a"

[shortcuts]
focus_left = "h"
focus_down = "j"
focus_up = "k"
focus_right = "l"
split_left = "shift+h"
split_down = "shift+j"
split_up = "shift+k"
split_right = "shift+l"
resize_left = "ctrl+h"
resize_down = "ctrl+j"
resize_up = "ctrl+k"
resize_right = "ctrl+l"
new_tab = "t"
close_pane = "x"
shutdown = "q"
```

只需要写想覆盖的动作；没有写出的动作继续使用默认值。保存后退出并重新运行控制器；首次运行态会出现一次 UAC 确认：

```powershell
winter
```

键名不区分大小写，支持：

- 单个 `a-z`、`0-9`。
- `left`、`right`、`up`、`down`、`space`、`tab`、`F1`–`F12`。
- `comma`、`period`、`semicolon`、`slash`、`backslash`、`minus`、`equals`、`quote`、`backtick`、`left-bracket`、`right-bracket`。
- `ctrl`、`alt`、`shift` 修饰键，例如 `ctrl+left` 或 `alt+shift+k`。

规则：

- Prefix 必须包含 `ctrl` 或 `alt`，避免普通输入被拦截。
- `Escape`、`Alt+Tab`、`Ctrl+Tab`、`Ctrl+Shift+Tab`、`Alt+F4`、`Alt+Escape`、`Ctrl+Escape`、`Ctrl+Shift+T`、`Ctrl+Shift+W` 和 Windows 键组合不能绑定。
- 两个动作不能使用同一 chord；启动时会报告冲突动作名。
- 可把非必要动作设为 `"disabled"`；`shutdown` 必须保留一个键位。
- 用户快捷键只改变 Rust Prefix 映射，不修改隐藏 Action Bridge，因此无需重新执行 `winter install`。

## 行为说明

- Prefix 默认 1500 ms 后超时。
- 未绑定的第二键会被吞掉并取消 Prefix。
- Prefix 后仍按住 Ctrl 再按方向键，执行 resize；若要移动焦点，请先释放 Ctrl。
- `send_prefix_literal`（默认第二键 `B`）由控制器直接注入当前配置的 Prefix 组合，自定义 Prefix 后仍然有效，无需重新安装桥接。
- `closePane` 沿用 Windows Terminal 原生语义：没有分屏时会关闭标签页，最后一个标签页时会关闭窗口。
- 分屏与新建标签页的当前目录继承由 Windows Terminal 的 Shell Integration 提供。`winter install` 会向 `Documents\WindowsPowerShell\Microsoft.PowerShell_profile.ps1`（以及存在时的 `Documents\PowerShell\...`）追加一段受管提示符包装，使提示符输出 `OSC 9;9` 工作目录；`winter uninstall` 只移除未被修改的受管块。修改后需重新打开 PowerShell 标签页（或在会话中重新加载 Profile）才能生效。
- `winter run`、`winter launch` 和 `winterd.exe` 会在未提权时通过 UAC 自重启；控制器核心拒绝以普通权限安装 Hook 或向 Terminal 派发动作。
- 鼠标左键在分隔线以外完全透传；点击或开始拖动会取消尚未完成的 Prefix，避免下一键误触发动作。

桥接只使用 F13、F14、F15、F18–F24 的隐藏组合键；F16/F17 已因本机 Stable 1.24 实测不可靠而禁用。用户不需要直接按这些桥接键，也不应把它们绑定给其他动作。
