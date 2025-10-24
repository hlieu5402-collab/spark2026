use std::{
    env,
    fmt::Write,
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;

/// 构建脚本入口：读取 TOML 契约，生成 `spark-core/src/error/category_matrix.rs`。
///
/// # 教案式说明（Why）
/// - 避免手写静态矩阵导致的代码/文档漂移，将错误分类声明集中在 `contracts/error_matrix.toml`；
/// - 在编译阶段自动生成矩阵，实现“契约即代码”。
///
/// # 契约定义（What）
/// - 输入：仓库根目录下的 `contracts/error_matrix.toml`；
/// - 前置条件：文件内容符合 [`ErrorMatrixContract`] 的结构约束；
/// - 后置条件：在源码目录输出生成文件，供 `spark_core::error::category_matrix` 模块直接编译。
///
/// # 逻辑解析（How）
/// 1. 解析合约文件，展开多错误码行；
/// 2. 按错误码排序，确保 diff 稳定；
/// 3. 渲染 Rust 模块模板并写回磁盘。
fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let contract_path = manifest_dir.join("../contracts/error_matrix.toml");
    let contract = read_contract(&contract_path);
    let entries = expand_entries(&contract);
    let generated = render_category_matrix(&entries);
    let output_path = manifest_dir.join("src/error/category_matrix.rs");
    fs::write(&output_path, generated).expect("写入 category_matrix.rs");
}

/// 合约文件的顶层结构：包含若干行数据。
#[derive(Debug, Deserialize)]
struct ErrorMatrixContract {
    rows: Vec<ErrorMatrixRow>,
}

/// 每行描述一组共享分类模板的错误码。
#[derive(Debug, Deserialize)]
struct ErrorMatrixRow {
    codes: Vec<String>,
    category: CategoryTemplateSpec,
    #[allow(dead_code)]
    doc: DocSpec,
}

/// 文档列对应的文本说明，构建脚本虽然不直接使用，但保留以验证完整性。
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct DocSpec {
    rationale: String,
    tuning: String,
}

/// 错误分类模板的声明式表示。
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CategoryTemplateSpec {
    Retryable {
        wait_ms: u64,
        reason: String,
        #[serde(default)]
        busy: Option<BusyDispositionSpec>,
    },
    Timeout,
    ProtocolViolation {
        close_message: String,
    },
    ResourceExhausted {
        budget: BudgetDispositionSpec,
    },
    Cancelled,
    NonRetryable,
    Security {
        class: SecurityClassSpec,
    },
}

/// 可选的 Busy 语义。
#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum BusyDispositionSpec {
    Upstream,
    Downstream,
}

/// 预算分类枚举。
#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum BudgetDispositionSpec {
    Decode,
    Flow,
}

/// 安全事件分类。
#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum SecurityClassSpec {
    Authentication,
    Authorization,
    Confidentiality,
    Integrity,
    Audit,
    Unknown,
}

/// 展开后的矩阵条目，每个错误码对应一条记录。
struct ExpandedEntry {
    code: String,
    template: CategoryTemplateSpec,
}

/// 读取并解析 TOML 合约。
///
/// # Why
/// - 将解析逻辑集中到单处，方便未来扩展字段并统一错误处理。
///
/// # How
/// - 使用 `toml::from_str` 将文本映射到强类型结构，解析失败时提供上下文。
///
/// # What
/// - 输入：合约文件路径；返回：结构化的 [`ErrorMatrixContract`]。
fn read_contract(path: &Path) -> ErrorMatrixContract {
    let raw = fs::read_to_string(path).unwrap_or_else(|err| {
        panic!("读取 {path:?} 失败: {err}");
    });
    toml::from_str(&raw).unwrap_or_else(|err| {
        panic!("解析 {path:?} 失败: {err}");
    })
}

/// 将包含多个错误码的行展开为逐条记录，并按字典序排序。
///
/// # Why
/// - 生成文件需要稳定顺序，避免无意义 diff；
/// - 测试依赖 `entries()` 遍历顺序，与文档一致更易核对。
///
/// # How
/// - 遍历每行，对其中每个 `code` 克隆模板；
/// - 使用 `sort_by` 依据错误码字符串排序。
///
/// # What
/// - 输入：解析后的合约；输出：排序后的 [`ExpandedEntry`] 列表。
fn expand_entries(contract: &ErrorMatrixContract) -> Vec<ExpandedEntry> {
    let mut entries = Vec::new();
    for row in &contract.rows {
        for code in &row.codes {
            entries.push(ExpandedEntry {
                code: code.clone(),
                template: row.category.clone(),
            });
        }
    }
    entries.sort_by(|a, b| a.code.cmp(&b.code));
    entries
}

/// 渲染最终的 Rust 模块文本。
///
/// # Why
/// - 保留原有手写模块的教案级注释与类型定义，降低后续维护成本；
/// - 将矩阵常量的数据段自动拼装，杜绝人工疏漏。
///
/// # How
/// - 使用字符串构建：先写入固定模板，再逐条追加矩阵条目；
/// - 通过 [`render_template`] 将声明式模板转换为具体的 Rust 表达式。
///
/// # What
/// - 输入：展开后的条目；输出：完整的模块源码字符串。
fn render_category_matrix(entries: &[ExpandedEntry]) -> String {
    let mut buffer = String::new();
    buffer.push_str("// @generated 自动生成文件，请勿手工修改。\n");
    buffer.push_str("// 由 spark-core/build.rs 根据 contracts/error_matrix.toml 生成。\n\n");
    buffer.push_str("use crate::{\n");
    buffer.push_str("    contract::BudgetKind,\n");
    buffer.push_str("    error::ErrorCategory,\n");
    buffer.push_str("    security::SecurityClass,\n");
    buffer.push_str("    status::{BusyReason, RetryAdvice},\n");
    buffer.push_str("};\n");
    buffer.push_str("use core::time::Duration;\n\n");
    buffer.push_str(r#"/// 默认错误分类矩阵的只读数据源，集中声明“错误码 → ErrorCategory → 自动响应动作”的三段映射。
///
/// # 教案式背景说明（Why）
/// - 构建脚本从 `contracts/error_matrix.toml` 读取声明式数据，统一驱动代码、文档与测试；
/// - 该模块作为框架内的单一事实来源（Single Source of Truth），确保新增错误码时无需手动同步多处文件。
///
/// # 契约定义（What）
/// - 对外暴露 [`entries`]、[`entry_for_code`]、[`default_autoresponse`] 三个只读查询接口；
/// - **输入约束**：调用方必须传入在文档中登记的稳定错误码；
/// - **返回承诺**：表中提供对应的 [`ErrorCategory`] 与默认动作（重试/背压/关闭/取消/无动作）。
///
/// # 实现思路（How）
/// - 使用 [`CategoryMatrixEntry`] 承载错误码与 [`CategoryTemplate`]；
/// - `CategoryTemplate` 负责按需构造 [`ErrorCategory`] 与 [`DefaultAutoResponse`]，避免在静态表中直接存放运行期对象；
/// - 静态常量 `MATRIX` 由构建脚本生成，确保条目顺序稳定；
/// - 通过辅助枚举 [`BusyDisposition`] 与 [`BudgetDisposition`] 描述“繁忙主语义”与“预算类型”，避免跨模块直接依赖内部细节。
///
/// # 风险与权衡（Trade-offs & Gotchas）
/// - 若未来扩展新的自动响应动作，需同步扩展 [`DefaultAutoResponse`] 及默认处理器；
/// - 表项按字母顺序维护，便于审查；测试会确保文档同步，避免遗漏更新。
"#);
    buffer.push_str("pub mod matrix {\n");
    buffer.push_str("    use super::{CategoryMatrixEntry, DefaultAutoResponse};\n\n");
    buffer.push_str(
        r#"    /// 暴露内部静态矩阵，供调用方遍历但禁止修改。
    pub const fn entries() -> &'static [CategoryMatrixEntry] {
        super::MATRIX
    }

    /// 按错误码查找矩阵条目。
    ///
    /// # 契约说明
    /// - **输入**：遵循 `<域>.<语义>` 规则的稳定错误码；
    /// - **前置条件**：错误码必须事先收录于矩阵中；
    /// - **返回值**：若存在条目返回引用，否则 `None`（表示需显式指定分类）。
    pub fn entry_for_code(code: &str) -> Option<&'static CategoryMatrixEntry> {
        entries().iter().find(|entry| entry.code == code)
    }

    /// 提供默认的自动响应动作，供 Pipeline 或测试驱动行为一致性。
    pub fn default_autoresponse(code: &str) -> Option<DefaultAutoResponse> {
        entry_for_code(code).map(CategoryMatrixEntry::default_response)
    }

    /// 内部使用：根据错误码返回默认 [`ErrorCategory`]。
    pub(crate) fn lookup_default_category(code: &str) -> Option<crate::error::ErrorCategory> {
        entry_for_code(code).map(CategoryMatrixEntry::category)
    }
"#,
    );
    buffer.push_str("}\n\n");
    buffer.push_str("pub use matrix::{default_autoresponse, entries, entry_for_code};\n\n");
    buffer.push_str(
        r#"/// 描述单条“错误码 → 分类模板”的映射关系。
///
/// # 字段契约
/// - `code`：稳定错误码，匹配 `docs/error-category-matrix.md` 中的第一列；
/// - `template`：分类模板，封装 `ErrorCategory` 的构造逻辑与默认动作定义。
#[derive(Clone, Copy)]
pub struct CategoryMatrixEntry {
    code: &'static str,
    template: CategoryTemplate,
}

impl CategoryMatrixEntry {
    /// 返回错误码。
    pub const fn code(&self) -> &'static str {
        self.code
    }

    /// 根据模板构造默认分类。
    pub fn category(&self) -> ErrorCategory {
        self.template.instantiate()
    }

    /// 计算默认自动响应动作。
    pub fn default_response(&self) -> DefaultAutoResponse {
        self.template.default_response()
    }
}

/// 表达如何从静态表生成 `ErrorCategory` 及自动响应动作的模板。
#[derive(Clone, Copy)]
pub enum CategoryTemplate {
    /// 对应 `Retryable` 分类，携带等待窗口、原因描述与繁忙主语义。
    Retryable {
        wait_ms: u64,
        reason: &'static str,
        busy: Option<BusyDisposition>,
    },
    /// `Timeout` 分类。
    Timeout,
    /// 协议违规。
    ProtocolViolation { close_message: &'static str },
    /// 预算耗尽（区分预算类型）。
    ResourceExhausted { budget: BudgetDisposition },
    /// 运行时取消。
    Cancelled,
    /// 非重试错误。
    NonRetryable,
    /// 安全事件，需要携带具体安全分类以生成关闭文案。
    Security { class: SecurityClass },
}

impl CategoryTemplate {
    /// 按模板实例化 [`ErrorCategory`]。
    pub fn instantiate(&self) -> ErrorCategory {
        match self {
            CategoryTemplate::Retryable {
                wait_ms, reason, ..
            } => {
                let advice =
                    RetryAdvice::after(Duration::from_millis(*wait_ms)).with_reason(*reason);
                ErrorCategory::Retryable(advice)
            }
            CategoryTemplate::Timeout => ErrorCategory::Timeout,
            CategoryTemplate::ProtocolViolation { .. } => ErrorCategory::ProtocolViolation,
            CategoryTemplate::ResourceExhausted { budget } => {
                ErrorCategory::ResourceExhausted(budget.to_budget_kind())
            }
            CategoryTemplate::Cancelled => ErrorCategory::Cancelled,
            CategoryTemplate::NonRetryable => ErrorCategory::NonRetryable,
            CategoryTemplate::Security { class } => ErrorCategory::Security(*class),
        }
    }

    /// 计算默认自动响应动作。
    pub fn default_response(&self) -> DefaultAutoResponse {
        match self {
            CategoryTemplate::Retryable {
                wait_ms,
                reason,
                busy,
            } => DefaultAutoResponse::RetryAfter {
                wait_ms: *wait_ms,
                reason,
                busy: *busy,
            },
            CategoryTemplate::Timeout | CategoryTemplate::Cancelled => DefaultAutoResponse::Cancel,
            CategoryTemplate::ProtocolViolation { close_message } => DefaultAutoResponse::Close {
                reason_code: "protocol.violation",
                message: close_message,
            },
            CategoryTemplate::ResourceExhausted { budget } => {
                DefaultAutoResponse::BudgetExhausted { budget: *budget }
            }
            CategoryTemplate::NonRetryable => DefaultAutoResponse::None,
            CategoryTemplate::Security { class } => DefaultAutoResponse::Close {
                reason_code: "security.violation",
                message: class.summary(),
            },
        }
    }
}

/// 默认自动响应动作的精简表达，用于测试与 Pipeline 复用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultAutoResponse {
    /// 广播 `Busy`（可选）随后发送 `RetryAfter`。
    RetryAfter {
        wait_ms: u64,
        reason: &'static str,
        busy: Option<BusyDisposition>,
    },
    /// 广播 `BudgetExhausted`。
    BudgetExhausted { budget: BudgetDisposition },
    /// 触发优雅关闭。
    Close {
        reason_code: &'static str,
        message: &'static str,
    },
    /// 标记取消令牌。
    Cancel,
    /// 默认不产生额外动作。
    None,
}

impl DefaultAutoResponse {
    /// 若为背压分支，返回对应的预算类型。
    pub const fn budget(&self) -> Option<BudgetDisposition> {
        match self {
            DefaultAutoResponse::BudgetExhausted { budget } => Some(*budget),
            _ => None,
        }
    }

    /// 若为关闭分支，返回关闭原因元组。
    pub const fn close_reason(&self) -> Option<(&'static str, &'static str)> {
        match self {
            DefaultAutoResponse::Close {
                reason_code,
                message,
            } => Some((*reason_code, *message)),
            _ => None,
        }
    }

    /// 若为重试分支，返回等待窗口与繁忙主语义。
    pub const fn retry(&self) -> Option<(u64, &'static str, Option<BusyDisposition>)> {
        match self {
            DefaultAutoResponse::RetryAfter {
                wait_ms,
                reason,
                busy,
            } => Some((*wait_ms, *reason, *busy)),
            _ => None,
        }
    }
}

/// 描述在退避信号之前广播的繁忙主语义（上游/下游）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusyDisposition {
    Upstream,
    Downstream,
}

impl BusyDisposition {
    /// 转换为 [`BusyReason`]，供 Pipeline 默认处理器复用。
    pub fn to_busy_reason(self) -> BusyReason {
        match self {
            BusyDisposition::Upstream => BusyReason::upstream(),
            BusyDisposition::Downstream => BusyReason::downstream(),
        }
    }
}

/// 描述预算耗尽场景的预算类型，避免在常量表中直接引用 `BudgetKind`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetDisposition {
    Decode,
    Flow,
}

impl BudgetDisposition {
    /// 转换为框架实际使用的 [`BudgetKind`]。
    pub fn to_budget_kind(self) -> BudgetKind {
        match self {
            BudgetDisposition::Decode => BudgetKind::Decode,
            BudgetDisposition::Flow => BudgetKind::Flow,
        }
    }
}
"#,
    );
    buffer.push_str("\n/// 静态矩阵：保持按错误码字典序排列，方便审查与 diff。\nconst MATRIX: &[CategoryMatrixEntry] = &[\n");
    for entry in entries {
        let const_name = code_constant(&entry.code);
        let template_expr = render_template(&entry.template);
        writeln!(
            buffer,
            "    CategoryMatrixEntry {{\n        code: crate::error::codes::{const_name},\n        template: {template_expr},\n    }},"
        )
        .expect("写入矩阵条目");
    }
    buffer.push_str("];\n");
    buffer
}

/// 将错误码转为常量名称，例如 `transport.io` → `TRANSPORT_IO`。
fn code_constant(code: &str) -> String {
    let mut name = String::with_capacity(code.len());
    for ch in code.chars() {
        match ch {
            '.' | '-' => name.push('_'),
            _ => name.push(ch),
        }
    }
    name.to_ascii_uppercase()
}

/// 渲染分类模板的 Rust 表达式。
fn render_template(template: &CategoryTemplateSpec) -> String {
    match template {
        CategoryTemplateSpec::Retryable {
            wait_ms,
            reason,
            busy,
        } => {
            let reason_literal = to_rust_string(reason);
            let busy_expr = match busy {
                Some(BusyDispositionSpec::Upstream) => {
                    "Some(BusyDisposition::Upstream)".to_string()
                }
                Some(BusyDispositionSpec::Downstream) => {
                    "Some(BusyDisposition::Downstream)".to_string()
                }
                None => "None".to_string(),
            };
            format!(
                "CategoryTemplate::Retryable {{\n            wait_ms: {wait_ms},\n            reason: {reason_literal},\n            busy: {busy_expr},\n        }}"
            )
        }
        CategoryTemplateSpec::Timeout => "CategoryTemplate::Timeout".to_string(),
        CategoryTemplateSpec::ProtocolViolation { close_message } => {
            let literal = to_rust_string(close_message);
            format!(
                "CategoryTemplate::ProtocolViolation {{\n            close_message: {literal},\n        }}"
            )
        }
        CategoryTemplateSpec::ResourceExhausted { budget } => {
            let budget_expr = match budget {
                BudgetDispositionSpec::Decode => "BudgetDisposition::Decode",
                BudgetDispositionSpec::Flow => "BudgetDisposition::Flow",
            };
            format!(
                "CategoryTemplate::ResourceExhausted {{\n            budget: {budget_expr},\n        }}"
            )
        }
        CategoryTemplateSpec::Cancelled => "CategoryTemplate::Cancelled".to_string(),
        CategoryTemplateSpec::NonRetryable => "CategoryTemplate::NonRetryable".to_string(),
        CategoryTemplateSpec::Security { class } => {
            let class_expr = match class {
                SecurityClassSpec::Authentication => "SecurityClass::Authentication",
                SecurityClassSpec::Authorization => "SecurityClass::Authorization",
                SecurityClassSpec::Confidentiality => "SecurityClass::Confidentiality",
                SecurityClassSpec::Integrity => "SecurityClass::Integrity",
                SecurityClassSpec::Audit => "SecurityClass::Audit",
                SecurityClassSpec::Unknown => "SecurityClass::Unknown",
            };
            format!("CategoryTemplate::Security {{\n            class: {class_expr},\n        }}")
        }
    }
}

/// 将任意字符串转换为合法的 Rust 字面量，处理转义字符。
fn to_rust_string(value: &str) -> String {
    let mut literal = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => literal.push_str("\\\\"),
            '"' => literal.push_str("\\\""),
            '\n' => literal.push_str("\\n"),
            '\r' => literal.push_str("\\r"),
            '\t' => literal.push_str("\\t"),
            c if c.is_control() => literal.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => literal.push(c),
        }
    }
    literal.push('"');
    literal
}
