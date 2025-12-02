use alloc::{borrow::ToOwned, boxed::Box, format, string::String, sync::Arc, vec::Vec};
use core::future;
use core::str;
use core::task::{Context as TaskContext, Poll};

use spark_codec_sdp::{
    Attribute, MediaDesc, SessionDesc, format_sdp,
    offer_answer::{
        AnswerCapabilities, AudioAcceptPlan, AudioAnswer, AudioCaps, AudioCodec, TelephoneEvent,
        apply_offer_answer,
    },
    parse_sdp,
};
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

use crate::core::{
    CallSession, SessionManager,
    session::{AudioNegotiationProfile, AudioNegotiationState, DtmfProfile},
};
use crate::error::SwitchError;

use super::location::LocationStore;

/// `ProxyService` 尚未接入 INVITE 编排时返回的占位错误码。
const CODE_PROXY_UNIMPLEMENTED: &str = "switch.proxy.unimplemented";
const CODE_PROXY_INVALID_MESSAGE: &str = "switch.proxy.invalid_message";
const CODE_PROXY_ROUTING_FAILURE: &str = "switch.proxy.routing_failure";
const CODE_PROXY_CLIENT_FACTORY_UNAVAILABLE: &str = "switch.proxy.client_factory_unavailable";

/// 默认的媒体能力：PCMU 为首选，同时接受 PCMA 与 RFC 4733 DTMF。
const DEFAULT_NEGOTIATION_CAPS: AnswerCapabilities =
    AnswerCapabilities::audio_only(AudioCaps::new(AudioCodec::Pcmu, true, true, true));

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
        let sip_payload = read_sip_payload(message)?;
        let sip_text = sip_payload.as_text()?;
        let sip_message = parse_request(sip_text).map_err(map_invite_parse_error)?;
        let request_line = ensure_invite_request(&sip_message)?;
        let call_id = extract_call_id(&sip_message)?;
        let offer_sdp = extract_offer_sdp(&sip_message)?;
        let destination = self.resolve_contact(&sip_message, request_line)?;

        let a_leg = build_a_leg_placeholder_service();
        let mut session = CallSession::new(call_id, a_leg);
        session.set_offer_sdp(offer_sdp);
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

    /// 根据拨号计划为当前 INVITE 选择合适的 B-leg。
    ///
    /// # 教案式注释
    /// - **意图 (Why)**：聚合“内部分机（Registrar 命中）”与“外部路由（DynRouter）”两种拨号路径，
    ///   避免上层重复实现判定逻辑；
    /// - **契约 (What)**：
    ///   - 输入：`CallContext`（为连接外部服务提供 `ClientFactory` 等运行时能力）与请求行；
    ///   - 输出：可直接挂载到 [`CallSession`] 的 [`BoxService`]；
    ///   - 前置条件：`CallContext` 在构造阶段已注入 `ClientFactory`；
    ///   - 后置条件：若 Registrar 命中则返回基于 `Contact` 的 B-leg，否则进入外部路由流程；
    /// - **执行逻辑 (How)**：
    ///   1. 调用 [`Self::lookup_internal_destination`] 检查 Request-URI 是否存在注册记录；
    ///   2. 命中后调用 [`ClientFactory::connect`](spark_core::service::ClientFactory::connect)
    ///      生成直连 B-leg；
    ///   3. 未命中则调用 [`Self::route_external_call`]，为未来的 DynRouter 接入预留扩展点；
    /// - **风险 (Trade-offs)**：当前仓库尚未提供 DynRouter 契约，因此外部路由暂时返回错误；
    ///   一旦框架补齐 `spark_core::router` 模块，即可在该函数中无缝切换到外部路由逻辑。
    fn route_b_leg(
        &self,
        ctx: &CallContext,
        request_line: &RequestLine<'_>,
    ) -> spark_core::Result<BoxService, SparkError> {
        if let Some(contact) = self.lookup_internal_destination(request_line) {
            let client_factory = ctx.client_factory().ok_or_else(|| {
                SparkError::new(
                    CODE_PROXY_CLIENT_FACTORY_UNAVAILABLE,
                    "CallContext 未注入 ClientFactory，无法建立 B-leg",
                )
            })?;
            return client_factory.connect(ctx, contact.as_ref());
        }

        self.route_external_call()
    }

    /// 依据 Request-URI 检查 Registrar 中是否存在匹配的 Contact。
    ///
    /// - **意图 (Why)**：Dialplan 的第一步是尝试命中内部分机，若成功即可直接复用注册表信息；
    /// - **契约 (What)**：输入为解析后的请求行，输出为可选的 [`ContactUri`]；
    /// - **逻辑 (How)**：使用 `Aor::from_uri` 将 Request-URI 规整为 Registrar 的索引，再调用
    ///   [`LocationStore::lookup`]；
    /// - **边界情况 (Risk)**：若 INVITE 使用别名 URI 或未在 Registrar 注册，将返回 `None`，
    ///   并触发外部路由分支。
    fn lookup_internal_destination(&self, request_line: &RequestLine<'_>) -> Option<ContactUri> {
        let aor = Aor::from_uri(&request_line.uri);
        self.location_store.lookup(&aor)
    }

    /// 为外部呼叫预留的路由逻辑。
    ///
    /// - **现状说明**：`spark-core` 尚未暴露 DynRouter 契约，无法真正执行 R3.2 要求；
    ///   本函数以领域错误明确提示“外部路由未实现”，便于后续 Epic 衔接；
    /// - **后续扩展**：一旦 `CallContext` 能够提供 `DynRouter` 引用，可在此构造 `RoutingIntent`
    ///   并调用 `router.route_dyn`，将返回的 `BoxService` 作为 B-leg。
    fn route_external_call(&self) -> spark_core::Result<BoxService, SparkError> {
        Err(SparkError::new(
            CODE_PROXY_ROUTING_FAILURE,
            "Registrar 未命中，且 DynRouter 功能尚未在 spark-core 中开放",
        ))
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
/// 承载 SIP 文本的零拷贝包装器，避免在 INVITE 入口处强制分配 `String`。
///
/// # 教案式说明
/// - **意图 (Why)**：
///   - 规避原先 `read_sip_text` 将缓冲整体转换为 `String` 的额外分配，保留原始字节以便后续零拷贝解析；
///   - 为 SIP 解析器提供 UTF-8 受控视图，同时确保底层缓冲在解析期间保持有效生命周期。
/// - **契约 (What)**：
///   - 输入：必须是 [`PipelineMessage::Buffer`] 变体，内部承载完整的 SIP 报文字节；
///   - 输出：`SipPayload` 保存原始字节所有权，可通过 [`SipPayload::as_text`] 获取 `&str` 视图；
///   - 前置条件：调用方需确保缓冲内容即完整的 SIP 报文；
///   - 后置条件：成功创建后，`SipPayload` 会在自身生命周期内维护字节所有权，`as_text` 返回的切片始终有效。
/// - **解析逻辑 (How)**：
///   1. 匹配 `PipelineMessage` 变体，非缓冲则返回领域错误；
///   2. 通过 `try_into_vec` 将只读缓冲扁平化为 `Vec<u8>`（当前实现仍需一次复制，但避免了 `String` 的双份内存）；
///   3. `as_text` 中使用 `str::from_utf8` 将字节安全地视为 UTF-8，失败时返回带错误码的 `SparkError`；
/// - **风险提示 (Trade-offs & Gotchas)**：
///   - 若未来提供真正零拷贝的 SIP 解析器，可在此处改为借用切片而非分配 `Vec`；
///   - 仍依赖 UTF-8 语义，若遇到非 UTF-8 报文会被拒绝。
struct SipPayload {
    bytes: Vec<u8>,
}

impl SipPayload {
    /// 从管道消息中提取 SIP 原始字节。
    fn from_pipeline_message(message: PipelineMessage) -> spark_core::Result<Self, SparkError> {
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

        Ok(Self { bytes })
    }

    /// 暴露 UTF-8 视图，供 SIP 解析器使用。
    fn as_text(&self) -> spark_core::Result<&str, SparkError> {
        str::from_utf8(&self.bytes).map_err(|err| {
            SparkError::new(
                codes::APP_INVALID_ARGUMENT,
                format!("SIP 报文必须为 UTF-8：{err}"),
            )
        })
    }
}

fn read_sip_payload(message: PipelineMessage) -> spark_core::Result<SipPayload, SparkError> {
    SipPayload::from_pipeline_message(message)
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

/// 提取 INVITE 报文中的 SDP Offer。
///
/// # 教案式说明
/// - **意图 (Why)**：将 `SipMessage` 的原始主体转换为可复用的 UTF-8 文本，供会话状态机执行媒体协商；
/// - **契约 (What)**：若主体为空或非 UTF-8，将返回领域错误；成功时返回拥有所有权的 `String`；
/// - **实现 (How)**：直接依赖 `str::from_utf8`，避免额外复制。
fn extract_offer_sdp(message: &SipMessage<'_>) -> spark_core::Result<String, SparkError> {
    if message.body.is_empty() {
        return Err(SparkError::new(
            CODE_PROXY_INVALID_MESSAGE,
            "INVITE 缺少 SDP Offer 正文，无法进入媒体协商",
        ));
    }

    str::from_utf8(message.body)
        .map(|body| body.to_owned())
        .map_err(|err| {
            SparkError::new(
                codes::APP_INVALID_ARGUMENT,
                format!("INVITE SDP 必须为 UTF-8：{err}"),
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
    session_manager: Arc<SessionManager>,
    call_id: Arc<str>,
) -> spark_core::Result<(), SparkError> {
    let call_id_str = call_id.as_ref();
    let offer_sdp = {
        let session = session_manager.get_session(call_id_str).ok_or_else(|| {
            SwitchError::SessionNotFound {
                call_id: call_id_str.to_owned(),
            }
        })?;
        session.offer_sdp().map(str::to_owned)
    };

    let offer_sdp = match offer_sdp {
        Some(sdp) => sdp,
        None => {
            return Err(SwitchError::Internal {
                detail: format!(
                    "call flow missing SDP offer before negotiation for Call-ID `{call_id_str}`",
                ),
            }
            .into());
        }
    };

    let offer_desc = parse_sdp(&offer_sdp).map_err(|err| SwitchError::ServiceFailure {
        context: "proxy.call_flow.offer.parse".to_owned(),
        detail: format!("failed to parse SDP offer: {err}"),
    })?;

    let plan = apply_offer_answer(&offer_desc, &DEFAULT_NEGOTIATION_CAPS);
    let audio_state = build_audio_state(plan.audio);
    let b_leg_offer = build_b_leg_offer_sdp(&offer_desc, &audio_state);
    let a_leg_answer = build_a_leg_answer_sdp(&offer_desc, &audio_state);

    {
        let mut session = session_manager
            .get_session_mut(call_id_str)
            .ok_or_else(|| SwitchError::SessionNotFound {
                call_id: call_id_str.to_owned(),
            })?;
        session.set_audio_negotiation(audio_state);
        if let Some(sdp) = b_leg_offer {
            session.set_b_leg_offer_sdp(sdp);
        }
        if let Some(answer) = a_leg_answer {
            session.set_b_leg_answer_sdp(answer);
        }
    }

    Ok(())
}

/// 将 Offer/Answer 的结论转化为 `CallSession` 可持久化的状态。
fn build_audio_state(answer: Option<AudioAnswer<'_>>) -> AudioNegotiationState {
    match answer {
        None => AudioNegotiationState::NoAudio,
        Some(AudioAnswer::Rejected) => AudioNegotiationState::Rejected,
        Some(AudioAnswer::Accepted(plan)) => {
            AudioNegotiationState::Accepted(build_audio_profile(plan))
        }
    }
}

fn build_audio_profile(plan: AudioAcceptPlan<'_>) -> AudioNegotiationProfile {
    let telephone_event = plan.telephone_event.map(build_dtmf_profile);

    AudioNegotiationProfile {
        payload_type: plan.payload_type,
        encoding_name: plan.rtpmap.encoding.to_owned(),
        clock_rate: plan.rtpmap.clock_rate,
        encoding_params: plan.rtpmap.encoding_params.map(ToOwned::to_owned),
        telephone_event,
    }
}

fn build_dtmf_profile(event: TelephoneEvent<'_>) -> DtmfProfile {
    DtmfProfile {
        payload_type: event.payload_type,
        clock_rate: event.clock_rate,
        events: event.events.to_owned(),
    }
}

/// 根据协商结果裁剪发往 B-leg 的 SDP Offer，确保只暴露框架可支持的负载。
///
/// # 教案式注释
/// - **意图 (Why)**：在 A-leg 提供的 Offer 中可能包含框架尚未支持的编解码器或 DTMF 扩展；
///   直接转发会导致 B-leg 选择无法处理的能力。通过裁剪，保证 B-leg 仅看到框架已验证过的组合。
/// - **实现逻辑 (How)**：使用 [`SessionDesc`] 的结构化表示执行过滤，避免手工字符串拼接：
///   - 仅在媒体类型为 `audio` 的块上过滤 `m=` 负载列表；
///   - 依据 `rtpmap`/`fmtp` 属性中的 payload 编号筛选对应属性；
///   - 将过滤后的结构回写为文本时复用 [`format_sdp`]，保持合法行序。
/// - **契约 (What)**：
///   - 输入为解析后的 SDP [`SessionDesc`] 与音频协商状态；
///   - 当 `state` 非 `Accepted` 时返回 `None`，表示无需裁剪；
///   - 成功时返回新的 SDP 文本，由 `format_sdp` 保证行尾 `CRLF`。
/// - **风险 (Trade-offs)**：当前实现仅处理单一音频块，若 Offer 存在多个音频流需在后续扩展。
fn build_b_leg_offer_sdp(offer: &SessionDesc<'_>, state: &AudioNegotiationState) -> Option<String> {
    let selection = payload_selection(state)?;

    let filtered_media = offer
        .media
        .iter()
        .map(|media| filter_media_for_b_leg(media, &selection))
        .collect();

    let filtered = SessionDesc {
        version: offer.version,
        origin: offer.origin.clone(),
        session_name: offer.session_name,
        connection: offer.connection.clone(),
        timing: offer.timing.clone(),
        attributes: offer.attributes.clone(),
        media: filtered_media,
    };

    Some(format_sdp(&filtered))
}

/// 为回送给 A-leg 的 200 OK 构造 SDP Answer。
///
/// # 教案式注释
/// - **意图 (Why)**：在 B-leg 尚未接入前，Proxy 至少要依据本地能力给出一致的 Answer，
///   让主叫侧获得稳定的媒体指示；
/// - **实现逻辑 (How)**：
///   1. 复用 Offer 中的版本、会话、连接等会话级字段，保证 Answer 结构合法；
///   2. 将 `m=audio` 重写为只包含已接受的负载，并补充 `a=rtpmap`/`a=fmtp`；
/// - **契约 (What)**：当 `state` 为 `Accepted` 且 Offer 含音频块时返回 `Some(String)`，否则 `None`；
/// - **风险 (Trade-offs)**：当前直接复用 Offer 的 `o=`/`c=`，真实 B2BUA 应根据自身地址生成独立值，
///   后续引入媒体中继后需同步更新该函数。
fn build_a_leg_answer_sdp(
    offer: &SessionDesc<'_>,
    state: &AudioNegotiationState,
) -> Option<String> {
    let AudioNegotiationState::Accepted(profile) = state else {
        return None;
    };

    let audio_media = offer
        .media
        .iter()
        .find(|media| media.media.eq_ignore_ascii_case("audio"));
    let port = audio_media.map(|media| media.port).unwrap_or("0");
    let proto = audio_media.map(|media| media.proto).unwrap_or("RTP/AVP");
    let connection = audio_media
        .and_then(|media| media.connection.as_ref())
        .or_else(|| offer.connection.as_ref());

    let mut lines = Vec::new();
    lines.push(format!("v={}", offer.version));
    lines.push(format!(
        "o={} {} {} {} {} {}",
        offer.origin.username,
        offer.origin.session_id,
        offer.origin.session_version,
        offer.origin.net_type,
        offer.origin.addr_type,
        offer.origin.address
    ));
    lines.push(format!("s={}", offer.session_name));
    if let Some(connection) = connection {
        lines.push(format!(
            "c={} {} {}",
            connection.net_type, connection.addr_type, connection.address
        ));
    }
    lines.push(format!("t={} {}", offer.timing.start, offer.timing.stop));

    let mut m_line = format!("m=audio {port} {proto} {}", profile.payload_type);
    if let Some(dtmf) = profile.telephone_event.as_ref() {
        m_line.push(' ');
        m_line.push_str(&dtmf.payload_type.to_string());
    }
    lines.push(m_line);

    let mut rtpmap = format!(
        "a=rtpmap:{} {}/{}",
        profile.payload_type, profile.encoding_name, profile.clock_rate
    );
    if let Some(params) = &profile.encoding_params {
        rtpmap.push('/');
        rtpmap.push_str(params);
    }
    lines.push(rtpmap);

    if let Some(dtmf) = profile.telephone_event.as_ref() {
        lines.push(format!(
            "a=rtpmap:{} telephone-event/{}",
            dtmf.payload_type, dtmf.clock_rate
        ));
        lines.push(format!("a=fmtp:{} {}", dtmf.payload_type, dtmf.events));
    }

    Some(join_sdp_lines(lines))
}

/// 选择被接受的负载编号，用于 SDP 裁剪/生成。
fn payload_selection(state: &AudioNegotiationState) -> Option<PayloadSelection> {
    let AudioNegotiationState::Accepted(profile) = state else {
        return None;
    };

    Some(PayloadSelection {
        audio: profile.payload_type,
        dtmf: profile
            .telephone_event
            .as_ref()
            .map(|dtmf| dtmf.payload_type),
    })
}

#[derive(Clone, Copy, Debug)]
struct PayloadSelection {
    audio: u8,
    dtmf: Option<u8>,
}

impl PayloadSelection {
    fn allows(&self, payload: u8) -> bool {
        payload == self.audio || self.dtmf == Some(payload)
    }

    fn is_dtmf(&self, payload: u8) -> bool {
        self.dtmf == Some(payload)
    }
}

fn filter_media_for_b_leg<'a>(
    media: &'a MediaDesc<'a>,
    selection: &PayloadSelection,
) -> MediaDesc<'a> {
    if !media.media.eq_ignore_ascii_case("audio") {
        return media.clone();
    }

    MediaDesc {
        media: media.media,
        port: media.port,
        proto: media.proto,
        formats: media
            .formats
            .iter()
            .copied()
            .filter(|payload| {
                parse_payload_number(payload)
                    .map(|value| selection.allows(value))
                    .unwrap_or(true)
            })
            .collect(),
        connection: media.connection.clone(),
        attributes: filter_audio_attributes(&media.attributes, selection),
    }
}

fn filter_audio_attributes<'a>(
    attributes: &'a [Attribute<'a>],
    selection: &PayloadSelection,
) -> Vec<Attribute<'a>> {
    attributes
        .iter()
        .cloned()
        .filter(|attribute| match attribute.key {
            key if key.eq_ignore_ascii_case("rtpmap") => parse_payload_from_attribute(attribute)
                .map(|payload| selection.allows(payload))
                .unwrap_or(true),
            key if key.eq_ignore_ascii_case("fmtp") => {
                match parse_payload_from_attribute(attribute) {
                    Some(payload) if selection.is_dtmf(payload) => selection.dtmf.is_some(),
                    _ => true,
                }
            }
            _ => true,
        })
        .collect()
}

fn parse_payload_number(value: &str) -> Option<u8> {
    let token = value
        .split_once(':')
        .map(|(_, rest)| rest)
        .unwrap_or(value)
        .split_whitespace()
        .next()?;
    token.parse().ok()
}

fn parse_payload_from_attribute(attribute: &Attribute<'_>) -> Option<u8> {
    attribute.value.and_then(parse_payload_number)
}

fn join_sdp_lines(lines: Vec<String>) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut text = lines.join("\r\n");
    if !text.ends_with("\r\n") {
        text.push_str("\r\n");
    }
    text
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::core::session::AudioNegotiationState;
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
        let sdp = sample_offer_sdp();
        let text = format!(
            "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP client.invalid;branch=z9hG4bK776asdhds\r\n\
From: \"Alice\" <sip:alice@example.com>;tag=1928301774\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: a84b4c76e66710\r\n\
CSeq: 314159 INVITE\r\n\
Contact: <sip:alice@client.invalid>\r\n\
Content-Length: {}\r\n\
\r\n{}",
            sdp.len(),
            sdp
        );
        PipelineMessage::from_buffer(Box::new(OwnedReadableBuffer::new(text.as_bytes().to_vec())))
    }

    fn sample_offer_sdp() -> &'static str {
        "v=0\r\n\
o=- 0 0 IN IP4 203.0.113.1\r\n\
s=-\r\n\
c=IN IP4 203.0.113.1\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0 8 101\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fmtp:101 0-15\r\n"
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
            assert_eq!(session.offer_sdp(), Some(sample_offer_sdp()));

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

    #[test]
    fn call_flow_negotiates_audio_and_stores_plan() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let manager = Arc::new(SessionManager::new());
            let mut session = CallSession::new("nego-call", build_a_leg_placeholder_service());
            session.set_offer_sdp(sample_offer_sdp().to_owned());
            manager.create_session(session).expect("register session");

            let call_id = {
                let session = manager.get_session("nego-call").expect("session");
                session.call_id_arc().clone()
            };

            run_call_flow(Arc::clone(&manager), call_id)
                .await
                .expect("call flow success");

            let session = manager.get_session("nego-call").expect("session");
            match session.audio_negotiation() {
                AudioNegotiationState::Accepted(profile) => {
                    assert_eq!(profile.payload_type, 0);
                    assert_eq!(profile.encoding_name, "PCMU");
                    assert_eq!(profile.clock_rate, 8000);
                    assert_eq!(profile.encoding_params, None);
                    let events = profile
                        .telephone_event
                        .as_ref()
                        .map(|dtmf| dtmf.events.as_str());
                    assert_eq!(events, Some("0-15"));
                }
                other => panic!("unexpected negotiation state: {other:?}"),
            }

            let expected = "v=0\r\n\
o=- 0 0 IN IP4 203.0.113.1\r\n\
s=-\r\n\
c=IN IP4 203.0.113.1\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0 101\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fmtp:101 0-15\r\n";
            assert_eq!(session.b_leg_offer_sdp(), Some(expected));
            assert_eq!(session.b_leg_answer_sdp(), Some(expected));
        });
    }
}
