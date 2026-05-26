pub mod apply;
mod checks;
mod config;
pub mod defaults;
pub mod engine;
mod findings;
pub mod path_match;
pub mod predicates;
mod repair;
pub mod substitution;
mod summary;

pub use apply::{
    apply_delete, apply_file_changes, apply_link_rewrites, apply_move, changes_by_path,
    validate_plan_for_apply, ApplyError, DeleteResult, LinkRewriteResult, MoveResult,
    RepairApplyPlanContext, RepairApplyReport, RepairApplyVerification, RepairApplyWarning,
};
pub use config::{
    parse_config, parse_config_compiled, CompiledConfig, CompiledRule, ConfigError, FilesConfig,
    RemoveFrontmatterAction, RepairAction, RepairConfig, RepairRule, RepairRuleMatch, RuleExclude,
    RuleSelector, SetFrontmatterAction, TemplatesConfig, ValidateConfig, ValidateRule, VaultConfig,
    CURRENT_SCHEMA_VERSION,
};
pub use defaults::{
    applicable_rules, merge_defaults, path_variables, resolve_to_fixpoint, ResolveError,
};
pub use engine::{
    validate, validate_rule, validate_rule_compiled, validate_with_alias_field,
    validate_with_compiled,
};
pub use findings::{Finding, FindingBody};
pub use repair::link_risk::{classify as classify_link_risk, AffectedLink, LinkRisk};
pub use repair::warnings::{detect_stem_collision, PlanWarning};
pub use repair::{
    plan_repairs, Confidence, ConfidenceFilter, PlanFootnote, PlannedChange, RepairPlan,
    RepairPlanFilters, RepairPlanSummary, SkipReason, SkippedFinding, SkippedSummary,
    REPAIR_PLAN_SCHEMA_VERSION,
};
pub use summary::{summarize, Summary};
