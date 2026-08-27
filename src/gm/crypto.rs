//! Phone↔browser payload crypto (AES-256-CTR + HMAC-SHA256) and the P-256 key that
//! signs Tachyon token refreshes.
use aes::cipher::{KeyIvInit, StreamCipher};
use anyhow::{Result, bail};
use hmac::{Hmac, Mac};
use p256::ecdsa::SigningKey;
use p256::pkcs8::EncodePublicKey;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type Aes256Ctr = ctr::Ctr128BE<aes::Aes256>;

fn random(n: usize) -> Vec<u8> { let mut v = vec![0u8; n]; rand::rng().fill_bytes(&mut v); v }

/// Keys shared with the phone through the QR code. Layout of a blob:
/// `ciphertext ‖ iv(16) ‖ hmac(32)`, MAC computed over `ciphertext ‖ iv`.
#[derive(Clone, Serialize, Deserialize)]
pub struct RequestCrypto {
    #[serde(with = "b64")] pub aes_key: Vec<u8>,
    #[serde(with = "b64")] pub hmac_key: Vec<u8>,
}

impl RequestCrypto {
    pub fn generate() -> Self { Self { aes_key: random(32), hmac_key: random(32) } }

    pub fn encrypt(&self, plain: &[u8]) -> Vec<u8> {
        let iv = random(16);
        let mut out = plain.to_vec();
        Aes256Ctr::new(self.aes_key.as_slice().into(), iv.as_slice().into()).apply_keystream(&mut out);
        out.extend_from_slice(&iv);
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.hmac_key).unwrap();
        mac.update(&out);
        out.extend_from_slice(&mac.finalize().into_bytes());
        out
    }

    pub fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 48 { bail!("encrypted payload too short ({} bytes)", data.len()); }
        let (body, sig) = data.split_at(data.len() - 32);
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.hmac_key).unwrap();
        mac.update(body);
        if mac.verify_slice(sig).is_err() { bail!("HMAC mismatch"); }
        let (ct, iv) = body.split_at(body.len() - 16);
        let mut out = ct.to_vec();
        Aes256Ctr::new(self.aes_key.as_slice().into(), iv.into()).apply_keystream(&mut out);
        Ok(out)
    }
}

/// P-256 key whose public half is registered with the relay; refreshes are signed with it.
#[derive(Clone, Serialize, Deserialize)]
pub struct RefreshKey {
    #[serde(with = "b64")] pub d: Vec<u8>,
}

impl RefreshKey {
    pub fn generate() -> Self { Self { d: SigningKey::random(&mut rand_core06()).to_bytes().to_vec() } }
    fn signing_key(&self) -> Result<SigningKey> { Ok(SigningKey::from_slice(&self.d)?) }
    /// SubjectPublicKeyInfo DER, as `RegisterPhoneRelay` expects.
    pub fn public_der(&self) -> Result<Vec<u8>> {
        Ok(self.signing_key()?.verifying_key().to_public_key_der()?.into_vec())
    }
    /// ASN.1 DER ECDSA signature over SHA-256(`request_id:timestamp`) — the digest is what gets signed.
    pub fn sign_refresh(&self, request_id: &str, timestamp_us: i64) -> Result<Vec<u8>> {
        use sha2::Digest;
        let digest = Sha256::digest(format!("{request_id}:{timestamp_us}").as_bytes());
        use p256::ecdsa::signature::hazmat::PrehashSigner;
        let sig: p256::ecdsa::Signature = self.signing_key()?.sign_prehash(&digest)?;
        Ok(sig.to_der().as_bytes().to_vec())
    }
}

/// p256 0.13 still speaks rand_core 0.6; bridge from the OS RNG.
pub fn rand_core06() -> impl p256::elliptic_curve::rand_core::CryptoRngCore {
    struct Os;
    impl p256::elliptic_curve::rand_core::RngCore for Os {
        fn next_u32(&mut self) -> u32 { rand::rng().next_u32() }
        fn next_u64(&mut self) -> u64 { rand::rng().next_u64() }
        fn fill_bytes(&mut self, d: &mut [u8]) { rand::rng().fill_bytes(d) }
        fn try_fill_bytes(&mut self, d: &mut [u8]) -> std::result::Result<(), p256::elliptic_curve::rand_core::Error> { self.fill_bytes(d); Ok(()) }
    }
    impl p256::elliptic_curve::rand_core::CryptoRng for Os {}
    Os
}

pub mod b64 {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> { s.serialize_str(&STANDARD.encode(v)) }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        STANDARD.decode(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        let c = RequestCrypto::generate();
        let ct = c.encrypt(b"hello owl");
        assert_eq!(c.decrypt(&ct).unwrap(), b"hello owl");
        let mut bad = ct.clone(); bad[0] ^= 1;
        assert!(c.decrypt(&bad).is_err());
    }
    #[test]
    fn keys() {
        let k = RefreshKey::generate();
        assert_eq!(k.public_der().unwrap().len(), 91);
        assert!(k.sign_refresh("x", 1).unwrap().len() >= 68);
    }
}
