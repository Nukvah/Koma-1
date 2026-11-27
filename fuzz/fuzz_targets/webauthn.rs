//! Passkeys: what a browser sends when adding one (client data, the CBOR
//! attestation object and its COSE key) and when signing in with one (the
//! authenticator data and a DER signature).
#![no_main]

use koma::webauthn;
use libfuzzer_sys::fuzz_target;

const X: [u8; 32] = [
    0x6b, 0x17, 0xd1, 0xf2, 0xe1, 0x2c, 0x42, 0x47, 0xf8, 0xbc, 0xe6, 0xe5, 0x63, 0xa4, 0x40, 0xf2,
    0x77, 0x03, 0x7d, 0x81, 0x2d, 0xeb, 0x33, 0xa0, 0xf4, 0xa1, 0x39, 0x45, 0xd8, 0x98, 0xc2, 0x96,
];
const Y: [u8; 32] = [
    0x4f, 0xe3, 0x42, 0xe2, 0xfe, 0x1a, 0x7f, 0x9b, 0x8e, 0xe7, 0xeb, 0x4a, 0x7c, 0x0f, 0x9e, 0x16,
    0x2b, 0xce, 0x33, 0x57, 0x6b, 0x31, 0x5e, 0xce, 0xcb, 0xb6, 0x40, 0x68, 0x37, 0xbf, 0x51, 0xf5,
];

fuzz_target!(|data: &[u8]| {
    let _ = webauthn::client_data(data);
    let _ = webauthn::cbor::read(data);
    let _ = webauthn::attestation(data, "koma.local");
    let _ = webauthn::der_signature(data);
    // P-256's generator as the key: a valid point, so signatures are checked.
    let key = koma::accounts::Passkey {
        id: vec![1],
        account: 1,
        name: String::new(),
        alg: webauthn::ES256,
        key: [&[4u8][..], &X[..], &Y[..]].concat(),
        counter: 0,
        added: 0,
        used: None,
    };
    let half = data.len() / 2;
    let _ = webauthn::assertion(&data[..half], data, &data[half..], "koma.local", &key);
    let ed = koma::accounts::Passkey {
        alg: webauthn::EDDSA,
        key: vec![0x3d; 32],
        ..key
    };
    let _ = webauthn::assertion(&data[..half], data, &data[half..], "koma.local", &ed);
});
