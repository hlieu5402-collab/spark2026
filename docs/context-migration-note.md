# Context 三元组迁移笔记

## 背景
- 目标：所有公开调用链显式携带取消（Cancellation）、截止（Deadline）、预算（Budget）三元组。
- 新增模块：`spark-core::context::ExecutionContext`，提供三元组的零拷贝只读视图。
- 更新重点：`Service::poll_ready`、`DynService::poll_ready_dyn` 等公共 Trait 现要求传入 `&ExecutionContext`。

## 迁移步骤
1. **引用新视图**：在需要读取三元组的调用点（如 `poll_ready`）调用 `CallContext::execution()` 或 `PipelineContext::execution_context()`，再将返回值传递给目标 Trait。
2. **更新签名**：若自定义 Service/Layers 仍使用旧签名，请调整为 `fn poll_ready(&mut self, ctx: &ExecutionContext<'_>, cx: &mut TaskContext<'_>)` 并适配对象安全接口。
3. **预算判定**：使用 `ctx.budget(BudgetKind::Flow)` 或 `ctx.budgets()` 迭代预算，预算耗尽时返回 `Poll::Ready(ReadyCheck::Ready(ReadyState::BudgetExhausted(_)))` 或等价语义。
4. **取消与截止**：在耗时操作前检查 `ctx.cancellation().is_cancelled()`；若 `ctx.deadline().is_expired(now)` 为真，需及时终止并返回语义化错误。

## 生命周期/传递路径
```
CallContext::builder()         Pipeline Handler             Service::poll_ready
        │                            │                              │
        │ build()                    │ call_context()               │
        ▼                            ▼                              │
   CallContext ──────────────► PipelineContext ──────────────► ExecutionContext
        │                            │                              │
        │ execution()                │ execution_context()          │
        ▼                            ▼                              ▼
ExecutionContext (Cancellation, Deadline, Budgets) ──► 预算检查/超时判断/取消传播
```

## 常见问题
- **预算为空？** `CallContext::builder()` 会默认注入无限 Flow 预算，确保现有调用链不因迁移产生 `None`。
- **零预算请求**：上层若显式配置为 0，应在 `poll_ready` 中立即返回 `Poll::Ready(ReadyCheck::Ready(ReadyState::BudgetExhausted(_)))`，并触发 `Cancellation` 以阻止后续处理。
- **兼容对象安全实现**：`DynService::poll_ready_dyn` 同步接受 `&ExecutionContext`，桥接器 `ServiceObject` 已内置转换逻辑，无需额外改动。
