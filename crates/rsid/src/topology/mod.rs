#![allow(clippy::redundant_pub_crate)]

pub(crate) mod agent;
pub(crate) mod auto_dispatch;
pub(crate) mod baton;
pub(crate) mod catalog;
pub(crate) mod command;
pub(crate) mod custody;
pub(crate) mod decision;
pub(crate) mod executor;
pub(crate) mod gate;
pub(crate) mod graph;
pub(crate) mod issue_close;
pub(crate) mod issue_hold;
pub(crate) mod land;
pub(crate) mod launch;
pub(crate) mod oncall;
pub(crate) mod outcome;
pub(crate) mod recovery;
pub(crate) mod repair_review;
pub(crate) mod resolve;
pub(crate) mod review;
pub(crate) mod review_policy;
pub(crate) mod review_receipts;
pub(crate) mod review_work;
pub(crate) mod starters;
pub(crate) mod steps;
pub(crate) mod store;
pub(crate) mod verify;

#[cfg(test)]
mod agent_tests;
#[cfg(test)]
mod tests;
