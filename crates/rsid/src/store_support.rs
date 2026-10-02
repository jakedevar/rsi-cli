//! Leaf definitions that sit below `store`: types, limits and pure helpers the
//! store and config need, moved out of higher modules so `store` has no edge
//! into them (issue #1021 S3). Each higher module re-exports what it gave up,
//! so call sites keep their old paths. Nothing here may depend on a module
//! above `store`.

pub(crate) mod app_server_approval;
pub(crate) mod closure_selector;
pub(crate) mod compatible_table;
pub(crate) mod config_types;
pub(crate) mod event_types;
pub(crate) mod issue_tracker;
pub(crate) mod provider_defaults;
pub(crate) mod provider_settings;
pub(crate) mod restart_record;
pub(crate) mod satellite;
pub(crate) mod satellite_registry;
pub(crate) mod spawn_single_flight;
pub(crate) mod topology_usage;
pub(crate) mod wake_target;
