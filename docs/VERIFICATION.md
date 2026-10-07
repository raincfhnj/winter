# WinTerminalP v0.2 验证记录

本文件记录一次真实环境验证的方法与结果。路径、哈希、PID 等与本机相关的值已被泛化；如需复现，请以自己机器上的实际输出为准。

| 项目 | 结果 |
|---|---|
| 验证日期 | 2026-08-30 |
| 操作系统 | Windows x64 |
| Windows Terminal | Stable 1.24.11911.0 |
| 集成状态 | 已安装，`doctor.healthy = true` |
| 托管绑定 | 29，冲突 0 |
| 短命令 | 已确认 Cargo bin 目录在 PATH 上，可直接运行 `winter --help` |
| 用户现有 Terminal | 保留；几何/步长探针均使用独立命名窗口并按精确 HWND 清理 |

## 1. 自动化质量门

以下命令均在仓库根目录通过：

```powershell
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release --bins
```

测试结果：

- Library：全部通过，0 failed。
- `tests/live_bridge.rs`：默认基线仅运行不接触真实桌面的用例；其余用例标记为 `ignored`，因为它们会读取全局热键、安装临时输入 Hook、读取专用窗格几何或向专用前台 Terminal 注入真实桥接键。
- Release 的三个二进制均构建成功。

## 2. Release 产物

| 文件 | 说明 |
|---|---|
| `target/release/winter.exe` | 推荐的 `plan`/`install`/`uninstall`/`doctor`/`run`/`launch` 短命令入口 |
| `target/release/winterminalp.exe` | 兼容别名，与 `winter.exe` 共用 CLI 实现 |
| `target/release/winterd.exe` | Windows subsystem 隐藏控制器，可直接双击后台运行并打开原生 Windows Terminal |

## 3. 真实配置事务

目标设置：`%LOCALAPPDATA%\Packages\Microsoft.WindowsTerminal_8wekyb3d8bbwe\LocalState\settings.json`。

| 证据 | 说明 |
|---|---|
| 安装前原始 settings | 记录原始字节 SHA-256 |
| 当前已安装 settings | 记录安装后 SHA-256 |
| 原字节备份 | `%LOCALAPPDATA%\WinTerminalP\integration\backups\*.json` |
| Action fragment | 记录 fragment SHA-256 |
| Manifest | `%LOCALAPPDATA%\WinTerminalP\integration\manifest.json` |

已经完成一次“安装 → 真实动作验证 → 卸载 → 原始哈希恢复 → 重新安装”。卸载后 settings 与安装前 SHA-256 完全一致。

## 4. 真实 Windows Terminal 动作

真实动作只在专用测试窗口中执行，验证结束后已清理；用户原有 Terminal 窗口保持不变。

| 能力 | 验证结果 | 证据方法 |
|---|---|---|
| Split Left/Right/Up/Down | 通过 | 分割后第一次 `closePane` 保留同一测试 HWND，第二次关闭最后窗格后测试窗口消失 |
| Focus Left/Right/Up/Down | 通过 | 四个窗格使用不同命令行标题，派发后目标窗口标题分别切换 |
| Close Pane | 通过 | 分屏窗口第一次关闭只移除活动窗格 |
| Previous/Next Tab | 通过 | 固定标题标签页按预期切换 |
| Activate Tab 0/1 | 通过 | 固定索引标签页按预期切换 |
| 精确前台保护 | 通过 | 焦点变化时返回 `TargetNotForeground`，未转投另一 Windows Terminal 窗口 |
| 单实例 | 通过 | 第二个控制器实例返回 already-running，退出后无控制器进程残留 |

Stable 1.24 的真实派发还发现 F16/F17 虽能写入配置、`SendInput` 也报告插入，但对应 Action 不可靠。最终桥接表已完全排除 F16/F17，改用 F13、F14、F15、F18–F24，并由单元测试锁定。完整复盘见 `docs/fault-reviews/2026-08-30-Windows-Terminal-F16-F17桥接未触发.md`。

## 5. 尚需人工可视确认

以下边界没有被自动化结果替代：

- 物理键盘完整按下 `Ctrl+B` 再按第二键。Hook 按安全设计忽略 injected 输入，因此不能用本控制器的 `SendInput` 自测 Prefix。
- Resize Left/Right/Up/Down 的可视尺寸变化。
- Toggle Zoom 与原生标签页 Rename 输入框的可视行为。
- 普通权限控制器对管理员权限 Terminal 的 UIPI 拒绝路径。
- Windows 高负载下长期 Hook 调度。
- 物理鼠标在横向、纵向及嵌套分隔线上的完整 down/move/up 手感与缩放光标反馈。

Windows Terminal 没有公开 pane tree 或 Action 执行回执 API，所以“输入事件已插入”不能被错误表述为“布局状态已读取确认”。人工步骤见 `docs/TESTING.md`。

## 6. 已知恢复边界

- 正常运行期任一步安装失败都会按逆操作栈回滚已经完成的目标；每次恢复前执行 CAS，不覆盖并发用户修改。
- 强杀或断电若恰好发生在多文件写入和 committed manifest 落盘之间，当前版本没有持久化 prepared journal。此时应先保留当前文件，再用原字节备份和 `doctor` 诊断恢复。
- 可随时执行 `winter uninstall`；它只删除语义仍与 manifest 一致的托管项，用户后来修改的项会保留并报告。

## 7. 自定义 Shortcut 验证

- 现有三字段 schema 1 配置可加载，缺少的新字段自动获得默认 Prefix、动作和鼠标配置；加载后只在内存中迁移为 schema 2，原文件不被改写。
- `winter config` 能输出完整有效 TOML；`winter config --path` 返回 `%LOCALAPPDATA%\WinTerminalP\config.toml`。
- 自定义 `alt+a` Prefix、`new_tab = "t"` 和 `disabled` 可选动作已通过状态机/配置契约测试。
- 未知动作、重复 chord、普通无修饰 Prefix、系统保留组合和禁用 `shutdown` 均会拒绝启动。

## 8. 原生边界拖动与架构拆分验证

- UI Automation 实测在 Stable 1.24.11911.0 的独立 3 窗格窗口返回 3 个有效 `TermControl` 矩形，纯几何层推导出 3 段可命中分隔线；仓库探针 `native_pane_geometry_from_environment` 通过。
- 独立根分栏实测一次 `resizePane` 移动约父区域 5%；同方向嵌套分栏实测移动约局部父区域 5%，与 `PaneDrag` 的动态步长模型一致。
- `combined_input_hooks_install_and_stop` 已确认键盘与鼠标低级 Hook 可在同一消息线程安装并干净卸载。
- `cargo fmt --all -- --check`、`cargo test`、严格 Clippy 和 Release 三二进制构建均通过。
- 物理鼠标事件不能由自动探针伪造：统一 Hook 按安全设计透传 `LLMHF_INJECTED`，最终可视拖动仍需人工在 Release 控制器上确认。
