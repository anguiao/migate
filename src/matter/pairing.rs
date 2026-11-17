use rs_matter::{
    dm::{clusters::basic_info::BasicInfoConfig, devices::test::TEST_DEV_COMM},
    error::Error,
    pairing::{
        DiscoveryCapabilities,
        qr::{CommFlowType, QrPayload, no_optional_data},
    },
};

pub enum PairingEvent {
    Opened {
        window_seconds: u16,
        manual_code: String,
        qr_payload: String,
    },
    Expired,
}

pub(super) fn opened(
    info: &BasicInfoConfig<'_>,
    window_seconds: u16,
) -> Result<PairingEvent, Error> {
    let payload = QrPayload::new_from_basic_info(
        DiscoveryCapabilities::IP,
        CommFlowType::Standard,
        TEST_DEV_COMM,
        info,
        no_optional_data,
    );
    let mut buffer = [0; 1024];
    let (text, _) = payload.as_str(&mut buffer)?;
    let qr_payload = text.to_owned();
    Ok(PairingEvent::Opened {
        window_seconds,
        manual_code: TEST_DEV_COMM.compute_pairing_code().to_string(),
        qr_payload,
    })
}

#[cfg(test)]
mod tests;
