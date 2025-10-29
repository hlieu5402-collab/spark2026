use std::{
    borrow::Cow,
    sync::{Arc, OnceLock},
};

use opentelemetry::{
    Context, KeyValue, global,
    trace::{
        self as otel_trace, Span, SpanContext as OtelSpanContext, SpanKind, TraceContextExt,
        Tracer, TracerProvider as _,
    },
};
use opentelemetry_sdk::{
    Resource,
    trace::{self, TracerProvider},
};
use spark_core::{
    observability::{
        ResourceAttrSet, SpanId, TraceContext, TraceFlags, TraceId, TraceState, TraceStateEntry,
        keys::tracing::pipeline as pipeline_keys,
    },
    pipeline::{
        controller::HandlerDirection,
        instrument::{
            HandlerSpan, HandlerSpanGuard, HandlerSpanParams, HandlerSpanTracer,
            HandlerTracerError, install_handler_tracer,
        },
    },
};
use tracing::dispatcher;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt};

#[cfg(feature = "test-util")]
use opentelemetry_sdk::export::trace::SpanData;
#[cfg(feature = "test-util")]
use test_support::InMemorySpanExporter;

/// 安装状态的全局缓存，确保 `install` 仅执行一次。
static INSTALL_STATE: OnceLock<InstallState> = OnceLock::new();

/// spark-otel 安装过程可能出现的错误类型。
///
/// # 教案式说明
/// - **意图（Why）**：框架化归纳安装阶段的全部失败路径，便于调用方在集成测试或启动流程中统一处理。
/// - **逻辑（How）**：覆盖“重复安装”“外部已设置 tracing Subscriber”“TraceState 转换失败”三类典型错误，并保留底层错误信息。
/// - **契约（What）**：所有错误都实现 [`std::error::Error`]，可直接交给 `anyhow`/`eyre` 等上层框架处理。
#[derive(Debug)]
pub enum Error {
    /// `spark_otel::install` 被重复调用。
    AlreadyInstalled,
    /// 外部提前设置了全局 `tracing` Subscriber，无法再次注册。
    SubscriberAlreadySet,
    /// `spark-core` 的 HandlerTracer 插槽已存在实现。
    HandlerTracerInstalled,
    /// W3C TraceState 在互转过程中校验失败。
    TraceStateConversion(String),
    /// 设置全局 Subscriber 失败的底层错误。
    SetGlobalSubscriber(tracing::dispatcher::SetGlobalDefaultError),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::AlreadyInstalled => f.write_str("spark-otel 已完成安装，禁止重复调用 install"),
            Error::SubscriberAlreadySet => {
                f.write_str("全局 tracing Subscriber 已存在，spark-otel 无法覆盖")
            }
            Error::HandlerTracerInstalled => f.write_str("spark-core 已注册其他 HandlerSpanTracer"),
            Error::TraceStateConversion(reason) => write!(f, "TraceState 互转失败: {reason}"),
            Error::SetGlobalSubscriber(err) => {
                write!(f, "设置 tracing 全局 Subscriber 失败: {err}")
            }
        }
    }
}

impl std::error::Error for Error {}

/// 框架安装后的持久状态，用于保持 OpenTelemetry Provider 与 Handler Tracer 生命周期。
struct InstallState {
    // 教案级说明（Why）
    // - `TracerProvider` 需保持有效以支撑进程生命周期内的导出/刷新操作；
    //   尽管目前仅依赖全局注册，也保留本地引用以便未来实现显式 shutdown。
    //
    // 教案级说明（Trade-offs）
    // - 该字段当前仅为生命周期护栏，因此以 `#[allow(dead_code)]` 抑制“未读取”告警，保持命名语义。
    #[allow(dead_code)]
    provider: TracerProvider,
    // 教案级说明（Why）
    // - `Arc<OtelHandlerTracer>` 需要与 Provider 同生命周期，以确保 Handler Span 创建时始终可用。
    // - 当前仅用于维持引用，不直接读取；未来若支持卸载可在此结构上扩展 Drop 逻辑。
    //
    // 教案级说明（Trade-offs）
    // - 通过 `#[allow(dead_code)]` 抑制编译器告警，换取命名语义的清晰度；若改名为 `_handler_tracer` 将丧失表达力。
    #[allow(dead_code)]
    handler_tracer: Arc<OtelHandlerTracer>,
}

/// 零配置安装入口：构建 OpenTelemetry Provider、注册 tracing 层并接管 Handler Span 追踪。
///
/// # 教案式说明
/// - **意图（Why）**：提供“一键式”体验，开发者只需调用一次 `install`，即可获得 Trace/Span 自动注入、`tracing`
///   日志桥接与 Handler 自动建 Span 的完整链路。
/// - **逻辑（How）**：
///   1. 防御性检查避免重复安装或外部提前设置的 Subscriber；
///   2. 构建 `TracerProvider` 并注册到 `opentelemetry::global`；
///   3. 使用 `tracing-subscriber` 组装 `fmt + EnvFilter + OpenTelemetry` Layer，设置为全局 Subscriber；
///   4. 构造 `OtelHandlerTracer`，通过 `spark-core` 的安装入口注入到 Pipeline；
///   5. 将安装状态写入 `INSTALL_STATE`，确保 Provider 与 Tracer 在进程生命周期内保持有效。
/// - **契约（What）**：多次调用返回 [`Error::AlreadyInstalled`]；调用前若外部已配置 Subscriber，返回
///   [`Error::SubscriberAlreadySet`]；成功后全局可观测性能力立即生效。
pub fn install() -> spark_core::Result<(), Error> {
    if INSTALL_STATE.get().is_some() {
        return Err(Error::AlreadyInstalled);
    }
    if dispatcher::has_been_set() {
        return Err(Error::SubscriberAlreadySet);
    }

    let state = install_impl()?;
    INSTALL_STATE
        .set(state)
        .map_err(|_| Error::AlreadyInstalled)
        .map(|_| ())
}

/// 根据 `spark-core` 的资源属性集合构造 OpenTelemetry `Resource`。
///
/// # 教案式说明
/// - **意图（Why）**：宿主在构建 Resource 时复用 `spark-core` 的轻量类型，保持可观测性标签语义一致；
/// - **逻辑（How）**：逐条映射为 OpenTelemetry 的 [`KeyValue`]，并调用 [`Resource::new`] 生成 SDK 所需结构；
/// - **契约（What）**：
///   - 输入切片应来自 `ResourceAttrSet`，通常由 `OwnedResourceAttrs` 提供；
///   - 返回的 `Resource` 不包含 schema URL；
///   - 若存在重复键，OpenTelemetry `Resource::new` 将保留最后一次出现的值。
pub fn resource_from_attrs(attrs: ResourceAttrSet<'_>) -> Resource {
    let owned = attrs
        .iter()
        .map(|attr| KeyValue::new(attr.key().to_string(), attr.value().to_string()));
    Resource::new(owned)
}

fn install_impl() -> spark_core::Result<InstallState, Error> {
    let tracer_provider = build_tracer_provider();
    global::set_tracer_provider(tracer_provider.clone());

    let tracer = tracer_provider.versioned_tracer(
        "spark.pipeline",
        Some(env!("CARGO_PKG_VERSION")),
        Some(Cow::Borrowed(env!("CARGO_PKG_NAME"))),
        None,
    );

    let subscriber = tracing_subscriber::registry()
        .with(build_env_filter())
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_opentelemetry::layer().with_tracer(tracer.clone()));
    tracing::subscriber::set_global_default(subscriber).map_err(Error::SetGlobalSubscriber)?;

    let handler_tracer = Arc::new(OtelHandlerTracer::new(tracer));
    let trait_tracer: Arc<dyn HandlerSpanTracer> = handler_tracer.clone();
    install_handler_tracer(trait_tracer).map_err(|err| match err {
        HandlerTracerError::AlreadyInstalled => Error::HandlerTracerInstalled,
    })?;

    Ok(InstallState {
        provider: tracer_provider,
        handler_tracer,
    })
}

fn build_env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

fn build_tracer_provider() -> TracerProvider {
    #[allow(unused_mut)]
    let mut builder = TracerProvider::builder().with_config(
        trace::config()
            .with_sampler(trace::Sampler::AlwaysOn)
            .with_resource(Resource::default()),
    );

    #[cfg(feature = "test-util")]
    {
        let exporter = fetch_in_memory_exporter();
        builder = builder.with_simple_exporter(exporter.clone());
    }

    builder.build()
}

#[cfg(feature = "test-util")]
fn fetch_in_memory_exporter() -> InMemorySpanExporter {
    static EXPORTER: OnceLock<InMemorySpanExporter> = OnceLock::new();
    EXPORTER.get_or_init(InMemorySpanExporter::default).clone()
}

#[cfg(feature = "test-util")]
mod test_support {
    use std::sync::{Arc, Mutex};

    use futures_util::future::BoxFuture;
    use opentelemetry::trace::{TraceError, TraceResult};
    use opentelemetry_sdk::export::trace::{ExportResult, SpanData, SpanExporter};

    /// InMemorySpanExporter 的教学级封装，复刻官方 testing 实现以规避 `rt-async-std` 特性强制
    /// 引入过时运行时的依赖。
    ///
    /// # 教案式说明
    /// - **意图（Why）**：仅保留“收集已完成 Span 供断言”这一测试能力，避免 `opentelemetry-sdk/testing`
    ///   间接启用 `async-std`（已被 RustSec 判定为弃用）。
    /// - **架构定位（Where）**：模块私有于 `spark-otel`，仅在 `test-util` 特性开启时构建，供
    ///   `fetch_in_memory_exporter` 提供稳定出口。
    /// - **实现逻辑（How）**：
    ///   1. 通过 `Arc<Mutex<Vec<SpanData>>>` 保存导出结果，确保跨线程共享与可变访问；
    ///   2. `export` 将批量 Span 追加至内部缓冲，并以 `BoxFuture` 立即返回；
    ///   3. `get_finished_spans`/`reset` 分别用于断言与清理，锁失败时转换为 `TraceError`。
    /// - **契约（What）**：
    ///   - `get_finished_spans`：返回当前缓冲快照；锁被毒化时返回错误；
    ///   - `reset`：清空缓存；
    ///   - `SpanExporter::export`：接收一批已完成 Span，追加成功即返回 `Ok(())`。
    /// - **权衡（Trade-offs）**：
    ///   - 牺牲官方实现附带的 builder 与统计字段，以换取零额外依赖；
    ///   - 使用 `Mutex` 而非无锁结构，因测试场景规模有限、简单性优先。
    #[derive(Clone, Debug, Default)]
    pub struct InMemorySpanExporter {
        spans: Arc<Mutex<Vec<SpanData>>>,
    }

    impl InMemorySpanExporter {
        /// 返回当前已收集的 Span 数据副本，供断言使用。
        ///
        /// # 教案式说明
        /// - **前置条件**：调用方仅在测试线程中使用，避免与生产导出流程混用；
        /// - **后置条件**：若成功，返回时内部缓冲保持不变；失败时原样保留原始数据。
        pub fn get_finished_spans(&self) -> TraceResult<Vec<SpanData>> {
            self.spans
                .lock()
                .map(|guard| guard.iter().cloned().collect())
                .map_err(TraceError::from)
        }

        /// 清空内部缓冲，确保多轮测试之间互不干扰。
        ///
        /// # 教案式说明
        /// - **副作用**：仅在持有锁成功时清除，锁毒化时静默忽略以最大化测试延续性。
        pub fn reset(&self) {
            if let Ok(mut guard) = self.spans.lock() {
                guard.clear();
            }
        }
    }

    impl SpanExporter for InMemorySpanExporter {
        /// 将批量完成的 Span 追加到内存缓冲。
        ///
        /// # 教案式说明
        /// - **实现细节（How）**：尝试获取互斥锁并将批次逐条追加；若锁获取失败，返回 `TraceError`，
        ///   让调用方可见具体失败原因。
        /// - **并发契约（What）**：遵循 OpenTelemetry“单线程调用同一 exporter”的约定，仍以锁
        ///   保护数据以抵御测试误用。
        fn export(&mut self, mut batch: Vec<SpanData>) -> BoxFuture<'static, ExportResult> {
            let result = self
                .spans
                .lock()
                .map(|mut guard| guard.append(&mut batch))
                .map_err(TraceError::from);

            Box::pin(async move { result })
        }

        /// 关闭导出器时清理残留数据，保证下一次安装得到干净状态。
        fn shutdown(&mut self) {
            self.reset();
        }
    }
}

/// OpenTelemetry 版本的 Handler Span Tracer，实现 `spark-core` 的自动建 Span 需求。
///
/// # 教案式说明
/// - **意图（Why）**：在 Pipeline Handler 调用前后自动维护分布式追踪 Span，并向业务层提供 `TraceContext`。
/// - **逻辑（How）**：使用 OpenTelemetry `Tracer` 构建 Span、设置父上下文与语义属性，再通过 [`HandlerSpanGuard`] 在作用域末尾调用
///   `Span::end` 完成收尾。
/// - **契约（What）**：始终返回与父上下文同一 `trace_id` 的新子上下文，并保证在 Span 生命周期结束时正确退出。
#[derive(Clone)]
struct OtelHandlerTracer {
    tracer: opentelemetry_sdk::trace::Tracer,
}

impl OtelHandlerTracer {
    fn new(tracer: opentelemetry_sdk::trace::Tracer) -> Self {
        Self { tracer }
    }

    fn start_with_parent(
        &self,
        params: &HandlerSpanParams<'_>,
        parent: &TraceContext,
    ) -> spark_core::Result<HandlerSpan, Error> {
        let parent_context = trace_context_to_otel(parent)?;
        let span_name = format!(
            "spark.pipeline.{}.{}",
            direction_tag(params.direction),
            params.descriptor.name()
        );

        let mut builder = self.tracer.span_builder(span_name);
        builder.span_kind = Some(otel_span_kind(params.direction));
        builder.attributes = Some(vec![
            KeyValue::new(
                pipeline_keys::ATTR_DIRECTION,
                direction_tag(params.direction).to_string(),
            ),
            KeyValue::new(pipeline_keys::ATTR_LABEL, params.label.to_string()),
            KeyValue::new(
                pipeline_keys::ATTR_COMPONENT,
                params.descriptor.name().to_string(),
            ),
            KeyValue::new(
                pipeline_keys::ATTR_CATEGORY,
                params.descriptor.category().to_string(),
            ),
            KeyValue::new(
                pipeline_keys::ATTR_SUMMARY,
                params.descriptor.summary().to_string(),
            ),
        ]);

        let span = self.tracer.build_with_context(builder, &parent_context);
        let derived = trace_context_from_otel(span.span_context())?;
        let guard = Box::new(OtelHandlerSpanGuard::new(span));

        Ok(HandlerSpan::from_parts(derived, Some(guard)))
    }
}

impl HandlerSpanTracer for OtelHandlerTracer {
    fn start_span(&self, params: &HandlerSpanParams<'_>, parent: &TraceContext) -> HandlerSpan {
        self.start_with_parent(params, parent)
            .unwrap_or_else(|_| fallback_span(parent))
    }
}

/// 持有 `tracing` Span 的 Guard，负责在 Handler 回调结束后退出 Span。
struct OtelHandlerSpanGuard {
    span: opentelemetry_sdk::trace::Span,
}

impl OtelHandlerSpanGuard {
    fn new(span: opentelemetry_sdk::trace::Span) -> Self {
        Self { span }
    }
}

impl HandlerSpanGuard for OtelHandlerSpanGuard {
    fn finish(mut self: Box<Self>) {
        self.span.end();
    }
}

fn direction_tag(direction: HandlerDirection) -> &'static str {
    match direction {
        HandlerDirection::Inbound => pipeline_keys::DIRECTION_INBOUND,
        HandlerDirection::Outbound => pipeline_keys::DIRECTION_OUTBOUND,
        _ => pipeline_keys::DIRECTION_UNSPECIFIED,
    }
}

fn otel_span_kind(direction: HandlerDirection) -> SpanKind {
    match direction {
        HandlerDirection::Inbound => SpanKind::Server,
        HandlerDirection::Outbound => SpanKind::Client,
        _ => SpanKind::Internal,
    }
}

/// 将 `spark-core` 的 [`TraceContext`] 转换为 OpenTelemetry 的远程 [`Context`]。
///
/// # 教案式说明
/// - **意图（Why）**：`spark-core` 采用自有的轻量结构存储 Trace/Span 信息，而 `tracing-opentelemetry`
///   在创建子 Span 时需要 W3C 语义的 `Context`；该函数承担两种表示之间的桥梁角色，保障跨组件的一致性。
/// - **逻辑（How）**：
///   1. 将 `TraceState` 键值对拷贝到 OpenTelemetry 的 [`otel_trace::TraceState`]；
///   2. 依据传入的 Trace/Span/Flags 构造 [`OtelSpanContext`]，并标记为“远程父级”；
///   3. 将 SpanContext 嵌入新的 [`Context`]，供 `tracing-opentelemetry` 在创建 Span 时作为父上下文。
/// - **契约（What）**：
///   - **输入参数**：`parent` 为当前 Handler 的父级 [`TraceContext`]，必须来源于可信的上游；
///   - **返回值**：成功时得到承载远程 Span 的 [`Context`]；失败时返回 [`Error::TraceStateConversion`]。
/// - **前置条件**：`parent.trace_state` 内的键值需满足 W3C 校验规则；
/// - **后置条件**：返回的 `Context` 与 `parent` 语义等价，可安全地传递到 OpenTelemetry 生态。
/// - **风险与取舍（Trade-offs）**：
///   - 直接重建 `TraceState` 会执行一次分配，但可确保与 W3C 规范对齐；
///   - 若未来需要零分配，可考虑预先缓存字符串，但需权衡内存占用与实现复杂度。
fn trace_context_to_otel(parent: &TraceContext) -> spark_core::Result<Context, Error> {
    let otel_state = otel_trace::TraceState::from_key_value(
        parent
            .trace_state
            .iter()
            .map(|entry| (entry.key.as_str(), entry.value.as_str())),
    )
    .map_err(|err| Error::TraceStateConversion(format!("构造 otel TraceState 失败: {err}")))?;

    let span_context = OtelSpanContext::new(
        otel_trace::TraceId::from_bytes(parent.trace_id.to_bytes()),
        otel_trace::SpanId::from_bytes(parent.span_id.to_bytes()),
        otel_trace::TraceFlags::new(parent.trace_flags.bits()),
        true,
        otel_state,
    );

    Ok(Context::new().with_remote_span_context(span_context))
}

/// 将 OpenTelemetry 的 [`OtelSpanContext`] 回转为 `spark-core` 的 [`TraceContext`]。
///
/// # 教案式说明
/// - **意图（Why）**：`spark-core` Pipeline 以 `TraceContext` 作为日志与上下文传递的统一媒介，Span 创建后需立即
///   将 OpenTelemetry 产生的新 SpanContext 映射回内部结构，才能继续向 Handler 传播。
/// - **逻辑（How）**：
///   1. 读取 `TraceState` 字符串表示，按 `key=value` 拆分并重建 [`TraceStateEntry`]；
///   2. 将 Trace/Span ID 以字节数组形式写入新的 [`TraceContext`]；
///   3. 将 [`otel_trace::TraceFlags`] 转换为 `spark-core` 的 [`TraceFlags`]。
/// - **契约（What）**：
///   - **输入参数**：`ctx` 必须来自可信的 OpenTelemetry Span；
///   - **返回值**：成功时返回新的 `TraceContext`，失败时给出 [`Error::TraceStateConversion`]。
/// - **前置条件**：`ctx.trace_state()` 的字符串需符合 `key=value` 且逗号分隔的格式；
/// - **后置条件**：返回的结构保证与 `ctx` 表达的 Trace/Span 信息一致，可直接注入日志或继续派生子 Span。
/// - **风险与取舍（Trade-offs）**：
///   - 字符串拆分会产生暂存分配，但换取了实现清晰度；
///   - 若未来存在大量 TraceState 条目，可引入自定义解析器以提升性能。
fn trace_context_from_otel(ctx: &OtelSpanContext) -> spark_core::Result<TraceContext, Error> {
    let state = ctx.trace_state().header();
    let trace_state = if state.is_empty() {
        TraceState::default()
    } else {
        let entries = state
            .split(',')
            .filter_map(|pair| pair.split_once('='))
            .map(|(key, value)| TraceStateEntry::new(key.to_string(), value.to_string()))
            .collect();
        TraceState::from_entries(entries).map_err(|err| {
            Error::TraceStateConversion(format!("otel TraceState -> spark 失败: {err}"))
        })?
    };

    Ok(TraceContext {
        trace_id: TraceId::from_bytes(ctx.trace_id().to_bytes()),
        span_id: SpanId::from_bytes(ctx.span_id().to_bytes()),
        trace_flags: TraceFlags::new(ctx.trace_flags().to_u8()),
        trace_state,
    })
}

/// 回退逻辑：在追踪安装缺失或出错时，本地生成子 [`TraceContext`]。
///
/// # 教案式说明
/// - **意图（Why）**：保持 Pipeline 的 Trace 结构完整，即便未成功对接 OpenTelemetry，也能保障日志继续携带合理的
///   `trace_id`/`span_id` 并维持父子关系。
/// - **逻辑（How）**：
///   1. 调用 [`TraceContext::generate_span_id`] 生成新的子 Span 标识；
///   2. 复用父级 `trace_id` 与状态，通过 [`TraceContext::child_context`] 派生子上下文；
///   3. 构造不携带 Guard 的 [`HandlerSpan`]，以零成本结束生命周期。
/// - **契约（What）**：输入为父级 TraceContext；输出的 HandlerSpan 仅用于内部传播，不会触发外部导出。
/// - **前置条件**：`parent` 必须来自合法调用链；
/// - **后置条件**：返回的 TraceContext 与父级共享同一 Trace ID，Span ID 为新生成值。
/// - **风险与取舍（Trade-offs）**：
///   - 放弃真实 Span 导出可确保 Pipeline 不中断，但失去可观测性后端的链路展示；
///   - 若频繁触发该逻辑，应在监控中及时告警以提醒安装流程可能异常。
fn fallback_span(parent: &TraceContext) -> HandlerSpan {
    let span_id = TraceContext::generate().span_id;
    let trace_context = parent.child_context(span_id);
    HandlerSpan::from_parts(trace_context, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::Value;
    use spark_core::observability::OwnedResourceAttrs;
    use std::collections::HashMap;

    #[test]
    fn resource_mapping_preserves_all_attributes() {
        let mut owned = OwnedResourceAttrs::new();
        owned.push_owned("service.name", "demo");
        owned.push_owned("deployment.environment", "staging");

        let resource = resource_from_attrs(owned.as_slice());
        let mut map = HashMap::new();
        for (key, value) in resource.iter() {
            let text = match value {
                Value::String(s) => s.to_string(),
                Value::Bool(flag) => flag.to_string(),
                Value::F64(number) => number.to_string(),
                Value::I64(number) => number.to_string(),
                Value::Array(array) => format!("{:?}", array),
            };
            map.insert(key.as_str().to_string(), text);
        }

        assert_eq!(map.get("service.name").unwrap(), "demo");
        assert_eq!(map.get("deployment.environment").unwrap(), "staging");
    }
}

#[cfg(feature = "test-util")]
/// 测试辅助工具：提供访问导出 Span 的接口。
pub mod testing {
    use super::*;

    /// 强制刷新 Provider，确保导出的 Span 已进入导出器。
    pub fn force_flush() {
        if let Some(state) = INSTALL_STATE.get() {
            for result in state.provider.force_flush() {
                let _ = result;
            }
        }
    }

    /// 获取自安装以来导出的全部 Span。
    pub fn finished_spans() -> Vec<SpanData> {
        fetch_in_memory_exporter()
            .get_finished_spans()
            .unwrap_or_default()
    }

    /// 清空 In-Memory Exporter 中的 Span，便于隔离测试案例。
    pub fn reset() {
        fetch_in_memory_exporter().reset();
    }
}
