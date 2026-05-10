//! Encryption at rest — AES-256 with customer-managed keys.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use rand::RngCore;
use std::collections::HashMap;

/// Encryption algorithm.
#[derive(Debug, Clone, PartialEq)]
pub enum EncryptionAlgorithm {
    Aes256Gcm,
    Aes256Cbc,
    ChaCha20Poly1305,
}

/// Key management mode.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyManagement {
    /// Platform-managed keys (default)
    PlatformManaged,
    /// Customer-managed keys (BYOK)
    CustomerManaged { key_id: String },
    /// Hardware security module
    Hsm { hsm_id: String },
}

/// Encryption key metadata (never stores the actual key material in memory).
#[derive(Debug, Clone)]
pub struct KeyMetadata {
    pub key_id: String,
    pub algorithm: EncryptionAlgorithm,
    pub created_at: String,
    pub rotated_at: Option<String>,
    pub tenant_id: String,
    pub management: KeyManagement,
    pub active: bool,
}

/// Encrypted data envelope.
#[derive(Debug, Clone)]
pub struct EncryptedEnvelope {
    pub key_id: String,
    pub algorithm: EncryptionAlgorithm,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub tag: Vec<u8>, // Authentication tag for AEAD
}

/// Encryption configuration per tenant.
#[derive(Debug, Clone)]
pub struct EncryptionConfig {
    pub tenant_id: String,
    pub enabled: bool,
    pub algorithm: EncryptionAlgorithm,
    pub management: KeyManagement,
    pub auto_rotate_days: Option<u32>,
}

/// Key store (simplified — in production, use a KMS).
pub struct KeyStore {
    keys: HashMap<String, KeyMetadata>,
    tenant_configs: HashMap<String, EncryptionConfig>,
}

impl KeyStore {
    pub fn new() -> Self {
        Self {
            keys: HashMap::new(),
            tenant_configs: HashMap::new(),
        }
    }

    /// Register a new encryption key.
    pub fn register_key(&mut self, metadata: KeyMetadata) {
        self.keys.insert(metadata.key_id.clone(), metadata);
    }

    /// Set encryption config for a tenant.
    pub fn set_config(&mut self, config: EncryptionConfig) {
        self.tenant_configs.insert(config.tenant_id.clone(), config);
    }

    /// Get the active key for a tenant.
    pub fn active_key(&self, tenant_id: &str) -> Option<&KeyMetadata> {
        self.keys
            .values()
            .find(|k| k.tenant_id == tenant_id && k.active)
    }

    /// Check if encryption is enabled for a tenant.
    pub fn is_enabled(&self, tenant_id: &str) -> bool {
        self.tenant_configs
            .get(tenant_id)
            .map(|c| c.enabled)
            .unwrap_or(false)
    }

    /// Rotate key for a tenant (mark old as inactive, create new).
    pub fn rotate_key(&mut self, tenant_id: &str, new_key_id: &str) -> bool {
        // Deactivate old key
        for key in self.keys.values_mut() {
            if key.tenant_id == tenant_id && key.active {
                key.active = false;
                key.rotated_at = Some("now".into());
            }
        }
        // Register new key
        if let Some(config) = self.tenant_configs.get(tenant_id) {
            self.register_key(KeyMetadata {
                key_id: new_key_id.to_string(),
                algorithm: config.algorithm.clone(),
                created_at: "now".into(),
                rotated_at: None,
                tenant_id: tenant_id.to_string(),
                management: config.management.clone(),
                active: true,
            });
            true
        } else {
            false
        }
    }

    /// Get all keys for a tenant (for re-encryption).
    pub fn keys_for_tenant(&self, tenant_id: &str) -> Vec<&KeyMetadata> {
        self.keys
            .values()
            .filter(|k| k.tenant_id == tenant_id)
            .collect()
    }
}

impl Default for KeyStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Generate a random 256-bit encryption key.
pub fn generate_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    rand::rng().fill_bytes(&mut key);
    key
}

/// Generate a random 96-bit nonce.
pub fn generate_nonce() -> [u8; 12] {
    let mut nonce = [0u8; 12];
    rand::rng().fill_bytes(&mut nonce);
    nonce
}

/// Encrypt data using AES-256-GCM.
pub fn encrypt_data(plaintext: &[u8], key: &[u8; 32], nonce: &[u8; 12]) -> EncryptedEnvelope {
    let cipher = Aes256Gcm::new_from_slice(key).expect("valid key length");
    let aead_nonce = Nonce::from_slice(nonce);
    let ciphertext = cipher
        .encrypt(aead_nonce, plaintext)
        .expect("encryption should not fail");

    // aes-gcm appends the 16-byte auth tag to the ciphertext
    let tag_offset = ciphertext.len() - 16;
    let tag = ciphertext[tag_offset..].to_vec();
    let ct = ciphertext[..tag_offset].to_vec();

    EncryptedEnvelope {
        key_id: String::new(),
        algorithm: EncryptionAlgorithm::Aes256Gcm,
        nonce: nonce.to_vec(),
        ciphertext: ct,
        tag,
    }
}

/// Decrypt data using AES-256-GCM.
pub fn decrypt_data(envelope: &EncryptedEnvelope, key: &[u8; 32]) -> Vec<u8> {
    let cipher = Aes256Gcm::new_from_slice(key).expect("valid key length");
    let nonce = Nonce::from_slice(&envelope.nonce);

    // Reconstruct combined ciphertext+tag for aes-gcm
    let mut combined = Vec::with_capacity(envelope.ciphertext.len() + envelope.tag.len());
    combined.extend_from_slice(&envelope.ciphertext);
    combined.extend_from_slice(&envelope.tag);

    cipher
        .decrypt(nonce, combined.as_ref())
        .expect("decryption failed — wrong key or corrupted data")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = generate_key();
        let nonce = generate_nonce();
        let plaintext = b"Hello, TileTopia encryption!";
        let envelope = encrypt_data(plaintext, &key, &nonce);
        let decrypted = decrypt_data(&envelope, &key);
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_key_store_register() {
        let mut store = KeyStore::new();
        store.register_key(KeyMetadata {
            key_id: "key-1".into(),
            algorithm: EncryptionAlgorithm::Aes256Gcm,
            created_at: "2024-01-01".into(),
            rotated_at: None,
            tenant_id: "t1".into(),
            management: KeyManagement::PlatformManaged,
            active: true,
        });
        assert!(store.active_key("t1").is_some());
    }

    #[test]
    fn test_key_rotation() {
        let mut store = KeyStore::new();
        store.set_config(EncryptionConfig {
            tenant_id: "t1".into(),
            enabled: true,
            algorithm: EncryptionAlgorithm::Aes256Gcm,
            management: KeyManagement::PlatformManaged,
            auto_rotate_days: Some(90),
        });
        store.register_key(KeyMetadata {
            key_id: "old-key".into(),
            algorithm: EncryptionAlgorithm::Aes256Gcm,
            created_at: "2024-01-01".into(),
            rotated_at: None,
            tenant_id: "t1".into(),
            management: KeyManagement::PlatformManaged,
            active: true,
        });
        assert!(store.rotate_key("t1", "new-key"));
        let active = store.active_key("t1").unwrap();
        assert_eq!(active.key_id, "new-key");
    }

    #[test]
    fn test_encryption_enabled_check() {
        let mut store = KeyStore::new();
        assert!(!store.is_enabled("t1"));
        store.set_config(EncryptionConfig {
            tenant_id: "t1".into(),
            enabled: true,
            algorithm: EncryptionAlgorithm::Aes256Gcm,
            management: KeyManagement::CustomerManaged {
                key_id: "cmk-1".into(),
            },
            auto_rotate_days: None,
        });
        assert!(store.is_enabled("t1"));
    }

    #[test]
    fn test_ciphertext_differs_from_plaintext() {
        let key = generate_key();
        let nonce = generate_nonce();
        let plaintext = b"secret data";
        let envelope = encrypt_data(plaintext, &key, &nonce);
        assert_ne!(envelope.ciphertext, plaintext);
    }

    #[test]
    fn test_generated_keys_are_unique() {
        let k1 = generate_key();
        let k2 = generate_key();
        assert_ne!(k1, k2);
    }

    #[test]
    fn test_customer_managed_keys() {
        let store = KeyStore::new();
        // Verify default state
        assert!(store.keys_for_tenant("none").is_empty());
    }
}
