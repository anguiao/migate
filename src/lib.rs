pub mod config;
pub mod device;
pub(crate) mod lighting;
pub mod matter;
pub mod runtime;
pub mod storage;
pub mod terminal;
pub mod xiaomi;

pub type RuntimeError = Box<dyn std::error::Error>;
