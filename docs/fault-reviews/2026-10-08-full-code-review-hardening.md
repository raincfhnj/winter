# 2026-10-08 全量代码审查与加固会话复盘

| 项目 | 内容 |
|---|---|
| 类型 | 全仓代码审查 + 分波次并行修复 |
| 范围 | 全部源码（controller / integration / platform / config / CLI / 测试 / CI） |
| 结果 | 3 个高危 + 约 15 个中危缺陷修复；提交 `0b038b4`…`6130a48` 及本次重构批次 |
| 门禁 | fmt、clippy `-D warnings`、234 测试（lib 215 + bin 15 + cli 3 + live_bridge 1）、doc tests、release build 全绿 |

## 1. 问题描述

4 个并行审查 agent 对仓库做只读审查，发现的问题集中在五类：

1. **时序与竞态（TOCTOU）**：`send_chord` 的前台窗口校验发生在慢速
   `validate_window_identity` 之前，`SendInput` 前不复检 —— 焦点切换时合成
   快捷键会注入到任意前台窗口，与 `input.rs` 文档承诺直接矛盾。
2. **事务性缺失**：`uninstall` 先删 shell/fragment 再循环处理 target，中途
   `?` 失败导致已删内容无 manifest 记录（`install` 有回滚、`uninstall` 没有）；
   fragment 在"语义相等但哈希变化"时丢失 ownership 记录，之后永远
   `Missing` 卸不掉。
3. **静默失效**：低层 hook handler panic 被 catch 后仅置 `enabled=false`，
   主线程在 `shutdown_receiver.recv()` 上永久阻塞 —— 服务假活且无诊断；
   `winter doctor` 遇到坏配置直接 `?` 退出，诊断命令不诊断。
4. **手工双表同步**：`SHORTCUT_SPECS`(30) 与 `MANAGED_BINDINGS`(29) 靠运行期
   count 断言维系；键码 parse/format/forward/reverse 三张表靠巧合互逆。
5. **错误模型分裂**：四种错误风格并存，`SettingsConflict` 一词五义，
   `PlatformError` 被 `to_string()` 拍平丢 source 链。

## 2. 根本原因

- 关键路径把"最易变的检查"放在"最慢的检查"之前，且检查后不再确认；
- 多步变更操作没有统一的 pre-flight → execute → persist 进度范式；
- FFI 回调内的失败只有局部 catch，没有把失效状态暴露给监督者；
- 同一份领域知识（动作表、键码表）在多个文件里各写一份，靠人肉与断言同步；
- 错误按"发生地"而非"语义"建模（用户可解决 vs 工具未完成 vs 平台失败）。

## 3. 解决方案

- **注入前重校验**：`input.rs::send_chord` 顺序改为 plan → 慢校验 → 前台
  HWND 复检 → 修饰键快照复检（fail-closed）→ `SendInput`，并修正文档。
- **事务化**：`uninstall` 采用 pre-flight（全量只读校验）→ 执行（每步后
  持久化 manifest）→ 失败返回 `Ok(report)+issues`；fragment ownership 改为
  "path 匹配 +（哈希匹配 ∨ 语义相等）"即保留，且从不收养外部文件。
- **监督与看门狗**：`HookHealth`（panic 计数 + enabled）暴露给控制器，
  `recv_timeout(200ms)` 轮询，handler 失效即报错退出；hook 关闭带
  `WM_QUIT` 重试 + 3s 有界 join（新增 `HookShutdownTimeout`）；shutdown
  握手 3s 过期 + 交错输入失效。
- **单一源**：`src/registry.rs` 常量注册表派生两张表，`const` 断言编译期
  查重/查数/查置换；`src/keys.rs` 66 行键码表驱动 parse/display/forward/
  reverse，编译期唯一性 + 全表往返/互逆属性测试。
- **错误三分法**：`SettingsConflict`（用户可解决的内容冲突）/
  `OperationIncomplete`（工具侧未完成，系统可能部分变更）/
  `Platform { #[source] }`（保留 Win32 源链）；worker 内部改 `WorkerError`
  枚举、对外报告字段保持字符串不变。

## 4. 预防措施

| 措施 | 落点 |
|---|---|
| 跨表一致性一律编译期断言，不靠运行期 count | `registry.rs` / `keys.rs` 的 `const _: ()` 块 |
| FFI 回调失效必须可观测（计数/状态位 + 监督者轮询） | `hook.rs::HookHealth` + `controller.rs::wait_for_shutdown` |
| 多文件写操作统一 pre-flight → execute → 持久化进度 | `integration/{fragment,targets,rollback}.rs` |
| 只读命令与写路径分离（analyze vs merge） | `jsonc.rs::analyze_keybindings` |
| 错误按语义建模，禁止一词多义的聚合变体 | `error.rs` 三分法 + `settings_context` 不再拍平 |
| CI 并行化 + `--locked` + MSRV job + doc tests | `.github/workflows/ci.yml` |

## 5. 经验教训

- 审查产出的"不可行项"要当场证伪：`Win32_System_Ole` 被标记为未使用，
  实际 `accessibility.rs` 的 `VARIANT` 依赖它，移除后立即编译失败 —— grep
  结论必须过编译验证再执行。
- 并行 agent 必须有显式文件所有权矩阵与跨 agent API 契约（签名级），
  否则同文件并发编辑会互相覆盖；被中断的 agent 要按"现状核对 + 补完"
  模式重派，而不是盲目重做。
- 行为契约（doctor JSON、退出码）应先定死再实现，并让测试按契约写、
  文档按契约改，三方由集成阶段对齐。
