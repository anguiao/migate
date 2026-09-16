use base64::{Engine as _, engine::general_purpose::STANDARD};

use super::DiscoveryError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayProfile {
    pub gateway_did: u64,
    pub home_group: String,
    pub master: bool,
    pub mqtt: bool,
}

impl GatewayProfile {
    pub fn parse_base64(encoded: &str) -> Result<Self, DiscoveryError> {
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| DiscoveryError::new("invalid gateway profile encoding"))?;
        if bytes.len() < 23 {
            return Err(DiscoveryError::new("gateway profile is too short"));
        }

        let gateway_did = u64::from_be_bytes(
            bytes[1..9]
                .try_into()
                .map_err(|_| DiscoveryError::new("invalid gateway identifier"))?,
        );
        if gateway_did == 0 {
            return Err(DiscoveryError::new("gateway identifier is zero"));
        }
        let home_group = bytes[9..17]
            .iter()
            .rev()
            .map(|byte| format!("{byte:02x}"))
            .collect();

        Ok(Self {
            gateway_did,
            home_group,
            master: bytes[20] >> 4 == 1,
            mqtt: bytes[22] & 0x02 != 0,
        })
    }
}
