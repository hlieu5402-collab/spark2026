# 最终验收报告（2025-10-21）

## 一致性

### 命令：`rg -nEI "enum\s+PollReady" --glob 'crates/spark-core/**'`

```
rg: error parsing flag -E: grep config error: unknown encoding: I
```

### 命令：`rg -nEI "\bBackpressureReason\b" --glob 'crates/spark-core/**'`

```
rg: error parsing flag -E: grep config error: unknown encoding: I
```

### 命令：`rg -nEI "Busy\s*\(\s*.*Budget"`

```
rg: error parsing flag -E: grep config error: unknown encoding: I
```

### 命令：`rg -n "fn .*\\(.*&Context" crates/spark-core/src/transport/traits`

```
crates/spark-core/src/transport/traits/generic.rs:60:    fn local_addr(&self, ctx: &Context<'_>) -> TransportSocketAddr;
crates/spark-core/src/transport/traits/generic.rs:140:    fn scheme(&self, ctx: &Context<'_>) -> &'static str;
crates/spark-core/src/transport/traits/object.rs:54:    fn local_addr_dyn(&self, ctx: &Context<'_>) -> TransportSocketAddr;
crates/spark-core/src/transport/traits/object.rs:110:    fn local_addr_dyn(&self, ctx: &Context<'_>) -> TransportSocketAddr {
crates/spark-core/src/transport/traits/object.rs:160:    fn scheme_dyn(&self, ctx: &Context<'_>) -> &'static str;
crates/spark-core/src/transport/traits/object.rs:243:    fn scheme_dyn(&self, ctx: &Context<'_>) -> &'static str {
```

## DevX

### 命令：`rg -n "\bhuman\(\)|\bhint\(\)" spark-core`

```
crates/spark-core/src/error.rs:283:        self.core.human()
crates/spark-core/src/error.rs:299:        self.core.hint()
crates/spark-core/src/error.rs:583:        self.core.human()
crates/spark-core/src/error.rs:589:    /// - 在域层封装 `hint()`，可让业务同学快速找到排障手册，而无需了解框架内部结构。
crates/spark-core/src/error.rs:599:        self.core.hint()
crates/spark-core/src/error.rs:659:/// - 若新增错误码，需要同步更新此表、文档以及集成测试，否则 `hint()` 将返回 `None`。
crates/spark-core/tests/errors_human_hint.rs:1://! 集成测试：确保 `human()` / `hint()` 行为与文档对齐。
crates/spark-core/tests/errors_human_hint.rs:4://! - 框架 DoD 要求“新人 30 分钟内按 hint() 修复样例”，因此必须锁定常见错误码的提示文案。
crates/spark-core/tests/errors_human_hint.rs:23:    // human() 应返回静态摘要，hint() 返回详细操作指引。
crates/spark-core/tests/errors_human_hint.rs:24:    assert_eq!(core_error.human(), "传输层超时：请求在约定时限内未获得响应");
crates/spark-core/tests/errors_human_hint.rs:26:        core_error.hint().as_deref(),
crates/spark-core/tests/errors_human_hint.rs:33:        spark_error.human(),
crates/spark-core/tests/errors_human_hint.rs:37:        spark_error.hint().as_deref(),
crates/spark-core/tests/errors_human_hint.rs:43:        domain_error.human(),
crates/spark-core/tests/errors_human_hint.rs:47:        domain_error.hint().as_deref(),
crates/spark-core/tests/errors_human_hint.rs:52:/// 未备案的错误码应回退到 message()，同时不提供 hint()，确保调用方感知待补充文档。
crates/spark-core/tests/errors_human_hint.rs:58:    // human() 回退到原始消息，hint() 为 None。
crates/spark-core/tests/errors_human_hint.rs:59:    assert_eq!(core_error.human(), "raw implementation detail");
crates/spark-core/tests/errors_human_hint.rs:60:    assert!(core_error.hint().is_none());
```

## 测试/生产

### 命令：`cargo test --workspace`

```
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.18s
     Running unittests src/lib.rs (target/debug/deps/spark_codec_line-4fceca62fb07f013)

running 4 tests
test line::tests::decode_respects_frame_budget ... ok
test line::tests::decode_marks_incomplete_without_newline ... ok
test line::tests::decode_returns_complete_when_newline_present ... ok
test line::tests::encode_appends_newline ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running unittests src/lib.rs (target/debug/deps/spark_core-294e61ef70492e54)

running 28 tests
test buffer::message::tests::try_into_user_preserves_original_on_failure ... ok
test buffer::message::tests::downcast_helpers_work_for_user_message ... ok
test configuration::builder::tests::apply_change_detects_hash_gap ... ok
test configuration::builder::tests::apply_change_emits_audit_event ... ok
test configuration::builder::tests::apply_change_rolls_back_on_audit_failure ... ok
test configuration::builder::tests::build_generates_redacted_snapshot ... ok
test configuration::builder::tests::build_propagates_source_failure ... ok
test configuration::builder::tests::build_requires_profile ... ok
test configuration::builder::tests::build_requires_sources ... ok
test configuration::snapshot::tests::snapshot_redacts_encrypted_text ... ok
test deprecation::tests::emit_only_once ... ok
[spark-core][deprecation] symbol=demo since=0.1.0 removal=0.3.0 tracking=None migration=None
test deprecation::tests::emit_with_fields ... ok
test deprecation::tests::has_emitted_tracks_state ... ok
test error::tests::impl_to_domain_to_core_roundtrip_preserves_message_and_cause ... ok
test limits::tests::default_plan_matches_resource ... ok
test limits::tests::evaluate_queue_allows_until_capacity ... ok
test limits::tests::metrics_hook_records_drop_and_usage ... ok
test limits::tests::settings_parse_overrides ... ok
test observability::events::tests::application_event_helpers_work ... ok
test runtime::slo::tests::ensure_hot_reloadable_guard ... ok
test runtime::slo::tests::parse_and_evaluate_rule ... ok
test runtime::slo::tests::trigger_evaluate_with_hysteresis ... ok
test status::ready::tests::budget_decision_exhausted_maps_to_budget_state ... ok
test status::ready::tests::budget_decision_granted_with_remaining_maps_to_ready ... ok
test status::ready::tests::budget_decision_granted_zero_remaining_maps_to_budget_state ... ok
test status::ready::tests::budget_exhaustion_maps_to_budget_state ... ok
test status::ready::tests::queue_full_maps_to_busy ... ok
test status::ready::tests::recovery_maps_to_ready ... ok

test result: ok. 28 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/codec_limits.rs (target/debug/deps/codec_limits-0110d3e223c2bef3)

running 3 tests
test decode_context_rejects_oversized_frame ... ok
test decode_context_budget_consumption_and_refund ... ok
test encode_context_budget_and_depth_behavior_matches_decoder ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/errors_human_hint.rs (target/debug/deps/errors_human_hint-9c046ea352c0a831)

running 2 tests
test errors_human_hint_known_code ... ok
test errors_human_hint_unknown_code ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/loom_concurrency.rs (target/debug/deps/loom_concurrency-2312a36e1f39bbcb)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/slo_policy_map.rs (target/debug/deps/slo_policy_map-2615565ed5b7912d)

running 1 test
test slo_policy_mapping_baseline ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

   Doc-tests spark_codec_line

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

   Doc-tests spark_core

running 8 tests
test crates/spark-core/src/buffer/pool.rs - buffer::pool::BufferPool (line 32) ... ok
test crates/spark-core/src/buffer/message.rs - buffer::message::PipelineMessage (line 122) ... ok
test crates/spark-core/src/cluster/flow_control.rs - cluster::flow_control::SubscriptionStream (line 145) ... ignored
test crates/spark-core/src/buffer/pool.rs - buffer::pool::PoolStats (line 216) ... ok
test crates/spark-core/src/error.rs - error::CoreError::new (line 67) ... ok
test crates/spark-core/src/observability/events.rs - observability::events::OpsEventBus::set_event_policy (line 391) ... ignored
test crates/spark-core/src/security/policy.rs - security::policy (line 9) ... ignored
test crates/spark-core/src/observability/events.rs - observability::events::CoreUserEvent (line 111) ... ok

test result: ok. 5 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 0.02s

```

### 命令：`promtool check rules docs/observability/prometheus-rules.yml`

```
bash: command not found: promtool
```

## 治理/供应链

### 命令：`cargo semver-checks check-release`

```
error: no such command: `semver-checks`

help: view all installed commands with `cargo --list`
help: find a package to install `semver-checks` with `cargo search cargo-semver-checks`
```

