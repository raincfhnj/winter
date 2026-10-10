# 2026-10-10 已实现功能审查报告

> 本报告由 5 个 agent 并行审查产出：4 个只读领域审查 + 1 个独立验证，结论由 Lead 逐条复核。
> 审查期间工作树被另一进程持续修改，因此**所有结论都锚定到某个快照**，见 §1。

## 0. 修复进展（本报告之后的动作）

| 缺陷 | 状态 | 改动 |
|---|---|---|
| P1-1 系统切换键不清吞键账本 | **已修复** | `src/prefix.rs` `handle_armed_key_down` 的系统键分支改用 `reset_transient_state()`；新增回归测试 `system_shortcut_ends_the_session_with_its_suppressed_ledger`，已用「临时还原修复」证明该测试在未修复版本上失败 |
| P1-3 legacy 双 P shell 标记漏清 | **已修复** | `src/integration/shell.rs` 的 `LEGACY_SHELL_MARKER_PAIRS` 表覆盖 `WinTerminalPP`/`WinTerminalP` 两种拼写，`uninstall_legacy` 逐个清扫并按 profile 归并报告；`src/integration/legacy.rs` 的 fixture 参数化出双 P 变体 + 新增测试 `double_p_shell_markers_are_swept_too`（同样已证明"未修复即失败"）。顺带修正了旧断言 `!contains("WinTerminalP")` 的子串假阴性 |
| P1-2 鼠标拖动选错分隔线 | **已修复** | `drag-fixer`（task-5）在 `src/pane_layout.rs` 引入"可证聚焦目标"：只接受贴布局前缘（或镜像的贴后缘）且一边正落在该线上的窗格作为 focus 目标；`capturable_divider_at` 在无法证明时返回 `None`，`src/controller.rs` 据此 fail-closed 透传。判定依据是 `reference_resize`（复刻 Pane.cpp `ResizePane`/`_Resize` 语义）+ `consistent_trees`（枚举所有与矩形一致的切分树）——断言写成"对**每一棵**一致树，被移动的线都必须是指针下那条"。新增 8 个测试。依赖的 WT 源码片段由 Lead 在 `_wtres/` 被删除前抄录 |
| P2/P3 各项 | 未处理 | 见 §3 |

修复后的门禁（工作树，2026-10-10 22:4x 由 Lead 复跑）：`fmt --check` exit 0、`clippy -D warnings` exit 0、`test --all-targets` 全绿 = **lib 310 + winterd 15 + cli 12 + live_bridge 2**（5 ignored、另 live_autostart 1 ignored），**0 失败**。

三个 P1 的修复**各自都过了"还原修复后测试必须失败"的反向验证**（Lead 独立复跑，非采信修复者自述）：
- P1-1：还原后 `system_shortcut_ends_the_session_with_its_suppressed_ledger` 失败于"the cancelled session must not keep swallowing the prefix key"。
- P1-3：把标记表还原为只认单 P 后 `double_p_shell_markers_are_swept_too` 失败。
- P1-2：把 focus 选择还原为"总是取 leading 窗格"后，4 个测试失败，报错原文即契约——`dragging the divider at (600,250) … moved the splitter at Some(300) in some tree consistent with the observed rectangles; only the divider under the pointer may move`（staggered 布局为 `Some(300)` vs `Some(700)`）。

同步的文档改动：`README.md`、`docs/SHORTCUTS.md`、`docs/ARCHITECTURE.md`（§10 限制）写明 fail-closed 取舍；`CHANGELOG.md` 的 `### Fixed` 记录三项修复。

> 取证环境变化：`_wtres/`（WT 参考源码，未被 git 跟踪）、`docs/TESTING.md`、`docs/DEVELOPMENT.md`、`docs/fault-reviews/*`、`CONTRIBUTING.md`、`SECURITY.md`、`.github/workflows/ci.yml` 在本次审查进行中被另一开发者删除（README 同步精简）。因此本报告中对 `docs/TESTING.md` 行号的引用指向的是**删除前的内容**，可读作"当时文档声称的不变量"。

## 1. 快照与取证基线

| 项目 | 值 |
|---|---|
| 已提交基线 | `09800996696a800725ea65a44ca891b9ed39179b`（`feat: winter ui becomes a tmux-style session manager`），干净 worktree `%TEMP%\winter-head-check` |
| 工作树 | 审查期间从未提交 → 14~15 个 tracked 改动 + 8 个 untracked；tracked diff 快照 `ee9b76d603edbb7db3822233b80b0059cf27dd93` |
| 审查中间态 | 一度 `E0432` 编译失败（`Cargo.toml` 缺 `Win32_System_Registry`），随后自愈 |
| 取证时间 | 2026-10-10 21:20 – 22:2x |


**基线（HEAD）质量门全绿**（由 Lead 与独立验证者各自实跑复现）：`fmt` / `clippy -D warnings` / `test --all-targets` / `test --doc` / `doc -D warnings` / `release build --bins` 全部 exit 0；`cargo +1.88 check --all-targets --locked` 亦 exit 0。

| 二进制 | 会执行 | ignored |
|---|---|---|
| lib（`src/**`） | 276 | 0 |
| `winterd` | 15 | 0 |
| `tests/cli.rs` | 10 | 0 |
| `tests/live_bridge.rs` | 2 | 4 |
| **合计** | **303** | **4** |

**未提交工作树的门禁同样全绿**（lib 已增至 298、`tests/cli.rs` 12、新增 `tests/live_autostart.rs` 1 个 ignored → 327 runnable），但它是一个**正在施工**的快照：期间不可编译过一次，且新增了一个尚需安全加固的提权持久化功能。

> 编译失败的真实原因（Lead 复核）：`elevation.rs:12` 引入 `SHELLEXECUTEINFOW` / `ShellExecuteExW` / `SEE_MASK_NOCLOSEPROCESS`，windows 0.62.2 把这三个 API gate 在 `Win32_System_Registry` 之后，而 `Cargo.toml` 当时尚未启用该 feature。
> 注意：**仅靠 grep 会得出错误结论**——`Win32_System_Registry` 在 Rust 代码里"零直接调用"，但它是编译 `Win32_UI_Shell` 中这三个符号的必要条件。这正是 `fault-reviews/2026-10-08` 那条教训（"grep 结论必须过编译验证再执行"）的又一次命中，本次审查中也真的有一次 grep 误判被编译证伪。

## 2. 功能实现状态总览

图例：✅ 已实现且有测试 ｜ ⚠️ 已实现但有已知缺陷/文档不符 ｜ 🚧 未提交在建 ｜ ⛔ 未实现

| 能力 | 状态 | 证据 |
|---|---|---|
| 两段式 Prefix（Ctrl+B + 第二键，可配置、带超时） | ✅ | `prefix.rs` 27 测试；`prefix_only_arms_in_a_verified_terminal`、`timeout_wins_at_the_deadline_and_does_not_replay_prefix` |
| 仅 Windows Terminal 前台生效，其余全程透传 | ✅ | `handle_idle_key_down` 要求 `foreground_terminal`；`injected_prefix_passes_through` |
| 四向 split / focus / resize | ✅ | `registry.rs` 30 条动作单源派生；`arrows_map_to_focus_split_and_resize` |
| 标签页 new / next / prev / 0–9 / rename | ✅ | `activate_tab_0..9`、`character_commands_cover_tabs_panes_zoom_rename_and_shutdown` |
| close pane / toggle zoom | ✅ | 同上 |
| 隐藏 Action Bridge（fragment + 29 条 keybinding） | ✅ | `keymap.rs` 7 测试；编译期唯一性断言；F16/F17 被禁止 |
| 桥接安装/卸载/诊断（plan/install/uninstall/doctor） | ✅ | `integration/` 12 文件 + 81 个测试 |
| 无损 JSONC 编辑 + CAS + 原始字节备份 + 回滚 | ✅ | `jsonc.rs`/`transaction.rs`/`rollback.rs`；每步写前复检 CAS |
| legacy `WinTerminalP(P)` 迁移 | ✅（修复后） | 目录/id/标记三种标识现均覆盖双拼写；`double_p_shell_markers_are_swept_too` |
| PowerShell `OSC 9;9` 目录继承（含 pwsh 目录创建） | 🚧 | `shell.rs` +202/-22 未提交；真机已验证创建 `Documents\PowerShell` |
| 鼠标拖动原生分隔线 resize | ✅（修复后，含 fail-closed） | 可证布局正常捕获；不可证的嵌套布局透传。8 个新测试 + `consistent_trees` 全树断言 |
| 外围拖放（Explorer）不截获 | ✅ | 全 src 无 OLE/窗口创建；`external_drag_started_outside_a_divider_is_never_captured` |
| `winter ui` tmux 风格会话管理器 | ✅ | `ui.rs` 13 测试 + `dashboard.rs` 4 测试；`Enter` 经 `command.json` 回传控制器 |
| CLI：config / plan / install / uninstall / doctor / run / launch | ✅ | `tests/cli.rs` 10 条；退出码 0/1/2 与 README 一致 |
| UAC 提权自重启（run / launch / winterd） | ✅ | `run_controller` 在装 hook 前拒绝未提权 |
| 单实例互斥 | ✅ | `second_guard_is_rejected_until_first_is_dropped` |
| 登录自动启动（计划任务持久化提权） | 🚧 | `autostart.rs`/`scheduled_task.rs`/`identity.rs` 未提交，主体已实现但有安全前置条件未满足 |

## 3. 已验证缺陷清单（Lead 逐条复核）

每条都标注了**复核方式**：`[L]` = Lead 直接读代码/真机确认；`[T]` = 审查者 probe/测试实证。

### P1 — 影响已声明功能，建议发布前修

**P1-1 系统切换键取消 Prefix 时不清吞键账本** `[L]`
`src/prefix.rs:565-570`（`handle_armed_key_down` 的 `is_system_shortcut` 分支）只写 `self.state = Idle`，未走 `reset_transient_state()`（对比 `expire()`/`cancel()`/`cancel_if_foreground_changed()` 都会清账本）。

后果两条，都由 `handle_key_event:451-466` 的账本分支导致：
1. 会话结束后 prefix 键的 keyup 仍被判 `Consume` → 前台切换后的应用收到"没有 down 的 up"。
2. 切换回 Terminal 再按 Ctrl+B，第一次 keydown 被账本吞掉且只做 `remove()`（`prefix.rs:453`）→ **Prefix 不激活，用户必须按两次**。

复现：WT 前台 `Ctrl+B` → `Alt+Tab` → 释放 `B` → 切回 → 按一次 `Ctrl+B`。
修法：该分支改用 `reset_transient_state()`。

**P1-2 嵌套布局拖动会移动"另一条"分隔线** `[L]`
`controller.rs:519-556` + `action_worker.rs:275-303` 的策略是"聚焦被拖分隔线 leading 窗格 → 发 `resizePane(direction)` 步进"。但 Windows Terminal 的 `Pane::_Resize` 只会移动**当前节点自己的**分隔线：

```cpp
// _wtres/wtrepo/src/cascadia/TerminalApp/Pane.cpp:250-278  (main@7d06b26)
bool Pane::_Resize(const ResizeDirection& direction) {
    if (!DirectionMatchesSplit(direction, _splitState)) return false;
    auto amount = .05f;
    if (direction == ResizeDirection::Right || direction == ResizeDirection::Down) amount = -amount;
    ...
    _desiredSplitPosition = _ClampSplitPosition(changeWidth, _desiredSplitPosition - amount, actualDimension);
}
// Pane.cpp:291-331 ResizePane → 命中含焦点子树后 return _Resize(direction)
```

推演 `[A|B]|C`（外层水平分栏，左子节点内含 A|B）：拖 B|C 边界 → 聚焦 B → `resizePane(Right)` → 根节点发现含焦点的子节点是内层 → `子节点.ResizePane(Right) || root._Resize(Right)` → 内层（垂直、含焦点叶子 B）`_Resize` 成功返回 true → **移动的是 A|B 边界，B|C 边界不动**。`_HasFocusedChild`（`Pane.cpp:1208-1216`）与 `Tab::_UpdateActivePane`（`Tab.cpp:1344-1349`，`ClearActive()` 后只给活动叶子 `SetActive()`）确认了"只有叶子 `_lastActive`"这一前提。

影响：README/docs 把"鼠标拖动原生分隔线"列为核心特性，在 ≥3 窗格且被拖分隔线不在根层时行为错误。
待确认：本机 WT 1.24 二进制与参考源码是否同语义（需真机拖 3 窗格探针）。

**P1-3 legacy 迁移漏清 `WinTerminalPP` 双 P shell 标记** `[L]`
`src/integration/shell.rs:30-31` 只定义单 P 标记 `# >>> WinTerminalP shell integration >>>`，而 `legacy.rs:33/36` 的目录名与 action id 前缀都覆盖了双拼写 `WinTerminalPP`。真实机器证据：

```
C:\Users\Administrator\Documents\WindowsPowerShell\Microsoft.PowerShell_profile.ps1   (1592 字节, mtime 2026-10-08)
  L1: # >>> WinTerminalPP shell integration >>>
  L18: # <<< WinTerminalPP shell integration <<<
  L20: # >>> Winter shell integration >>>          ← 新块与旧块并存
%LOCALAPPDATA%\WinTerminalPP       exists=False
%LOCALAPPDATA%\WinTerminalP        exists=False
```

即迁移清掉了目录/fragment/settings 里的 id，却留下旧 prompt 包装，`winter doctor` 仍报 `healthy: true`。后果：PowerShell 5.1 提示符双重包装、重复 `OSC 9;9`。
修法：把 legacy 标记也做成多拼写表（与 `LEGACY_DIR_NAMES`/`LEGACY_ID_PREFIXES` 一致），并加一条播种双 P 的单测。

### P2 — 真实但非阻塞

| # | 缺陷 | 位置 | 复核 |
|---|---|---|---|
| P2-1 | `winter config`/`doctor` 加载 schema 1 配置时会**写回磁盘**，与 `docs/ARCHITECTURE.md:173`、`docs/TESTING.md:36`"只在内存中迁移、不改写用户文件"直接矛盾；有测试固化写回 | `config.rs:301-317` | `[L]` |
| P2-2 | Prefix chord 与动作 chord 不在同一冲突域：`prefix = "ctrl+a"` + `new_tab = "ctrl+a"` 被接受，而 `handle_idle_key_down` 优先匹配 prefix → `new_tab` 永久不可达 | `config.rs:220-264`、`prefix.rs:538-544` | `[L]` + `[T]` |
| P2-3 | `mouse_resize.enabled = false` 时**完全跳过**范围校验，越界值被接受后改回 `true` 即生效，与 `docs/TESTING.md:42` 不符 | `config.rs:53-56` | `[L]` |
| P2-4 | `winterd` 的 panic hook 在 `catch_unwind` 生效前 `process::abort()`，使 `HookHealth` fail-open + 看门狗在守护进程里**不可达**（`winter run` 仍可达）；与 `ARCHITECTURE.md` §9 失败语义表矛盾（`winterd.rs` 注释已自认这是有意取舍，但文档未同步） | `winterd.rs:175-182`、`hook.rs:265-283` | `[L]` |
| P2-5 | 拖动步进在队列饱和时被 `try_send` 丢弃后**不退回 residual**，光标与分隔线错位且无法补偿；队列 32 × 8 步 = 最多 256 次合成输入积压 | `controller.rs:528-542`、`pane_layout.rs:259-295` | `[T]` |
| P2-6 | "hook 有界 join 超时分离线程 + worker 无界 join"组合可让控制器**无法退出**（分离线程仍持 `worker_sender`，worker 只在所有 sender 掉落后才退出）。已排除 use-after-free | `hook.rs:226-244`、`action_worker.rs:175-185` | `[T]` |
| P2-7 | 破坏性 legacy 迁移是 `install` 的第一步，可行性检查（Terminal 未安装 / fragment 冲突）全在其后 → 失败时用户既无旧集成也无新集成 | `integration/mod.rs:80` vs `82-96` | `[T]` |
| P2-8 | `install`/`uninstall` 因 autostart 而**弹 UAC 并阻塞**，与 `ARCHITECTURE.md:207` 矛盾；UAC 被拒时降级为 `Skipped` 但**仍 exit 0** | `cli.rs:132-160`、`autostart.rs` | `[L]` |
| P2-9 | install/uninstall/doctor 的 JSON 顶层新增 `autostart` 字段，README 契约未同步；`tests/cli.rs` 无 install/uninstall 断言 | `cli.rs:178-202` | `[T]` |
| P2-10 | `docs/TESTING.md` 第 7 节多项 Win32 测试实际不存在（尤其"PID/启动标记变化时拒绝派发"只有假 HWND 覆盖） | `docs/TESTING.md:82-90` | `[T]` |

### P3 — 打磨项（节选）

- `keys.rs` 的 `logical_key_name` 用 `unreachable!()`，公共 `Display` 可被非表内 key 触发 panic（当前生产路径不可达）。
- `is_reserved_system_chord` 与 `SHORTCUTS.md:89` 清单不符：`alt+space`（系统菜单键）未在保留表内，Prefix 武装时会被吞。
- `hook.rs` 仍读 `dwExtraInfo == CONTROLLER_INPUT_MARKER`，而 `ARCHITECTURE.md:201` 称"不再读取该标记"。
- `dashboard.rs` 的 `take_command` 是 read→remove 非原子，UI 在两步之间写入的新命令会被静默删除。
- 强杀/断电会永久留下 `.winter-*.tmp-*` 暂存文件，无启动清理（`ARCHITECTURE.md:189` 已承认无 journal）。
- `transaction.rs` 的 `KEEP_BACKUPS_PER_LABEL` 注释引用的行号已过期；`CHANGELOG.md:139` 引用的 `jsonc-parser 0.33` 实际为 `0.34.0`。
- `docs/TESTING.md` §6 声称覆盖 `/* */` 注释与"不同缩进"，实际 fixture 中**不存在块注释**。
- `discovery.rs` 静默排除 symlink settings，导致首次启动报误导性的 `TerminalNotInstalled`。

## 4. 在建功能（未提交，不计入基线）

`src/autostart.rs`(542) + `src/platform/windows/scheduled_task.rs` + `identity.rs` + `elevation.rs` 扩展 + `cli.rs` 子命令：以登录计划任务 `RunLevel=HighestAvailable` 持久化提权控制器。主体已实现（含 17 个新测试），但：

- `[P1·安全]` 任务指向 `current_exe()` 同目录的 `winterd.exe`，而 `install.ps1` 把它装在用户可写的 `%USERPROFILE%\.cargo\bin` → 同用户中完整性进程可替换该二进制，下次登录获得 High IL 执行；无签名/哈希/ACL/admin-only 路径校验。
- `[P2]` 提权 helper 把任务 XML 写到 `%TEMP%\winter-task-<pid>.xml`（用户可写、pid 可预测），`fs::write` 跟随符号链接。
- `[P2]` `run_current_process_elevated_and_wait` 用 `WaitForSingleObject(INFINITE)`，helper 卡住则 `install` 永久挂起。

结论：**不可作为发布基线**，需先补安全前置条件 + staging 原子化 + 文档/TESTING 矩阵同步。

## 5. 文档与实现差异汇总

| 文档 | 实现 | 判定 |
|---|---|---|
| `ARCHITECTURE.md:173` / `TESTING.md:36` 内存迁移不改写文件 | `config.rs:301-317` 写回 | 矛盾（P2-1） |
| `TESTING.md:42` 越界即拒绝启动 | `enabled=false` 时跳过校验 | 缺口（P2-3） |
| `SHORTCUTS.md:90` 两动作不可同 chord | prefix 不入冲突域 | 缺口（P2-2） |
| `SHORTCUTS.md:89` 保留组合清单 | `alt+space` 被吞 | 清单不实（P3） |
| `ARCHITECTURE.md:201` 不再读 extra-info 标记 | `hook.rs` 仍读 | 相反（P3） |
| `ARCHITECTURE.md:207` install/uninstall 不触发 UAC | autostart 使其弹 UAC | 矛盾（P2-8） |
| `README.md:124` doctor JSON 形状 | 多出 `autostart` | 未同步（P2-9） |
| `README.md:220-230` 退出码表 | install/uninstall 部分失败仍 0 | 未覆盖（P2-9） |
| `CHANGELOG.md:81-85` 迁移会删 shell block | 双 P 标记漏清 | 不成立（P1-3） |
| `CHANGELOG.md:113-115` uninstall 完全事务化 | shell 步骤不在回滚栈 | 表述过强 |
| `fault-reviews/2026-10-08` 第 3 行"234 测试" | 当前 lib 276 / 合计 303（HEAD） | 计数过期 |
| `registry.rs` 模块注释引用 `MANAGED_BINDINGS` 表 | 表已改名 `MANAGED_BINDINGS`→仍存在，但 `SHORTCUT_SPECS` 已是 static 派生 | 措辞轻微过期 |

## 6. 测试与覆盖评估

- HEAD：lib 276 + winterd 15 + cli 10 + live_bridge 2（+4 ignored）= **303 会执行、4 ignored、307 listed**；`cargo test --doc` 0 个 doc test；release build 经 `cargo clean --release -p winter` 后真编译通过。
- 无空测试/恒真断言；仅 1 条测试名不副实（`keys.rs::generic_control_down_still_sets_left_slot_only` 未断言左侧 slot），3 处覆盖面落差（只测 `Direction::Left`、只测 `'7'`、`set_modifier` 重复分支无覆盖）。
- 无测试文件 13 个：其中**可测但漏测**的是 `integration/types.rs::default_documents_dir()`（唯一绕过环境变量直读真实 Documents/USERPROFILE/OneDrive 的路径）、`platform/windows/error.rs`（25 个 variant 的 Display 文案）、`bin/winterminalp.rs`（全仓只用 `cargo_bin("winter")`，该二进制从未被执行）；其余（`accessibility.rs`、`cursor.rs`、`launcher.rs`、schtasks/提权写路径）属 Win32 边界，只能靠 ignored 探针。
- `tests/cli.rs` 不是 smoke test（精确 exit code + JSON 逐字段 + 路径 + 逐字渲染串），但弱点明确：`doctor_accepts_a_fresh_config_directory` 只接受 `Some(0) | Some(2)`（会掩盖 0→2 回归），且**没有 install/uninstall 的集成断言**。
- `LOCALAPPDATA` 重定向真隔离但不彻底：config/integration 走 `env::var_os`（有断言证明落在 temp），但 `plan`/`doctor` 仍会读真实 Documents（只读）。
- `tests/live_bridge.rs` 的 4 个真实桌面探针需要专用 Terminal 窗口，永不进 CI —— 这是 P1-2 这类缺陷只能在真机暴露的结构性原因。
- MSRV 1.88 实测：`cargo +1.88 check` exit 0，但产生 **17 条 `dead_code` warning**，全部来自"只被 `const _: () = {…}` 断言块引用"的 const fn（`keys.rs`/`registry.rs`）。这是 rustc lint 的跨版本差异（1.99 不报），不是代码缺陷；且用 `assert!(1+1==3)` 在 1.88 上实测得到 `E0080`，**证明 const 断言在 1.88 上确实会被求值**。CI 的 MSRV 作业没有 `-D warnings` 所以保持绿，一旦加上就会红。
- 文档计数：`fault-reviews/2026-10-08` 第 8 行的"234 测试"在其引入 commit `eb5c312` 上**逐项吻合**（215+15+3+1，排除 ignored 口径），但相对当前 HEAD 已少算 **69** 个 runnable 测试；建议标注 commit/日期。


## 7. 建议的处理顺序

1. **P1-1** 一行级修复（系统键分支改用 `reset_transient_state()`）+ 单测。
2. **P1-3** legacy 标记多拼写 + 播种双 P 单测（当前真机即可验证）。
3. **P1-2** 重新设计拖动映射：需要按"被拖分隔线所属节点"选择步进目标，或改用绝对几何（例如按拖动像素直接计算 5% 步数序列），并补一条真机 E2E 断言两条 divider 坐标。
4. **P2-1/P2-2/P2-3** 三处配置层校验/语义，与文档二选一同步。
5. **P2-4** 定 winterd panic 语义并同步 §9；**P2-5/P2-6** 补有界退出与 residual 补偿。
6. **在建 autostart**：先补路径/签名校验、staging 原子化、有界等待，再补 README/ARCHITECTURE/TESTING 契约与 `tests/cli.rs` 断言。

## 8. 审查方法与可复核性

- 5 个 agent：4 个只读领域审查（键盘核心 / 运行时与 Win32 / integration 与 CLI / 独立门禁验证）+ Lead 综合。全部只读，未修改仓库任何文件；Lead 另建了 `%TEMP%\winter-head-check`（detached HEAD）用于不受在建改动影响的门禁取证。
- Lead 复核了各审查者报告的**全部 P1**：P1-1 读 `prefix.rs` 状态机确认账本未清；P1-2 直接读 `_wtres/wtrepo` 的 `Pane::_Resize`（250-278）与 `Pane::ResizePane`（291-331）确认只移动本节点分隔线，并用 `_HasFocusedChild`（1208-1216）、`Tab::_UpdateActivePane`（1344-1349）确认"只有叶子 `_lastActive`"前提；P1-3 在真实机器上逐串匹配用户 profile 确认双 P 标记残留且 `%LOCALAPPDATA%` 旧目录已删。
- 反向证伪（避免误报）：审查者用一次性 probe 排除了"未知键/Escape 路径账本泄漏"、"指针取消路径账本泄漏"、"`winter config` 回环丢失自定义 prefix"、"探测把未安装 shell 误判为已安装"等假设；Lead 证伪了独立验证者关于 `Win32_System_Registry` 是死配置的 grep 结论。
- 未执行：全部 `#[ignore]` 真实桌面探针（5 个，需要专用 Terminal 窗口 / 可应答 UAC 的提权会话）、真实 GitHub Actions、`cargo test --release`。
- 已知未决真机验证项：WT 1.24 二进制与参考源码的拖拽语义一致性；隐藏标签页的 `TermControl` 是否进入 UIA 树（会形成伪分隔线）；hook 线程 `SetCursor` 是否可见；`wt.exe` 从无控制台进程启动是否闪窗。

