//! Local author account. Transport identity stays available to guests.

use super::codec;
use bincode::Options;
use mesh_identity::{account::Account, device::DeviceIdentity};
use mesh_storage::{
    EncryptionKey, NONCE_SIZE, SALT_SIZE, decrypt_data, encrypt_data, generate_salt,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    path::Path,
};
use zeroize::Zeroizing;

const FILE: &str = "author-account";
const VERSION: u8 = 1;
const MAX_ENVELOPE: u64 = 4096;

#[derive(Serialize, Deserialize)]
struct Envelope {
    version: u8,
    salt: [u8; SALT_SIZE],
    nonce: [u8; NONCE_SIZE],
    ciphertext: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct AuthorRecord {
    name: String,
    signing_seed: [u8; 32],
}

impl Drop for AuthorRecord {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.signing_seed.zeroize();
    }
}

pub(super) struct Unlocked {
    pub(super) name: String,
    pub(super) signer: DeviceIdentity,
}

impl AuthorRecord {
    fn unlock(&self, transport: [u8; 32]) -> Unlocked {
        Unlocked {
            name: self.name.clone(),
            signer: DeviceIdentity::from_secret_bytes(self.signing_seed, transport),
        }
    }
}

pub(super) fn exists(root: &Path) -> bool {
    root.join(FILE).is_file()
}

pub(super) fn valid_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name == name.trim()
        && name.len() <= 96
        && !name.chars().any(char::is_control)
}

pub(super) fn create(
    root: &Path,
    name: &str,
    password: &str,
    transport: [u8; 32],
    legacy_seed: Option<Zeroizing<[u8; 32]>>,
) -> io::Result<Unlocked> {
    let name = name.trim();
    if !valid_name(name) || password.chars().count() < 8 || password.len() > 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "请输入有效的名字和至少 8 个字符的密码",
        ));
    }
    let record = AuthorRecord {
        name: name.to_owned(),
        signing_seed: legacy_seed.map_or_else(|| Account::generate().secret_bytes(), |seed| *seed),
    };
    let plaintext = Zeroizing::new(codec().serialize(&record).map_err(io::Error::other)?);
    let salt = generate_salt();
    let key = EncryptionKey::from_password(password, &salt).map_err(io::Error::other)?;
    let (ciphertext, nonce) = encrypt_data(&plaintext, &key).map_err(io::Error::other)?;
    let envelope = codec()
        .serialize(&Envelope {
            version: VERSION,
            salt,
            nonce,
            ciphertext,
        })
        .map_err(io::Error::other)?;
    persist(&root.join(FILE), &envelope)?;
    Ok(record.unlock(transport))
}

fn persist(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|error| error.error)?;
    Ok(())
}

fn read(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_ENVELOPE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "账号数据无效"));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_ENVELOPE + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_ENVELOPE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "账号数据无效"));
    }
    Ok(bytes)
}

pub(super) fn open(root: &Path, password: &str, transport: [u8; 32]) -> io::Result<Unlocked> {
    decrypt(&read(&root.join(FILE))?, password, transport)
}

pub(super) fn export(root: &Path, destination: &Path) -> io::Result<()> {
    persist(destination, &read(&root.join(FILE))?)
}

pub(super) fn restore(
    root: &Path,
    source: &Path,
    password: &str,
    transport: [u8; 32],
) -> io::Result<Unlocked> {
    let bytes = read(source)?;
    let author = decrypt(&bytes, password, transport)?;
    persist(&root.join(FILE), &bytes)?;
    Ok(author)
}

fn decrypt(bytes: &[u8], password: &str, transport: [u8; 32]) -> io::Result<Unlocked> {
    if password.len() > 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "密码过长"));
    }
    let envelope: Envelope = codec()
        .with_limit(MAX_ENVELOPE)
        .deserialize(bytes)
        .map_err(io::Error::other)?;
    if envelope.version != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "账号格式不受支持",
        ));
    }
    let key = EncryptionKey::from_password(password, &envelope.salt).map_err(io::Error::other)?;
    let plaintext = Zeroizing::new(
        decrypt_data(&envelope.ciphertext, &envelope.nonce, &key).map_err(|_| {
            io::Error::new(io::ErrorKind::PermissionDenied, "密码错误或账号文件已损坏")
        })?,
    );
    let record: AuthorRecord = codec()
        .with_limit(MAX_ENVELOPE)
        .deserialize(&plaintext)
        .map_err(io::Error::other)?;
    Ok(record.unlock(transport))
}
