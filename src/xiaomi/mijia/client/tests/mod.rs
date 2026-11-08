use super::*;

mod support;

#[test]
fn signed_form_uses_binary_hmac_key_and_exact_json_path_and_minute_nonce() {
    let (nonce, signature) = signed_fields(
        b"0123456789abcdef",
        [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77],
        28_333_333,
        "/app/miotspec/prop/get",
        "{\"name\":\"A&B + /中文\",\"value\":true}",
    );
    assert_eq!(nonce, "ABEiM0RVZncBsFUV");
    assert_eq!(signature, "2nnuksC5chgtjTkiegPnjtWkm0pcVQgTZKunQIrtM2Y=");
}
