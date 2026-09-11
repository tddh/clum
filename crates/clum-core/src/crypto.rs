//! Recording encryption: X25519 envelope + chunked streaming AES-256-GCM.
//!
//! On-disk format (see `clum-docs/recording-encryption-design.md`):
//!
//! ```text
//! <plaintext header JSON line>\n
//! [chunk]*
//! chunk  = [4B LE payload_len][1B last_flag][ciphertext+tag]
//!   payload_len = ciphertext bytes + 16B GCM tag
//! nonce  = [4B counter BE][8B nonce_prefix]      (12B)
//! aad    = filename || [4B counter BE] || [1B last_flag]
//! ```
//!
//! - `counter` increments per chunk (0,1,2,...) and is implied by position;
//!   the final chunk sets `last_flag = 1`.
//! - Each chunk carries its own tag, so a recording interrupted at any point is
//!   decryptable up to the last complete chunk (tail truncation is detectable).
//! - `ciphertext_len` does NOT include the 16-byte tag.
//! - AAD binds the recording **filename** (not host/date) plus the chunk index and
//!   last-flag, preventing reorder/truncation.
//!
//! The server holds the X25519 static private key (keyring, one file per key_id).
//! The bridge holds only the current public key (delivered over TLS at registration).

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

/// Magic format marker written as the first field of the plaintext header.
pub const FMT: &str = "clum-enc";
/// Algorithm identifier.
pub const ALG: &str = "x25519-aes256gcm-v1";
/// Format version.
pub const VERSION: u8 = 1;
/// Random salt length (bytes).
pub const SALT_LEN: usize = 32;
/// Per-file nonce prefix length (bytes).
pub const NONCE_PREFIX_LEN: usize = 8;
/// Data encryption key length (bytes).
pub const DEK_LEN: usize = 32;
/// Key id length (hex characters; 8 bytes of the SHA-256 of the public key).
pub const KEY_ID_HEX_LEN: usize = 16;
/// Default plaintext bytes accumulated before emitting a chunk.
pub const DEFAULT_CHUNK_SIZE: usize = 64 * 1024;

const HKDF_INFO: &[u8] = b"clum-recording-v1";

/// Plaintext header line of an encrypted recording.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EncHeader {
    pub fmt: String,
    pub v: u8,
    pub alg: String,
    pub key_id: String,
    pub name: String,
    pub epk: String,
    pub salt: String,
    pub nonce_prefix: String,
    pub wrapped_dek: String,
}

/// Compute the 16-hex-char key id from a 32-byte X25519 public key.
pub fn key_id_of(public_key: &[u8; 32]) -> String {
    let digest = Sha256::digest(public_key);
    let mut s = String::with_capacity(KEY_ID_HEX_LEN);
    for b in &digest[..KEY_ID_HEX_LEN / 2] {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Server-side static recording key.
#[derive(Clone)]
pub struct RecordingKey {
    secret: StaticSecret,
}

impl RecordingKey {
    /// Generate a fresh random key.
    pub fn generate() -> anyhow::Result<Self> {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).map_err(|e| anyhow::anyhow!("os rng failed: {e}"))?;
        Ok(Self {
            secret: StaticSecret::from(bytes),
        })
    }

    /// Construct from raw 32-byte secret.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self {
            secret: StaticSecret::from(bytes),
        }
    }

    /// Raw secret bytes (for persisting to the keyring file).
    pub fn to_bytes(&self) -> [u8; 32] {
        self.secret.to_bytes()
    }

    /// Corresponding public key.
    pub fn public(&self) -> PublicKey {
        PublicKey::from(&self.secret)
    }

    /// Base64 of the public key (what gets delivered to the bridge).
    pub fn public_b64(&self) -> String {
        B64.encode(self.public().as_bytes())
    }

    /// 16-hex key id.
    pub fn key_id(&self) -> String {
        key_id_of(self.public().as_bytes())
    }

    /// Load from a hex-encoded keyring file, or generate+persist if absent.
    ///
    /// File mode is forced to 0600 on Unix.
    pub fn load_or_create(path: &std::path::Path) -> anyhow::Result<Self> {
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            let raw = hex_decode_32(content.trim())
                .map_err(|e| anyhow::anyhow!("invalid recording key {}: {e}", path.display()))?;
            return Ok(Self::from_bytes(raw));
        }
        let key = Self::generate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        std::fs::write(path, hex_encode(&key.to_bytes()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(key)
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn hex_decode_32(s: &str) -> anyhow::Result<[u8; 32]> {
    if s.len() != 64 {
        anyhow::bail!("expected 64 hex chars, got {}", s.len());
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)?;
    }
    Ok(out)
}

/// Streaming encryptor used by the bridge while a session is recorded.
pub struct RecordingEncryptor {
    cipher: Aes256Gcm,
    nonce_prefix: [u8; NONCE_PREFIX_LEN],
    aad_name: Vec<u8>,
    counter: u32,
    header_line: String,
    buf: Vec<u8>,
    finished: bool,
}

impl RecordingEncryptor {
    /// Build an encryptor from the server's base64 public key + the recording filename.
    pub fn new(server_pub_b64: &str, key_id: &str, filename: &str) -> anyhow::Result<Self> {
        let pub_raw = B64
            .decode(server_pub_b64)
            .map_err(|e| anyhow::anyhow!("bad server pubkey base64: {e}"))?;
        let pub_arr: [u8; 32] = pub_raw
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("server pubkey must be 32 bytes"))?;
        let server_pub = PublicKey::from(pub_arr);

        let mut esk_bytes = [0u8; 32];
        getrandom::getrandom(&mut esk_bytes).map_err(|e| anyhow::anyhow!("os rng failed: {e}"))?;
        let esk = StaticSecret::from(esk_bytes);
        let epk = PublicKey::from(&esk);
        let ss = esk.diffie_hellman(&server_pub);

        let mut salt = [0u8; SALT_LEN];
        getrandom::getrandom(&mut salt).map_err(|e| anyhow::anyhow!("os rng failed: {e}"))?;
        let mut nonce_prefix = [0u8; NONCE_PREFIX_LEN];
        getrandom::getrandom(&mut nonce_prefix)
            .map_err(|e| anyhow::anyhow!("os rng failed: {e}"))?;
        let mut dek = [0u8; DEK_LEN];
        getrandom::getrandom(&mut dek).map_err(|e| anyhow::anyhow!("os rng failed: {e}"))?;

        let kek = derive_kek(&ss.to_bytes(), epk.as_bytes(), server_pub.as_bytes(), &salt)?;
        let kek_cipher =
            Aes256Gcm::new_from_slice(&kek).map_err(|_| anyhow::anyhow!("invalid kek length"))?;
        let wrapped_dek = kek_cipher
            .encrypt(Nonce::from_slice(&[0u8; 12]), dek.as_ref())
            .map_err(|_| anyhow::anyhow!("dek wrap failed"))?;

        let header = EncHeader {
            fmt: FMT.to_string(),
            v: VERSION,
            alg: ALG.to_string(),
            key_id: key_id.to_string(),
            name: filename.to_string(),
            epk: B64.encode(epk.as_bytes()),
            salt: B64.encode(salt),
            nonce_prefix: B64.encode(nonce_prefix),
            wrapped_dek: B64.encode(&wrapped_dek),
        };
        let header_line = serde_json::to_string(&header)?;

        Ok(Self {
            cipher: Aes256Gcm::new_from_slice(&dek)
                .map_err(|_| anyhow::anyhow!("invalid dek length"))?,
            nonce_prefix,
            aad_name: filename.as_bytes().to_vec(),
            counter: 0,
            header_line,
            buf: Vec::with_capacity(DEFAULT_CHUNK_SIZE),
            finished: false,
        })
    }

    /// The plaintext header line (without trailing newline).
    pub fn header_line(&self) -> &str {
        &self.header_line
    }

    /// Feed plaintext; returns any framed chunks produced (possibly empty).
    pub fn write(&mut self, data: &[u8]) -> Vec<u8> {
        if self.finished {
            return Vec::new();
        }
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        while self.buf.len() >= DEFAULT_CHUNK_SIZE {
            let rest = self.buf.split_off(DEFAULT_CHUNK_SIZE);
            let chunk_pt = std::mem::replace(&mut self.buf, rest);
            out.extend_from_slice(&self.encrypt_chunk(&chunk_pt, false));
        }
        out
    }

    /// Flush remaining plaintext as the final chunk (idempotent).
    pub fn finish(&mut self) -> Vec<u8> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let pt = std::mem::take(&mut self.buf);
        self.encrypt_chunk(&pt, true)
    }

    fn encrypt_chunk(&mut self, plaintext: &[u8], last: bool) -> Vec<u8> {
        let nonce = build_nonce(self.counter, &self.nonce_prefix);
        let aad = build_aad(&self.aad_name, self.counter, last);
        let ct = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .expect("AES-GCM encrypt cannot fail for valid key/nonce");

        let mut framed = Vec::with_capacity(5 + ct.len());
        framed.extend_from_slice(&(ct.len() as u32).to_le_bytes());
        framed.push(u8::from(last));
        framed.extend_from_slice(&ct);
        if !last {
            self.counter = self.counter.wrapping_add(1);
        }
        framed
    }
}

fn derive_kek(
    ss: &[u8; 32],
    epk: &[u8; 32],
    server_pub: &[u8; 32],
    salt: &[u8; SALT_LEN],
) -> anyhow::Result<[u8; 32]> {
    let mut ikm = Vec::with_capacity(96);
    ikm.extend_from_slice(ss);
    ikm.extend_from_slice(epk);
    ikm.extend_from_slice(server_pub);
    let hk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut okm = [0u8; 32];
    hk.expand(HKDF_INFO, &mut okm)
        .map_err(|_| anyhow::anyhow!("hkdf expand failed"))?;
    Ok(okm)
}

fn build_nonce(counter: u32, prefix: &[u8; NONCE_PREFIX_LEN]) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(&counter.to_be_bytes());
    nonce[4..].copy_from_slice(prefix);
    nonce
}

fn build_aad(name: &[u8], counter: u32, last: bool) -> Vec<u8> {
    let mut aad = Vec::with_capacity(name.len() + 5);
    aad.extend_from_slice(name);
    aad.extend_from_slice(&counter.to_be_bytes());
    aad.push(u8::from(last));
    aad
}

/// True if `data` looks like an encrypted clum recording (header line sniff).
pub fn is_encrypted(data: &[u8]) -> bool {
    let end = data.iter().position(|&b| b == b'\n').unwrap_or(data.len());
    let first = &data[..end];
    first.starts_with(br#"{"fmt":"clum-enc""#)
}

/// Decrypt an encrypted recording. `lookup` resolves `key_id -> RecordingKey`.
/// `aad_name` must be the recording **filename** (basename) used at encryption time.
pub fn decrypt_recording(
    data: &[u8],
    lookup: &dyn Fn(&str) -> Option<RecordingKey>,
) -> anyhow::Result<Vec<u8>> {
    let nl = data
        .iter()
        .position(|&b| b == b'\n')
        .ok_or_else(|| anyhow::anyhow!("encrypted recording missing header line"))?;
    let header: EncHeader = serde_json::from_slice(&data[..nl])
        .map_err(|e| anyhow::anyhow!("invalid encrypted header: {e}"))?;
    if header.fmt != FMT {
        anyhow::bail!("unexpected fmt '{}'", header.fmt);
    }

    let key = lookup(&header.key_id)
        .ok_or_else(|| anyhow::anyhow!("no recording key for key_id {}", header.key_id))?;

    let epk_arr: [u8; 32] = B64
        .decode(&header.epk)
        .map_err(|e| anyhow::anyhow!("bad epk: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("epk must be 32 bytes"))?;
    let salt: [u8; SALT_LEN] = B64
        .decode(&header.salt)
        .map_err(|e| anyhow::anyhow!("bad salt: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("salt must be {SALT_LEN} bytes"))?;
    let nonce_prefix: [u8; NONCE_PREFIX_LEN] = B64
        .decode(&header.nonce_prefix)
        .map_err(|e| anyhow::anyhow!("bad nonce_prefix: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("nonce_prefix must be {NONCE_PREFIX_LEN} bytes"))?;
    let wrapped_dek = B64
        .decode(&header.wrapped_dek)
        .map_err(|e| anyhow::anyhow!("bad wrapped_dek: {e}"))?;

    let server_pub = key.public();
    let ss = key
        .secret
        .diffie_hellman(&PublicKey::from(epk_arr))
        .to_bytes();
    let kek = derive_kek(&ss, &epk_arr, server_pub.as_bytes(), &salt)?;
    let kek_cipher =
        Aes256Gcm::new_from_slice(&kek).map_err(|_| anyhow::anyhow!("invalid kek length"))?;
    let dek = kek_cipher
        .decrypt(Nonce::from_slice(&[0u8; 12]), wrapped_dek.as_ref())
        .map_err(|_| anyhow::anyhow!("dek unwrap failed (wrong key or tampered header)"))?;
    let cipher =
        Aes256Gcm::new_from_slice(&dek).map_err(|_| anyhow::anyhow!("invalid dek length"))?;

    let name = header.name.as_bytes();
    let mut out = Vec::new();
    let mut pos = nl + 1;
    let mut counter: u32 = 0;
    loop {
        if pos + 5 > data.len() {
            break;
        }
        let ct_len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let last = data[pos + 4] != 0;
        pos += 5;
        if pos + ct_len > data.len() {
            anyhow::bail!("truncated chunk at offset {pos}");
        }
        let ct_with_tag = &data[pos..pos + ct_len];
        let nonce = build_nonce(counter, &nonce_prefix);
        let aad = build_aad(name, counter, last);
        let pt = cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: ct_with_tag,
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("chunk {counter} authentication failed"))?;
        out.extend_from_slice(&pt);
        pos += ct_len;
        if last {
            break;
        }
        counter = counter.wrapping_add(1);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(plaintext: &[u8], filename: &str) {
        let server = RecordingKey::generate().unwrap();
        let mut enc =
            RecordingEncryptor::new(&server.public_b64(), &server.key_id(), filename).unwrap();
        let mut body = Vec::new();
        body.extend_from_slice(enc.header_line().as_bytes());
        body.push(b'\n');
        let mut off = 0;
        for step in [1usize, 7, 100, 65536, 5000] {
            if off >= plaintext.len() {
                break;
            }
            let end = (off + step).min(plaintext.len());
            body.extend_from_slice(&enc.write(&plaintext[off..end]));
            off = end;
        }
        if off < plaintext.len() {
            body.extend_from_slice(&enc.write(&plaintext[off..]));
        }
        body.extend_from_slice(&enc.finish());

        assert!(is_encrypted(&body));
        let key = server.clone();
        let out = decrypt_recording(&body, &|id| {
            if id == key.key_id() {
                Some(key.clone())
            } else {
                None
            }
        })
        .unwrap();
        assert_eq!(out, plaintext);
    }

    #[test]
    fn roundtrip_small() {
        roundtrip(b"hello world\n", "sess_pane_1_abcd.cast");
    }

    #[test]
    fn roundtrip_multi_chunk() {
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        roundtrip(&data, "big.cast");
    }

    #[test]
    fn roundtrip_empty() {
        roundtrip(b"", "empty.cast");
    }

    #[test]
    fn tampered_header_name_fails() {
        let server = RecordingKey::generate().unwrap();
        let mut enc =
            RecordingEncryptor::new(&server.public_b64(), &server.key_id(), "a.cast").unwrap();
        let mut body = Vec::new();
        body.extend_from_slice(enc.header_line().as_bytes());
        body.push(b'\n');
        body.extend_from_slice(&enc.write(b"secret"));
        body.extend_from_slice(&enc.finish());
        let text = String::from_utf8_lossy(&body).replace("\"a.cast\"", "\"b.cast\"");
        let tampered = text.into_bytes();
        let err = decrypt_recording(&tampered, &|_| Some(server.clone()));
        assert!(err.is_err(), "AAD (header name) mismatch must fail");
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let server = RecordingKey::generate().unwrap();
        let mut enc =
            RecordingEncryptor::new(&server.public_b64(), &server.key_id(), "t.cast").unwrap();
        let mut body = Vec::new();
        body.extend_from_slice(enc.header_line().as_bytes());
        body.push(b'\n');
        body.extend_from_slice(&enc.write(b"payload"));
        body.extend_from_slice(&enc.finish());
        let mid = body.len() - 5;
        body[mid] ^= 0x01;
        let err = decrypt_recording(&body, &|_| Some(server.clone()));
        assert!(err.is_err(), "tampered data must fail authentication");
    }

    #[test]
    fn key_id_is_stable_and_16_hex() {
        let k = RecordingKey::generate().unwrap();
        let id = k.key_id();
        assert_eq!(id.len(), KEY_ID_HEX_LEN);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(id, key_id_of(k.public().as_bytes()));
    }

    #[test]
    fn encryption_is_randomized() {
        let server = RecordingKey::generate().unwrap();
        let e1 = RecordingEncryptor::new(&server.public_b64(), &server.key_id(), "r.cast").unwrap();
        let e2 = RecordingEncryptor::new(&server.public_b64(), &server.key_id(), "r.cast").unwrap();
        assert_ne!(e1.header_line(), e2.header_line(), "epk/salt must differ");
    }

    #[test]
    fn wrong_key_fails() {
        let server = RecordingKey::generate().unwrap();
        let other = RecordingKey::generate().unwrap();
        let mut enc =
            RecordingEncryptor::new(&server.public_b64(), &server.key_id(), "w.cast").unwrap();
        let mut body = Vec::new();
        body.extend_from_slice(enc.header_line().as_bytes());
        body.push(b'\n');
        body.extend_from_slice(&enc.write(b"data"));
        body.extend_from_slice(&enc.finish());
        let err = decrypt_recording(&body, &|_| Some(other.clone()));
        assert!(err.is_err(), "wrong key must fail");
    }
}
