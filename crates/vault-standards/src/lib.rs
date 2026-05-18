pub mod apply;
mod checks;
mod config;
mod config_schema;
mod engine;
mod findings;
mod predicates;
mod repair;
mod summary;

pub use apply::{
    apply_file_changes, changes_by_path, validate_plan_for_apply, ApplyError,
    RepairApplyPlanContext, RepairApplyReport, RepairApplyVerification,
};
pub use config::{
    FilesConfig, RepairAction, RepairConfig, RepairRule, RepairRuleMatch, RuleExclude,
    RuleSelector, ValidateConfig, ValidateRule, VaultConfig,
};
pub use config_schema::validate_config_yaml;
pub use engine::validate;
pub use findings::{Finding, FindingBody};
pub use repair::{plan_repairs, PlannedChange, RepairPlan, RepairPlanFilters, RepairPlanSummary};
pub use summary::{summarize, Summary};
