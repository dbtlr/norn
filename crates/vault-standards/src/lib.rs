pub mod apply;
mod checks;
mod config;
mod engine;
mod findings;
mod predicates;
mod repair;
mod summary;

pub use apply::{
    apply_file_changes, apply_link_rewrites, apply_move, changes_by_path, validate_plan_for_apply,
    ApplyError, LinkRewriteResult, MoveResult, RepairApplyPlanContext, RepairApplyReport,
    RepairApplyVerification, RepairApplyWarning,
};
pub use config::{
    parse_config, ConfigError, FilesConfig, RemoveFrontmatterAction, RepairAction, RepairConfig,
    RepairRule, RepairRuleMatch, RuleExclude, RuleSelector, SetFrontmatterAction, ValidateConfig,
    ValidateRule, VaultConfig, CURRENT_SCHEMA_VERSION,
};
pub use engine::{validate, validate_rule, validate_with_alias_field};
pub use findings::{Finding, FindingBody};
pub use repair::link_risk::{classify as classify_link_risk, AffectedLink, LinkRisk};
pub use repair::warnings::{detect_stem_collision, PlanWarning};
pub use repair::{
    plan_repairs, PlannedChange, RepairPlan, RepairPlanFilters, RepairPlanSummary, SkipReason,
    SkippedFinding, SkippedSummary, REPAIR_PLAN_SCHEMA_VERSION,
};
pub use summary::{summarize, Summary};
