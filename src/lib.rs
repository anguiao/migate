pub mod config;
pub mod device;
mod lighting;
pub mod matter;
mod publication;
pub mod runtime;
pub mod storage;
pub mod terminal;
pub mod xiaomi;

pub type RuntimeError = Box<dyn std::error::Error>;
