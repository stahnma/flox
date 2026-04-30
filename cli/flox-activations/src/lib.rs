pub mod activate_script_builder;
pub mod activation_diff;
pub mod attach;
pub mod cli;
pub mod env_diff;
pub mod gen_rc;
pub mod logger;
pub mod message;
mod process_compose;
mod start;
mod vars_from_env;

pub type Error = anyhow::Error;
