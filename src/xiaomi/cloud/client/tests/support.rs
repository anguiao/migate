use super::super::*;

impl CloudClient {
    pub(crate) fn for_test(base_url: &str, timeout: Duration) -> Result<Self, CloudError> {
        let mut client = Self::build(base_url, timeout)?;
        client.instance_base_url = base_url.trim_end_matches('/').to_owned();
        Ok(client)
    }
}
