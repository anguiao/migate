pub mod config;
pub mod device;
pub mod matter;
pub mod storage;
pub mod terminal;
pub mod xiaomi;

pub type RuntimeError = Box<dyn std::error::Error>;
