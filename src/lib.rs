pub mod app;
pub mod config;
mod configuration;
mod device;
mod lighting;
pub mod matter;
pub mod storage;
pub mod terminal;
pub mod xiaomi;

pub type RuntimeError = Box<dyn std::error::Error>;
