use crate::{invalid, Lane, Role};
use graviola::{
    aead::ChaCha20Poly1305,
    hashing::{hmac::Hmac, Sha256},
};
use serde::{Deserialize, Serialize};
use std::{fmt, io};
use zeroize::Zeroize;

/// Check Graviola 0.4.2's mandatory gates before entering any crypto routine.
pub fn ensure_supported_cpu() -> io::Result<()> {
    #[cfg(target_arch = "x86_64")]
    let features = [
        ("aes", std::arch::is_x86_feature_detected!("aes")),
        (
            "pclmulqdq",
            std::arch::is_x86_feature_detected!("pclmulqdq"),
        ),
        ("bmi1", std::arch::is_x86_feature_detected!("bmi1")),
        ("adx", std::arch::is_x86_feature_detected!("adx")),
        ("avx", std::arch::is_x86_feature_detected!("avx")),
        ("avx2", std::arch::is_x86_feature_detected!("avx2")),
    ];
    #[cfg(target_arch = "aarch64")]
    let features = [
        ("neon", std::arch::is_aarch64_feature_detected!("neon")),
        ("aes", std::arch::is_aarch64_feature_detected!("aes")),
        ("pmull", std::arch::is_aarch64_feature_detected!("pmull")),
        ("sha2", std::arch::is_aarch64_feature_detected!("sha2")),
    ];
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let features = [("architecture", false)];
    for (name, supported) in features {
        // Graviola exposes these debug-only feature denial switches itself.
        let disabled = cfg!(debug_assertions)
            && std::env::var_os(format!("GRAVIOLA_CPU_DISABLE_{name}")).is_some();
        if !supported || disabled {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("relay crypto requires CPU feature {name}"),
            ));
        }
    }
    Ok(())
}

pub(crate) fn random<const N: usize>() -> io::Result<[u8; N]> {
    ensure_supported_cpu()?;
    let mut bytes = [0; N];
    graviola::random::fill(&mut bytes)
        .map_err(|_| io::Error::other("relay random source failed"))?;
    Ok(bytes)
}

/// Four independent direction/lane keys, delivered only through authorized control.
/// Generate once per grant. Serialization intentionally includes secret material.
#[derive(Clone, Serialize, Deserialize)]
pub struct SessionKeys {
    generation: u64,
    keys: [[u8; 32]; 4],
}
impl SessionKeys {
    pub fn generate() -> io::Result<Self> {
        Ok(Self {
            generation: u64::from_be_bytes(random()?),
            keys: [random()?, random()?, random()?, random()?],
        })
    }
}
impl fmt::Debug for SessionKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionKeys([REDACTED])")
    }
}
impl Drop for SessionKeys {
    fn drop(&mut self) {
        self.keys.zeroize();
        self.generation.zeroize();
    }
}

pub(crate) struct Cipher {
    key: ChaCha20Poly1305,
    aad: Vec<u8>,
    counter: u64,
}

/// RFC 5869 extract and the first expand block, sufficient for a 256-bit key.
fn hkdf_sha256_32(salt: &[u8], input_key: &[u8], info: &[u8]) -> [u8; 32] {
    let mut extract = Hmac::<Sha256>::new(salt);
    extract.update(input_key);
    let prk = extract.finish();
    let mut expand = Hmac::<Sha256>::new(prk.as_ref());
    expand.update(info);
    expand.update([1]);
    let derived = expand.finish();
    let mut key = [0; 32];
    key.copy_from_slice(derived.as_ref());
    key
}

impl Cipher {
    pub fn new(keys: &SessionKeys, id: &[u8; 32], lane: Lane, direction: Role) -> io::Result<Self> {
        ensure_supported_cpu()?;
        let mut aad = b"latch-relay/1\0".to_vec();
        aad.extend_from_slice(id);
        aad.extend_from_slice(&keys.generation.to_be_bytes());
        aad.extend_from_slice(&[lane as u8, direction as u8]);
        // Binding the key to the grant prevents nonce reuse across grants.
        let mut derived =
            hkdf_sha256_32(id, &keys.keys[lane as usize * 2 + direction as usize], &aad);
        let key = ChaCha20Poly1305::new(derived);
        derived.zeroize();
        Ok(Self {
            key,
            aad,
            counter: 0,
        })
    }
    fn nonce(&self) -> [u8; 12] {
        let mut nonce = [0; 12];
        nonce[4..].copy_from_slice(&self.counter.to_be_bytes());
        nonce
    }
    pub fn seal(&mut self, plaintext: &[u8]) -> io::Result<Vec<u8>> {
        ensure_supported_cpu()?;
        if plaintext.is_empty() || plaintext.len() > crate::wire::CHUNK || self.counter == u64::MAX
        {
            return Err(invalid(
                "invalid encrypted record length or exhausted counter",
            ));
        }
        let mut aad = self.aad.clone();
        aad.extend_from_slice(&self.counter.to_be_bytes());
        let mut ciphertext = plaintext.to_vec();
        let mut tag = [0; 16];
        self.key
            .encrypt(&self.nonce(), &aad, &mut ciphertext, &mut tag);
        let mut record = self.counter.to_be_bytes().to_vec();
        record.extend(ciphertext);
        record.extend(tag);
        self.counter += 1;
        Ok(record)
    }
    pub fn open(&mut self, record: &[u8]) -> io::Result<Vec<u8>> {
        ensure_supported_cpu()?;
        if record.len() < 25
            || record.len() > crate::wire::CHUNK + 24
            || self.counter == u64::MAX
            || record[..8] != self.counter.to_be_bytes()
        {
            return Err(invalid("invalid encrypted record or replay"));
        }
        let mut aad = self.aad.clone();
        aad.extend_from_slice(&record[..8]);
        let mut plaintext = record[8..record.len() - 16].to_vec();
        self.key
            .decrypt(
                &self.nonce(),
                &aad,
                &mut plaintext,
                &record[record.len() - 16..],
            )
            .map_err(|_| invalid("relay record authentication failed"))?;
        self.counter += 1;
        Ok(plaintext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn key_derivation_matches_rfc5869_case_one() {
        ensure_supported_cpu().unwrap();
        let output = hkdf_sha256_32(
            &(0x00..=0x0c).collect::<Vec<_>>(),
            &[0x0b; 22],
            &(0xf0..=0xf9).collect::<Vec<_>>(),
        );
        assert_eq!(
            output
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf"
        );
    }

    #[test]
    fn authentication_binds_counter_lane_direction_session_and_generation() {
        let keys = SessionKeys::generate().unwrap();
        let id = [1; 32];
        let mut tx = Cipher::new(&keys, &id, Lane::Priority, Role::Host).unwrap();
        let record = tx.seal(b"authenticated control").unwrap();
        let mut rx = Cipher::new(&keys, &id, Lane::Priority, Role::Host).unwrap();
        let mut tampered = record.clone();
        tampered[9] ^= 1;
        assert!(rx.open(&tampered).is_err());
        assert_eq!(rx.open(&record).unwrap(), b"authenticated control");
        assert!(rx.open(&record).is_err());
        for (lane, direction, session) in [
            (Lane::Video, Role::Host, id),
            (Lane::Priority, Role::Viewer, id),
            (Lane::Priority, Role::Host, [2; 32]),
        ] {
            assert!(Cipher::new(&keys, &session, lane, direction)
                .unwrap()
                .open(&record)
                .is_err());
        }
        let mut changed = keys.clone();
        changed.generation ^= 1;
        assert!(Cipher::new(&changed, &id, Lane::Priority, Role::Host)
            .unwrap()
            .open(&record)
            .is_err());
        assert_eq!(format!("{keys:?}"), "SessionKeys([REDACTED])");
    }
    #[test]
    fn cpu_rejection_is_a_result() {
        if std::env::var_os("LATCH_RELAY_CPU_CHILD").is_some() {
            assert_eq!(
                ensure_supported_cpu().unwrap_err().kind(),
                io::ErrorKind::Unsupported
            );
            assert_eq!(
                SessionKeys::generate().unwrap_err().kind(),
                io::ErrorKind::Unsupported
            );
            assert_eq!(
                random::<32>().unwrap_err().kind(),
                io::ErrorKind::Unsupported
            );
            let endpoint = crate::Endpoint::new("relay.example", 443).unwrap();
            assert_eq!(
                crate::ClientConfig::production(endpoint)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::Unsupported
            );
            let keys = SessionKeys {
                generation: 0,
                keys: [[0; 32]; 4],
            };
            assert!(
                matches!(Cipher::new(&keys, &[0;32], Lane::Video, Role::Host), Err(e) if e.kind()==io::ErrorKind::Unsupported)
            );
            return;
        }
        if !cfg!(debug_assertions) {
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crypto::tests::cpu_rejection_is_a_result"])
            .env("LATCH_RELAY_CPU_CHILD", "1")
            .env("GRAVIOLA_CPU_DISABLE_aes", "1")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "unsupported CPU must return errors without aborting"
        );
    }
}
