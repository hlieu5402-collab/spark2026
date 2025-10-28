#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::borrow::Cow;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;
use spark_core::router::binding::RouteValidation;
use spark_core::router::catalog::{RouteCatalog, RouteDescriptor};
use spark_core::router::context::{RoutingContext, RoutingSnapshot};
use spark_core::router::metadata::RouteMetadata;
use spark_core::router::route::{RouteId, RoutePattern, RouteSegment};
use spark_core::router::traits::generic::RouteError;
use spark_core::router::traits::object::{DynRouter, RouteBindingObject, RouteDecisionObject};
use spark_core::service::BoxService;
use spark_core::{SparkError, buffer::PipelineMessage};

/// `ServiceFactory` 定义路由表中“如何按需生成对象层 Service” 的抽象。
///
/// # 设计初衷（Why）
/// - 避免在路由表中直接缓存 `Service` 实例，降低生命周期管理难度；
/// - 支持在每次命中后重新构造 `BoxService`，满足无状态或短生命周期 Handler 的需求；
/// - 配合 [`DefaultRouter`] 的 `ArcSwap` 路由表，实现“读路径零锁、写路径整表替换”的热更新模式。
///
/// # 契约说明（What）
/// - **输入/输出**：`create` 不接受参数，返回新的 [`BoxService`]；
/// - **错误语义**：若构造失败，需返回 [`SparkError`] 以便路由器统一包装为 [`RouteError::Internal`]；
/// - **线程安全**：实现必须满足 `Send + Sync + 'static`，以保证在 `Arc` 中跨线程复用。
///
/// # 风险提示（Trade-offs）
/// - 若工厂内部复用连接池或缓存，请自行处理并发；`DefaultRouter` 不会加锁；
/// - 频繁分配新服务可能带来开销，可通过在工厂内部维护 `Arc<BoxService>` 并克隆方式折中。
pub trait ServiceFactory: Send + Sync + 'static {
    /// 构造新的对象层服务实例。
    #[allow(clippy::result_large_err)]
    fn create(&self) -> spark_core::Result<BoxService, SparkError>;
}

/// `RouteRegistration` 描述向 [`DefaultRouter`] 注册的单条路由记录。
///
/// # 设计意图（Why）
/// - 将 `RoutePattern`、`RouteId`、静态元数据与 `ServiceFactory` 聚合在一个结构中，
///   方便控制面在热更新时一次性交付完整配置；
/// - 为 `ArcSwap` 整表替换提供拷贝友好的数据承载体。
///
/// # 字段语义（What）
/// - `id`：命中的稳定路由标识；
/// - `pattern`：匹配规则，支持字面量、参数、通配符；
/// - `metadata`：静态属性，将与意图/动态属性合并；
/// - `factory`：按需生成对象层服务的工厂。
#[derive(Clone)]
pub struct RouteRegistration {
    /// 稳定的路由 ID。
    pub id: RouteId,
    /// 匹配模式。
    pub pattern: RoutePattern,
    /// 静态元数据。
    pub metadata: RouteMetadata,
    /// Service 工厂。
    pub factory: Arc<dyn ServiceFactory>,
}

impl fmt::Debug for RouteRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RouteRegistration")
            .field("id", &self.id)
            .field("pattern", &self.pattern)
            .field("metadata", &self.metadata)
            .finish()
    }
}

/// `DefaultRouter` 提供基于 `ArcSwap` 的对象层路由实现，满足无锁读取与热更新需求。
///
/// # 设计动机（Why）
/// - **热更新友好**：控制面替换整张路由表时仅需一次 `store`，现有读者持有的旧 `Arc` 可自然过渡；
/// - **读路径零锁**：查询时只需 `load` + 顺序遍历，避免互斥锁；
/// - **契约贴合**：直接实现 [`DynRouter`]，供动态插件或脚本环境复用。
///
/// # 行为概览（How）
/// 1. 路由表使用 [`ArcSwap`] 持有内部的 `RouteTable`；
/// 2. `route_dyn` 会加载快照，匹配意图目标，命中后通过工厂构造 `BoxService`；
/// 3. 匹配失败时返回 [`RouteError::NotFound`]，附带原始意图信息；
/// 4. `snapshot`/`validate` 分别返回快照视图与简单预检结果。
///
/// # 使用契约（What）
/// - **前置条件**：调用者需通过 [`Self::update`] 提前装载路由表；
/// - **后置条件**：命中后返回的 `RouteDecisionObject` 包含新的 `BoxService` 与合并后的元数据；
/// - **线程安全**：结构体本身 `Send + Sync`，可在多线程运行时共享。
pub struct DefaultRouter {
    table: ArcSwap<RouteTable>,
    catalog: ArcSwap<RouteCatalog>,
    revision: AtomicU64,
}

impl DefaultRouter {
    /// 构建一个空路由器实例。
    ///
    /// # 设计说明（Why）
    /// - 提供最小可用入口，便于在运行时先构造实例，再异步加载配置；
    /// - 初始路由表为空，`route_dyn` 会直接返回 `NotFound`。
    ///
    /// # 契约（What）
    /// - **前置条件**：调用方需在首次路由前调用 [`Self::update`] 装载路由；
    /// - **后置条件**：返回的路由器可安全跨线程共享。
    pub fn new() -> Self {
        Self {
            table: ArcSwap::from_pointee(RouteTable::default()),
            catalog: ArcSwap::from_pointee(RouteCatalog::new()),
            revision: AtomicU64::new(0),
        }
    }

    /// 批量替换路由表，支持热更新。
    ///
    /// # 教案级说明
    /// - **意图 (Why)**：通过 `ArcSwap::store` 一次性替换整张表，避免逐条更新产生竞态；
    /// - **输入 (What)**：`entries` 为新路由集合，`revision` 明确快照世代号；
    /// - **流程 (How)**：
    ///   1. 根据条目构建新的 [`RouteCatalog`] 与内部 `RouteEntry`；
    ///   2. 组装为 `RouteTable` 并交由 `ArcSwap` 接管；
    ///   3. 更新内部修订号，供 `snapshot` 暴露。
    /// - **前置条件 (Contract)**：`entries` 中的 `id`/`pattern` 应保持唯一，避免匹配歧义；
    /// - **后置条件 (Contract)**：新表立即对后续调用可见，旧表在无读者后自动释放。
    pub fn update<I>(&self, revision: u64, entries: I)
    where
        I: IntoIterator<Item = RouteRegistration>,
    {
        let mut catalog = RouteCatalog::new();
        let mut route_entries = Vec::new();
        for RouteRegistration {
            id,
            pattern,
            metadata,
            factory,
        } in entries
        {
            // `RouteDescriptor` 需要拥有模式和元数据的克隆，以向外部暴露快照。
            let descriptor = RouteDescriptor::new(pattern.clone())
                .with_id(id.clone())
                .with_metadata(metadata.clone());
            catalog.push(descriptor);
            route_entries.push(RouteEntry {
                id,
                pattern,
                metadata,
                factory,
            });
        }

        let catalog_arc = Arc::new(catalog);
        let table = RouteTable {
            entries: route_entries,
        };

        self.revision.store(revision, Ordering::Release);
        self.table.store(Arc::new(table));
        self.catalog.store(Arc::clone(&catalog_arc));
    }
}

impl Default for DefaultRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl DynRouter for DefaultRouter {
    fn route_dyn(
        &self,
        context: RoutingContext<'_, PipelineMessage>,
    ) -> spark_core::Result<RouteDecisionObject, RouteError<SparkError>> {
        // 教案式注释：路由流程拆分为“加载快照 → 匹配 → 构建绑定”三步，
        // 其中匹配阶段使用只读快照避免锁竞争，构建绑定阶段负责将静态/动态
        // 元数据、QoS、安全偏好合并，确保对象层能获得完整上下文。
        let table = self.table.load();
        let intent = context.intent();
        let target_pattern = intent.target();

        for entry in &table.entries {
            if let Some(resolved_segments) = match_pattern(&entry.pattern, target_pattern) {
                let service = entry.factory.create().map_err(RouteError::Internal)?;

                let merged_metadata = merge_metadata([
                    &entry.metadata,
                    intent.preferred_metadata(),
                    context.dynamic_metadata(),
                ]);

                let effective_qos = intent.expected_qos();
                let effective_security = intent.security_preference().cloned();

                let route_id = if resolved_segments.is_empty() {
                    entry.id.clone()
                } else {
                    RouteId::new(entry.id.kind().clone(), resolved_segments)
                };

                let binding = RouteBindingObject::new(
                    route_id,
                    service,
                    merged_metadata,
                    effective_qos,
                    effective_security,
                );

                return Ok(RouteDecisionObject::new(binding, Vec::new()));
            }
        }

        let not_found_metadata =
            merge_metadata([intent.preferred_metadata(), context.dynamic_metadata()]);

        Err(RouteError::NotFound {
            pattern: target_pattern.clone(),
            metadata: not_found_metadata,
        })
    }

    fn snapshot(&self) -> RoutingSnapshot<'_> {
        let catalog_arc = self.catalog.load();
        // SAFETY: `self.catalog` 内部保存了当前快照的 `Arc<RouteCatalog>` 强引用。
        // 这里获取的克隆在返回后立刻被丢弃，但 `ArcSwap` 自身仍旧持有强引用，
        // 因此通过裸指针转换成借用引用是安全的。
        let catalog_ref = unsafe { &*Arc::as_ptr(&catalog_arc) };
        RoutingSnapshot::new(catalog_ref, self.revision.load(Ordering::Acquire))
    }

    fn validate(&self, _descriptor: &RouteDescriptor) -> RouteValidation {
        RouteValidation::new()
    }
}

/// `RouteTable` 封装路由器的内部只读视图。
///
/// - **结构角色 (Why)**：作为 `ArcSwap` 的载荷，仅承载匹配所需的最小数据集；
/// - **字段说明 (What)**：`entries` 为按优先级排列的路由记录集合；
/// - **生命周期**：表结构由 `ArcSwap` 管理，热更新时整体替换，避免与快照互相干扰。
#[derive(Default)]
struct RouteTable {
    entries: Vec<RouteEntry>,
}

/// `RouteEntry` 是内部匹配使用的最小单元。
///
/// - **存在意义 (Why)**：在 `RouteRegistration` 基础上移除调试所需的克隆，避免重复分配；
/// - **字段语义 (What)**：携带静态元数据与工厂，供匹配命中后直接构造绑定。
struct RouteEntry {
    id: RouteId,
    pattern: RoutePattern,
    metadata: RouteMetadata,
    factory: Arc<dyn ServiceFactory>,
}

/// 基于匹配结果生成用于 `RouteId` 的字面量段集合。
///
/// # 说明（Why/How）
/// - 逐段比较 `RoutePattern` 与请求意图，支持 `Literal`、`Parameter`、`Wildcard`；
/// - 若存在通配符，则收集剩余段并立即返回；
/// - 若匹配失败返回 `None`。
fn match_pattern(pattern: &RoutePattern, target: &RoutePattern) -> Option<Vec<Cow<'static, str>>> {
    if pattern.kind() != target.kind() {
        return None;
    }

    let mut resolved_segments = Vec::new();
    let mut target_iter = target.segments();

    for segment in pattern.segments() {
        match segment {
            RouteSegment::Literal(expected) => {
                let candidate = target_iter.next()?;
                if let RouteSegment::Literal(actual) = candidate {
                    if actual != expected {
                        return None;
                    }
                    resolved_segments.push(actual.clone());
                } else {
                    return None;
                }
            }
            RouteSegment::Parameter(_) => {
                let candidate = target_iter.next()?;
                if let RouteSegment::Literal(actual) = candidate {
                    resolved_segments.push(actual.clone());
                } else {
                    return None;
                }
            }
            RouteSegment::Wildcard => {
                for remaining in target_iter {
                    if let RouteSegment::Literal(actual) = remaining {
                        resolved_segments.push(actual.clone());
                    } else {
                        return None;
                    }
                }
                return Some(resolved_segments);
            }
            _ => return None,
        }
    }

    if target_iter.next().is_some() {
        return None;
    }

    Some(resolved_segments)
}

/// 合并多份路由元数据，后者覆盖前者。
///
/// # 设计意图（Why）
/// - 控制面静态属性、调用方意图、运行时动态标签需在路由结果中统一体现；
/// - 使用覆盖语义，保证调用方可在意图中覆盖静态默认值。
///
/// # 输入/输出（What）
/// - `sources`：按优先级排序的元数据切片集合；
/// - 返回新的 [`RouteMetadata`]，不会修改输入。
fn merge_metadata<const N: usize>(sources: [&RouteMetadata; N]) -> RouteMetadata {
    let mut merged = RouteMetadata::new();
    for source in sources {
        for (key, value) in source.iter() {
            merged.insert(key.clone(), value.clone());
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use core::future;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use spark_core::contract::CallContext;
    use spark_core::router::metadata::{MetadataKey, MetadataValue};
    use spark_core::router::route::{RouteKind, RouteSegment};
    use spark_core::service::Service;
    use spark_core::service::auto_dyn::bridge_to_box_service;
    use spark_core::status::{PollReady, ReadyCheck, ReadyState};

    struct CountingFactory {
        counter: Arc<AtomicUsize>,
    }

    impl ServiceFactory for CountingFactory {
        fn create(&self) -> spark_core::Result<BoxService, SparkError> {
            self.counter.fetch_add(1, Ordering::SeqCst);
            Ok(bridge_to_box_service(EchoService))
        }
    }

    struct EchoService;

    impl Service<PipelineMessage> for EchoService {
        type Response = PipelineMessage;
        type Error = SparkError;
        type Future = future::Ready<spark_core::Result<Self::Response, Self::Error>>;

        fn poll_ready(
            &mut self,
            _: &spark_core::context::ExecutionContext<'_>,
            _: &mut core::task::Context<'_>,
        ) -> PollReady<Self::Error> {
            core::task::Poll::Ready(ReadyCheck::Ready(ReadyState::Ready))
        }

        fn call(&mut self, _ctx: CallContext, req: PipelineMessage) -> Self::Future {
            future::ready(Ok(req))
        }
    }

    #[test]
    fn route_dyn_returns_new_service() {
        let router = DefaultRouter::new();
        let counter = Arc::new(AtomicUsize::new(0));

        let mut static_metadata = RouteMetadata::new();
        static_metadata.insert(
            MetadataKey::new(Cow::Borrowed("static.tag")),
            MetadataValue::Text(Cow::Borrowed("alpha")),
        );

        let registration = RouteRegistration {
            id: RouteId::new(
                RouteKind::Rpc,
                vec![Cow::Borrowed("orders"), Cow::Borrowed("create")],
            ),
            pattern: RoutePattern::new(
                RouteKind::Rpc,
                vec![
                    RouteSegment::Literal(Cow::Borrowed("orders")),
                    RouteSegment::Literal(Cow::Borrowed("create")),
                ],
            ),
            metadata: static_metadata,
            factory: Arc::new(CountingFactory {
                counter: Arc::clone(&counter),
            }),
        };

        router.update(7, [registration]);

        let snapshot = router.snapshot();

        let mut intent_metadata = RouteMetadata::new();
        intent_metadata.insert(
            MetadataKey::new(Cow::Borrowed("intent.tenant")),
            MetadataValue::Text(Cow::Borrowed("tenant-a")),
        );

        let mut dynamic_metadata = RouteMetadata::new();
        dynamic_metadata.insert(
            MetadataKey::new(Cow::Borrowed("dynamic.trace")),
            MetadataValue::Text(Cow::Borrowed("trace-1")),
        );

        let intent = spark_core::router::context::RoutingIntent::new(
            snapshot.catalog().iter().next().unwrap().pattern().clone(),
        )
        .with_metadata(intent_metadata);

        let request = PipelineMessage::from_user(String::from("payload"));
        let context =
            RoutingContext::new(&request, &intent, None, None, &dynamic_metadata, snapshot);

        let decision = router.route_dyn(context).expect("应命中默认路由");
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        let binding = decision.binding();
        assert_eq!(
            binding.id(),
            &RouteId::new(
                RouteKind::Rpc,
                vec![Cow::Borrowed("orders"), Cow::Borrowed("create")],
            )
        );
        assert!(
            binding
                .metadata()
                .iter()
                .any(|(key, _)| key.as_str() == "static.tag")
        );
        assert!(
            binding
                .metadata()
                .iter()
                .any(|(key, _)| key.as_str() == "intent.tenant")
        );
        assert!(
            binding
                .metadata()
                .iter()
                .any(|(key, _)| key.as_str() == "dynamic.trace")
        );
    }

    #[test]
    fn route_dyn_not_found() {
        let router = DefaultRouter::new();
        let snapshot = router.snapshot();
        let intent = spark_core::router::context::RoutingIntent::new(RoutePattern::new(
            RouteKind::Rpc,
            vec![RouteSegment::Literal(Cow::Borrowed("unknown"))],
        ));
        let request = PipelineMessage::from_user(String::from("payload"));
        let empty_metadata = RouteMetadata::new();
        let context = RoutingContext::new(&request, &intent, None, None, &empty_metadata, snapshot);

        let result = router.route_dyn(context);
        assert!(matches!(result, Err(RouteError::NotFound { .. })));
    }
}
