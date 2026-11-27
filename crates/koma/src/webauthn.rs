//! Passkeys (WebAuthn level 2), the little Koma-1 needs: adding one
//! ([`attestation`], "none" attestation: the key is trusted as the signed-in
//! account's browser hands it over) and signing in with one ([`assertion`]),
//! with ES256 (P-256) and EdDSA (Ed25519) keys.
//!
//! What the browser sends is hostile input: the CBOR reader is bounded
//! (depth, sizes, the input's own length) and fuzzed (`fuzz/webauthn`).

use sha2::{Digest, Sha256};

pub const ES256: i64 = -7;
pub const EDDSA: i64 = -8;

/// What `clientDataJSON` says.
#[derive(Debug, PartialEq, Eq)]
pub struct ClientData {
    /// `webauthn.create` or `webauthn.get`.
    pub kind: String,
    pub challenge: Vec<u8>,
    pub origin: String,
}

pub fn client_data(json: &[u8]) -> Result<ClientData, &'static str> {
    if json.len() > 4096 {
        return Err("client data too long");
    }
    let v: serde_json::Value = serde_json::from_slice(json).map_err(|_| "client data not JSON")?;
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .ok_or("client data incomplete")
    };
    Ok(ClientData {
        kind: s("type")?.to_owned(),
        challenge: crate::b64::decode(s("challenge")?).ok_or("bad challenge")?,
        origin: s("origin")?.to_owned(),
    })
}

/// The relying party id for a `Host` header: its name without the port.
pub fn rp_id(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        // [::1]:8080
        return rest.split(']').next().unwrap_or(rest);
    }
    host.rsplit_once(':').map_or(host, |(h, port)| {
        if port.bytes().all(|b| b.is_ascii_digit()) {
            h
        } else {
            host
        }
    })
}

/// Whether a passkey answer's origin is this site, as `Host` names it:
/// HTTPS, or plain HTTP on the machine itself (browsers allow passkeys
/// there only).
pub fn origin_ok(origin: &str, host: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    if origin.strip_prefix("https://") == Some(host) {
        return true;
    }
    origin.strip_prefix("http://") == Some(host)
        && matches!(rp_id(host), "localhost" | "127.0.0.1" | "::1")
}

/// A passkey being added.
#[derive(Debug, PartialEq, Eq)]
pub struct Credential {
    pub id: Vec<u8>,
    pub alg: i64,
    /// SEC1 uncompressed point (ES256) or the 32-byte key (EdDSA).
    pub key: Vec<u8>,
    pub counter: u32,
}

/// Reads the attestation object of a new passkey for `rp_id`: its
/// authenticator data must be for this site, with the user present, and
/// hold a credential with an ES256 or EdDSA key.
pub fn attestation(att: &[u8], rp_id: &str) -> Result<Credential, &'static str> {
    let (obj, _) = cbor::read(att)?;
    let auth = obj
        .get_text("authData")
        .and_then(cbor::Value::bytes)
        .ok_or("no authData")?;
    let (flags, counter, rest) = auth_data(auth, rp_id)?;
    if flags & 0x40 == 0 {
        return Err("no credential in authData");
    }
    // aaguid (16), id length (2), id, then the COSE key.
    if rest.len() < 18 {
        return Err("authData too short");
    }
    let len = usize::from(u16::from_be_bytes([rest[16], rest[17]]));
    let rest = &rest[18..];
    if len == 0 || len > 1023 || rest.len() < len {
        return Err("bad credential id");
    }
    let id = rest[..len].to_vec();
    let (key, _) = cbor::read(&rest[len..])?;
    let (alg, key) = cose_key(&key)?;
    Ok(Credential {
        id,
        alg,
        key,
        counter,
    })
}

/// Checks the authenticator data's site and user-present flag; returns its
/// flags, counter and what follows.
fn auth_data<'a>(auth: &'a [u8], rp_id: &str) -> Result<(u8, u32, &'a [u8]), &'static str> {
    if auth.len() < 37 {
        return Err("authData too short");
    }
    if auth[..32] != Sha256::digest(rp_id.as_bytes())[..] {
        return Err("passkey for another site");
    }
    let flags = auth[32];
    if flags & 0x01 == 0 {
        return Err("user not present");
    }
    let counter = u32::from_be_bytes([auth[33], auth[34], auth[35], auth[36]]);
    Ok((flags, counter, &auth[37..]))
}

/// A COSE key: (alg, key bytes).
fn cose_key(k: &cbor::Value) -> Result<(i64, Vec<u8>), &'static str> {
    let int = |n: i64| k.get_int(n).and_then(cbor::Value::int);
    let bytes = |n: i64| k.get_int(n).and_then(cbor::Value::bytes);
    match (int(1), int(3), int(-1)) {
        // EC2, ES256, P-256.
        (Some(2), Some(ES256), Some(1)) => {
            let (x, y) = (bytes(-2).ok_or("no x")?, bytes(-3).ok_or("no y")?);
            if x.len() != 32 || y.len() != 32 {
                return Err("bad P-256 point");
            }
            let mut point = vec![4];
            point.extend_from_slice(x);
            point.extend_from_slice(y);
            p256::ecdsa::VerifyingKey::from_sec1_bytes(&point).map_err(|_| "bad P-256 point")?;
            Ok((ES256, point))
        }
        // OKP, EdDSA, Ed25519.
        (Some(1), Some(EDDSA), Some(6)) => {
            let x = bytes(-2).ok_or("no x")?;
            let x: [u8; 32] = x.try_into().map_err(|_| "bad Ed25519 key")?;
            ed25519_dalek::VerifyingKey::from_bytes(&x).map_err(|_| "bad Ed25519 key")?;
            Ok((EDDSA, x.to_vec()))
        }
        _ => Err("key type not supported (ES256 or EdDSA only)"),
    }
}

/// Checks a sign-in with the passkey `key`: the authenticator data is for
/// this site with the user present, the signature over it and the client
/// data's hash is good, and the counter (when the authenticator keeps one)
/// went up. Returns the new counter.
pub fn assertion(
    auth: &[u8],
    client: &[u8],
    sig: &[u8],
    rp_id: &str,
    key: &crate::accounts::Passkey,
) -> Result<u32, &'static str> {
    let (_, counter, _) = auth_data(auth, rp_id)?;
    let mut signed = auth.to_vec();
    signed.extend_from_slice(&Sha256::digest(client));
    match key.alg {
        ES256 => {
            use p256::ecdsa::signature::Verifier;
            let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(&key.key).map_err(|_| "bad key")?;
            let s = p256::ecdsa::Signature::from_slice(&der_signature(sig)?)
                .map_err(|_| "bad signature")?;
            vk.verify(&signed, &s).map_err(|_| "bad signature")?;
        }
        EDDSA => {
            let k: [u8; 32] = key.key.as_slice().try_into().map_err(|_| "bad key")?;
            let vk = ed25519_dalek::VerifyingKey::from_bytes(&k).map_err(|_| "bad key")?;
            let s = ed25519_dalek::Signature::from_slice(sig).map_err(|_| "bad signature")?;
            vk.verify_strict(&signed, &s).map_err(|_| "bad signature")?;
        }
        _ => return Err("key type not supported"),
    }
    if (counter != 0 || key.counter != 0) && counter <= key.counter {
        return Err("counter went back: a copied passkey?");
    }
    Ok(counter)
}

/// An ECDSA signature in DER (`SEQUENCE { INTEGER r, INTEGER s }`) as the
/// 64 bytes `r || s`.
pub fn der_signature(der: &[u8]) -> Result<[u8; 64], &'static str> {
    const BAD: &str = "bad DER signature";
    let body = match der {
        [0x30, len, body @ ..] if usize::from(*len) == body.len() && *len < 0x80 => body,
        _ => return Err(BAD),
    };
    let mut out = [0u8; 64];
    let mut rest = body;
    for half in 0..2 {
        let (len, tail) = match rest {
            [0x02, len, tail @ ..] if usize::from(*len) <= tail.len() && *len > 0 => {
                (usize::from(*len), tail)
            }
            _ => return Err(BAD),
        };
        let mut n = &tail[..len];
        rest = &tail[len..];
        // A leading zero only keeps the number positive.
        if n.len() > 1 && n[0] == 0 {
            n = &n[1..];
        }
        if n.len() > 32 {
            return Err(BAD);
        }
        out[half * 32 + 32 - n.len()..half * 32 + 32].copy_from_slice(n);
    }
    if rest.is_empty() { Ok(out) } else { Err(BAD) }
}

/// Just enough CBOR (RFC 8949) for attestation objects and COSE keys.
pub mod cbor {
    #[derive(Debug, PartialEq, Eq)]
    pub enum Value {
        Int(i128),
        Bytes(Vec<u8>),
        Text(String),
        Array(Vec<Value>),
        Map(Vec<(Value, Value)>),
        Bool(bool),
        Null,
    }

    impl Value {
        pub fn int(&self) -> Option<i64> {
            match self {
                Value::Int(n) => i64::try_from(*n).ok(),
                _ => None,
            }
        }

        pub fn bytes(&self) -> Option<&[u8]> {
            match self {
                Value::Bytes(b) => Some(b),
                _ => None,
            }
        }

        fn get(&self, key: impl Fn(&Value) -> bool) -> Option<&Value> {
            match self {
                Value::Map(m) => m.iter().find(|(k, _)| key(k)).map(|(_, v)| v),
                _ => None,
            }
        }

        pub fn get_text(&self, key: &str) -> Option<&Value> {
            self.get(|k| matches!(k, Value::Text(t) if t == key))
        }

        pub fn get_int(&self, key: i64) -> Option<&Value> {
            self.get(|k| matches!(k, Value::Int(n) if *n == i128::from(key)))
        }
    }

    const MAX_DEPTH: u32 = 8;

    /// Reads one value; returns it and the bytes it took.
    pub fn read(input: &[u8]) -> Result<(Value, usize), &'static str> {
        let mut at = 0;
        let v = value(input, &mut at, 0)?;
        Ok((v, at))
    }

    fn take<'a>(input: &'a [u8], at: &mut usize, n: usize) -> Result<&'a [u8], &'static str> {
        let end = at
            .checked_add(n)
            .filter(|&e| e <= input.len())
            .ok_or("CBOR cut short")?;
        let s = &input[*at..end];
        *at = end;
        Ok(s)
    }

    fn value(input: &[u8], at: &mut usize, depth: u32) -> Result<Value, &'static str> {
        if depth > MAX_DEPTH {
            return Err("CBOR nested too deep");
        }
        let head = take(input, at, 1)?[0];
        let (major, info) = (head >> 5, head & 31);
        let arg: u64 = match info {
            0..=23 => u64::from(info),
            24 => u64::from(take(input, at, 1)?[0]),
            25 => u64::from(u16::from_be_bytes(
                take(input, at, 2)?.try_into().map_err(|_| "CBOR")?,
            )),
            26 => u64::from(u32::from_be_bytes(
                take(input, at, 4)?.try_into().map_err(|_| "CBOR")?,
            )),
            27 => u64::from_be_bytes(take(input, at, 8)?.try_into().map_err(|_| "CBOR")?),
            _ => return Err("CBOR: indefinite lengths not supported"),
        };
        // A length can't be more than what is left: no huge allocations.
        let len = |n: u64| -> Result<usize, &'static str> {
            usize::try_from(n)
                .ok()
                .filter(|&n| n <= input.len() - *at)
                .ok_or("CBOR length too long")
        };
        Ok(match major {
            0 => Value::Int(i128::from(arg)),
            1 => Value::Int(-1 - i128::from(arg)),
            2 => Value::Bytes(take(input, at, len(arg)?)?.to_vec()),
            3 => Value::Text(
                String::from_utf8(take(input, at, len(arg)?)?.to_vec())
                    .map_err(|_| "CBOR text not UTF-8")?,
            ),
            4 => {
                let n = len(arg)?;
                let mut items = Vec::with_capacity(n.min(64));
                for _ in 0..n {
                    items.push(value(input, at, depth + 1)?);
                }
                Value::Array(items)
            }
            5 => {
                let n = len(arg)?;
                let mut items = Vec::with_capacity(n.min(64));
                for _ in 0..n {
                    let k = value(input, at, depth + 1)?;
                    let v = value(input, at, depth + 1)?;
                    items.push((k, v));
                }
                Value::Map(items)
            }
            // A tag: the value it tags.
            6 => value(input, at, depth + 1)?,
            7 => match info {
                20 => Value::Bool(false),
                21 => Value::Bool(true),
                22 | 23 => Value::Null,
                _ => return Err("CBOR: floats and simple values not supported"),
            },
            _ => unreachable!("three bits"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cbor_bytes(b: &[u8]) -> Vec<u8> {
        let mut out = if b.len() < 24 {
            vec![0x40 | b.len() as u8]
        } else if b.len() < 256 {
            vec![0x58, b.len() as u8]
        } else {
            let l = b.len() as u16;
            vec![0x59, (l >> 8) as u8, l as u8]
        };
        out.extend_from_slice(b);
        out
    }

    /// A made-up authenticator's attestation object for an ES256 key.
    fn attestation_object(rp: &str, id: &[u8], point: &[u8]) -> Vec<u8> {
        let mut key = vec![0xa5, 0x01, 0x02, 0x03, 0x26, 0x20, 0x01, 0x21];
        key.extend(cbor_bytes(&point[1..33]));
        key.push(0x22);
        key.extend(cbor_bytes(&point[33..65]));
        let mut auth = Sha256::digest(rp.as_bytes()).to_vec();
        auth.push(0x45); // user present, verified, credential
        auth.extend_from_slice(&0u32.to_be_bytes());
        auth.extend_from_slice(&[0; 16]);
        auth.extend_from_slice(&(id.len() as u16).to_be_bytes());
        auth.extend_from_slice(id);
        auth.extend(key);
        let mut obj = vec![0xa3];
        obj.extend(b"\x63fmt\x64none\x67attStmt\xa0\x68authData");
        obj.extend(cbor_bytes(&auth));
        obj
    }

    #[test]
    fn rp_ids_and_origins() {
        assert_eq!(rp_id("koma.local:8080"), "koma.local");
        assert_eq!(rp_id("koma.local"), "koma.local");
        assert_eq!(rp_id("[::1]:8080"), "::1");
        assert!(origin_ok("https://koma.example", "koma.example"));
        assert!(origin_ok("http://localhost:8080", "localhost:8080"));
        assert!(!origin_ok("http://koma.example", "koma.example"));
        assert!(!origin_ok("https://evil.example", "koma.example"));
        assert!(!origin_ok("https://", ""));
    }

    #[test]
    fn der_signatures() {
        let mut der = vec![0x30, 0x45, 0x02, 0x21, 0x00];
        der.extend([0x80; 32]);
        der.extend([0x02, 0x20]);
        der.extend([0x11; 32]);
        let sig = der_signature(&der).unwrap();
        assert_eq!(&sig[..32], &[0x80; 32]);
        assert_eq!(&sig[32..], &[0x11; 32]);
        // Short numbers are padded on the left.
        let sig = der_signature(&[0x30, 0x06, 0x02, 0x01, 0x05, 0x02, 0x01, 0x07]).unwrap();
        assert_eq!((sig[31], sig[63], sig[0]), (5, 7, 0));
        assert!(der_signature(&[0x30, 0x03, 0x02, 0x01]).is_err());
        assert!(der_signature(&[]).is_err());
    }

    #[test]
    fn cbor_is_bounded() {
        // A byte string claiming 4 GB in a few bytes.
        assert!(cbor::read(&[0x5a, 0xff, 0xff, 0xff, 0xff]).is_err());
        // Nested too deep.
        assert!(cbor::read(&[0x81; 20]).is_err());
        let (v, used) = cbor::read(&[0xa1, 0x01, 0x20, 0xff]).unwrap();
        assert_eq!(used, 3);
        assert_eq!(v.get_int(1).and_then(cbor::Value::int), Some(-1));
    }

    #[test]
    fn adds_and_signs_in_with_a_p256_passkey() {
        use p256::ecdsa::SigningKey;
        use p256::ecdsa::signature::Signer;
        // A made-up authenticator: a fixed key, for the test only.
        let sk = SigningKey::from_slice(&[0x42; 32]).unwrap();
        let point = sk.verifying_key().to_sec1_point(false);
        let att = attestation_object("koma.local", b"cred-1", point.as_bytes());
        assert!(attestation(&att, "evil.example").is_err());
        let cred = attestation(&att, "koma.local").unwrap();
        assert_eq!((cred.id.as_slice(), cred.alg), (&b"cred-1"[..], ES256));

        let key = crate::accounts::Passkey {
            id: cred.id,
            account: 1,
            name: "phone".into(),
            alg: cred.alg,
            key: cred.key,
            counter: 0,
            added: 0,
            used: None,
        };
        let mut auth = Sha256::digest(b"koma.local").to_vec();
        auth.push(0x05);
        auth.extend_from_slice(&3u32.to_be_bytes());
        let client = br#"{"type":"webauthn.get","challenge":"AAAA","origin":"https://koma.local"}"#;
        let mut msg = auth.clone();
        msg.extend_from_slice(&Sha256::digest(client));
        let sig: p256::ecdsa::Signature = sk.sign(&msg);
        // DER, as browsers send it.
        let bytes = sig.to_bytes();
        let (r, s) = bytes.split_at(32);
        let int = |n: &[u8]| {
            let mut v = vec![0x02];
            let mut n = n.to_vec();
            if n[0] & 0x80 != 0 {
                n.insert(0, 0);
            }
            v.push(n.len() as u8);
            v.extend(n);
            v
        };
        let (r, s) = (int(r), int(s));
        let mut der = vec![0x30, (r.len() + s.len()) as u8];
        der.extend(r);
        der.extend(s);
        assert_eq!(assertion(&auth, client, &der, "koma.local", &key), Ok(3));
        let mut forged = client.to_vec();
        forged[10] ^= 1;
        assert!(assertion(&auth, &forged, &der, "koma.local", &key).is_err());
        let used = crate::accounts::Passkey { counter: 3, ..key };
        assert!(
            assertion(&auth, client, &der, "koma.local", &used).is_err(),
            "counter must go up"
        );
    }
}
