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
    observability::{TraceContext, TraceFlags, TraceState, TraceStateEntry},
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
use opentelemetry_sdk::testing::trace::InMemorySpanExporter;

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
pub fn install() -> Result<(), Error> {
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

fn install_impl() -> Result<InstallState, Error> {
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
    ) -> Result<HandlerSpan, Error> {
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
                "spark.pipeline.direction",
                direction_tag(params.direction).to_string(),
            ),
            KeyValue::new("spark.pipeline.label", params.label.to_string()),
            KeyValue::new(
                "spark.pipeline.component",
                params.descriptor.name().to_string(),
            ),
            KeyValue::new(
                "spark.pipeline.category",
                params.descriptor.category().to_string(),
            ),
            KeyValue::new(
                "spark.pipeline.summary",
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
        HandlerDirection::Inbound => "inbound",
        HandlerDirection::Outbound => "outbound",
        _ => "unspecified",
    }
}

fn otel_span_kind(direction: HandlerDirection) -> SpanKind {
    match direction {
        HandlerDirection::Inbound => SpanKind::Server,
        HandlerDirection::Outbound => SpanKind::Client,
        _ => SpanKind::Internal,
    }
}

fn trace_context_to_otel(parent: &TraceContext) -> Result<Context, Error> {
    let otel_state = otel_trace::TraceState::from_key_value(
        parent
            .trace_state
            .iter()
            .map(|entry| (entry.key.as_str(), entry.value.as_str())),
    )
    .map_err(|err| Error::TraceStateConversion(format!("构造 otel TraceState 失败: {err}")))?;

    let span_context = OtelSpanContext::new(
        otel_trace::TraceId::from_bytes(parent.trace_id),
        otel_trace::SpanId::from_bytes(parent.span_id),
        otel_trace::TraceFlags::new(parent.trace_flags.bits()),
        true,
        otel_state,
    );

    Ok(Context::new().with_remote_span_context(span_context))
}

fn trace_context_from_otel(ctx: &OtelSpanContext) -> Result<TraceContext, Error> {
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
        trace_id: ctx.trace_id().to_bytes(),
        span_id: ctx.span_id().to_bytes(),
        trace_flags: TraceFlags::new(ctx.trace_flags().to_u8()),
        trace_state,
    })
}

fn fallback_span(parent: &TraceContext) -> HandlerSpan {
    let span_id = TraceContext::generate().span_id;
    let trace_context = parent.child_context(span_id);
    HandlerSpan::from_parts(trace_context, None)
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
