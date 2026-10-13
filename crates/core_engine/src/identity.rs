//! Long-term device identity (Ed25519). Only used to authenticate to the
//! signaling service and to derive the public device id; session traffic keys
//! are always ephemeral (see [`crate::handshake`]).

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use remoti_proto::signaling::{AUTH_CONTEXT, DeviceId};
use ring::digest::{SHA256, digest};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair as _};

const KEY_FILE: &str = "device_ed25519.pk8";

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("identity storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("stored device key is corrupt")]
    Corrupt,
    #[error("random number generator failure")]
    Rng,
}

pub struct DeviceIdentity {
    keypair: Ed25519KeyPair,
    device_id: DeviceId,
}

impl DeviceIdentity {
    /// Loads the key from `dir`, creating it (owner-only permissions) on first run.
    pub fn load_or_create(dir: &Path, rng: &SystemRandom) -> Result<Self, IdentityError> {
        fs::create_dir_all(dir)?;
        let path = dir.join(KEY_FILE);
        let pkcs8 = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let doc = Ed25519KeyPair::generate_pkcs8(rng).map_err(|_| IdentityError::Rng)?;
                write_private(&path, doc.as_ref())?;
                doc.as_ref().to_vec()
            }
            Err(e) => return Err(e.into()),
        };
        Self::from_pkcs8(&pkcs8)
    }

    /// Non-persistent identity (tests, `--ephemeral-identity`).
    pub fn ephemeral(rng: &SystemRandom) -> Result<Self, IdentityError> {
        let doc = Ed25519KeyPair::generate_pkcs8(rng).map_err(|_| IdentityError::Rng)?;
        Self::from_pkcs8(doc.as_ref())
    }

    fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, IdentityError> {
        let keypair = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|_| IdentityError::Corrupt)?;
        let device_id = device_id_for(keypair.public_key().as_ref());
        Ok(Self { keypair, device_id })
    }

    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    pub fn public_key(&self) -> &[u8] {
        self.keypair.public_key().as_ref()
    }

    /// Signs the server's authentication challenge.
    pub fn sign_challenge(&self, nonce: &[u8]) -> Vec<u8> {
        let mut msg = Vec::with_capacity(AUTH_CONTEXT.len() + nonce.len());
        msg.extend_from_slice(AUTH_CONTEXT);
        msg.extend_from_slice(nonce);
        self.keypair.sign(&msg).as_ref().to_vec()
    }
}

pub fn device_id_for(public_key: &[u8]) -> DeviceId {
    let d = digest(&SHA256, public_key);
    let mut arr = [0u8; 32];
    arr.copy_from_slice(d.as_ref());
    DeviceId::from_key_digest(&arr)
}

fn write_private(path: &Path, data: &[u8]) -> Result<(), std::io::Error> {
    let tmp = path.with_extension("tmp");
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    fs::rename(tmp, path)
}

/// Platform default for persistent agent state.
pub fn default_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("REMOTI_DATA_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("Remoti");
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Library/Application Support/Remoti");
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("remoti");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/remoti");
    }
    PathBuf::from(".remoti")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{ED25519, UnparsedPublicKey};

    #[test]
    fn persists_and_signs() {
        let rng = SystemRandom::new();
        let dir = std::env::temp_dir().join(format!("remoti-id-test-{}", std::process::id()));
        let a = DeviceIdentity::load_or_create(&dir, &rng).unwrap();
        let b = DeviceIdentity::load_or_create(&dir, &rng).unwrap();
        assert_eq!(a.device_id(), b.device_id());
        let sig = a.sign_challenge(b"nonce");
        let mut msg = AUTH_CONTEXT.to_vec();
        msg.extend_from_slice(b"nonce");
        UnparsedPublicKey::new(&ED25519, b.public_key()).verify(&msg, &sig).unwrap();
        let _ = fs::remove_dir_all(dir);
    }
}
