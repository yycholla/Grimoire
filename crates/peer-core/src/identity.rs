use std::{fs::OpenOptions, io::Write as _, path::Path};

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{
    XChaCha20Poly1305,
    aead::{Aead, KeyInit, Payload},
};
use ed25519_dalek::SigningKey;
use zeroize::{Zeroize, Zeroizing};

use crate::{CommunityId, MemberId, NodeError, crypto::ContentEpoch, store::Store};

const MAGIC: &[u8] = b"peer-community-identity/v1\0";
const SALT_BYTES: usize = 16;
const NONCE_BYTES: usize = 24;
const MATERIAL_BYTES: usize = 201;
const TAG_BYTES: usize = 16;
const BACKUP_BYTES: usize = MAGIC.len() + SALT_BYTES + NONCE_BYTES + MATERIAL_BYTES + TAG_BYTES;

#[derive(Clone)]
pub(crate) struct IdentityMaterial {
    pub(crate) identity_seed: [u8; 32],
    pub(crate) hpke_seed: [u8; 32],
    pub(crate) community_id: CommunityId,
    pub(crate) owner: MemberId,
    pub(crate) active_epoch: Option<ContentEpoch>,
}

impl Drop for IdentityMaterial {
    fn drop(&mut self) {
        self.identity_seed.zeroize();
        self.hpke_seed.zeroize();
        if let Some(epoch) = &mut self.active_epoch {
            epoch.key.zeroize();
        }
    }
}

pub(crate) fn export(
    path: impl AsRef<Path>,
    passphrase: &str,
    material: IdentityMaterial,
) -> Result<(), NodeError> {
    validate_passphrase(passphrase)?;
    let salt: [u8; SALT_BYTES] = rand::random();
    let nonce: [u8; NONCE_BYTES] = rand::random();
    let key = derive_key(passphrase, &salt)?;
    let mut plaintext = Zeroizing::new(Vec::with_capacity(MATERIAL_BYTES));
    plaintext.extend_from_slice(&material.identity_seed);
    plaintext.extend_from_slice(&material.hpke_seed);
    plaintext.extend_from_slice(material.community_id.as_bytes());
    plaintext.extend_from_slice(material.owner.as_bytes());
    if let Some(epoch) = material.active_epoch {
        plaintext.push(1);
        plaintext.extend_from_slice(&epoch.number.to_be_bytes());
        plaintext.extend_from_slice(&epoch.head);
        plaintext.extend_from_slice(&epoch.key);
    } else {
        plaintext.extend_from_slice(&[0; 73]);
    }
    let mut aad = Vec::with_capacity(MAGIC.len() + SALT_BYTES);
    aad.extend_from_slice(MAGIC);
    aad.extend_from_slice(&salt);
    let ciphertext = XChaCha20Poly1305::new((&*key).into())
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: &plaintext,
                aad: &aad,
            },
        )
        .map_err(NodeError::protocol)?;
    let mut backup = Vec::with_capacity(BACKUP_BYTES);
    backup.extend_from_slice(MAGIC);
    backup.extend_from_slice(&salt);
    backup.extend_from_slice(&nonce);
    backup.extend_from_slice(&ciphertext);

    write_new(path.as_ref(), &backup)
}

pub async fn restore_identity(
    data_dir: impl AsRef<Path>,
    backup_path: impl AsRef<Path>,
    passphrase: &str,
) -> Result<MemberId, NodeError> {
    validate_passphrase(passphrase)?;
    let backup_path = backup_path.as_ref();
    let metadata = std::fs::metadata(backup_path)?;
    if metadata.len() != BACKUP_BYTES as u64 {
        return Err(NodeError::protocol("identity backup has an invalid size"));
    }
    let backup = std::fs::read(backup_path)?;
    let material = decrypt(passphrase, &backup)?;
    let member = MemberId::from_bytes(
        SigningKey::from_bytes(&material.identity_seed)
            .verifying_key()
            .to_bytes(),
    );
    Store::open(data_dir.as_ref())
        .await?
        .restore_identity(&material)
        .await?;
    Ok(member)
}

fn decrypt(passphrase: &str, backup: &[u8]) -> Result<IdentityMaterial, NodeError> {
    if backup.len() != BACKUP_BYTES || &backup[..MAGIC.len()] != MAGIC {
        return Err(NodeError::protocol("identity backup format is invalid"));
    }
    let salt: [u8; SALT_BYTES] = backup[MAGIC.len()..MAGIC.len() + SALT_BYTES]
        .try_into()
        .expect("validated backup has a fixed salt");
    let nonce_start = MAGIC.len() + SALT_BYTES;
    let nonce: [u8; NONCE_BYTES] = backup[nonce_start..nonce_start + NONCE_BYTES]
        .try_into()
        .expect("validated backup has a fixed nonce");
    let key = derive_key(passphrase, &salt)?;
    let plaintext = Zeroizing::new(
        XChaCha20Poly1305::new((&*key).into())
            .decrypt(
                (&nonce).into(),
                Payload {
                    msg: &backup[nonce_start + NONCE_BYTES..],
                    aad: &backup[..MAGIC.len() + SALT_BYTES],
                },
            )
            .map_err(|_| NodeError::protocol("identity backup could not be decrypted"))?,
    );
    if plaintext.len() != MATERIAL_BYTES {
        return Err(NodeError::protocol(
            "identity backup payload has an invalid size",
        ));
    }
    Ok(IdentityMaterial {
        identity_seed: plaintext[0..32]
            .try_into()
            .expect("validated payload has an identity seed"),
        hpke_seed: plaintext[32..64]
            .try_into()
            .expect("validated payload has an HPKE seed"),
        community_id: CommunityId::from_bytes(
            plaintext[64..96]
                .try_into()
                .expect("validated payload has a community id"),
        ),
        owner: MemberId::from_bytes(
            plaintext[96..128]
                .try_into()
                .expect("validated payload has an owner id"),
        ),
        active_epoch: match plaintext[128] {
            0 => None,
            1 => Some(ContentEpoch {
                number: u64::from_be_bytes(
                    plaintext[129..137]
                        .try_into()
                        .expect("validated payload has an epoch number"),
                ),
                head: plaintext[137..169]
                    .try_into()
                    .expect("validated payload has an epoch head"),
                key: plaintext[169..201]
                    .try_into()
                    .expect("validated payload has an epoch key"),
            }),
            _ => {
                return Err(NodeError::protocol(
                    "identity backup epoch marker is invalid",
                ));
            }
        },
    })
}

fn derive_key(passphrase: &str, salt: &[u8; SALT_BYTES]) -> Result<Zeroizing<[u8; 32]>, NodeError> {
    let params = Params::new(64 * 1024, 3, 1, Some(32)).map_err(NodeError::protocol)?;
    let mut key = Zeroizing::new([0; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), salt, &mut *key)
        .map_err(NodeError::protocol)?;
    Ok(key)
}

fn validate_passphrase(passphrase: &str) -> Result<(), NodeError> {
    if passphrase.chars().count() < 12 {
        Err(NodeError::protocol(
            "identity backup passphrase must be at least 12 characters",
        ))
    } else {
        Ok(())
    }
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), NodeError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let result = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(NodeError::from);
    if result.is_err() {
        drop(file);
        let _ = std::fs::remove_file(path);
    }
    result
}
