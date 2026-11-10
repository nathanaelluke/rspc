//! Read Soloist's session without changing the player's files. The scrambling
//! follows the legacy credential format implemented by upstream librespot (MIT).
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use openssl::{
    bn::BigNumContext,
    hash::MessageDigest,
    pkcs5::pbkdf2_hmac,
    pkey::{PKey, Private},
    sha::sha1,
    sign::Signer,
    symm::{Cipher, Crypter, Mode},
};
use protobuf::Message;
use serde_json::json;
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};
use zeroize::{Zeroize, Zeroizing};

use super::super::protocol::soloist_session::{SessionPayload, Storage};
const MAX_STORE_BYTES: usize = 64 * 1024;

pub(super) struct StoredSession {
    pub username: String,
    pub refresh: Zeroizing<String>,
    pub nonce: String,
    key: PKey<Private>,
}

pub(super) fn read(data: &Path) -> Result<Option<StoredSession>> {
    let Some(bytes) = read_bytes(data)? else {
        return Ok(None);
    };
    let device = private_file(&data.join(".device_id"))?;
    let device = std::str::from_utf8(&device)
        .context("Invalid Soloist device identifier")?
        .trim();
    decode(&bytes, device).map(Some)
}

// Account matching must not depend on the platform's signing-key format. The
// normal catalog login leaves the encrypted payload entirely to Soloist.
pub(super) fn read_username(data: &Path) -> Result<Option<String>> {
    read_bytes(data)?
        .map(|bytes| envelope(&bytes).map(|store| store.items[0].username.clone()))
        .transpose()
}

fn read_bytes(data: &Path) -> Result<Option<Zeroizing<Vec<u8>>>> {
    match private_file(&data.join("cache/dbrts")) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error.context("Cannot read Soloist's saved pairing")),
    }
}

pub(super) fn private_file(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    // geteuid has no preconditions and retains no pointers.
    ensure!(
        meta.is_file() && meta.uid() == unsafe { libc::geteuid() },
        "Invalid Soloist session file ownership or type"
    );
    ensure!(
        meta.len() <= MAX_STORE_BYTES as u64,
        "Soloist session file is too large"
    );
    let mut bytes = Zeroizing::new(Vec::new());
    file.take((MAX_STORE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_STORE_BYTES,
        "Soloist session file is too large"
    );
    Ok(bytes)
}

#[cfg(test)]
pub(super) fn write_fixture(data: &Path) -> Result<()> {
    use super::super::protocol::soloist_session as proto;
    use protobuf::MessageField;
    let device = "test-device";
    let username = "test-user";
    let group = openssl::ec::EcGroup::from_curve_name(openssl::nid::Nid::X9_62_PRIME256V1)?;
    let key = PKey::from_ec_key(openssl::ec::EcKey::generate(&group)?)?;
    let payload = proto::SessionPayload {
        refresh_token: MessageField::some(proto::RefreshToken {
            refresh_token: "test-refresh".into(),
            ..Default::default()
        }),
        dpop_info: MessageField::some(proto::DpopInfo {
            nonce: "initial-nonce".into(),
            key_reference: MessageField::some(proto::KeyReference {
                key_reference: key.private_key_to_pem_pkcs8()?,
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut clear = Zeroizing::new(payload.write_to_bytes()?);
    let padding = 16 - clear.len() % 16;
    clear.extend(std::iter::repeat_n(0x55, padding - 1));
    clear.push(padding as u8);
    for i in 16..clear.len() {
        clear[i] ^= clear[i - 16];
    }
    let mut derived = [0u8; 20];
    pbkdf2_hmac(
        &sha1(device.as_bytes()),
        username.as_bytes(),
        256,
        MessageDigest::sha1(),
        &mut derived,
    )?;
    let mut aes_key = sha1(&derived).to_vec();
    aes_key.extend(20u32.to_be_bytes());
    let mut encrypt = Crypter::new(Cipher::aes_192_ecb(), Mode::Encrypt, &aes_key, None)?;
    encrypt.pad(false);
    let mut ciphertext = vec![0; clear.len() + 16];
    let used = encrypt.update(&clear, &mut ciphertext)?;
    let last = encrypt.finalize(&mut ciphertext[used..])?;
    ciphertext.truncate(used + last);
    let storage = Storage {
        items: vec![proto::Item {
            username: username.into(),
            encrypted_payload: MessageField::some(proto::EncryptedPayload {
                version: 1,
                ciphertext,
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
    crate::paths::write_private_file_atomic(&data.join(".device_id"), device.as_bytes())?;
    crate::paths::write_private_file_atomic(&data.join("cache/dbrts"), &storage.write_to_bytes()?)?;
    Ok(())
}

fn envelope(bytes: &[u8]) -> Result<Storage> {
    let store = Storage::parse_from_bytes(bytes)
        .map_err(|_| anyhow::anyhow!("Invalid Soloist session envelope"))?;
    ensure!(
        store.items.len() == 1,
        "Soloist session must contain exactly one account"
    );
    let item = &store.items[0];
    ensure!(!item.username.is_empty(), "Soloist session has no account");
    Ok(store)
}

fn decode(bytes: &[u8], device: &str) -> Result<StoredSession> {
    ensure!(
        !device.is_empty() && device.len() <= 128,
        "Invalid Soloist device identifier"
    );
    let store = envelope(bytes)?;
    let item = &store.items[0];
    let encrypted = item
        .encrypted_payload
        .as_ref()
        .context("Soloist session has no payload")?;
    ensure!(
        encrypted.version == 1,
        "Unsupported Soloist session version"
    );
    let cipher = Cipher::aes_192_ecb();
    let ciphertext = &encrypted.ciphertext;
    ensure!(
        !ciphertext.is_empty() && ciphertext.len().is_multiple_of(cipher.block_size()),
        "Invalid Soloist session ciphertext"
    );
    let mut derived = Zeroizing::new([0u8; 20]);
    pbkdf2_hmac(
        &sha1(device.as_bytes()),
        item.username.as_bytes(),
        256,
        MessageDigest::sha1(),
        &mut *derived,
    )?;
    let mut key = Zeroizing::new([0u8; 24]);
    key[..20].copy_from_slice(&sha1(&*derived));
    key[20..].copy_from_slice(&20u32.to_be_bytes());
    let mut decrypt = Crypter::new(cipher, Mode::Decrypt, &*key, None)?;
    decrypt.pad(false);
    let mut clear = Zeroizing::new(vec![0u8; ciphertext.len() + cipher.block_size()]);
    let used = decrypt.update(ciphertext, &mut clear)?;
    let last = decrypt.finalize(&mut clear[used..])?;
    clear.truncate(used + last);
    for i in (16..clear.len()).rev() {
        clear[i] ^= clear[i - 16];
    }
    unpad(&mut clear)?;
    let mut payload = SessionPayload::parse_from_bytes(&clear)
        .map_err(|_| anyhow::anyhow!("Invalid Soloist session payload"))?;
    let refresh = payload
        .refresh_token
        .as_mut()
        .context("Soloist session has no refresh token")?;
    ensure!(
        !refresh.refresh_token.is_empty(),
        "Soloist session has no refresh token"
    );
    let refresh = Zeroizing::new(std::mem::take(&mut refresh.refresh_token));
    let dpop = payload
        .dpop_info
        .as_mut()
        .context("Soloist session has no signing key")?;
    let reference = dpop
        .key_reference
        .as_mut()
        .context("Soloist session has no signing key")?;
    let key = PKey::private_key_from_pem(&reference.key_reference);
    reference.key_reference.zeroize();
    let key = key.context("Unsupported Soloist signing key representation")?;
    let ec = key
        .ec_key()
        .context("Soloist signing key is not an EC key")?;
    ensure!(
        ec.group().curve_name() == Some(openssl::nid::Nid::X9_62_PRIME256V1),
        "Unsupported Soloist signing curve"
    );
    Ok(StoredSession {
        username: item.username.clone(),
        refresh,
        nonce: dpop.nonce.clone(),
        key,
    })
}

fn unpad(bytes: &mut Vec<u8>) -> Result<()> {
    let n = usize::from(*bytes.last().context("Empty Soloist session payload")?);
    ensure!(
        n > 0 && n <= 16 && n <= bytes.len(),
        "Invalid Soloist session padding"
    );
    // This format records the padding length in the final byte; preceding
    // padding bytes are not PKCS#7 and need not contain the same value.
    bytes.truncate(bytes.len() - n);
    Ok(())
}

impl StoredSession {
    pub fn proof(&self, endpoint: &str, nonce: &str) -> Result<String> {
        let ec = self.key.ec_key()?;
        let mut context = BigNumContext::new()?;
        let point = ec.public_key().to_bytes(
            ec.group(),
            openssl::ec::PointConversionForm::UNCOMPRESSED,
            &mut context,
        )?;
        ensure!(
            point.len() == 65 && point[0] == 4,
            "Invalid Soloist public key"
        );
        let header = json!({"typ":"dpop+jwt","alg":"ES256","jwk":{
            "kty":"EC","crv":"P-256","x":URL_SAFE_NO_PAD.encode(&point[1..33]),"y":URL_SAFE_NO_PAD.encode(&point[33..])
        }});
        let mut claims = json!({"htm":"POST","htu":endpoint,"iat":chrono::Utc::now().timestamp(),"jti":uuid::Uuid::new_v4().to_string()});
        if !nonce.is_empty() {
            claims["nonce"] = json!(nonce);
        }
        let unsigned = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?)
        );
        let mut signer = Signer::new(MessageDigest::sha256(), &self.key)?;
        signer.update(unsigned.as_bytes())?;
        let sig = openssl::ecdsa::EcdsaSig::from_der(&signer.sign_to_vec()?)?;
        let mut raw = sig.r().to_vec_padded(32)?;
        raw.extend(sig.s().to_vec_padded(32)?);
        Ok(format!("{unsigned}.{}", URL_SAFE_NO_PAD.encode(raw)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn account_matching_does_not_require_a_decryptable_payload() {
        use super::super::super::protocol::soloist_session::Item;
        let temp = tempfile::tempdir().unwrap();
        assert!(read_username(temp.path()).unwrap().is_none());
        let mut store = Storage {
            items: vec![Item {
                username: "test-user".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let path = temp.path().join("cache/dbrts");
        crate::paths::write_private_file_atomic(&path, &store.write_to_bytes().unwrap()).unwrap();
        assert_eq!(
            read_username(temp.path()).unwrap().as_deref(),
            Some("test-user")
        );
        assert!(read(temp.path()).is_err());
        store.items.push(Item {
            username: "another-user".into(),
            ..Default::default()
        });
        crate::paths::write_private_file_atomic(&path, &store.write_to_bytes().unwrap()).unwrap();
        assert!(read_username(temp.path()).is_err());
    }
    #[test]
    fn session_decryption_and_dpop_signatures_are_usable() {
        let temp = tempfile::tempdir().unwrap();
        write_fixture(temp.path()).unwrap();
        let stored = read(temp.path()).unwrap().unwrap();
        assert_eq!(stored.username, "test-user");
        assert_eq!(stored.refresh.as_str(), "test-refresh");
        let endpoint = "https://example.invalid/token";
        let proof = stored.proof(endpoint, "server-nonce").unwrap();
        let parts: Vec<_> = proof.split('.').collect();
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["htu"], endpoint);
        assert_eq!(claims["nonce"], "server-nonce");
        let raw = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        assert_eq!(raw.len(), 64);
        let signature = openssl::ecdsa::EcdsaSig::from_private_components(
            openssl::bn::BigNum::from_slice(&raw[..32]).unwrap(),
            openssl::bn::BigNum::from_slice(&raw[32..]).unwrap(),
        )
        .unwrap()
        .to_der()
        .unwrap();
        let mut verify =
            openssl::sign::Verifier::new(MessageDigest::sha256(), &stored.key).unwrap();
        verify
            .update(format!("{}.{}", parts[0], parts[1]).as_bytes())
            .unwrap();
        assert!(verify.verify(&signature).unwrap());
    }
    #[test]
    fn unknown_versions_and_ambiguous_accounts_are_rejected() {
        use super::super::super::protocol::soloist_session::{EncryptedPayload, Item};
        let item = Item {
            username: "example".into(),
            encrypted_payload: protobuf::MessageField::some(EncryptedPayload {
                version: 99,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut store = Storage {
            items: vec![item.clone()],
            ..Default::default()
        };
        assert!(
            decode(&store.write_to_bytes().unwrap(), "device")
                .err()
                .unwrap()
                .to_string()
                .contains("version")
        );
        store.items.push(item);
        assert!(
            decode(&store.write_to_bytes().unwrap(), "device")
                .err()
                .unwrap()
                .to_string()
                .contains("one account")
        );
    }
    #[test]
    fn padding_length_is_bounded_but_padding_contents_are_not_pkcs7() {
        let mut random_padding = vec![42, 99, 88, 3];
        unpad(&mut random_padding).unwrap();
        assert_eq!(random_padding, [42]);
        assert!(unpad(&mut vec![0]).is_err());
        let mut valid = vec![42, 3, 3, 3];
        unpad(&mut valid).unwrap();
        assert_eq!(valid, [42]);
    }
    #[test]
    fn session_reader_does_not_follow_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file");
        std::fs::write(&file, "private").unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(file, &link).unwrap();
        assert!(private_file(&link).is_err());
    }
}
