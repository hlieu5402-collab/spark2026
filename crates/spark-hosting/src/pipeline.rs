use core::fmt;

#[cfg(feature = "alloc")]
use alloc::{collections::BTreeMap, string::String, sync::Arc, vec::Vec};

use spark_core::{
    Error as SparkErrorTrait, Result as SparkResult, pipeline::middleware::Middleware,
};

/// 记录中间件注册失败的原因。
#[derive(Debug)]
pub enum MiddlewareRegistrationError {
    /// 名称重复。
    Duplicate { name: String },
}

impl fmt::Display for MiddlewareRegistrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MiddlewareRegistrationError::Duplicate { name } => {
                write!(f, "middleware `{name}` already registered")
            }
        }
    }
}

impl SparkErrorTrait for MiddlewareRegistrationError {
    fn source(&self) -> Option<&(dyn SparkErrorTrait + 'static)> {
        None
    }
}

/// `MiddlewareRegistry` 以名称索引对象层中间件。
///
/// # 教案级注释
/// - **设计动机 (Why)**
///   - 宿主需要在装配阶段确定 Pipeline 所包含的所有中间件，并保证顺序与名称的确定性。
///   - 对标 Envoy FilterChain 与 Tower Layer Stack，提供集中式的注册与遍历能力。
/// - **系统位置 (Where)**
///   - 位于 `spark-hosting` crate 内部，被 [`HostBuilder`](crate::builder::HostBuilder) 在 `configure_pipeline` 阶段调用；
///   - 运行时也可通过 [`Host`](crate::Host) 暴露的接口读取该注册表，执行真正的链路装配。
/// - **实现策略 (How)**
///   - 使用 `BTreeMap<String, Arc<dyn Middleware>>` 保存中间件：
///     - `Arc` 确保对象层实现可在多个控制器间共享；
///     - `BTreeMap` 提供稳定的迭代顺序，便于生成可重现的管线描述。
/// - **契约 (What)**
///   - 名称必须唯一；若重复注册将返回 [`MiddlewareRegistrationError::Duplicate`]；
///   - 中间件应满足对象层契约：`Send + Sync + 'static`。
/// - **风险提示 (Trade-offs)**
///   - 注册表本身不关心执行顺序；若需要更复杂的拓扑（如 DAG），可在未来扩展为存储邻接表或拓扑标签。
#[derive(Default, Clone)]
pub struct MiddlewareRegistry {
    entries: BTreeMap<String, Arc<dyn Middleware>>,
}

impl fmt::Debug for MiddlewareRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.entries.keys().map(|name| name.as_str()).collect();
        f.debug_struct("MiddlewareRegistry")
            .field("names", &names)
            .finish()
    }
}

impl MiddlewareRegistry {
    /// 构造空的注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个中间件实例。
    ///
    /// # 教案级注释
    /// - **输入参数**
    ///   - `name`：宿主侧的唯一标识；
    ///   - `middleware`：满足 [`Middleware`] 契约的对象层实现。
    /// - **前置条件**：调用方需确保不存在同名条目。
    /// - **后置条件**：成功后中间件被 `Arc` 包装，供后续装配阶段共享。
    /// - **返回值**：结构化错误 [`MiddlewareRegistrationError`]，仅包含重名场景。
    pub fn register(
        &mut self,
        name: impl Into<String>,
        middleware: Arc<dyn Middleware>,
    ) -> SparkResult<(), MiddlewareRegistrationError> {
        let name = name.into();
        if self.entries.contains_key(&name) {
            return Err(MiddlewareRegistrationError::Duplicate { name });
        }
        self.entries.insert(name, middleware);
        Ok(())
    }

    /// 查询指定名称的中间件。
    ///
    /// # 教案级注释
    /// - **目的 (Why)**：在装配 Pipeline 时按名称引用具体中间件，保持配置与实际实例一致；
    /// - **契约 (What)**：返回 `Arc` 引用的借用视图，不会转移所有权；
    /// - **风险提示**：若名称未注册返回 `None`，宿主应在编排阶段显式处理缺失情况。
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Middleware>> {
        self.entries.get(name)
    }

    /// 返回注册表的有序视图。
    ///
    /// # 教案级注释
    /// - **用途 (Why)**：生成文档、可视化拓扑或按顺序装配链路时需要完整遍历；
    /// - **输出 (What)**：提供只读迭代器，顺序与 `BTreeMap` 的键排序一致；
    /// - **注意事项**：迭代过程中若需修改注册表，请先收集所需条目再执行更新，避免可变借用冲突。
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Arc<dyn Middleware>)> {
        self.entries.iter()
    }

    /// 消费注册表并返回内部向量。
    ///
    /// # 教案级注释
    /// - **动机 (Why)**：在宿主启动后将中间件列表交由其他系统（如控制器工厂）托管；
    /// - **行为 (How)**：移动 `self`，将键值对转换为 `Vec`，便于后续排序或自定义存储；
    /// - **后置条件 (What)**：原注册表被消耗，不再保留任何所有权。
    pub fn into_entries(self) -> Vec<(String, Arc<dyn Middleware>)> {
        self.entries.into_iter().collect()
    }
}
