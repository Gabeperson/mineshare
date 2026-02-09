use std::time::Duration;

pub const DEFAULT_URL: &str = "na.mineshare.dev";
pub const CANCEL: &str = " Cancel ";
pub const ADVANCED: &str = " Advanced ";
pub const START: &str = "Start";
pub const TICK_LEN: Duration = Duration::from_micros(33333);
pub const RETRY_START: f32 = 2.0;
pub const RETRY_MULT: f32 = 1.5;
pub const RETRY_MAX: f32 = 60.0;
