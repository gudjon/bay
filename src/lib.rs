//! bay: land, verify and recover changes from many coding agents on one Git repository.
//!
//! The design is `DESIGN`; the contracts are `INTERFACES`.

pub mod agent_settings;
pub mod canonical;
pub mod change;
pub mod check;
pub mod clock;
pub mod config;
pub mod deny;
pub mod doctor;
pub mod evidence;
pub mod executor;
pub mod git;
pub mod hook;
pub mod host;
pub mod ids;
pub mod install;
pub mod log;
pub mod merge;
pub mod names;
pub mod policy;
pub mod publish;
pub mod queue;
pub mod receipts;
pub mod registry;
pub mod repo;
pub mod result;
pub mod session;
pub mod share;
pub mod signing;
pub mod skill;
pub mod start;
pub mod status;
pub mod undo;
pub mod update;
pub mod version;
pub mod view;
