use crate::device::LightMode;

/// Matter cluster settings and projected color values. Trajectory state lives
/// in the shared scheduler.
#[derive(Default)]
pub(in crate::matter::lighting) struct ColorControlState {
    pub xy: Option<(u16, u16)>,
    pub color_temperature_mireds: Option<u16>,
    pub mode: Option<LightMode>,
    pub options: u8,
}
