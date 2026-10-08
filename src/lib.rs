pub mod cli;
pub mod config;
pub mod controller;
pub mod dashboard;
pub mod error;
pub mod integration;
pub mod keymap;
pub(crate) mod keys;
pub mod model;
pub mod pane_layout;
pub mod platform;
pub mod prefix;
pub mod registry;
pub mod ui;

pub use config::{ControllerConfig, MouseResizeConfig};
pub use controller::{ControllerOptions, ControllerRunReport, bridge_is_ready, run_controller};
pub use error::{AppError, AppResult};
pub use model::{Direction, TerminalAction, TerminalChannel, WindowIdentity};
