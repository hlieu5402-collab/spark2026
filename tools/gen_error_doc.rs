use std::{
    env, fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;

/// 文档生成入口：将 `contracts/error_matrix.toml` 转换为 Markdown 表格。
///
/// # 教案式说明（Why）
/// - 让运维/值班团队依赖的知识库与代码保持完全同步；
/// - 通过结构化描述生成文档，避免手工表格遗漏或排序差异。
///
/// # 契约定义（What）
/// - 输入：与构建脚本共享的合约文件；
/// - 前置条件：需启用 `std` + `error_contract_doc` feature，以确保 `toml` 依赖与文件系统可用；
/// - 后置条件：覆盖写入 `docs/error-category-matrix.md`，生成带提示的 Markdown。
///
/// # 运行流程（How）
/// 1. 定位仓库根目录，读取合约；
/// 2. 使用 [`render_markdown`] 生成文档字符串；
/// 3. 写入目标文件，供 CI 与评审校验。
fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let contract_path = manifest_dir.join("../../contracts/error_matrix.toml");
    let contract = read_contract(&contract_path);
    let markdown = render_markdown(&contract);
    let doc_path = manifest_dir.join("../../docs/error-category-matrix.md");
    fs::write(&doc_path, markdown).expect("写入 docs/error-category-matrix.md");
}

/// 与构建脚本一致的顶层结构定义。
#[derive(Debug, Deserialize)]
struct ErrorMatrixContract {
    rows: Vec<ErrorMatrixRow>,
}

/// 单行条目，支持多个错误码共享模板。
#[derive(Debug, Deserialize)]
struct ErrorMatrixRow {
    codes: Vec<String>,
    category: CategoryTemplateSpec,
    doc: DocSpec,
}

/// 文档列信息。
#[derive(Debug, Deserialize)]
struct DocSpec {
    rationale: String,
    tuning: String,
}

/// 分类模板规格。
#[derive(Debug, Deserialize)]
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
        #[serde(rename = "close_message")]
        _close_message: String,
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

/// Busy 语义。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BusyDispositionSpec {
    Upstream,
    Downstream,
}

/// 预算类型。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BudgetDispositionSpec {
    Decode,
    Flow,
}

/// 安全分类。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SecurityClassSpec {
    Authentication,
    Authorization,
    Confidentiality,
    Integrity,
    Audit,
    Unknown,
}

/// 读取并解析合约文件。
fn read_contract(path: &Path) -> ErrorMatrixContract {
    let raw = fs::read_to_string(path).unwrap_or_else(|err| {
        panic!("读取 {path:?} 失败: {err}");
    });
    toml::from_str(&raw).unwrap_or_else(|err| {
        panic!("解析 {path:?} 失败: {err}");
    })
}

/// 生成完整的 Markdown 内容。
fn render_markdown(contract: &ErrorMatrixContract) -> String {
    let mut buf = String::new();
    buf.push_str("<!-- @generated 自动生成，请勿手工编辑 -->\n");
    buf.push_str("# 错误分类矩阵（ErrorCategory Matrix）\n\n");
    buf.push_str("> 目标：错误契约机器可读，默认异常处理器可据此自动触发退避、背压或关闭。\n>");
    buf.push_str(
        "\n> 适用范围：`spark-core` 模块的稳定错误码（参见 `spark_core::error::codes`）。\n\n",
    );
    buf.push_str("## 阅读指引\n\n");
    buf.push_str("- **来源**：稳定错误码，统一使用 `<域>.<语义>` 格式。\n");
    buf.push_str(
        "- **分类**：[`ErrorCategory`](../crates/spark-core/src/error.rs) 枚举分支，用于驱动默认策略。\n",
    );
    buf.push_str("- **单一事实来源**：分类矩阵由 `contracts/error_matrix.toml` 声明，经构建脚本与本工具生成代码与文档。\n");
    buf.push_str("- **默认动作**：`ExceptionAutoResponder::on_exception_caught` 在无显式覆盖时执行的行为：\n");
    buf.push_str("  - `Busy`：向上游广播 `ReadyState::Busy`（表示暂时不可用）；\n");
    buf.push_str("  - `BudgetExhausted`：广播 `ReadyState::BudgetExhausted`，携带预算快照；\n");
    buf.push_str("  - `RetryAfter`：广播 `ReadyState::RetryAfter`，附带退避建议；\n");
    buf.push_str("  - `Close`：调用 `Context::close_graceful` 优雅关闭通道；\n");
    buf.push_str("  - `Cancel`：标记 `CallContext::cancellation()`，终止剩余逻辑；\n");
    buf.push_str("  - `None`：默认不产生额外动作，由业务自行处理。\n");
    buf.push_str("- **可配置策略**：通过 `CoreError::with_category` / `set_category` 覆盖分类，或在 pipeline 中注册自定义 Handler。\n\n");
    buf.push_str("## 分类明细\n\n");
    buf.push_str("| 来源（错误码） | 分类 | 默认动作 | 说明 | 可配置策略 |\n");
    buf.push_str("| --- | --- | --- | --- | --- |\n");

    for row in &contract.rows {
        let codes = row
            .codes
            .iter()
            .map(|code| format!("`{code}`"))
            .collect::<Vec<_>>()
            .join(" / ");
        let category = describe_category(&row.category);
        let action = describe_action(&row.category);
        buf.push_str("| ");
        buf.push_str(&codes);
        buf.push_str(" | ");
        buf.push_str(&category);
        buf.push_str(" | ");
        buf.push_str(&action);
        buf.push_str(" | ");
        buf.push_str(&row.doc.rationale);
        buf.push_str(" | ");
        buf.push_str(&row.doc.tuning);
        buf.push_str(" |\n");
    }

    buf.push_str("\n> **提示**：当新增错误码时，请执行：\n");
    buf.push_str("> 1. 更新 `contracts/error_matrix.toml`；\n");
    buf.push_str("> 2. 运行 `cargo run -p spark-core --bin gen_error_doc --features std,error_contract_doc` 生成文档；\n");
    buf.push_str("> 3. 触发构建脚本（`cargo build -p spark-core`）更新 `src/error/category_matrix.rs` 并补充契约测试。\n");

    buf
}

/// 生成分类列的描述文字。
fn describe_category(category: &CategoryTemplateSpec) -> String {
    match category {
        CategoryTemplateSpec::Retryable {
            wait_ms, reason, ..
        } => {
            format!("Retryable（{}ms，{}）", wait_ms, reason)
        }
        CategoryTemplateSpec::Timeout => "Timeout".to_string(),
        CategoryTemplateSpec::ProtocolViolation { .. } => "ProtocolViolation".to_string(),
        CategoryTemplateSpec::ResourceExhausted { budget } => match budget {
            BudgetDispositionSpec::Decode => "ResourceExhausted(BudgetKind::Decode)".to_string(),
            BudgetDispositionSpec::Flow => "ResourceExhausted(BudgetKind::Flow)".to_string(),
        },
        CategoryTemplateSpec::Cancelled => "Cancelled".to_string(),
        CategoryTemplateSpec::NonRetryable => "NonRetryable".to_string(),
        CategoryTemplateSpec::Security { class } => match class {
            SecurityClassSpec::Authentication => {
                "Security(SecurityClass::Authentication)".to_string()
            }
            SecurityClassSpec::Authorization => {
                "Security(SecurityClass::Authorization)".to_string()
            }
            SecurityClassSpec::Confidentiality => {
                "Security(SecurityClass::Confidentiality)".to_string()
            }
            SecurityClassSpec::Integrity => "Security(SecurityClass::Integrity)".to_string(),
            SecurityClassSpec::Audit => "Security(SecurityClass::Audit)".to_string(),
            SecurityClassSpec::Unknown => "Security(SecurityClass::Unknown)".to_string(),
        },
    }
}

/// 生成默认动作列文本。
fn describe_action(category: &CategoryTemplateSpec) -> String {
    match category {
        CategoryTemplateSpec::Retryable { busy, .. } => match busy {
            Some(BusyDispositionSpec::Upstream) => "RetryAfter + Busy(Upstream)".to_string(),
            Some(BusyDispositionSpec::Downstream) => "RetryAfter + Busy(Downstream)".to_string(),
            None => "RetryAfter".to_string(),
        },
        CategoryTemplateSpec::Timeout => "Cancel".to_string(),
        CategoryTemplateSpec::ProtocolViolation { .. } => "Close".to_string(),
        CategoryTemplateSpec::ResourceExhausted { .. } => "BudgetExhausted".to_string(),
        CategoryTemplateSpec::Cancelled => "Cancel".to_string(),
        CategoryTemplateSpec::NonRetryable => "None".to_string(),
        CategoryTemplateSpec::Security { .. } => "Close".to_string(),
    }
}
