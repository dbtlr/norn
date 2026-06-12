pub(crate) mod apply;
mod checks;
mod config;
mod defaults;
mod duration;
pub(crate) mod engine;
mod findings;
pub(crate) mod path_match;
pub(crate) mod predicates;
mod repair;
mod substitution;
mod summary;

pub(crate) use config::{
    parse_config, parse_config_compiled, CacheConfig, CompiledConfig, RepairConfig,
    TelemetryConfig, ValidateConfig, ValidateRule, VaultConfig, CURRENT_SCHEMA_VERSION,
    DEFAULT_CACHE_RETENTION, DEFAULT_RETENTION,
};
pub(crate) use duration::parse_duration;
// Test-only re-exports for fixtures inside norn tests.
#[cfg(test)]
pub(crate) use config::{RuleExclude, RuleSelector};
pub(crate) use defaults::{applicable_rules, path_variables, resolve_to_fixpoint};
pub(crate) use engine::validate_with_compiled;
pub(crate) use findings::{Finding, FindingBody};
pub(crate) use repair::link_risk::classify as classify_link_risk;
pub(crate) use repair::warnings::PlanWarning;
pub(crate) use repair::{
    plan_repairs, Confidence, ConfidenceFilter, FootnoteDetails, PlannedChange, RepairPlan,
    RepairPlanFilters, RepairPlanSummary, SkippedSummary, REPAIR_PLAN_SCHEMA_VERSION,
};
pub(crate) use summary::summarize;
