use std::{
    collections::HashMap,
    error::Error,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use gitreforgelib::objs::CommitEditable;
use rsa::signature::{SignatureEncoding, Signer};
use rustc_hash::FxHashSet;
use sha2::Sha512;
use ssh_key::{
    Algorithm, HashAlg, LineEnding, PrivateKey, PublicKey, Signature, SshSig,
    private::KeypairData, public::KeyData,
};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

const SSHSIG_NAMESPACE: &str = "git";

pub struct SigningPolicy {
    committers: FxHashSet<Vec<u8>>,
    signer: Option<SshSigner>,
}

impl SigningPolicy {
    pub fn create(repository_path: &Path, committers: Vec<String>) -> Result<Self> {
        let committers = committers
            .into_iter()
            .map(|committer| committer.into_bytes())
            .collect::<FxHashSet<_>>();
        let signer = if committers.is_empty() {
            None
        } else {
            Some(SshSigner::create(repository_path)?)
        };

        Ok(Self { committers, signer })
    }

    /// Signs the commit if the rewrite changed it and its committer is
    /// selected. Unchanged commits keep their id and existing signature.
    pub fn sign_if_selected(&self, commit: &mut CommitEditable) -> Result<()> {
        if self.committers.is_empty() || !commit.has_changes() {
            return Ok(());
        }
        let Some(email) = commit.committer_email() else {
            return Ok(());
        };
        if !self.committers.contains(email) {
            return Ok(());
        }

        let signer = self
            .signer
            .as_ref()
            .ok_or("committer was selected for signing but no signer was configured")?;
        let unsigned = commit.unsigned_bytes();
        let signature = signer.sign(unsigned.get_bytes())?;
        commit.set_signature(signature);
        Ok(())
    }
}

/// Every commit's id depends on its parent's signature, so the signatures
/// are made one after another and their cost adds up: keys are prepared
/// once, not for every signature.
enum SshSigner {
    Ed25519 {
        key: ed25519_dalek::SigningKey,
        public: KeyData,
    },
    Rsa {
        key: rsa::pkcs1v15::SigningKey<Sha512>,
        public: KeyData,
    },
    /// Other key types, signed through ssh-key
    PrivateKey(PrivateKey),
    Agent {
        stream: Mutex<AgentStream>,
        /// Key blob of the agent identity to sign with
        identity: Vec<u8>,
        public: KeyData,
        flags: u32,
    },
}

impl SshSigner {
    fn create(repository_path: &Path) -> Result<Self> {
        let config = GitConfig::read(repository_path);
        let format = config.get("gpg.format").unwrap_or("openpgp");
        if format != "ssh" {
            return Err(format!(
                "unsupported signing format `{format}` without external command execution; set gpg.format=ssh"
            )
            .into());
        }

        if config.contains("gpg.ssh.program") {
            return Err(
                "gpg.ssh.program is not supported because external command execution is disabled"
                    .into(),
            );
        }

        let Some(signing_key) = config.get("user.signingkey") else {
            return Self::agent(None);
        };

        if let Some(key) = signing_key.strip_prefix("key::") {
            return Self::agent(Some(&PublicKey::from_openssh(key)?));
        }

        if signing_key.starts_with("ssh-") {
            return Self::agent(Some(&PublicKey::from_openssh(signing_key)?));
        }

        let path = expand_path(signing_key);
        let contents = fs::read_to_string(&path)
            .map_err(|e| format!("cannot read user.signingkey {}: {e}", path.display()))?;
        if contents
            .trim_start()
            .starts_with("-----BEGIN OPENSSH PRIVATE KEY-----")
        {
            let key = PrivateKey::from_openssh(&contents)?;
            if key.is_encrypted() {
                return Err(format!(
                    "{} is encrypted; add it to an SSH agent and set user.signingkey to its public key",
                    path.display()
                )
                .into());
            }
            return Self::private_key(key);
        }

        Self::agent(Some(&PublicKey::from_openssh(contents.trim())?))
    }

    /// ssh-key derives Ed25519 and RSA keys again for every signature,
    /// which costs more than the signature itself, so those are prepared here.
    fn private_key(key: PrivateKey) -> Result<Self> {
        let public = key.public_key().key_data().clone();
        Ok(match key.key_data() {
            KeypairData::Ed25519(keypair) => Self::Ed25519 {
                key: ed25519_dalek::SigningKey::try_from(keypair)?,
                public,
            },
            KeypairData::Rsa(keypair) => {
                // ssh-key 0.6 passes p twice instead of p and q here, so its
                // own RSA signing always fails
                let key = rsa::RsaPrivateKey::from_components(
                    rsa::BigUint::try_from(&keypair.public.n)?,
                    rsa::BigUint::try_from(&keypair.public.e)?,
                    rsa::BigUint::try_from(&keypair.private.d)?,
                    vec![
                        rsa::BigUint::try_from(&keypair.private.p)?,
                        rsa::BigUint::try_from(&keypair.private.q)?,
                    ],
                )?;
                Self::Rsa {
                    key: rsa::pkcs1v15::SigningKey::new(key),
                    public,
                }
            }
            _ => Self::PrivateKey(key),
        })
    }

    /// Picks the configured key, or the first one, from the agent.
    fn agent(configured_key: Option<&PublicKey>) -> Result<Self> {
        let socket = std::env::var("SSH_AUTH_SOCK")
            .map_err(|_| "SSH_AUTH_SOCK is not set; cannot use SSH agent for signing")?;
        let mut stream = connect_agent(&socket)?;
        let identities = request_identities(&mut stream)?;
        let identity = if let Some(configured_key) = configured_key {
            let configured_key_bytes = configured_key.to_bytes()?;
            identities
                .into_iter()
                .find(|identity| identity.blob == configured_key_bytes)
                .ok_or("configured SSH signing key is not available from SSH_AUTH_SOCK agent")?
        } else {
            identities
                .into_iter()
                .next()
                .ok_or("SSH_AUTH_SOCK agent has no signing identities")?
        };

        let public = PublicKey::from_bytes(&identity.blob)?.key_data().clone();
        // SSHSIG doesn't allow the agent's default SHA-1 RSA signatures
        let flags = if matches!(public.algorithm(), Algorithm::Rsa { .. }) {
            SSH_AGENT_RSA_SHA2_512
        } else {
            0
        };

        // one connection for all signatures
        Ok(Self::Agent {
            stream: Mutex::new(stream),
            identity: identity.blob,
            public,
            flags,
        })
    }

    fn sign(&self, payload: &[u8]) -> Result<Vec<u8>> {
        let hash = HashAlg::default();
        let signed_data = || SshSig::signed_data(SSHSIG_NAMESPACE, hash, payload);
        let (public, signature) = match self {
            Self::PrivateKey(key) => {
                let signature = key.sign(SSHSIG_NAMESPACE, hash, payload)?;
                return Ok(signature.to_pem(LineEnding::LF)?.into_bytes());
            }
            Self::Ed25519 { key, public } => {
                let signature = key.sign(&signed_data()?).to_bytes().to_vec();
                (public, Signature::new(Algorithm::Ed25519, signature)?)
            }
            Self::Rsa { key, public } => {
                let signature = key.try_sign(&signed_data()?)?.to_vec();
                let algorithm = Algorithm::Rsa {
                    hash: Some(HashAlg::Sha512),
                };
                (public, Signature::new(algorithm, signature)?)
            }
            Self::Agent {
                stream,
                identity,
                public,
                flags,
            } => {
                let mut stream = stream.lock().unwrap();
                let signature = agent_sign(&mut *stream, identity, &signed_data()?, *flags)?;
                (public, signature)
            }
        };

        let signature = SshSig::new(public.clone(), SSHSIG_NAMESPACE, hash, signature)?;
        Ok(signature.to_pem(LineEnding::LF)?.into_bytes())
    }
}

struct GitConfig {
    values: HashMap<String, String>,
}

impl GitConfig {
    /// Global and repository configuration, in git's order: later files
    /// override earlier ones.
    fn read(repository_path: &Path) -> Self {
        let mut values = HashMap::new();
        if let Some(global) = std::env::var_os("GIT_CONFIG_GLOBAL") {
            read_config_file(Path::new(&global), &mut values);
        } else {
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let xdg = std::env::var_os("XDG_CONFIG_HOME")
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(".config")));
            if let Some(xdg) = xdg {
                read_config_file(&xdg.join("git/config"), &mut values);
            }
            if let Some(home) = home {
                read_config_file(&home.join(".gitconfig"), &mut values);
            }
        }
        read_config_file(&repository_path.join("config"), &mut values);
        Self { values }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(|value| value.as_str())
    }

    fn contains(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }
}

fn read_config_file(path: &Path, values: &mut HashMap<String, String>) {
    let Ok(contents) = fs::read_to_string(path) else {
        return;
    };

    let mut section = String::new();
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1]
                .trim()
                .replace('"', "")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(".");
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };

        let key = name.trim().to_ascii_lowercase();
        let value = value.trim().trim_matches('"').to_owned();
        let full_key = if section.is_empty() {
            key
        } else {
            format!("{section}.{key}").to_ascii_lowercase()
        };
        values.insert(full_key, value);
    }
}

fn expand_path(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(stripped);
        }
    }
    PathBuf::from(path)
}

/// SSH_AGENT_RSA_SHA2_512 sign request flag
const SSH_AGENT_RSA_SHA2_512: u32 = 4;

#[cfg(unix)]
type AgentStream = std::os::unix::net::UnixStream;

#[cfg(not(unix))]
type AgentStream = std::io::Empty;

#[cfg(unix)]
fn connect_agent(socket: &str) -> Result<AgentStream> {
    Ok(AgentStream::connect(socket)?)
}

#[cfg(not(unix))]
fn connect_agent(_socket: &str) -> Result<AgentStream> {
    Err("SSH agent signing is currently implemented for Unix sockets only".into())
}

struct AgentIdentity {
    blob: Vec<u8>,
}

fn request_identities(stream: &mut (impl Read + Write)) -> Result<Vec<AgentIdentity>> {
    write_agent_message(stream, &[11])?;
    let response = read_agent_message(stream)?;
    if response.first() != Some(&12) {
        return Err("SSH agent refused identity listing".into());
    }

    let mut cursor = Cursor::new(&response[1..]);
    let count = cursor.read_u32()? as usize;
    let mut identities = Vec::with_capacity(count);
    for _ in 0..count {
        let blob = cursor.read_string()?;
        let _comment = cursor.read_string()?;
        identities.push(AgentIdentity { blob });
    }
    Ok(identities)
}

fn agent_sign(
    stream: &mut (impl Read + Write),
    key_blob: &[u8],
    signed_data: &[u8],
    flags: u32,
) -> Result<Signature> {
    let mut request = vec![13];
    push_string(&mut request, key_blob);
    push_string(&mut request, signed_data);
    request.extend_from_slice(&flags.to_be_bytes());
    write_agent_message(stream, &request)?;

    let response = read_agent_message(stream)?;
    if response.first() != Some(&14) {
        return Err("SSH agent refused signing request".into());
    }

    let mut cursor = Cursor::new(&response[1..]);
    let signature_blob = cursor.read_string()?;
    let mut signature_cursor = Cursor::new(&signature_blob);
    let algorithm = String::from_utf8(signature_cursor.read_string()?)?;
    let signature = signature_cursor.read_string()?;
    Ok(Signature::new(Algorithm::new(&algorithm)?, signature)?)
}

fn write_agent_message(stream: &mut (impl Read + Write), message: &[u8]) -> Result<()> {
    stream.write_all(&(message.len() as u32).to_be_bytes())?;
    stream.write_all(message)?;
    Ok(())
}

fn read_agent_message(stream: &mut impl Read) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    let mut message = vec![0u8; len];
    stream.read_exact(&mut message)?;
    Ok(message)
}

fn push_string(target: &mut Vec<u8>, value: &[u8]) {
    target.extend_from_slice(&(value.len() as u32).to_be_bytes());
    target.extend_from_slice(value);
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn read_u32(&mut self) -> Result<u32> {
        if self.position + 4 > self.bytes.len() {
            return Err("truncated SSH agent response".into());
        }
        let value = u32::from_be_bytes(self.bytes[self.position..self.position + 4].try_into()?);
        self.position += 4;
        Ok(value)
    }

    fn read_string(&mut self) -> Result<Vec<u8>> {
        let len = self.read_u32()? as usize;
        if self.position + len > self.bytes.len() {
            return Err("truncated SSH agent string".into());
        }
        let value = self.bytes[self.position..self.position + len].to_vec();
        self.position += len;
        Ok(value)
    }
}
