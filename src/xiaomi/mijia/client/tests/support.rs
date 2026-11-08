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

    pub(in crate::xiaomi::mijia) fn for_signed_test(base: &str, timeout: Duration) -> Self {
        let mut client = Self::for_test(base, timeout);
        client.fixed_nonce = Some(([0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77], 28_333_333));
        client
    }
}
