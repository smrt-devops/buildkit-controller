//! Pool reconciler: the pure planner ([`plan`]), the
//! executor that applies its actions ([`exec`], [`pass`]) and the leader's
//! per-term loop ([`driver`]).

pub mod driver;
pub mod exec;
pub mod metrics;
pub mod ops;
pub mod pass;
pub mod plan;
pub mod pods;
pub mod registry_tls;
pub mod schedule;
pub mod spawn;
pub mod sweep;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_exec;
#[cfg(test)]
mod tests_props;
#[cfg(test)]
mod tests_spawn;

pub use driver::Reconciler;
pub use plan::{
    Action, BackoffSignal, CaEvent, CreateOutcome, CreateResult, Observed, Plan, PlanConfig,
    Requeue, RetireReason, TemplateCheck, ca_warning_at, plan,
};
pub use pods::{Alloc, Phase, WorkerPod, foreign_pod_name};
