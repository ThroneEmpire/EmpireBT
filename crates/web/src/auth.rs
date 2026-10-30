//! Who may use the web UI: the password, API tokens, sessions, one-time
//! login links, and the limits on guessing.
//!
//! Secrets are never stored as they are: the password as an Argon2id hash,
//! API tokens and session tokens as SHA-256 hashes. The file holding the
//! password and token hashes is readable by its owner only.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const HEADER: &str = "EmpireBT web auth 1";
/// A session unused this long ends.
const SESSION_IDLE: Duration = Duration::from_secs(24 * 60 * 60);
/// No session lasts longer than this.
const SESSION_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Sessions kept at once; the oldest goes first.
const MAX_SESSIONS: usize = 64;
/// How long a one-time login link works.
pub(crate) const LINK_LIFETIME: Duration = Duration::from_secs(10 * 60);
/// Failed logins from one address before it must wait.
const MAX_FAILURES: u32 = 5;
/// How long an address that failed too often waits.
const BAN: Duration = Duration::from_secs(5 * 60);

pub(crate) type Hash = [u8; 32];

/// The saved password hash and API tokens.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AuthFile {
    /// An Argon2id hash in PHC string form.
    pub password: Option<String>,
    /// Named API tokens, by hash.
    pub tokens: Vec<(String, Hash)>,
}

impl AuthFile {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) {
            return Err(format!("{} is not a web auth file", path.display()));
        }
        let mut file = Self::default();
        for line in lines {
            let mut fields = line.split('\t');
            match (fields.next(), fields.next(), fields.next()) {
                (Some("password"), Some(hash), None) => file.password = Some(hash.to_owned()),
                (Some("token"), Some(name), Some(hash)) => {
                    if let Some(hash) = unhex(hash) {
                        file.tokens.push((name.to_owned(), hash));
                    }
                }
                _ => {}
            }
        }
        Ok(file)
    }

    /// Writes the file atomically, readable by its owner only.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut text = format!("{HEADER}\n");
        if let Some(password) = &self.password {
            text.push_str(&format!("password\t{password}\n"));
        }
        for (name, hash) in &self.tokens {
            text.push_str(&format!("token\t{name}\t{}\n", hex(hash)));
        }
        let temporary = path.with_extension("tmp");
        let _ = std::fs::remove_file(&temporary);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let written = options.open(&temporary).and_then(|mut file| {
            std::io::Write::write_all(&mut file, text.as_bytes())?;
            file.sync_all()
        });
        written
            .and_then(|()| std::fs::rename(&temporary, path))
            .map_err(|error| format!("cannot save {}: {error}", path.display()))
    }

    /// Whether `token` is one of the API tokens.
    pub fn has_token(&self, token: &str) -> bool {
        let hash = token_hash(token);
        self.tokens
            .iter()
            .any(|(_, known)| constant_time_equal(known, &hash))
    }
}

/// An Argon2id hash of `password`, with a random salt.
pub(crate) fn hash_password(password: &str) -> Result<String, String> {
    use argon2::password_hash::{PasswordHasher, phc::PasswordHash};
    PasswordHasher::<PasswordHash>::hash_password_with_salt(
        &argon2::Argon2::default(),
        password.as_bytes(),
        &random_bytes::<16>(),
    )
    .map(|hash| hash.to_string())
    .map_err(|error| error.to_string())
}

/// Whether `password` matches an Argon2 hash, in the PHC string form
/// `hash_password` makes (and earlier versions made). Deliberately slow.
pub(crate) fn verify_password(hash: &str, password: &str) -> bool {
    use argon2::password_hash::PasswordVerifier;
    argon2::Argon2::default()
        .verify_password(password.as_bytes(), hash)
        .is_ok()
}

/// A random 256-bit token, as 64 hex digits.
pub(crate) fn random_token() -> String {
    hex(&random_bytes::<32>())
}

/// A random password that is easy to type: 20 characters from an alphabet
/// without look-alikes, about 115 bits.
pub(crate) fn generate_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyzABCDEFGHJKMNPQRSTUVWXYZ23456789";
    random_bytes::<20>()
        .iter()
        .map(|byte| ALPHABET[usize::from(*byte) % ALPHABET.len()] as char)
        .collect()
}

pub(crate) fn token_hash(token: &str) -> Hash {
    let digest = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
    digest.as_ref().try_into().expect("SHA-256 is 32 bytes")
}

fn random_bytes<const N: usize>() -> [u8; N] {
    use ring::rand::SecureRandom;
    let mut bytes = [0; N];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .expect("the system's random generator works");
    bytes
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0, |differences, (a, b)| differences | (a ^ b))
            == 0
}

#[derive(Debug, Clone, Copy)]
struct Session {
    created: Instant,
    last_seen: Instant,
}

/// Signed-in browsers, by the hash of their cookie token. Kept in memory:
/// restarting signs everyone out.
#[derive(Debug, Default)]
pub(crate) struct Sessions {
    sessions: Mutex<HashMap<Hash, Session>>,
}

impl Sessions {
    /// Starts a session and returns its token.
    pub fn create(&self) -> String {
        let token = random_token();
        let now = Instant::now();
        let mut sessions = lock(&self.sessions);
        expire(&mut sessions, now);
        if sessions.len() >= MAX_SESSIONS
            && let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, session)| session.last_seen)
                .map(|(hash, _)| *hash)
        {
            sessions.remove(&oldest);
        }
        sessions.insert(
            token_hash(&token),
            Session {
                created: now,
                last_seen: now,
            },
        );
        token
    }

    /// Whether `token` is a live session; using it keeps it alive.
    pub fn check(&self, token: &str) -> bool {
        let now = Instant::now();
        let mut sessions = lock(&self.sessions);
        expire(&mut sessions, now);
        match sessions.get_mut(&token_hash(token)) {
            Some(session) => {
                session.last_seen = now;
                true
            }
            None => false,
        }
    }

    pub fn end(&self, token: &str) {
        lock(&self.sessions).remove(&token_hash(token));
    }
}

fn expire(sessions: &mut HashMap<Hash, Session>, now: Instant) {
    sessions.retain(|_, session| {
        now.saturating_duration_since(session.last_seen) < SESSION_IDLE
            && now.saturating_duration_since(session.created) < SESSION_LIFETIME
    });
}

/// One-time login links: each token signs in once, within minutes.
#[derive(Debug, Default)]
pub(crate) struct LoginLinks {
    links: Mutex<HashMap<Hash, Instant>>,
}

impl LoginLinks {
    pub fn create(&self) -> String {
        let token = random_token();
        let now = Instant::now();
        let mut links = lock(&self.links);
        links.retain(|_, expires| *expires > now);
        links.insert(token_hash(&token), now + LINK_LIFETIME);
        token
    }

    /// Uses up a link token; true if it was valid.
    pub fn redeem(&self, token: &str) -> bool {
        let now = Instant::now();
        lock(&self.links)
            .remove(&token_hash(token))
            .is_some_and(|expires| expires > now)
    }
}

/// Limits password guessing: addresses that fail too often wait.
#[derive(Debug, Default)]
pub(crate) struct Guard {
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

impl Guard {
    /// How long `address` must still wait before trying again, if at all.
    pub fn wait(&self, address: IpAddr) -> Option<Duration> {
        let failures = lock(&self.failures);
        let (count, last) = failures.get(&address)?;
        let waited = last.elapsed();
        (*count >= MAX_FAILURES && waited < BAN).then(|| BAN - waited)
    }

    pub fn failed(&self, address: IpAddr) {
        let mut failures = lock(&self.failures);
        if failures.len() > 10_000 {
            failures.retain(|_, (_, last)| last.elapsed() < BAN);
        }
        let entry = failures.entry(address).or_insert((0, Instant::now()));
        // A ban that ran out starts the count again.
        if entry.1.elapsed() >= BAN {
            entry.0 = 0;
        }
        entry.0 += 1;
        entry.1 = Instant::now();
    }

    pub fn succeeded(&self, address: IpAddr) {
        lock(&self.failures).remove(&address);
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Option<Hash> {
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let mut hash = [0; 32];
    for (index, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(hash)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_saved_by_older_versions_still_work() {
        // Made by EmpireBT with argon2 0.5, before the move to 0.6: people's
        // saved web UI passwords must keep working after an update.
        let old = "$argon2id$v=19$m=19456,t=2,p=1$54QHr4uX7R1BqbVs6Al/ZA$\
                   ZxI89aL4n+4alv4TqbZN0czUPrrIHxfPeTY0BgWNnJo";
        assert!(verify_password(old, "correct horse battery staple"));
        assert!(!verify_password(old, "correct horse battery stable"));
        assert!(!verify_password(
            "not a hash",
            "correct horse battery staple"
        ));
        // New hashes have the same form, and their own salt each time.
        let new = hash_password("correct horse battery staple").unwrap();
        assert!(new.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"), "{new}");
        assert!(verify_password(&new, "correct horse battery staple"));
        assert_ne!(new, hash_password("correct horse battery staple").unwrap());
    }

    #[test]
    fn hashes_and_checks_passwords() {
        let hash = hash_password("correct horse").unwrap();
        assert!(hash.starts_with("$argon2id$"), "{hash}");
        assert!(verify_password(&hash, "correct horse"));
        assert!(!verify_password(&hash, "Correct horse"));
        assert!(!verify_password("not a hash", "correct horse"));
        // Salted: the same password hashes differently each time.
        assert_ne!(hash, hash_password("correct horse").unwrap());
    }

    #[test]
    fn generated_secrets_are_random_and_typeable() {
        let password = generate_password();
        assert_eq!(password.len(), 20);
        assert!(password.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(!password.contains(['0', 'O', 'l', '1', 'I']));
        assert_ne!(password, generate_password());
        assert_eq!(random_token().len(), 64);
        assert_ne!(random_token(), random_token());
    }

    #[test]
    fn saves_the_auth_file_privately() {
        let path = std::env::temp_dir().join(format!("empirebt-web-auth-{}", std::process::id()));
        let file = AuthFile {
            password: Some(hash_password("secret").unwrap()),
            tokens: vec![("script".to_owned(), token_hash("abc"))],
        };
        file.save(&path).unwrap();
        let loaded = AuthFile::load(&path).unwrap();
        assert_eq!(loaded, file);
        assert!(loaded.has_token("abc"));
        assert!(!loaded.has_token("abd"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sessions_and_links_work_once_and_end() {
        let sessions = Sessions::default();
        let token = sessions.create();
        assert!(sessions.check(&token));
        assert!(!sessions.check("forged"));
        sessions.end(&token);
        assert!(!sessions.check(&token));

        let links = LoginLinks::default();
        let link = links.create();
        assert!(links.redeem(&link));
        assert!(!links.redeem(&link), "single use");
        assert!(!links.redeem("forged"));
    }

    #[test]
    fn bans_addresses_that_keep_failing() {
        let guard = Guard::default();
        let address: IpAddr = "192.0.2.7".parse().unwrap();
        for _ in 0..MAX_FAILURES - 1 {
            guard.failed(address);
            assert_eq!(guard.wait(address), None);
        }
        guard.failed(address);
        assert!(
            guard
                .wait(address)
                .is_some_and(|wait| wait > BAN - Duration::from_secs(1))
        );
        assert_eq!(
            guard.wait("192.0.2.8".parse().unwrap()),
            None,
            "others are not affected"
        );
        guard.succeeded(address);
        assert_eq!(guard.wait(address), None);
    }
}
