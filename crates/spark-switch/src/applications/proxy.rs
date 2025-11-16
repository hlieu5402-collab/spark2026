use alloc::{boxed::Box, format, string::String, sync::Arc, vec::Vec};
use core::future;
use core::task::{Context as TaskContext, Poll};

use spark_codec_sip::types::HeaderKind;
use spark_codec_sip::{Aor, ContactUri};
use spark_codec_sip::{
    Header, Method, RequestLine, SipMessage, StatusLine, fmt::write_response, parse_request,
};
use spark_core::{
    CallContext, Context, CoreError, PipelineMessage, SparkError,
    buffer::{ErasedSparkBuf, ReadableBuffer},
    error::codes,
    service::{BoxService, Service, auto_dyn::bridge_to_box_service},
    status::{PollReady, ReadyCheck, ReadyState},
};
#[cfg(feature = "router-factory")]
use spark_router::ServiceFactory;
use tokio::task;

use crate::core::{CallSession, SessionManager};

use super::location::LocationStore;

/// `ProxyService` 尚未接入 INVITE 编排时返回的占位错误码。
const CODE_PROXY_UNIMPLEMENTED: &str = "switch.proxy.unimplemented";
const CODE_PROXY_INVALID_MESSAGE: &str = "switch.proxy.invalid_message";
const CODE_PROXY_ROUTING_FAILURE: &str = "switch.proxy.routing_failure";
const CODE_PROXY_CLIENT_FACTORY_UNAVAILABLE: &str = "switch.proxy.client_factory_unavailable";

/// `ProxyService` 承载 B2BUA 的 INVITE 处理入口。
///
/// # 教案式解读
/// - **意图 (Why)**：
///   - 聚合 [`LocationStore`]（终端位置存储）与 [`SessionManager`]（会话仓储），
///     为后续 A/B leg 编排提供统一的对象层 Service 实例；
///   - 作为 SIP Proxy/B2BUA 的入口 Handler，后续会解析 INVITE 并驱动双腿。
/// - **架构位置 (Where)**：
///   - 隶属于 `spark-switch::applications`，通常由 `spark_router` 根据路由
///     规则按需构造；
///   - 运行于对象层 Service 框架，与 [`spark_core::service::Service`] 契约对齐。
/// - **契约 (What)**：
///   - 必须持有 `LocationStore` 与 `SessionManager` 的共享引用以便在多实例
///     环境复用注册表与会话状态；
///   - `call` 返回 [`PipelineMessage`]，以保持与其他应用 Service 的一致性。
/// - **风险提示 (Trade-offs)**：当前实现仍返回占位错误，后续迭代需补齐
///   INVITE 解析、会话创建等逻辑；占位错误码 `switch.proxy.unimplemented`
///   能帮助上层快速识别尚未完成功能。
#[derive(Debug)]
pub struct ProxyService {
    location_store: Arc<LocationStore>,
    session_manager: Arc<SessionManager>,
}

impl ProxyService {
    /// 以共享状态构造 ProxyService。
    ///
    /// # 契约说明
    /// - **输入参数**：
    ///   - `location_store`：Registrar 注册表的 `Arc`，调用方需确保生命周期
    ///     长于 Service；
    ///   - `session_manager`：会话仓储的 `Arc`，负责跨线程维护 B2BUA 状态；
    /// - **前置条件**：调用方必须保证传入的 `Arc` 已经完成初始化，且能够在
    ///   Service 生命周期内保持可用；
    /// - **后置条件**：返回的实例实现 `Service<PipelineMessage>`，可直接交由
    ///   路由器调度。
    #[must_use]
    pub fn new(location_store: Arc<LocationStore>, session_manager: Arc<SessionManager>) -> Self {
        Self {
            location_store,
            session_manager,
        }
    }

    /// 处理输入的 INVITE 报文。
    ///
    /// # 教案式注释
    /// - **意图 (Why)**：集中封装 INVITE → B2BUA 状态机的处理入口，方便后续在
    ///   内部拆分解析、会话创建、B-leg 调度等子步骤；
    /// - **解析逻辑 (How)**：当前仅占位，直接返回 `switch.proxy.unimplemented`
    ///   错误；后续版本会根据 `PipelineMessage` 的 SIP 内容执行分支；
    /// - **契约 (What)**：
    ///   - **输入**：完整的 [`PipelineMessage`]，期望携带 SIP INVITE 字节；
    ///   - **输出**：`Result<PipelineMessage, SparkError>`，成功时返回下游应发
    ///     响应，失败时返回领域错误；
    ///   - **前置条件**：调用方应确保 `location_store`、`session_manager` 在构造
    ///     时已注入；
    ///   - **后置条件**：当前版本必定返回错误，提示功能尚未实现；
    /// - **风险提示 (Trade-offs)**：保留 `Err` 路径可避免外部误以为功能已可用，
    ///   同时使测试能够断言未来行为；TODO 逻辑会在后续 Epic 中补全。
    #[allow(clippy::result_large_err)]
    fn handle_request(
        &self,
        ctx: CallContext,
        message: PipelineMessage,
    ) -> spark_core::Result<PipelineMessage, SparkError> {
        let text = read_sip_text(message)?;
        let sip_message = parse_request(&text).map_err(map_invite_parse_error)?;
        let request_line = ensure_invite_request(&sip_message)?;
        let call_id = extract_call_id(&sip_message)?;
        let destination = self.resolve_contact(&sip_message, request_line)?;

        let a_leg = build_a_leg_placeholder_service();
        let mut session = CallSession::new(call_id, a_leg);
        let client_factory = ctx.client_factory().ok_or_else(|| {
            SparkError::new(
                CODE_PROXY_CLIENT_FACTORY_UNAVAILABLE,
                "CallContext 未注入 ClientFactory，无法建立 B-leg",
            )
        })?;
        let b_leg = client_factory.connect(&ctx, destination.as_ref())?;
        session.attach_b_leg(b_leg)?;
        let call_id_arc = session.call_id_arc().clone();
        self.session_manager.create_session(session)?;
        self.spawn_call_flow(call_id_arc);

        build_trying_response(&sip_message)
    }

    /// 根据解析后的 INVITE 与 Registrar 表计算被叫的 Contact。
    ///
    /// # 教案式注释
    /// - **意图 (Why)**：B-leg 拨号计划的最小实现，直接复用 Registrar 的 `Aor → Contact` 映射，
    ///   让 Proxy 能够在无外部依赖的情况下完成端到端连接决策；
    /// - **契约 (What)**：若 `To` 头缺失则退化为请求行 URI，若 Registrar 未命中则返回领域错误；
    /// - **风险 (Trade-offs)**：当前实现不考虑多注册实例或负载均衡，后续扩展可在此增加策略。
    fn resolve_contact(
        &self,
        message: &SipMessage<'_>,
        request_line: &RequestLine<'_>,
    ) -> spark_core::Result<ContactUri, SparkError> {
        let aor = message
            .headers
            .iter()
            .find_map(|header| match header {
                Header::To(addr) => Some(Aor::from_name_addr(addr)),
                _ => None,
            })
            .unwrap_or_else(|| Aor::from_uri(&request_line.uri));

        self.location_store.lookup(&aor).ok_or_else(|| {
            SparkError::new(
                CODE_PROXY_ROUTING_FAILURE,
                format!("Registrar 中缺少 `{aor}` 的 Contact，无法建立 B-leg"),
            )
        })
    }

    /// 启动异步任务运行会话状态机。
    ///
    /// - **意图**：将后续的 B2BUA 编排与 A/B leg 调用从同步 `call` 热路径拆离，避免阻塞
    ///   来电线程；
    /// - **前置条件**：`call_id` 必须已注册到 [`SessionManager`]；
    /// - **后置条件**：异步任务当前只是占位，后续会在此驱动真正的呼叫流程。
    fn spawn_call_flow(&self, call_id: Arc<str>) {
        let manager = Arc::clone(&self.session_manager);
        task::spawn(async move {
            let _ = run_call_flow(manager, call_id).await;
        });
    }
}

impl Service<PipelineMessage> for ProxyService {
    type Response = PipelineMessage;
    type Error = SparkError;
    type Future = future::Ready<spark_core::Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &Context<'_>, _: &mut TaskContext<'_>) -> PollReady<Self::Error> {
        Poll::Ready(ReadyCheck::Ready(ReadyState::Ready))
    }

    fn call(&mut self, ctx: CallContext, req: PipelineMessage) -> Self::Future {
        future::ready(self.handle_request(ctx, req))
    }
}

/// `ProxyServiceFactory` 将共享状态打包成可供路由器按需克隆的工厂。
///
/// # 教案式说明
/// - **意图 (Why)**：`spark_router` 在命中路由时会调用 `ServiceFactory::create`
///   生成新的对象层 Service，本结构封装 `Arc` 注入逻辑，避免每条路由重复编写
///   构造器；
/// - **契约 (What)**：实现 [`ServiceFactory`]，输出 `BoxService`；
/// - **架构 (Where)**：与 `ProxyService` 位于同一模块，由宿主在路由表更新时注入。
#[cfg(feature = "router-factory")]
#[derive(Clone, Debug)]
pub struct ProxyServiceFactory {
    location_store: Arc<LocationStore>,
    session_manager: Arc<SessionManager>,
}

#[cfg(feature = "router-factory")]
impl ProxyServiceFactory {
    /// 构造注入共享状态的工厂实例。
    ///
    /// - **输入**：与 [`ProxyService::new`] 相同的共享引用；
    /// - **前置条件**：传入的 `Arc` 需在多线程环境可安全复用；
    /// - **后置条件**：可交由 `spark_router` 存储并在命中路由后构造 Service。
    #[must_use]
    pub fn new(location_store: Arc<LocationStore>, session_manager: Arc<SessionManager>) -> Self {
        Self {
            location_store,
            session_manager,
        }
    }
}

#[cfg(feature = "router-factory")]
impl ServiceFactory for ProxyServiceFactory {
    fn create(&self) -> spark_core::Result<BoxService, SparkError> {
        let service = ProxyService::new(
            Arc::clone(&self.location_store),
            Arc::clone(&self.session_manager),
        );
        Ok(bridge_to_box_service(service))
    }
}

/// 将 Pipeline 消息还原为 UTF-8 文本。
///
/// # 教案式说明
/// - **意图 (Why)**：B2BUA 需要访问原始 INVITE 文本以执行 SIP 解析；
/// - **契约 (What)**：仅接受 `PipelineMessage::Buffer`，否则返回 `switch.proxy.invalid_message`；
/// - **风险 (Trade-offs)**：`try_into_vec` 会复制字节，若后续需要零拷贝可考虑共享切片生命周期。
fn read_sip_text(message: PipelineMessage) -> spark_core::Result<String, SparkError> {
    let buffer = match message {
        PipelineMessage::Buffer(buf) => buf,
        _ => {
            return Err(SparkError::new(
                CODE_PROXY_INVALID_MESSAGE,
                "Proxy 仅支持字节缓冲消息",
            ));
        }
    };

    let bytes = buffer.try_into_vec().map_err(|err| {
        SparkError::new(
            codes::APP_INVALID_ARGUMENT,
            format!("无法读取 INVITE 请求字节：{err}"),
        )
        .with_cause(err)
    })?;

    String::from_utf8(bytes).map_err(|err| {
        SparkError::new(
            codes::APP_INVALID_ARGUMENT,
            format!("SIP 报文必须为 UTF-8：{err}"),
        )
    })
}

/// 校验解析结果是否为 INVITE 请求。
fn ensure_invite_request<'a>(
    message: &'a SipMessage<'a>,
) -> spark_core::Result<&'a RequestLine<'a>, SparkError> {
    match &message.start_line {
        spark_codec_sip::types::StartLine::Request(line) if line.method == Method::Invite => {
            Ok(line)
        }
        spark_codec_sip::types::StartLine::Request(line) => Err(SparkError::new(
            CODE_PROXY_INVALID_MESSAGE,
            format!(
                "Proxy 当前仅支持 INVITE，请求方法为 {}",
                line.method.as_str()
            ),
        )),
        spark_codec_sip::types::StartLine::Response(_) => Err(SparkError::new(
            CODE_PROXY_INVALID_MESSAGE,
            "Proxy 期望 SIP 请求而非响应",
        )),
    }
}

/// 提取 Call-ID。
fn extract_call_id<'a>(message: &'a SipMessage<'a>) -> spark_core::Result<&'a str, SparkError> {
    message
        .find_header(HeaderKind::CallId)
        .and_then(|header| match header {
            Header::CallId(call_id) => Some(call_id.value),
            _ => None,
        })
        .ok_or_else(|| {
            SparkError::new(
                CODE_PROXY_INVALID_MESSAGE,
                "INVITE 缺少 Call-ID 头，无法创建会话",
            )
        })
}

/// 构造 100 Trying 响应，回送给主叫侧。
fn build_trying_response(
    message: &SipMessage<'_>,
) -> spark_core::Result<PipelineMessage, SparkError> {
    let mut headers: Vec<Header<'_>> = Vec::new();
    collect_headers(message, HeaderSelector::Via, &mut headers);
    collect_headers(message, HeaderSelector::From, &mut headers);
    collect_headers(message, HeaderSelector::To, &mut headers);
    collect_headers(message, HeaderSelector::CallId, &mut headers);
    collect_headers(message, HeaderSelector::CSeq, &mut headers);
    headers.push(Header::Extension {
        name: spark_codec_sip::HeaderName::new("Content-Length"),
        value: "0",
    });

    let status_line = StatusLine {
        version: "SIP/2.0",
        status_code: 100,
        reason: "Trying",
    };

    let mut text = String::new();
    write_response(&mut text, &status_line, &headers, &[]).map_err(|err| {
        SparkError::new(
            CODE_PROXY_UNIMPLEMENTED,
            format!("序列化 100 Trying 失败：{err}"),
        )
    })?;

    Ok(PipelineMessage::from_buffer(Box::new(
        OwnedReadableBuffer::new(text.into_bytes()),
    )))
}

/// 统一的 header 选择器，便于复用 registrar 的筛选逻辑。
#[derive(Clone, Copy)]
enum HeaderSelector {
    Via,
    From,
    To,
    CallId,
    CSeq,
}

fn collect_headers<'a>(
    message: &'a SipMessage<'a>,
    selector: HeaderSelector,
    output: &mut Vec<Header<'a>>,
) {
    output.extend(message.headers.iter().filter_map(|header| {
        matches!(
            (selector, header),
            (HeaderSelector::Via, Header::Via(_))
                | (HeaderSelector::From, Header::From(_))
                | (HeaderSelector::To, Header::To(_))
                | (HeaderSelector::CallId, Header::CallId(_))
                | (HeaderSelector::CSeq, Header::CSeq(_))
        )
        .then_some(*header)
    }));
}

fn map_invite_parse_error(err: spark_codec_sip::SipParseError) -> SparkError {
    SparkError::new(
        codes::APP_INVALID_ARGUMENT,
        format!("无法解析 INVITE 报文：{err}"),
    )
}

#[derive(Debug)]
struct OwnedReadableBuffer {
    data: Vec<u8>,
    cursor: usize,
}

impl OwnedReadableBuffer {
    fn new(data: Vec<u8>) -> Self {
        Self { data, cursor: 0 }
    }
}

impl ReadableBuffer for OwnedReadableBuffer {
    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.cursor)
    }

    fn chunk(&self) -> &[u8] {
        &self.data[self.cursor..]
    }

    fn split_to(&mut self, len: usize) -> spark_core::Result<Box<ErasedSparkBuf>, CoreError> {
        if len > self.remaining() {
            return Err(CoreError::new(
                codes::PROTOCOL_DECODE,
                "split length exceeds remaining bytes",
            ));
        }
        let end = self.cursor + len;
        let slice = self.data[self.cursor..end].to_vec();
        self.cursor = end;
        Ok(Box::new(Self::new(slice)))
    }

    fn advance(&mut self, len: usize) -> spark_core::Result<(), CoreError> {
        if len > self.remaining() {
            return Err(CoreError::new(
                codes::PROTOCOL_DECODE,
                "advance exceeds remaining bytes",
            ));
        }
        self.cursor += len;
        Ok(())
    }

    fn copy_into_slice(&mut self, dst: &mut [u8]) -> spark_core::Result<(), CoreError> {
        if dst.len() > self.remaining() {
            return Err(CoreError::new(
                codes::PROTOCOL_DECODE,
                "copy exceeds remaining bytes",
            ));
        }
        let end = self.cursor + dst.len();
        dst.copy_from_slice(&self.data[self.cursor..end]);
        self.cursor = end;
        Ok(())
    }

    fn try_into_vec(self: Box<Self>) -> spark_core::Result<Vec<u8>, CoreError> {
        Ok(self.data[self.cursor..].to_vec())
    }
}

fn build_a_leg_placeholder_service() -> BoxService {
    bridge_to_box_service(AlegPlaceholderService)
}

#[derive(Clone, Copy, Debug, Default)]
struct AlegPlaceholderService;

impl Service<PipelineMessage> for AlegPlaceholderService {
    type Response = PipelineMessage;
    type Error = SparkError;
    type Future = future::Ready<spark_core::Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &Context<'_>, _: &mut TaskContext<'_>) -> PollReady<Self::Error> {
        Poll::Ready(ReadyCheck::Ready(ReadyState::Ready))
    }

    fn call(&mut self, _ctx: CallContext, _req: PipelineMessage) -> Self::Future {
        future::ready(Err(SparkError::new(
            CODE_PROXY_UNIMPLEMENTED,
            "A-leg placeholder 无法处理媒体流，请等待 B2BUA 完整实现",
        )))
    }
}

#[allow(clippy::unused_async)]
async fn run_call_flow(
    _session_manager: Arc<SessionManager>,
    _call_id: Arc<str>,
) -> spark_core::Result<(), SparkError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use spark_core::{
        CallContext as CoreCallContext,
        service::{BoxService, ClientFactory},
    };

    struct RecordingClientFactory {
        targets: Mutex<Vec<String>>,
    }

    impl RecordingClientFactory {
        fn new() -> Self {
            Self {
                targets: Mutex::new(Vec::new()),
            }
        }

        fn targets(&self) -> Vec<String> {
            self.targets.lock().expect("poisoned").clone()
        }
    }

    impl ClientFactory for RecordingClientFactory {
        fn connect(
            &self,
            _ctx: &CoreCallContext,
            target: &str,
        ) -> spark_core::Result<BoxService, SparkError> {
            self.targets
                .lock()
                .expect("poisoned")
                .push(target.to_owned());
            Ok(build_a_leg_placeholder_service())
        }
    }

    fn build_invite_request() -> PipelineMessage {
        let text = "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP client.invalid;branch=z9hG4bK776asdhds\r\n\
From: \"Alice\" <sip:alice@example.com>;tag=1928301774\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: a84b4c76e66710\r\n\
CSeq: 314159 INVITE\r\n\
Contact: <sip:alice@client.invalid>\r\n\
Content-Length: 0\r\n\
\r\n";
        PipelineMessage::from_buffer(Box::new(OwnedReadableBuffer::new(text.as_bytes().to_vec())))
    }

    fn extract_text(message: PipelineMessage) -> String {
        match message {
            PipelineMessage::Buffer(buf) => {
                let bytes = buf.try_into_vec().expect("buffer conversion");
                String::from_utf8(bytes).expect("utf-8")
            }
            _ => panic!("unexpected message variant"),
        }
    }

    #[test]
    fn proxy_creates_session_and_responds_trying() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let location_store = Arc::new(LocationStore::new());
            location_store.register(
                Aor::new("sip:bob@example.com"),
                ContactUri::new("sip:bob@client.invalid"),
            );
            let session_manager = Arc::new(SessionManager::new());
            let service = ProxyService::new(location_store, Arc::clone(&session_manager));
            let factory = Arc::new(RecordingClientFactory::new());
            let ctx = CoreCallContext::builder()
                .with_client_factory(factory.clone())
                .build();
            let response = service
                .handle_request(ctx, build_invite_request())
                .expect("call success");

            let text = extract_text(response);
            assert!(text.starts_with("SIP/2.0 100"));
            assert!(text.contains("Call-ID: a84b4c76e66710"));

            let session = session_manager
                .get_session("a84b4c76e66710")
                .expect("session registered");
            assert!(session.b_leg().is_some());

            assert_eq!(
                factory.targets(),
                vec!["sip:bob@client.invalid".to_string()]
            );
        });
    }

    #[test]
    fn proxy_rejects_without_client_factory() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let location_store = Arc::new(LocationStore::new());
            location_store.register(
                Aor::new("sip:bob@example.com"),
                ContactUri::new("sip:bob@client.invalid"),
            );
            let session_manager = Arc::new(SessionManager::new());
            let service = ProxyService::new(location_store, session_manager);
            let ctx = CoreCallContext::builder().build();
            let err = service
                .handle_request(ctx, build_invite_request())
                .expect_err("missing factory");
            assert_eq!(err.code(), CODE_PROXY_CLIENT_FACTORY_UNAVAILABLE);
        });
    }
}
