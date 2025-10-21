use alloc::sync::Arc;
use arc_swap::ArcSwap;
use core::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use crate::configuration::{ConfigKey, ConfigScope, ConfigValue, ResolvedConfiguration};

/// 描述运行时关键超时阈值。
///
/// ### 设计目的（Why）
/// - 将请求超时与空闲连接超时集中管理，避免各组件散落维护导致语义不一致。
/// - 作为动态配置的解析结果，便于通过 [`TimeoutRuntimeConfig`] 在热更新时原子替换。
///
/// ### 契约说明（What）
/// - `request_timeout`：单次请求允许的最大持续时间，超过后运行时应主动取消；
/// - `idle_timeout`：连接或通道在无活动时允许保持的最长时间，用于清理闲置资源；
/// - 默认值分别为 5 秒与 60 秒，可满足大多数 RPC 场景的安全基准。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeoutSettings {
    request_timeout: Duration,
    idle_timeout: Duration,
}

impl TimeoutSettings {
    /// 构造带自定义阈值的设置。
    pub const fn new(request_timeout: Duration, idle_timeout: Duration) -> Self {
        Self {
            request_timeout,
            idle_timeout,
        }
    }

    /// 请求超时阈值。
    pub const fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    /// 空闲超时阈值。
    pub const fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    /// 从合并后的配置解析超时设置。
    pub fn from_configuration(config: &ResolvedConfiguration) -> Result<Self, TimeoutConfigError> {
        let mut settings = Self::default();

        if let Some(value) = config.values.get(&request_timeout_key()) {
            settings.request_timeout = parse_duration(value, TimeoutField::Request)?;
        }

        if let Some(value) = config.values.get(&idle_timeout_key()) {
            settings.idle_timeout = parse_duration(value, TimeoutField::Idle)?;
        }

        Ok(settings)
    }
}

impl Default for TimeoutSettings {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(60),
        }
    }
}

/// 热更新友好的超时配置容器。
///
/// ### 设计目的（Why）
/// - 结合 [`ArcSwap`] 与纪元计数，实现“读无锁、写常数时间”的动态超时分发机制。
/// - 通过 `config_epoch()` 提供与观测面对齐的指标，便于确认配置是否生效。
///
/// ### 契约说明（What）
/// - `new`：接受初始设置并将纪元置零；
/// - `snapshot`：返回当前快照的 `Arc`，调用方可持久保存并在并发路径读取；
/// - `replace`：直接替换为给定设置；
/// - `update_from_configuration`：解析 [`ResolvedConfiguration`] 并在成功后递增纪元；
/// - 所有更新均原子生效，读者不需要额外同步原语。
pub struct TimeoutRuntimeConfig {
    epoch: AtomicU64,
    settings: ArcSwap<TimeoutSettings>,
}

impl TimeoutRuntimeConfig {
    /// 创建新的运行时配置容器。
    pub fn new(initial: TimeoutSettings) -> Self {
        Self {
            epoch: AtomicU64::new(0),
            settings: ArcSwap::new(Arc::new(initial)),
        }
    }

    /// 返回当前配置快照。
    pub fn snapshot(&self) -> Arc<TimeoutSettings> {
        self.settings.load_full()
    }

    /// 查询配置纪元（从 0 开始），可用于导出指标或调试日志。
    pub fn config_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// 直接替换为新的设置。
    pub fn replace(&self, settings: TimeoutSettings) {
        self.settings.store(Arc::new(settings));
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// 解析并更新配置。
    pub fn update_from_configuration(
        &self,
        config: &ResolvedConfiguration,
    ) -> Result<(), TimeoutConfigError> {
        let parsed = TimeoutSettings::from_configuration(config)?;
        self.replace(parsed);
        Ok(())
    }
}

impl Default for TimeoutRuntimeConfig {
    fn default() -> Self {
        Self::new(TimeoutSettings::default())
    }
}

/// 超时配置解析过程中可能出现的错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimeoutConfigError {
    InvalidValueType {
        field: TimeoutField,
        expected: &'static str,
    },
    NonPositiveDuration {
        field: TimeoutField,
        provided: i64,
    },
}

impl fmt::Display for TimeoutConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TimeoutConfigError::InvalidValueType { field, expected } => {
                write!(f, "invalid {} value, expected {}", field, expected)
            }
            TimeoutConfigError::NonPositiveDuration { field, provided } => {
                write!(f, "{} duration must be > 0, got {}", field, provided)
            }
        }
    }
}

impl crate::Error for TimeoutConfigError {
    fn source(&self) -> Option<&(dyn crate::Error + 'static)> {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeoutField {
    Request,
    Idle,
}

impl fmt::Display for TimeoutField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TimeoutField::Request => f.write_str("timeouts.request"),
            TimeoutField::Idle => f.write_str("timeouts.idle"),
        }
    }
}

fn parse_duration(
    value: &ConfigValue,
    field: TimeoutField,
) -> Result<Duration, TimeoutConfigError> {
    match value {
        ConfigValue::Integer(v, _) => {
            if *v <= 0 {
                return Err(TimeoutConfigError::NonPositiveDuration {
                    field,
                    provided: *v,
                });
            }
            Ok(Duration::from_millis(*v as u64))
        }
        ConfigValue::Duration(duration, _) => {
            if duration.is_zero() {
                return Err(TimeoutConfigError::NonPositiveDuration { field, provided: 0 });
            }
            Ok(*duration)
        }
        _ => Err(TimeoutConfigError::InvalidValueType {
            field,
            expected: "integer milliseconds or duration",
        }),
    }
}

fn request_timeout_key() -> ConfigKey {
    ConfigKey::new(
        "runtime",
        "timeouts.request_ms",
        ConfigScope::Runtime,
        "maximum duration for a single request in milliseconds",
    )
}

fn idle_timeout_key() -> ConfigKey {
    ConfigKey::new(
        "runtime",
        "timeouts.idle_ms",
        ConfigScope::Runtime,
        "idle connection timeout in milliseconds",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration::{ConfigMetadata, ResolvedConfiguration};
    use alloc::collections::BTreeMap;

    #[test]
    fn parsing_accepts_integer_and_duration_values() {
        let mut values = BTreeMap::new();
        values.insert(
            request_timeout_key(),
            ConfigValue::Integer(1500, ConfigMetadata::default()),
        );
        values.insert(
            idle_timeout_key(),
            ConfigValue::Duration(Duration::from_secs(10), ConfigMetadata::default()),
        );
        let resolved = ResolvedConfiguration { values, version: 1 };
        let parsed = TimeoutSettings::from_configuration(&resolved).expect("parse");
        assert_eq!(parsed.request_timeout(), Duration::from_millis(1500));
        assert_eq!(parsed.idle_timeout(), Duration::from_secs(10));
    }

    #[test]
    fn runtime_config_updates_epoch() {
        let cfg = TimeoutRuntimeConfig::default();
        assert_eq!(cfg.config_epoch(), 0);
        cfg.replace(TimeoutSettings::new(
            Duration::from_secs(2),
            Duration::from_secs(3),
        ));
        assert_eq!(cfg.config_epoch(), 1);
    }
}
