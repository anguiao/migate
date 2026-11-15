use super::super::*;

impl MijiaClient {
    pub(in crate::xiaomi) fn for_test(base: &str, timeout: Duration) -> Self {
        assert_eq!(Url::parse(base).unwrap().host_str(), Some("127.0.0.1"));
        let mut client = Self::build(base, timeout).unwrap();
        client.client = HttpClient::builder()
            .timeout(timeout)
            .redirect_policy(RedirectPolicy::None)
            .proxy(None)
            .build()
            .unwrap();
        client
    }
}
