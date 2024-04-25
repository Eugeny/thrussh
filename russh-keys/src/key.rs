// Copyright 2016 Pierre-Étienne Meunier
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
use std::convert::TryFrom;

use ed25519_dalek::{Signer, Verifier};
use rand_core::OsRng;
use russh_cryptovec::CryptoVec;
use serde::{Deserialize, Serialize};
#[cfg(feature = "openssl")]
use {
    openssl::pkey::{Private, Public},
    sha2::{Digest, Sha256},
};
#[cfg(not(feature = "openssl"))]
use {
    rsa::{
        pss::{Pss, SigningKey},
        sha2::Sha512,
        sha2::{Digest, Sha256},
        signature::{RandomizedSigner, SignatureEncoding},
        traits::PublicKeyParts,
        BigUint, RsaPrivateKey, RsaPublicKey,
    },
    sha1::Sha1,
};

use crate::ec;
use crate::encoding::{Encoding, Reader};
pub use crate::signature::*;
use crate::Error;

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
/// Name of a public key algorithm.
pub struct Name(pub &'static str);

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        self.0
    }
}

/// The name of the ecdsa-sha2-nistp256 algorithm for SSH.
pub const ECDSA_SHA2_NISTP256: Name = Name("ecdsa-sha2-nistp256");
/// The name of the ecdsa-sha2-nistp384 algorithm for SSH.
pub const ECDSA_SHA2_NISTP384: Name = Name("ecdsa-sha2-nistp384");
/// The name of the ecdsa-sha2-nistp521 algorithm for SSH.
pub const ECDSA_SHA2_NISTP521: Name = Name("ecdsa-sha2-nistp521");
/// The name of the Ed25519 algorithm for SSH.
pub const ED25519: Name = Name("ssh-ed25519");
/// The name of the ssh-sha2-512 algorithm for SSH.
pub const RSA_SHA2_512: Name = Name("rsa-sha2-512");
/// The name of the ssh-sha2-256 algorithm for SSH.
pub const RSA_SHA2_256: Name = Name("rsa-sha2-256");

pub const NONE: Name = Name("none");

pub const SSH_RSA: Name = Name("ssh-rsa");

impl Name {
    /// Base name of the private key file for a key name.
    pub fn identity_file(&self) -> &'static str {
        match *self {
            ECDSA_SHA2_NISTP256 | ECDSA_SHA2_NISTP384 | ECDSA_SHA2_NISTP521 => "id_ecdsa",
            ED25519 => "id_ed25519",
            RSA_SHA2_512 => "id_rsa",
            RSA_SHA2_256 => "id_rsa",
            _ => unreachable!(),
        }
    }
}

#[doc(hidden)]
pub trait Verify {
    fn verify_client_auth(&self, buffer: &[u8], sig: &[u8]) -> bool;
    fn verify_server_auth(&self, buffer: &[u8], sig: &[u8]) -> bool;
}

/// The hash function used for signing with RSA keys.
#[derive(Eq, PartialEq, Clone, Copy, Debug, Hash, Serialize, Deserialize)]
#[allow(non_camel_case_types)]
pub enum SignatureHash {
    /// SHA2, 256 bits.
    SHA2_256,
    /// SHA2, 512 bits.
    SHA2_512,
    /// SHA1
    SHA1,
}

impl SignatureHash {
    pub fn name(&self) -> Name {
        match *self {
            SignatureHash::SHA2_256 => RSA_SHA2_256,
            SignatureHash::SHA2_512 => RSA_SHA2_512,
            SignatureHash::SHA1 => SSH_RSA,
        }
    }

    #[cfg(feature = "openssl")]
    fn message_digest(&self) -> openssl::hash::MessageDigest {
        use openssl::hash::MessageDigest;
        match *self {
            SignatureHash::SHA2_256 => MessageDigest::sha256(),
            SignatureHash::SHA2_512 => MessageDigest::sha512(),
            SignatureHash::SHA1 => MessageDigest::sha1(),
        }
    }

    pub fn from_rsa_hostkey_algo(algo: &[u8]) -> Option<Self> {
        if algo == b"rsa-sha2-256" {
            Some(Self::SHA2_256)
        } else if algo == b"rsa-sha2-512" {
            Some(Self::SHA2_512)
        } else {
            Some(Self::SHA1)
        }
    }
}

/// Public key
#[derive(Eq, Debug, Clone)]
pub enum PublicKey {
    #[doc(hidden)]
    Ed25519(ed25519_dalek::VerifyingKey),
    #[doc(hidden)]
    #[cfg(feature = "openssl")]
    RSA {
        key: OpenSSLPKey,
        hash: SignatureHash,
    },
    #[cfg(not(feature = "openssl"))]
    RSA {
        key: RsaPublicKey,
        hash: SignatureHash,
    },
    #[doc(hidden)]
    EC { key: ec::PublicKey },
}

impl PartialEq for PublicKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::RSA { key: a, .. }, Self::RSA { key: b, .. }) => a == b,
            (Self::Ed25519(a), Self::Ed25519(b)) => a == b,
            (Self::EC { key: a }, Self::EC { key: b }) => a == b,
            _ => false,
        }
    }
}

/// A public key from OpenSSL.
#[cfg(feature = "openssl")]
#[derive(Clone)]
pub struct OpenSSLPKey(pub openssl::pkey::PKey<Public>);

#[cfg(feature = "openssl")]
use std::cmp::{Eq, PartialEq};

#[cfg(feature = "openssl")]
impl PartialEq for OpenSSLPKey {
    fn eq(&self, b: &OpenSSLPKey) -> bool {
        self.0.public_eq(&b.0)
    }
}
#[cfg(feature = "openssl")]
impl Eq for OpenSSLPKey {}
#[cfg(feature = "openssl")]
impl std::fmt::Debug for OpenSSLPKey {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "OpenSSLPKey {{ (hidden) }}")
    }
}

impl PublicKey {
    /// Parse a public key in SSH format.
    pub fn parse(algo: &[u8], pubkey: &[u8]) -> Result<Self, Error> {
        match algo {
            b"ssh-ed25519" => {
                let mut p = pubkey.reader(0);
                let key_algo = p.read_string()?;
                let key_bytes = p.read_string()?;
                if key_algo != b"ssh-ed25519" {
                    return Err(Error::CouldNotReadKey);
                }
                let Ok(key_bytes) = <&[u8; ed25519_dalek::PUBLIC_KEY_LENGTH]>::try_from(key_bytes)
                else {
                    return Err(Error::CouldNotReadKey);
                };
                ed25519_dalek::VerifyingKey::from_bytes(key_bytes)
                    .map(PublicKey::Ed25519)
                    .map_err(Error::from)
            }
            b"ssh-rsa" | b"rsa-sha2-256" | b"rsa-sha2-512" => {
                #[cfg(feature = "openssl")]
                {
                    use log::debug;
                    let mut p = pubkey.reader(0);
                    let key_algo = p.read_string()?;
                    debug!("{:?}", std::str::from_utf8(key_algo));
                    if key_algo != b"ssh-rsa"
                        && key_algo != b"rsa-sha2-256"
                        && key_algo != b"rsa-sha2-512"
                    {
                        return Err(Error::CouldNotReadKey);
                    }
                    let key_e = p.read_string()?;
                    let key_n = p.read_string()?;
                    use openssl::bn::BigNum;
                    use openssl::pkey::PKey;
                    use openssl::rsa::Rsa;
                    Ok(PublicKey::RSA {
                        key: OpenSSLPKey(PKey::from_rsa(Rsa::from_public_components(
                            BigNum::from_slice(key_n)?,
                            BigNum::from_slice(key_e)?,
                        )?)?),
                        hash: SignatureHash::from_rsa_hostkey_algo(algo)
                            .unwrap_or(SignatureHash::SHA1),
                    })
                }
                #[cfg(not(feature = "openssl"))]
                {
                    // Assuming that `pubkey` is a reader that provides access to the key components
                    let mut p = pubkey.reader(0);

                    // Read the key algorithm
                    let key_algo = p.read_string()?;
                    if key_algo != b"ssh-rsa"
                        && key_algo != b"rsa-sha2-256"
                        && key_algo != b"rsa-sha2-512"
                    {
                        return Err(Error::CouldNotReadKey);
                    }

                    // Read the key components
                    let key_e = p.read_string()?;
                    let key_n = p.read_string()?;

                    // Convert byte slices to BigUints
                    let e = BigUint::from_bytes_be(key_e);
                    let n = BigUint::from_bytes_be(key_n);

                    Ok(PublicKey::RSA {
                        key: RsaPublicKey::new(n, e)?,
                        hash: SignatureHash::from_rsa_hostkey_algo(algo)
                            .unwrap_or(SignatureHash::SHA1),
                    })
                }
            }
            crate::KEYTYPE_ECDSA_SHA2_NISTP256
            | crate::KEYTYPE_ECDSA_SHA2_NISTP384
            | crate::KEYTYPE_ECDSA_SHA2_NISTP521 => {
                let mut p = pubkey.reader(0);
                let key_algo = p.read_string()?;
                let curve = p.read_string()?;
                let sec1_bytes = p.read_string()?;

                if key_algo != algo {
                    return Err(Error::CouldNotReadKey);
                }

                let key = ec::PublicKey::from_sec1_bytes(key_algo, sec1_bytes)?;
                if curve != key.ident().as_bytes() {
                    return Err(Error::CouldNotReadKey);
                }

                Ok(PublicKey::EC { key })
            }
            _ => Err(Error::CouldNotReadKey),
        }
    }

    /// Algorithm name for that key.
    pub fn name(&self) -> &'static str {
        match *self {
            PublicKey::Ed25519(_) => ED25519.0,
            PublicKey::RSA { ref hash, .. } => hash.name().0,
            PublicKey::EC { ref key } => key.algorithm(),
        }
    }

    /// Verify a signature.
    pub fn verify_detached(&self, buffer: &[u8], sig: &[u8]) -> bool {
        match self {
            PublicKey::Ed25519(ref public) => {
                let Ok(sig) = ed25519_dalek::ed25519::SignatureBytes::try_from(sig) else {
                    return false;
                };
                let sig = ed25519_dalek::Signature::from_bytes(&sig);
                public.verify(buffer, &sig).is_ok()
            }
            #[cfg(feature = "openssl")]
            PublicKey::RSA { ref key, ref hash } => {
                use openssl::sign::*;
                let verify = || {
                    let mut verifier = Verifier::new(hash.message_digest(), &key.0)?;
                    verifier.update(buffer)?;
                    verifier.verify(sig)
                };
                verify().unwrap_or(false)
            }
            #[cfg(not(feature = "openssl"))]
            PublicKey::RSA { ref key, ref hash } => {
                let pss = match hash {
                    SignatureHash::SHA1 => Pss::new::<Sha1>(),
                    SignatureHash::SHA2_256 => Pss::new::<Sha256>(),
                    SignatureHash::SHA2_512 => Pss::new::<Sha512>(),
                };
                key.verify(pss, buffer, sig).is_ok()
            }
            
            PublicKey::EC { ref key, .. } => ec_verify(key, buffer, sig).is_ok(),
        }
    }

    /// Compute the key fingerprint, hashed with sha2-256.
    pub fn fingerprint(&self) -> String {
        use super::PublicKeyBase64;
        let key = self.public_key_bytes();
        let mut hasher = Sha256::new();
        hasher.update(&key[..]);
        data_encoding::BASE64_NOPAD.encode(&hasher.finalize())
    }

    pub fn set_algorithm(&mut self, algorithm: &[u8]) {
        if let PublicKey::RSA { ref mut hash, .. } = self {
            if algorithm == b"rsa-sha2-512" {
                *hash = SignatureHash::SHA2_512
            } else if algorithm == b"rsa-sha2-256" {
                *hash = SignatureHash::SHA2_256
            } else if algorithm == b"ssh-rsa" {
                *hash = SignatureHash::SHA1
            }
        }
    }
}

impl Verify for PublicKey {
    fn verify_client_auth(&self, buffer: &[u8], sig: &[u8]) -> bool {
        self.verify_detached(buffer, sig)
    }
    fn verify_server_auth(&self, buffer: &[u8], sig: &[u8]) -> bool {
        self.verify_detached(buffer, sig)
    }
}

/// Public key exchange algorithms.
#[allow(clippy::large_enum_variant)]
pub enum KeyPair {
    Ed25519(ed25519_dalek::SigningKey),
    #[cfg(feature = "openssl")]
    RSA {
        key: openssl::rsa::Rsa<Private>,
        hash: SignatureHash,
    },
    #[cfg(not(feature = "openssl"))]
    RSA {
        key: RsaPrivateKey,
        hash: SignatureHash,
    },
    EC {
        key: ec::PrivateKey,
    },
}

impl Clone for KeyPair {
    fn clone(&self) -> Self {
        match self {
            #[allow(clippy::expect_used)]
            Self::Ed25519(kp) => {
                Self::Ed25519(ed25519_dalek::SigningKey::from_bytes(&kp.to_bytes()))
            }
            Self::RSA { key, hash } => Self::RSA {
                key: key.clone(),
                hash: *hash,
            },
            Self::EC { key } => Self::EC { key: key.clone() },
        }
    }
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match *self {
            KeyPair::Ed25519(ref key) => write!(
                f,
                "Ed25519 {{ public: {:?}, secret: (hidden) }}",
                key.verifying_key().as_bytes()
            ),
            KeyPair::RSA { .. } => write!(f, "RSA {{ (hidden) }}"),
            KeyPair::EC { .. } => write!(f, "EC {{ (hidden) }}"),
        }
    }
}

impl<'b> crate::encoding::Bytes for &'b KeyPair {
    fn bytes(&self) -> &[u8] {
        self.name().as_bytes()
    }
}

impl KeyPair {
    /// Copy the public key of this algorithm.
    pub fn clone_public_key(&self) -> Result<PublicKey, Error> {
        Ok(match self {
            KeyPair::Ed25519(ref key) => PublicKey::Ed25519(key.verifying_key()),
            #[cfg(feature = "openssl")]
            KeyPair::RSA { ref key, ref hash } => {
                use openssl::pkey::PKey;
                use openssl::rsa::Rsa;
                let key = Rsa::from_public_components(key.n().to_owned()?, key.e().to_owned()?)?;
                PublicKey::RSA {
                    key: OpenSSLPKey(PKey::from_rsa(key)?),
                    hash: *hash,
                }
            }
            #[cfg(not(feature = "openssl"))]
            KeyPair::RSA { ref key, ref hash } => {
                let cloned_key = RsaPublicKey::new(key.n().clone(), key.e().clone())?;
                PublicKey::RSA {
                    key: cloned_key,
                    hash: *hash,
                }
            }
            KeyPair::EC { ref key } => PublicKey::EC {
                key: key.to_public_key(),
            },
        })
    }

    /// Name of this key algorithm.
    pub fn name(&self) -> &'static str {
        match *self {
            KeyPair::Ed25519(_) => ED25519.0,
            KeyPair::RSA { ref hash, .. } => hash.name().0,
            KeyPair::EC { ref key } => key.algorithm(),
        }
    }

    /// Generate a key pair.
    pub fn generate_ed25519() -> Option<Self> {
        let keypair = ed25519_dalek::SigningKey::generate(&mut OsRng {});
        assert_eq!(
            keypair.verifying_key().as_bytes(),
            ed25519_dalek::VerifyingKey::from(&keypair).as_bytes()
        );
        Some(KeyPair::Ed25519(keypair))
    }

    pub fn generate_rsa(bits: usize, hash: SignatureHash) -> Option<Self> {
        #[cfg(feature = "openssl")]
        let key = openssl::rsa::Rsa::generate(bits as u32).ok()?;
        #[cfg(not(feature = "openssl"))]
        let key = RsaPrivateKey::new(&mut OsRng, bits).ok()?;
        Some(KeyPair::RSA { key, hash })
    }

    /// Sign a slice using this algorithm.
    pub fn sign_detached(&self, to_sign: &[u8]) -> Result<Signature, Error> {
        match self {
            #[allow(clippy::unwrap_used)]
            KeyPair::Ed25519(ref secret) => Ok(Signature::Ed25519(SignatureBytes(
                secret.sign(to_sign).to_bytes(),
            ))),
            KeyPair::RSA { ref key, ref hash } => Ok(Signature::RSA {
                bytes: rsa_signature(hash, key, to_sign)?,
                hash: *hash,
            }),
            KeyPair::EC { ref key } => Ok(Signature::ECDSA {
                algorithm: key.algorithm(),
                signature: ec_signature(key, to_sign)?,
            }),
        }
    }

    #[doc(hidden)]
    /// This is used by the server to sign the initial DH kex
    /// message. Note: we are not signing the same kind of thing as in
    /// the function below, `add_self_signature`.
    pub fn add_signature<H: AsRef<[u8]>>(
        &self,
        buffer: &mut CryptoVec,
        to_sign: H,
    ) -> Result<(), Error> {
        match self {
            KeyPair::Ed25519(ref secret) => {
                let signature = secret.sign(to_sign.as_ref());

                buffer.push_u32_be((ED25519.0.len() + signature.to_bytes().len() + 8) as u32);
                buffer.extend_ssh_string(ED25519.0.as_bytes());
                buffer.extend_ssh_string(signature.to_bytes().as_slice());
            }
            KeyPair::RSA { ref key, ref hash } => {
                // https://tools.ietf.org/html/draft-rsa-dsa-sha2-256-02#section-2.2
                let signature = rsa_signature(hash, key, to_sign.as_ref())?;
                let name = hash.name();
                buffer.push_u32_be((name.0.len() + signature.len() + 8) as u32);
                buffer.extend_ssh_string(name.0.as_bytes());
                buffer.extend_ssh_string(&signature);
            }
            KeyPair::EC { ref key } => {
                let algorithm = key.algorithm().as_bytes();
                let signature = ec_signature(key, to_sign.as_ref())?;
                buffer.push_u32_be((algorithm.len() + signature.len() + 8) as u32);
                buffer.extend_ssh_string(algorithm);
                buffer.extend_ssh_string(&signature);
            }
        }
        Ok(())
    }

    #[doc(hidden)]
    /// This is used by the client for authentication. Note: we are
    /// not signing the same kind of thing as in the above function,
    /// `add_signature`.
    pub fn add_self_signature(&self, buffer: &mut CryptoVec) -> Result<(), Error> {
        match self {
            KeyPair::Ed25519(ref secret) => {
                let signature = secret.sign(buffer);
                buffer.push_u32_be((ED25519.0.len() + signature.to_bytes().len() + 8) as u32);
                buffer.extend_ssh_string(ED25519.0.as_bytes());
                buffer.extend_ssh_string(signature.to_bytes().as_slice());
            }
            KeyPair::RSA { ref key, ref hash } => {
                // https://tools.ietf.org/html/draft-rsa-dsa-sha2-256-02#section-2.2
                let signature = rsa_signature(hash, key, buffer)?;
                let name = hash.name();
                buffer.push_u32_be((name.0.len() + signature.len() + 8) as u32);
                buffer.extend_ssh_string(name.0.as_bytes());
                buffer.extend_ssh_string(&signature);
            }
            KeyPair::EC { ref key } => {
                let signature = ec_signature(key, buffer)?;
                let algorithm = key.algorithm().as_bytes();
                buffer.push_u32_be((algorithm.len() + signature.len() + 8) as u32);
                buffer.extend_ssh_string(algorithm);
                buffer.extend_ssh_string(&signature);
            }
        }
        Ok(())
    }

    /// Create a copy of an RSA key with a specified hash algorithm.
    pub fn with_signature_hash(&self, hash: SignatureHash) -> Option<Self> {
        match self {
            KeyPair::Ed25519(_) => None,
            KeyPair::RSA { key, .. } => Some(KeyPair::RSA {
                key: key.clone(),
                hash,
            }),
            KeyPair::EC { .. } => None,
        }
    }
}

#[cfg(feature = "openssl")]
fn rsa_signature(
    hash: &SignatureHash,
    key: &openssl::rsa::Rsa<Private>,
    b: &[u8],
) -> Result<Vec<u8>, Error> {
    use openssl::pkey::*;
    use openssl::rsa::*;
    use openssl::sign::Signer;
    let pkey = PKey::from_rsa(Rsa::from_private_components(
        key.n().to_owned()?,
        key.e().to_owned()?,
        key.d().to_owned()?,
        key.p().ok_or(Error::KeyIsCorrupt)?.to_owned()?,
        key.q().ok_or(Error::KeyIsCorrupt)?.to_owned()?,
        key.dmp1().ok_or(Error::KeyIsCorrupt)?.to_owned()?,
        key.dmq1().ok_or(Error::KeyIsCorrupt)?.to_owned()?,
        key.iqmp().ok_or(Error::KeyIsCorrupt)?.to_owned()?,
    )?)?;
    let mut signer = Signer::new(hash.message_digest(), &pkey)?;
    signer.update(b)?;
    Ok(signer.sign_to_vec()?)
}

#[cfg(not(feature = "openssl"))]
fn rsa_signature(hash: &SignatureHash, key: &RsaPrivateKey, b: &[u8]) -> Result<Vec<u8>, Error> {
    let private_key = key.clone();
    let mut rng = OsRng;

    let signing_key = match hash {
        SignatureHash::SHA2_256 => {
            SigningKey::<Sha256>::new(private_key).sign_with_rng(&mut rng, b)
        }
        SignatureHash::SHA2_512 => {
            SigningKey::<Sha512>::new(private_key).sign_with_rng(&mut rng, b)
        }
        SignatureHash::SHA1 => SigningKey::<Sha1>::new(private_key).sign_with_rng(&mut rng, b),
    };

    Ok(signing_key.to_vec())
}

fn ec_signature(key: &ec::PrivateKey, b: &[u8]) -> Result<Vec<u8>, Error> {
    let (r, s) = key.try_sign(b)?;
    let mut buf = Vec::new();
    buf.extend_ssh_mpint(&r);
    buf.extend_ssh_mpint(&s);
    Ok(buf)
}

fn ec_verify(key: &ec::PublicKey, b: &[u8], sig: &[u8]) -> Result<(), Error> {
    let mut reader = sig.reader(0);
    key.verify(b, reader.read_mpint()?, reader.read_mpint()?)
}

/// Parse a public key from a byte slice.
pub fn parse_public_key(p: &[u8], refer_hash: Option<SignatureHash>) -> Result<PublicKey, Error> {
    let mut pos = p.reader(0);
    let t = pos.read_string()?;
    if t == b"ssh-ed25519" {
        if let Ok(pubkey) = pos.read_string() {
            let Ok(pubkey) = <&[u8; ed25519_dalek::PUBLIC_KEY_LENGTH]>::try_from(pubkey) else {
                return Err(Error::CouldNotReadKey);
            };
            let p = ed25519_dalek::VerifyingKey::from_bytes(pubkey).map_err(Error::from)?;
            return Ok(PublicKey::Ed25519(p));
        }
    }
    if t == b"ssh-rsa" {
        #[cfg(feature = "openssl")]
        {
            let e = pos.read_string()?;
            let n = pos.read_string()?;
            use openssl::bn::*;
            use openssl::pkey::*;
            use openssl::rsa::*;
            return Ok(PublicKey::RSA {
                key: OpenSSLPKey(PKey::from_rsa(Rsa::from_public_components(
                    BigNum::from_slice(n)?,
                    BigNum::from_slice(e)?,
                )?)?),
                hash: refer_hash.unwrap_or(SignatureHash::SHA2_256),
            });
        }
        #[cfg(not(feature = "openssl"))]
        {
            let e = pos.read_string()?;
            let n = pos.read_string()?;

            // Convert byte slices to BigUints
            let e = BigUint::from_bytes_be(e);
            let n = BigUint::from_bytes_be(n);

            return Ok(PublicKey::RSA {
                key: RsaPublicKey::new(n, e)?,
                hash: refer_hash.unwrap_or(SignatureHash::SHA2_256),
            });
        }
    }
    if t == crate::KEYTYPE_ECDSA_SHA2_NISTP256
        || t == crate::KEYTYPE_ECDSA_SHA2_NISTP384
        || t == crate::KEYTYPE_ECDSA_SHA2_NISTP521
    {
        let ident = pos.read_string()?;
        let sec1_bytes = pos.read_string()?;
        let key = ec::PublicKey::from_sec1_bytes(t, sec1_bytes)?;
        if ident != key.ident().as_bytes() {
            return Err(Error::CouldNotReadKey);
        }
        return Ok(PublicKey::EC { key });
    }
    Err(Error::CouldNotReadKey)
}

/// Obtain a cryptographic-safe random number generator.
pub fn safe_rng() -> impl rand::CryptoRng + rand::RngCore {
    rand::thread_rng()
}
