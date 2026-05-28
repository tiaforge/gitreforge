use std::{
    collections::HashMap,
    error::Error,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use gitrwlib::objs::CommitEditable;
use rustc_hash::FxHashSet;
use ssh_key::{Algorithm, HashAlg, LineEnding, PrivateKey, PublicKey, Signature, SshSig};

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

    pub fn sign_if_selected(&self, commit: &mut CommitEditable) -> Result<()> {
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

struct SshSigner {
    key: Option<PublicKey>,
    private_key_path: Option<PathBuf>,
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
            return Ok(Self {
                key: None,
                private_key_path: None,
            });
        };

        if signing_key.starts_with("key::") {
            return Ok(Self {
                key: Some(PublicKey::from_openssh(&signing_key[5..])?),
                private_key_path: None,
            });
        }

        if signing_key.starts_with("ssh-") {
            return Ok(Self {
                key: Some(PublicKey::from_openssh(signing_key)?),
                private_key_path: None,
            });
        }

        let path = expand_path(signing_key);
        let contents = fs::read_to_string(&path)?;
        if contents
            .trim_start()
            .starts_with("-----BEGIN OPENSSH PRIVATE KEY-----")
        {
            return Ok(Self {
                key: None,
                private_key_path: Some(path),
            });
        }

        Ok(Self {
            key: Some(PublicKey::from_openssh(contents.trim())?),
            private_key_path: None,
        })
    }

    fn sign(&self, payload: &[u8]) -> Result<Vec<u8>> {
        let signature = if let Some(private_key_path) = &self.private_key_path {
            let private_key = PrivateKey::read_openssh_file(private_key_path)?;
            private_key.sign(SSHSIG_NAMESPACE, HashAlg::default(), payload)?
        } else {
            sign_with_agent(self.key.as_ref(), payload)?
        };

        Ok(signature.to_pem(LineEnding::LF)?.into_bytes())
    }
}

struct GitConfig {
    values: HashMap<String, String>,
}

impl GitConfig {
    fn read(repository_path: &Path) -> Self {
        let mut values = HashMap::new();
        if let Some(home) = std::env::var_os("HOME") {
            read_config_file(&PathBuf::from(home).join(".gitconfig"), &mut values);
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

fn sign_with_agent(configured_key: Option<&PublicKey>, payload: &[u8]) -> Result<SshSig> {
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

    let signed_data = SshSig::signed_data(SSHSIG_NAMESPACE, HashAlg::default(), payload)?;
    let signature = agent_sign(&mut stream, &identity.blob, &signed_data)?;
    let public_key = PublicKey::from_bytes(&identity.blob)?;
    Ok(SshSig::new(
        public_key.key_data().clone(),
        SSHSIG_NAMESPACE,
        HashAlg::default(),
        signature,
    )?)
}

#[cfg(unix)]
fn connect_agent(socket: &str) -> Result<impl Read + Write> {
    Ok(std::os::unix::net::UnixStream::connect(socket)?)
}

#[cfg(not(unix))]
fn connect_agent(_socket: &str) -> Result<impl Read + Write> {
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
) -> Result<Signature> {
    let mut request = vec![13];
    push_string(&mut request, key_blob);
    push_string(&mut request, signed_data);
    request.extend_from_slice(&0u32.to_be_bytes());
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
