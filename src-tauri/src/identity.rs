// E2E identity crypto for profile sharing.
//
// Each account owns an X25519 keypair. The PUBLIC key is published to the
// server so others can share a profile *to* it; the PRIVATE key never leaves
// the device unwrapped. It is stored encrypted under a key derived from a
// separate "encryption passphrase" — distinct from the login password and
// never sent to the server — so a compromised server can't read shared data.
//
// A shared profile has a random 256-bit data key (DEK). To grant a member
// access we SEAL the DEK to their public key (an ECIES sealed box): only the
// matching private key opens it, and the server stores the sealed grant as
// opaque bytes. The DEK — not the login/vault password — is what actually
// decrypts the profile's per-entity blobs, which is what lets several people
// hold independent access to the same profile.
//
// Wrapping (private key at rest) reuses the crate's Argon2id KDF + AES-256-GCM
// helpers so parameters stay consistent with the vault. Sealing is implemented
// here because the ECIES construction (ephemeral ECDH → HKDF → AEAD) is
// specific to sharing.

// This module is the crypto foundation for sharing; its API is wired into
// commands incrementally (S3b onward), so some items are test-only until then.
#![allow(dead_code)]

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey, StaticSecret};
use zeroize::Zeroize;

const NONCE_LEN: usize = 12;
// Domain-separation label so these AEAD keys can never collide with any other
// HKDF use in the app, and a version tag so the construction can evolve.
const HKDF_INFO: &[u8] = b"submarine-share-seal-v1";

/// A freshly generated identity keypair. `secret` is the raw 32-byte X25519
/// scalar — wrap it with [`wrap_secret`] before it ever touches disk.
pub struct Keypair {
    pub public: [u8; 32],
    pub secret: [u8; 32],
}

/// Generate a new account identity keypair.
pub fn generate_keypair() -> Keypair {
    let secret = StaticSecret::random_from_rng(rand::thread_rng());
    let public = PublicKey::from(&secret);
    Keypair {
        public: public.to_bytes(),
        secret: secret.to_bytes(),
    }
}

/// Derive the public key for a stored private scalar (e.g. after unwrapping).
pub fn public_of(secret: &[u8; 32]) -> [u8; 32] {
    let s = StaticSecret::from(*secret);
    PublicKey::from(&s).to_bytes()
}

/// Encrypt the private scalar under a key derived from `passphrase` + `salt`.
/// Returns the crate's standard `nonce||ct` hex blob. The passphrase is the
/// user's separate encryption passphrase; `salt` should be stored alongside.
pub fn wrap_secret(passphrase: &str, salt: &[u8], secret: &[u8; 32]) -> Result<String, String> {
    let mut kek = crate::derive_key(passphrase, salt)?;
    let out = crate::encrypt_entity(secret, &kek);
    kek.zeroize();
    out
}

/// Recover the private scalar from a wrapped blob. Wrong passphrase → AEAD tag
/// mismatch → `Err` (never a silently-wrong key).
pub fn unwrap_secret(passphrase: &str, salt: &[u8], wrapped_hex: &str) -> Result<[u8; 32], String> {
    let mut kek = crate::derive_key(passphrase, salt)?;
    let pt = crate::decrypt_entity(wrapped_hex, &kek);
    kek.zeroize();
    let mut pt = pt?;
    if pt.len() != 32 {
        pt.zeroize();
        return Err("[SHARE] BAD_PRIVKEY_LEN".into());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&pt);
    pt.zeroize();
    Ok(out)
}

/// Seal `plaintext` (a profile DEK) to `recipient_pub` so only the holder of
/// the matching private key can open it.
/// Layout: `eph_pub(32) || nonce(12) || ciphertext`.
pub fn seal_to(recipient_pub: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let recipient = PublicKey::from(*recipient_pub);
    let eph = EphemeralSecret::random_from_rng(rand::thread_rng());
    let eph_pub = PublicKey::from(&eph);
    let shared = eph.diffie_hellman(&recipient);
    let mut key = derive_seal_key(shared.as_bytes(), &eph_pub.to_bytes(), recipient_pub)?;
    let cipher = Aes256Gcm::new((&key).into());
    key.zeroize();
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
        .map_err(|_| "[SHARE] SEAL_ENCRYPT".to_string())?;
    let mut out = Vec::with_capacity(32 + NONCE_LEN + ct.len());
    out.extend_from_slice(&eph_pub.to_bytes());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a sealed grant produced by [`seal_to`] with our private scalar.
pub fn unseal(recipient_secret: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>, String> {
    if blob.len() < 32 + NONCE_LEN + 16 {
        return Err("[SHARE] SEAL_TOO_SHORT".into());
    }
    let mut eph = [0u8; 32];
    eph.copy_from_slice(&blob[..32]);
    let nonce = &blob[32..32 + NONCE_LEN];
    let ct = &blob[32 + NONCE_LEN..];
    let secret = StaticSecret::from(*recipient_secret);
    let recipient_pub = PublicKey::from(&secret).to_bytes();
    let shared = secret.diffie_hellman(&PublicKey::from(eph));
    let mut key = derive_seal_key(shared.as_bytes(), &eph, &recipient_pub)?;
    let cipher = Aes256Gcm::new((&key).into());
    key.zeroize();
    cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| "[SHARE] UNSEAL_DECRYPT".to_string())
}

/// HKDF-SHA256 the ECDH shared secret into an AES key, binding it to BOTH the
/// ephemeral and recipient public keys. Binding the recipient means a captured
/// grant can't be re-derived against a different recipient key.
fn derive_seal_key(shared: &[u8], eph_pub: &[u8; 32], recipient_pub: &[u8; 32]) -> Result<[u8; 32], String> {
    let mut salt = [0u8; 64];
    salt[..32].copy_from_slice(eph_pub);
    salt[32..].copy_from_slice(recipient_pub);
    let hk = Hkdf::<Sha256>::new(Some(&salt), shared);
    let mut okm = [0u8; 32];
    hk.expand(HKDF_INFO, &mut okm)
        .map_err(|_| "[SHARE] HKDF_EXPAND".to_string())?;
    Ok(okm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_roundtrip() {
        let alice = generate_keypair();
        let dek = [7u8; 32];
        let grant = seal_to(&alice.public, &dek).unwrap();
        let opened = unseal(&alice.secret, &grant).unwrap();
        assert_eq!(opened, dek);
    }

    #[test]
    fn seal_is_not_openable_by_a_third_party() {
        let alice = generate_keypair();
        let mallory = generate_keypair();
        let grant = seal_to(&alice.public, b"top secret DEK").unwrap();
        assert!(unseal(&mallory.secret, &grant).is_err());
    }

    #[test]
    fn seal_output_is_nondeterministic() {
        // Ephemeral key + random nonce → two seals of the same plaintext differ,
        // but both decrypt back to it.
        let k = generate_keypair();
        let a = seal_to(&k.public, b"same").unwrap();
        let b = seal_to(&k.public, b"same").unwrap();
        assert_ne!(a, b);
        assert_eq!(unseal(&k.secret, &a).unwrap(), b"same");
        assert_eq!(unseal(&k.secret, &b).unwrap(), b"same");
    }

    #[test]
    fn tampered_grant_is_rejected() {
        let k = generate_keypair();
        let mut grant = seal_to(&k.public, b"payload").unwrap();
        let last = grant.len() - 1;
        grant[last] ^= 0x01; // flip a ciphertext bit
        assert!(unseal(&k.secret, &grant).is_err());
    }

    #[test]
    fn wrap_unwrap_roundtrip_and_public_matches() {
        let kp = generate_keypair();
        let salt = [3u8; 16];
        let wrapped = wrap_secret("correct horse battery staple", &salt, &kp.secret).unwrap();
        let recovered = unwrap_secret("correct horse battery staple", &salt, &wrapped).unwrap();
        assert_eq!(recovered, kp.secret);
        // The public key derived from the recovered scalar matches the original.
        assert_eq!(public_of(&recovered), kp.public);
    }

    #[test]
    fn wrong_passphrase_fails() {
        let kp = generate_keypair();
        let salt = [9u8; 16];
        let wrapped = wrap_secret("right-passphrase", &salt, &kp.secret).unwrap();
        assert!(unwrap_secret("wrong-passphrase", &salt, &wrapped).is_err());
    }

    #[test]
    fn a_sealed_dek_decrypts_a_real_entity_blob() {
        // End-to-end: seal a DEK to a member, they unseal it, and use it to
        // decrypt an entity blob encrypted under that DEK. This is the exact
        // path a shared-profile sync will take.
        let member = generate_keypair();
        let dek = {
            let mut d = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut d);
            d
        };
        let blob = crate::encrypt_entity(b"{\"host\":\"10.0.0.1\"}", &dek).unwrap();

        let grant = seal_to(&member.public, &dek).unwrap();
        let dek_at_member = unseal(&member.secret, &grant).unwrap();
        let mut dek_arr = [0u8; 32];
        dek_arr.copy_from_slice(&dek_at_member);

        let plain = crate::decrypt_entity(&blob, &dek_arr).unwrap();
        assert_eq!(plain, b"{\"host\":\"10.0.0.1\"}");
    }
}
