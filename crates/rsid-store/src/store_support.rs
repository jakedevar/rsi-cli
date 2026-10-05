//! Leaf definitions that sit below `store`: types, limits and pure helpers the
//! store and config need, moved out of higher modules so `store` has no edge
//! into them (issue #1021 S3). Each higher module re-exports what it gave up,
//! so call sites keep their old paths. Nothing here may depend on a module
//! above `store`.

pub mod app_server_approval;
pub mod closure_selector;
pub mod compatible_table;
pub mod config_types;
pub mod event_types;
pub mod issue_tracker;
pub mod manager_gates;
pub mod provider_defaults;
pub mod provider_settings;
pub mod restart_record;
pub mod satellite;
pub mod satellite_registry;
pub mod schedule_wake_job;
pub mod spawn_single_flight;
pub mod topology_usage;
pub mod topology_validation;
pub mod wake_target;
pub mod watch_limits;
