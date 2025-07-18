// # Responder Role
//
// Manages the [`Responder`] role in the Noise protocol handshake for secure communication between
// Sv2 roles. The responder is responsible for handling incoming handshake messages from the
// [`crate::Initiator`] (e.g., a mining proxy) and respond with the appropriate cryptographic
// data.
//
// The [`Responder`] role is equipped with utilities for handling elliptic curve Diffie-Hellman
// (ECDH) key exchanges, decrypting messages, and securely managing cryptographic state during the
// handshake phase. The responder's responsibilities include:
//
// - Generating an ephemeral key pair for the handshake.
// - Using the [`secp256k1`] elliptic curve for ECDH to compute a shared secret based on the
//   initiator's public key.
// - Decrypting and processing incoming handshake messages from the initiator.
// - Managing state transitions, including updates to the handshake hash, chaining key, and
//   encryption key as the session progresses.
//
// ## Usage
// The responder role is typically used by an upstream Sv2 role (e.g., a remote mining pool) to
// respond to an incoming handshake initiated by a downstream role (e.g., a local mining proxy).
// After receiving the initiator's public key, the responder computes a shared secret, which is
// used to securely encrypt further communication.
//
// The [`Responder`] struct implements the [`HandshakeOp`] trait, which defines the core
// cryptographic operations during the handshake. It ensures secure communication by supporting
// both the [`ChaCha20Poly1305`] or `AES-GCM` cipher, providing both confidentiality and message
// authentication for all subsequent communication.
//
// ### Secure Data Erasure
//
// The [`Responder`] includes functionality for securely erasing sensitive cryptographic material,
// ensuring that private keys and other sensitive data are wiped from memory when no longer needed.
// The [`Drop`] trait is implemented to automatically trigger secure erasure when the [`Responder`]
// instance goes out of scope, preventing potential misuse or leakage of cryptographic material.

use core::{ptr, time::Duration};

use crate::{
    cipher_state::{Cipher, CipherState, GenericCipher},
    error::Error,
    handshake::HandshakeOp,
    signature_message::SignatureNoiseMessage,
    NoiseCodec, ELLSWIFT_ENCODING_SIZE, ENCRYPTED_ELLSWIFT_ENCODING_SIZE,
    ENCRYPTED_SIGNATURE_NOISE_MESSAGE_SIZE, INITIATOR_EXPECTED_HANDSHAKE_MESSAGE_SIZE,
};
use aes_gcm::KeyInit;
use alloc::{
    boxed::Box,
    string::{String, ToString},
    vec::Vec,
};
use chacha20poly1305::ChaCha20Poly1305;
use secp256k1::{ellswift::ElligatorSwift, Keypair, Secp256k1, SecretKey};

const VERSION: u16 = 0;

/// Represents the state and operations of the responder in the Noise NX protocol handshake.
/// It handles cryptographic key exchanges, manages handshake state, and securely establishes
/// a connection with the initiator. The responder manages key generation, Diffie-Hellman exchanges,
/// message decryption, and state transitions, ensuring secure communication. Sensitive
/// cryptographic material is securely erased when no longer needed.
#[derive(Clone)]
pub struct Responder {
    // Cipher used for encrypting and decrypting messages during the handshake.
    //
    // It is initialized once enough information is available from the handshake process.
    handshake_cipher: Option<ChaCha20Poly1305>,
    // Optional static key used in the handshake. This key may be derived from the pre-shared key
    // (PSK) or generated during the handshake.
    k: Option<[u8; 32]>,
    // Current nonce used in the encryption process.
    //
    // Ensures that the same plaintext encrypted twice will produce different ciphertexts.
    n: u64,
    // Chaining key used in the key derivation process to generate new keys throughout the
    // handshake.
    ck: [u8; 32],
    // Handshake hash which accumulates all handshake messages to ensure integrity and prevent
    // tampering.
    h: [u8; 32],
    // Ephemeral key pair generated by the responder for this session, used for generating the
    // shared secret with the initiator.
    e: Keypair,
    // Static key pair of the responder, used to establish long-term identity and authenticity.
    //
    // Remains consistent across handshakes.
    s: Keypair,
    // Authority key pair, representing the responder's authority credentials.
    //
    // Used to sign messages and verify the identity of the responder.
    a: Keypair,
    // First [`CipherState`] used for encrypting messages from the initiator to the responder
    // after the handshake is complete.
    c1: Option<GenericCipher>,
    // Second [`CipherState`] used for encrypting messages from the responder to the initiator
    // after the handshake is complete.
    c2: Option<GenericCipher>,
    // Validity duration of the responder's certificate, in seconds.
    cert_validity: u32,
}

impl core::fmt::Debug for Responder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Responder").finish()
    }
}

// Ensures that the `Cipher` type is not `Sync`, which prevents multiple threads from
// simultaneously accessing the same instance of `Cipher`. This eliminates the need to handle
// potential issues related to visibility of changes across threads.
//
// After sending the `k` value, we immediately clear it to prevent the original thread from
// accessing the value again, thereby enhancing security by ensuring the sensitive data is no
// longer available in memory.
//
// The `Cipher` struct is neither `Sync` nor `Copy` due to its `cipher` field, which implements
// the `AeadCipher` trait. This trait requires mutable access, making the entire struct non-`Sync`
// and non-`Copy`, even though the key and nonce are simple types.

impl CipherState<ChaCha20Poly1305> for Responder {
    fn get_k(&mut self) -> &mut Option<[u8; 32]> {
        &mut self.k
    }

    fn get_n(&self) -> u64 {
        self.n
    }

    fn set_n(&mut self, n: u64) {
        self.n = n;
    }

    fn set_k(&mut self, k: Option<[u8; 32]>) {
        self.k = k;
    }

    fn get_cipher(&mut self) -> &mut Option<ChaCha20Poly1305> {
        &mut self.handshake_cipher
    }
}

impl HandshakeOp<ChaCha20Poly1305> for Responder {
    fn name(&self) -> String {
        "Responder".to_string()
    }

    fn get_h(&mut self) -> &mut [u8; 32] {
        &mut self.h
    }

    fn get_ck(&mut self) -> &mut [u8; 32] {
        &mut self.ck
    }

    fn set_h(&mut self, data: [u8; 32]) {
        self.h = data;
    }

    fn set_ck(&mut self, data: [u8; 32]) {
        self.ck = data;
    }

    fn set_handshake_cipher(&mut self, cipher: ChaCha20Poly1305) {
        self.handshake_cipher = Some(cipher);
    }
}

impl Responder {
    /// Creates a new [`Responder`] instance with the provided authority keypair and certificate
    /// validity.
    ///
    /// Constructs a new [`Responder`] with the necessary cryptographic state for the Noise NX
    /// protocol handshake. It generates ephemeral and static key pairs for the responder and
    /// prepares the handshake state. The authority keypair and certificate validity period are
    /// also configured.
    #[cfg(feature = "std")]
    pub fn new(a: Keypair, cert_validity: u32) -> Box<Self> {
        Self::new_with_rng(a, cert_validity, &mut rand::thread_rng())
    }

    /// Creates a new [`Responder`] instance with the provided authority keypair, certificate
    /// validity, and a custom random number generator.
    ///
    /// See [`Self::new`] for more details.
    ///
    /// The custom random number generator should be provided in order to not implicitely rely on
    /// `std` and allow `no_std` environments to provide a hardware random number generator for
    /// example.
    #[inline]
    pub fn new_with_rng<R: rand::Rng + ?Sized>(
        a: Keypair,
        cert_validity: u32,
        rng: &mut R,
    ) -> Box<Self> {
        let mut self_ = Self {
            handshake_cipher: None,
            k: None,
            n: 0,
            ck: [0; 32],
            h: [0; 32],
            e: Self::generate_key_with_rng(rng),
            s: Self::generate_key_with_rng(rng),
            a,
            c1: None,
            c2: None,
            cert_validity,
        };
        Self::initialize_self(&mut self_);
        Box::new(self_)
    }

    /// Creates a new [`Responder`] instance with the provided 32-byte authority key pair.
    ///
    /// Constructs a new [`Responder`] with a given public and private key pair, which represents
    /// the responder's authority credentials. It verifies that the provided public key matches the
    /// corresponding private key, ensuring the authenticity of the authority key pair. The
    /// certificate validity duration is also set here. Fails if the key pair is mismatched.
    #[cfg(feature = "std")]
    pub fn from_authority_kp(
        public: &[u8; 32],
        private: &[u8; 32],
        cert_validity: Duration,
    ) -> Result<Box<Self>, Error> {
        Self::from_authority_kp_with_rng(public, private, cert_validity, &mut rand::thread_rng())
    }

    /// Creates a new [`Responder`] instance with the provided 32-byte authority key pair and a
    /// custom random number generator.
    ///
    /// See [`Self::from_authority_kp`] for more details.
    ///
    /// The custom random number generator should be provided in order to not implicitely rely on
    /// `std` and allow `no_std` environments to provide a hardware random number generator for
    /// example.
    #[inline]
    pub fn from_authority_kp_with_rng<R: rand::Rng + ?Sized>(
        public: &[u8; 32],
        private: &[u8; 32],
        cert_validity: Duration,
        rng: &mut R,
    ) -> Result<Box<Self>, Error> {
        let secp = Secp256k1::new();
        let secret = SecretKey::from_slice(private).map_err(|_| Error::InvalidRawPrivateKey)?;
        let kp = Keypair::from_secret_key(&secp, &secret);
        let pub_ = kp.x_only_public_key().0.serialize();
        if public == &pub_[..] {
            Ok(Self::new_with_rng(kp, cert_validity.as_secs() as u32, rng))
        } else {
            Err(Error::InvalidRawPublicKey)
        }
    }

    /// Processes the first step of the Noise NX protocol handshake for the responder.
    ///
    /// This function manages the responder's side of the handshake after receiving the initiator's
    /// initial message. It processes the ephemeral public key provided by the initiator, derives
    /// the necessary shared secrets, and constructs the response message. The response includes
    /// the responder's ephemeral public key (in its ElligatorSwift-encoded form), the encrypted
    /// static public key, and a signature noise message. Additionally, it establishes the session
    /// ciphers for encrypting and decrypting further communication.
    ///
    /// On success, it returns a tuple containing the response message to be sent back to the
    /// initiator and a [`NoiseCodec`] instance, which is configured with the session ciphers for
    /// secure transmission of subsequent messages.
    ///
    /// On failure, the method returns an error if there is an issue during encryption, decryption,
    /// or any other step of the handshake process.
    #[cfg(feature = "std")]
    pub fn step_1(
        &mut self,
        elligatorswift_theirs_ephemeral_serialized: [u8; ELLSWIFT_ENCODING_SIZE],
    ) -> Result<([u8; INITIATOR_EXPECTED_HANDSHAKE_MESSAGE_SIZE], NoiseCodec), aes_gcm::Error> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as u32;

        self.step_1_with_now_rng(
            elligatorswift_theirs_ephemeral_serialized,
            now,
            &mut rand::thread_rng(),
        )
    }

    /// Executes the first step of the Noise NX protocol handshake for the responder given
    /// the current time and a custom random number generator.
    ///
    /// See [`Self::step_1`] for more details.
    ///
    /// The current time and the custom random number generatorshould be provided in order to not
    /// implicitely rely on `std` and allow `no_std` environments to provide a hardware random
    /// number generator for example.
    #[inline]
    pub fn step_1_with_now_rng<R: rand::Rng + rand::CryptoRng>(
        &mut self,
        elligatorswift_theirs_ephemeral_serialized: [u8; ELLSWIFT_ENCODING_SIZE],
        now: u32,
        rng: &mut R,
    ) -> Result<([u8; INITIATOR_EXPECTED_HANDSHAKE_MESSAGE_SIZE], NoiseCodec), aes_gcm::Error> {
        // 4.5.1.2 Responder
        Self::mix_hash(self, &elligatorswift_theirs_ephemeral_serialized[..]);
        Self::decrypt_and_hash(self, &mut vec![])?;

        // 4.5.2.1 Responder
        let mut out = [0; INITIATOR_EXPECTED_HANDSHAKE_MESSAGE_SIZE];
        let keypair = self.e;
        let elligatorswitf_ours_ephemeral = ElligatorSwift::from_pubkey(keypair.public_key());
        let elligatorswift_ours_ephemeral_serialized = elligatorswitf_ours_ephemeral.to_array();
        out[..ELLSWIFT_ENCODING_SIZE]
            .copy_from_slice(&elligatorswift_ours_ephemeral_serialized[..ELLSWIFT_ENCODING_SIZE]);

        // 3. calls `MixHash(e.public_key)`
        // what is here is not the public key encoded with ElligatorSwift, but the x-coordinate of
        // the public key (which is a point in the EC).

        Self::mix_hash(self, &elligatorswift_ours_ephemeral_serialized);

        // 4. calls `MixKey(ECDH(e.private_key, re.public_key))`
        let e_private_key = keypair.secret_key();
        let elligatorswift_theirs_ephemeral =
            ElligatorSwift::from_array(elligatorswift_theirs_ephemeral_serialized);
        let ecdh_ephemeral = ElligatorSwift::shared_secret(
            elligatorswift_theirs_ephemeral,
            elligatorswitf_ours_ephemeral,
            e_private_key,
            secp256k1::ellswift::ElligatorSwiftParty::B,
            None,
        )
        .to_secret_bytes();
        Self::mix_key(self, &ecdh_ephemeral);

        // 5. appends `EncryptAndHash(s.public_key)` (64 bytes encrypted elligatorswift  public key,
        //    16 bytes MAC)
        let mut encrypted_static_pub_k = vec![0; ELLSWIFT_ENCODING_SIZE];
        let elligatorswift_ours_static = ElligatorSwift::from_pubkey(self.s.public_key());
        let elligatorswift_ours_static_serialized: [u8; ELLSWIFT_ENCODING_SIZE] =
            elligatorswift_ours_static.to_array();
        encrypted_static_pub_k[..ELLSWIFT_ENCODING_SIZE]
            .copy_from_slice(&elligatorswift_ours_static_serialized[0..ELLSWIFT_ENCODING_SIZE]);
        self.encrypt_and_hash(&mut encrypted_static_pub_k)?;
        out[ELLSWIFT_ENCODING_SIZE..(ELLSWIFT_ENCODING_SIZE + ENCRYPTED_ELLSWIFT_ENCODING_SIZE)]
            .copy_from_slice(&encrypted_static_pub_k[..(ENCRYPTED_ELLSWIFT_ENCODING_SIZE)]);
        // note: 64+16+64 = 144

        // 6. calls `MixKey(ECDH(s.private_key, re.public_key))`
        let s_private_key = self.s.secret_key();
        let ecdh_static = ElligatorSwift::shared_secret(
            elligatorswift_theirs_ephemeral,
            elligatorswift_ours_static,
            s_private_key,
            secp256k1::ellswift::ElligatorSwiftParty::B,
            None,
        )
        .to_secret_bytes();
        Self::mix_key(self, &ecdh_static[..]);

        // 7. appends `EncryptAndHash(SIGNATURE_NOISE_MESSAGE)` to the buffer
        let valid_from = now;
        let not_valid_after = now + self.cert_validity;
        let signature_noise_message = self.get_signature(VERSION, valid_from, not_valid_after, rng);
        let mut signature_part = Vec::with_capacity(ENCRYPTED_SIGNATURE_NOISE_MESSAGE_SIZE);
        signature_part.extend_from_slice(&signature_noise_message[..]);
        Self::encrypt_and_hash(self, &mut signature_part)?;
        let ephemeral_plus_static_encrypted_length =
            ELLSWIFT_ENCODING_SIZE + ENCRYPTED_ELLSWIFT_ENCODING_SIZE;
        out[ephemeral_plus_static_encrypted_length..(INITIATOR_EXPECTED_HANDSHAKE_MESSAGE_SIZE)]
            .copy_from_slice(&signature_part[..ENCRYPTED_SIGNATURE_NOISE_MESSAGE_SIZE]);

        // 9. return pair of CipherState objects, the first for encrypting transport messages from
        //    initiator to responder, and the second for messages in the other direction:
        let ck = Self::get_ck(self);
        let (temp_k1, temp_k2) = Self::hkdf_2(ck, &[]);
        let c1 = ChaCha20Poly1305::new(&temp_k1.into());
        let c2 = ChaCha20Poly1305::new(&temp_k2.into());
        let c1: Cipher<ChaCha20Poly1305> = Cipher::from_key_and_cipher(temp_k1, c1);
        let c2: Cipher<ChaCha20Poly1305> = Cipher::from_key_and_cipher(temp_k2, c2);
        let to_send = out;
        self.c1 = None;
        self.c2 = None;
        let mut encryptor = GenericCipher::ChaCha20Poly1305(c2);
        let mut decryptor = GenericCipher::ChaCha20Poly1305(c1);
        encryptor.erase_k();
        decryptor.erase_k();
        let codec = crate::NoiseCodec {
            encryptor,
            decryptor,
        };
        Ok((to_send, codec))
    }

    // Generates a signature noise message for the responder's certificate.
    //
    // This method creates a signature noise message that includes the protocol version,
    // certificate validity period, and a cryptographic signature. The signature is created using
    // the responder's static public key and authority keypair, ensuring that the responder's
    // identity and certificate validity are cryptographically verifiable.
    #[inline]
    fn get_signature<R: rand::Rng + rand::CryptoRng>(
        &self,
        version: u16,
        valid_from: u32,
        not_valid_after: u32,
        rng: &mut R,
    ) -> [u8; 74] {
        let mut ret = [0; 74];
        let version = version.to_le_bytes();
        let valid_from = valid_from.to_le_bytes();
        let not_valid_after = not_valid_after.to_le_bytes();
        ret[0] = version[0];
        ret[1] = version[1];
        ret[2] = valid_from[0];
        ret[3] = valid_from[1];
        ret[4] = valid_from[2];
        ret[5] = valid_from[3];
        ret[6] = not_valid_after[0];
        ret[7] = not_valid_after[1];
        ret[8] = not_valid_after[2];
        ret[9] = not_valid_after[3];
        SignatureNoiseMessage::sign_with_rng(&mut ret, &self.s.x_only_public_key().0, &self.a, rng);
        ret
    }

    // Securely erases sensitive data in the responder's memory.
    //
    // Clears all sensitive cryptographic material within the [`Responder`] to prevent any
    // accidental leakage or misuse. It overwrites the stored keys, chaining key, handshake hash,
    // and session ciphers with zeros. This function is typically
    // called when the [`Responder`] instance is no longer needed or before deallocation.
    fn erase(&mut self) {
        if let Some(k) = self.k.as_mut() {
            for b in k {
                unsafe { ptr::write_volatile(b, 0) };
            }
        }
        for mut b in self.ck {
            unsafe { ptr::write_volatile(&mut b, 0) };
        }
        for mut b in self.h {
            unsafe { ptr::write_volatile(&mut b, 0) };
        }
        if let Some(c1) = self.c1.as_mut() {
            c1.erase_k()
        }
        if let Some(c2) = self.c2.as_mut() {
            c2.erase_k()
        }
        self.e.non_secure_erase();
        self.s.non_secure_erase();
        self.a.non_secure_erase();
    }
}

impl Drop for Responder {
    /// Ensures that sensitive data is securely erased when the [`Responder`] instance is dropped,
    /// preventing any potential leakage of cryptographic material.
    fn drop(&mut self) {
        self.erase();
    }
}
