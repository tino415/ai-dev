use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use http::HeaderMap;
use serde::{Deserialize, Serialize};
use webauthn_rs::prelude::*;

use crate::core::{hash_token, Result};
use crate::model::secret_token;

const ENROLL_VALIDITY_SECS: u64 = 600;
const COOKIE: &str = "aidev_session";
const PASSKEY_USER: Uuid = Uuid::from_u128(0x6f77_6e65_72);

enum Challenge {
    Register(PasskeyRegistration),
    Login(PasskeyAuthentication),
}

#[derive(Serialize, Deserialize)]
struct EnrollCode {
    hash: String,
    expires: u64,
}

pub struct Auth {
    dir: PathBuf,
    webauthn: Webauthn,
    secure_cookie: bool,
    sessions: Mutex<HashSet<String>>,
    challenges: Mutex<HashMap<String, Challenge>>,
}

impl Auth {
    pub fn new(dir: &Path, origin: &str) -> Result<Auth> {
        let origin_url = Url::parse(origin).map_err(|e| e.to_string())?;
        let rp_id = origin_url.host_str().ok_or("origin has no host")?.to_string();
        let webauthn = WebauthnBuilder::new(&rp_id, &origin_url)
            .map_err(|e| e.to_string())?
            .rp_name("ai-dev")
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Auth {
            dir: dir.to_path_buf(),
            webauthn,
            secure_cookie: origin_url.scheme() == "https",
            sessions: Mutex::new(HashSet::new()),
            challenges: Mutex::new(HashMap::new()),
        })
    }

    // static tokens for the MCP endpoint

    pub fn create_static_token(dir: &Path) -> Result<String> {
        let token = secret_token();
        let path = dir.join("static_tokens");
        let mut hashes = std::fs::read_to_string(&path).unwrap_or_default();
        hashes.push_str(&hash_token(&token));
        hashes.push('\n');
        std::fs::write(&path, hashes).map_err(|e| e.to_string())?;
        Ok(token)
    }

    pub fn static_token_valid(&self, token: &str) -> bool {
        let hash = hash_token(token);
        std::fs::read_to_string(self.dir.join("static_tokens"))
            .unwrap_or_default()
            .lines()
            .any(|line| line == hash)
    }

    // enrollment

    pub fn create_enroll_code(dir: &Path) -> Result<String> {
        let code = secret_token()[..12].to_string();
        let record = EnrollCode {
            hash: hash_token(&code),
            expires: now_secs() + ENROLL_VALIDITY_SECS,
        };
        std::fs::write(
            dir.join("enroll.json"),
            serde_json::to_vec(&record).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        Ok(code)
    }

    fn consume_enroll_code(&self, code: &str) -> bool {
        let path = self.dir.join("enroll.json");
        let Some(record) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<EnrollCode>(&bytes).ok())
        else {
            return false;
        };
        let valid = record.hash == hash_token(code) && record.expires >= now_secs();
        if valid {
            let _ = std::fs::remove_file(path);
        }
        valid
    }

    // sessions

    pub fn is_logged_in(&self, headers: &HeaderMap) -> bool {
        cookie_value(headers, COOKIE)
            .is_some_and(|value| self.sessions.lock().expect("sessions").contains(&value))
    }

    fn open_session(&self) -> String {
        let session = secret_token();
        self.sessions.lock().expect("sessions").insert(session.clone());
        let secure = if self.secure_cookie { "; Secure" } else { "" };
        format!("{COOKIE}={session}; HttpOnly; SameSite=Strict; Path=/{secure}")
    }

    pub fn close_session(&self, headers: &HeaderMap) {
        if let Some(value) = cookie_value(headers, COOKIE) {
            self.sessions.lock().expect("sessions").remove(&value);
        }
    }

    // passkey ceremonies

    pub fn start_register(
        &self,
        headers: &HeaderMap,
        code: &str,
        existing: &[Passkey],
    ) -> Result<(String, CreationChallengeResponse)> {
        if !self.is_logged_in(headers) && !self.consume_enroll_code(code) {
            return Err("enrollment code is missing, wrong or expired".into());
        }
        let exclude = existing.iter().map(|p| p.cred_id().clone()).collect::<Vec<_>>();
        let (options, state) = self
            .webauthn
            .start_passkey_registration(PASSKEY_USER, "owner", "owner", Some(exclude))
            .map_err(|e| e.to_string())?;
        Ok((self.remember(Challenge::Register(state)), options))
    }

    pub fn finish_register(&self, challenge_id: &str, credential: &RegisterPublicKeyCredential) -> Result<(Passkey, String)> {
        let Some(Challenge::Register(state)) = self.challenges.lock().expect("challenges").remove(challenge_id) else {
            return Err("unknown registration".into());
        };
        let passkey = self
            .webauthn
            .finish_passkey_registration(credential, &state)
            .map_err(|e| e.to_string())?;
        Ok((passkey, self.open_session()))
    }

    pub fn start_login(&self, passkeys: &[Passkey]) -> Result<(String, RequestChallengeResponse)> {
        let (options, state) = self
            .webauthn
            .start_passkey_authentication(passkeys)
            .map_err(|e| e.to_string())?;
        Ok((self.remember(Challenge::Login(state)), options))
    }

    pub fn finish_login(&self, challenge_id: &str, credential: &PublicKeyCredential) -> Result<(AuthenticationResult, String)> {
        let Some(Challenge::Login(state)) = self.challenges.lock().expect("challenges").remove(challenge_id) else {
            return Err("unknown login".into());
        };
        let result = self
            .webauthn
            .finish_passkey_authentication(credential, &state)
            .map_err(|e| e.to_string())?;
        Ok((result, self.open_session()))
    }

    fn remember(&self, challenge: Challenge) -> String {
        let id = secret_token();
        self.challenges.lock().expect("challenges").insert(id.clone(), challenge);
        id
    }
}

pub fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_string)
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| pair.trim().strip_prefix(&format!("{name}=")).map(str::to_string))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

pub const PASSKEY_SCRIPT: &str = r#"
const b64 = {
  toBuf: s => Uint8Array.from(atob(s.replace(/-/g, '+').replace(/_/g, '/')), c => c.charCodeAt(0)).buffer,
  fromBuf: b => btoa(String.fromCharCode(...new Uint8Array(b))).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, ''),
};
async function post(url, body) {
  const r = await fetch(url, {method: 'POST', headers: {'content-type': 'application/json'}, body: JSON.stringify(body)});
  if (!r.ok) throw new Error(await r.text());
  return r.json();
}
async function enroll(code) {
  const {challenge_id, options} = await post('/auth/register/start', {code});
  const pk = options.publicKey;
  pk.challenge = b64.toBuf(pk.challenge);
  pk.user.id = b64.toBuf(pk.user.id);
  (pk.excludeCredentials || []).forEach(c => c.id = b64.toBuf(c.id));
  const cred = await navigator.credentials.create({publicKey: pk});
  await post('/auth/register/finish', {challenge_id, credential: {
    id: cred.id, rawId: b64.fromBuf(cred.rawId), type: cred.type, extensions: {},
    response: {attestationObject: b64.fromBuf(cred.response.attestationObject), clientDataJSON: b64.fromBuf(cred.response.clientDataJSON)},
  }});
  location.href = '/';
}
async function login() {
  const {challenge_id, options} = await post('/auth/login/start', {});
  const pk = options.publicKey;
  pk.challenge = b64.toBuf(pk.challenge);
  (pk.allowCredentials || []).forEach(c => c.id = b64.toBuf(c.id));
  const cred = await navigator.credentials.get({publicKey: pk});
  await post('/auth/login/finish', {challenge_id, credential: {
    id: cred.id, rawId: b64.fromBuf(cred.rawId), type: cred.type, extensions: {},
    response: {
      authenticatorData: b64.fromBuf(cred.response.authenticatorData),
      clientDataJSON: b64.fromBuf(cred.response.clientDataJSON),
      signature: b64.fromBuf(cred.response.signature),
      userHandle: cred.response.userHandle ? b64.fromBuf(cred.response.userHandle) : null,
    },
  }});
  location.href = '/';
}
"#;
