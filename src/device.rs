pub const VIRTUAL_LIGHT_ID: &str = "virtual-light-1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    On,
    Off,
    Toggle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Snapshot {
    pub id: &'static str,
    pub power: bool,
}
