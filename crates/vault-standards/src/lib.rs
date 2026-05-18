mod checks;
mod config;
mod engine;
mod findings;
mod predicates;
mod repair;
mod summary;

pub use config::{
    parse_config, ConfigError, FilesConfig, RemoveFrontmatterAction, RepairAction, RepairConfig,
    RepairRule, RepairRuleMatch, RuleExclude, RuleSelector, SetFrontmatterAction, ValidateConfig,
    ValidateRule, VaultConfig,
};
pub use engine::validate;
pub use findings::{Finding, FindingBody};
pub use repair::{plan_repairs, PlannedChange, RepairPlan, RepairPlanFilters, RepairPlanSummary};
pub use summary::{summarize, Summary};
