use super::super::*;

impl CloudClient {
    pub(crate) fn for_test(base_url: &str, timeout: Duration) -> Result<Self, CloudError> {
        assert_eq!(Url::parse(base_url).unwrap().host_str(), Some("127.0.0.1"));
        let mut client = Self::build(base_url, timeout)?;
        client.client = HttpClient::builder()
            .timeout(timeout)
            .redirect_policy(RedirectPolicy::None)
            .proxy(None)
            .build()
            .unwrap();
        client.instance_base_url = base_url.trim_end_matches('/').to_owned();
        Ok(client)
    }
}

impl CloudClient {
    pub(in crate::xiaomi) fn test_base_url(&self) -> &str {
        &self.base_url
    }
}
