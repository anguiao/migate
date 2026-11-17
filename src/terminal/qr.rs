use qrcodegen_no_heap::{DataTooLong, QrCode, QrCodeEcc, Version};

pub(super) fn render(payload: &str) -> Result<String, DataTooLong> {
    let mut scratch = [0; 16384];
    let mut buffer = [0; 16384];
    let qr = QrCode::encode_text(
        payload,
        &mut scratch,
        &mut buffer,
        QrCodeEcc::Medium,
        Version::MIN,
        Version::MAX,
        None,
        false,
    )?;
    let mut text = String::from("\x1b[97;40m");
    for y in (-4..qr.size() + 4).step_by(2) {
        for x in -4..qr.size() + 4 {
            let top = !qr.get_module(x, y);
            let bottom = !qr.get_module(x, y + 1);
            text.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        text.push('\n');
    }
    text.push_str("\x1b[0m");
    Ok(text)
}
