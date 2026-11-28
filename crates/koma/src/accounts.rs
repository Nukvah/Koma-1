//! Accounts: who is asking, and what they may do. `<data>/accounts.db`, a
//! SQLite database of its own, is the only thing `koma serve` writes:
//! accounts, sessions, sign-in links, passkeys, what each account keeps
//! (reading positions, reader settings) and a queue of library-setting
//! changes that the worker applies to the catalogue. The catalogue stays the
//! worker's alone.
//!
//! Secrets are stored only as hashes: a session or sign-in link is a random
//! 32-byte token whose BLAKE3 hash is the key; passwords are argon2id
//! strings ([`crate::password`]).

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

pub use rusqlite::Error;

const SCHEMA_VERSION: i64 = 3;

const SCHEMA: &str = "
CREATE TABLE meta (
    key    TEXT PRIMARY KEY,
    value  BLOB NOT NULL
) WITHOUT ROWID;
CREATE TABLE accounts (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,  -- never reused
    name      TEXT NOT NULL UNIQUE,  -- [a-z0-9._-], also its folder /library/~name
    role      TEXT NOT NULL,         -- 'reader', 'curator' or 'admin' (one admin)
    password  TEXT,                  -- argon2id; NULL until set (a sign-in link, or the proxy)
    created   INTEGER NOT NULL,      -- unix seconds
    seen      INTEGER                -- last request, to the nearest few minutes
);
CREATE TABLE sessions (
    hash      BLOB PRIMARY KEY,      -- BLAKE3 of the cookie's token
    account   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    keep      INTEGER NOT NULL,      -- 1: 'keep me signed in'
    created   INTEGER NOT NULL,
    seen      INTEGER NOT NULL,
    agent     TEXT NOT NULL,         -- 'Firefox on Linux', from the User-Agent
    addr      TEXT NOT NULL          -- where it was last seen from
) WITHOUT ROWID;
CREATE INDEX sessions_account ON sessions(account);
CREATE TABLE links (
    hash      BLOB PRIMARY KEY,      -- BLAKE3 of the link's token
    account   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    expires   INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE passkeys (
    id        BLOB PRIMARY KEY,      -- the credential id
    account   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name      TEXT NOT NULL,
    alg       INTEGER NOT NULL,      -- COSE: -7 ES256, -8 EdDSA
    key       BLOB NOT NULL,         -- SEC1 point (ES256) or 32 bytes (EdDSA)
    counter   INTEGER NOT NULL,
    added     INTEGER NOT NULL,
    used      INTEGER
) WITHOUT ROWID;
CREATE INDEX passkeys_account ON passkeys(account);
CREATE TABLE challenges (
    hash      BLOB PRIMARY KEY,      -- BLAKE3 of the challenge
    kind      TEXT NOT NULL,         -- 'create' or 'get'
    account   INTEGER,               -- who is adding a passkey; NULL to sign in
    expires   INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE failures (
    name      TEXT PRIMARY KEY,      -- a name tried at sign-in
    count     INTEGER NOT NULL,      -- wrong passwords in a row
    last      INTEGER NOT NULL
) WITHOUT ROWID;
-- What an account keeps between devices: reading positions, reader
-- settings (crate::kept says which keys).
CREATE TABLE kept (
    account   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    key       TEXT NOT NULL,
    value     TEXT NOT NULL,
    at        INTEGER NOT NULL,
    PRIMARY KEY (account, key)
) WITHOUT ROWID;
-- Library settings changed in the UI; the worker writes them into the
-- catalogue and remembers the last id it applied.
CREATE TABLE setting_changes (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    folder    TEXT NOT NULL,
    name      TEXT NOT NULL,
    kind      TEXT NOT NULL,
    direction TEXT NOT NULL,         -- 'rtl' or 'ltr'
    colour    TEXT NOT NULL,         -- a name from crate::libraries::COLOURS
    account   INTEGER,
    at        INTEGER NOT NULL
);
";

/// Schema 1 to 2: the queue of changes to the library (organising), which
/// the worker carries out; `koma serve` writes them and their pause or
/// cancel, the worker records how each goes in the catalogue.
const OPS: &str = "CREATE TABLE ops (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    account   INTEGER,
    by        TEXT NOT NULL,          -- the account's name, kept if it is removed
    kind      TEXT NOT NULL,          -- see crate::organise
    payload   TEXT NOT NULL,          -- JSON
    title     TEXT NOT NULL,          -- 'Move 3 books of Starfall to Starfall Zero'
    control   TEXT NOT NULL DEFAULT '',  -- '', 'pause' or 'cancel'
    created   INTEGER NOT NULL
);";

/// Schema 2 to 3: names a curator chose to keep as they are, out of the
/// fix-up queue.
const KEPT_NAMES: &str = "CREATE TABLE kept_names (
    path      TEXT PRIMARY KEY,       -- relative to the library
    account   INTEGER,
    at        INTEGER NOT NULL
) WITHOUT ROWID;";

/// A session kept with 'keep me signed in' lasts this long after it was
/// last used.
pub const KEEP_SECS: i64 = 90 * 24 * 60 * 60;
/// A session without it lasts until the browser closes (its cookie has no
/// expiry), and at most this long after it was last used.
pub const BRIEF_SECS: i64 = 24 * 60 * 60;
/// A sign-in link works once, for this long.
pub const LINK_SECS: i64 = 24 * 60 * 60;
/// A passkey challenge is answered within this long.
pub const CHALLENGE_SECS: i64 = 5 * 60;
/// After this many wrong passwords in a row, a name waits [`WAIT_SECS`]
/// before each next try.
pub const MAX_FAILURES: i64 = 5;
pub const WAIT_SECS: i64 = 60;
/// `seen` is written at most this often per session.
pub const SEEN_EVERY: i64 = 5 * 60;
/// Most keys one account may keep.
pub const MAX_KEPT: i64 = 200_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Reader,
    Curator,
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Reader => "reader",
            Role::Curator => "curator",
            Role::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        Some(match s {
            "reader" => Role::Reader,
            "curator" => Role::Curator,
            "admin" => Role::Admin,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: i64,
    pub name: String,
    pub role: Role,
    /// Has a password (a new account has none until its link is used; one
    /// made by the proxy may never have one).
    pub password: Option<String>,
    pub created: i64,
    pub seen: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub hash: [u8; 32],
    pub keep: bool,
    pub created: i64,
    pub seen: i64,
    pub agent: String,
    pub addr: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Passkey {
    pub id: Vec<u8>,
    pub account: i64,
    pub name: String,
    pub alg: i64,
    pub key: Vec<u8>,
    pub counter: u32,
    pub added: i64,
    pub used: Option<i64>,
}

/// A change to the library, queued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Op {
    pub id: i64,
    pub account: Option<i64>,
    pub by: String,
    pub kind: String,
    pub payload: String,
    pub title: String,
    pub control: String,
    pub created: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingChange {
    pub id: i64,
    pub folder: String,
    pub name: String,
    pub kind: String,
    pub direction: String,
    pub colour: String,
}

/// Why an account could not be made.
#[derive(Debug, PartialEq, Eq)]
pub enum CreateError {
    BadName,
    Taken,
    Db(String),
}

impl From<Error> for CreateError {
    fn from(e: Error) -> Self {
        CreateError::Db(e.to_string())
    }
}

/// An account name: 1 to 32 of `a-z 0-9 . _ -`, starting with a letter or
/// digit. It is also the name of the account's folder, `/library/~name`.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|&c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-')
        })
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// 32 random bytes from the operating system.
pub fn random() -> [u8; 32] {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("the operating system's random source failed");
    b
}

pub fn hash(token: &[u8]) -> [u8; 32] {
    *blake3::hash(token).as_bytes()
}

pub struct Accounts {
    conn: Connection,
}

impl Accounts {
    /// Opens (creating if needed) the accounts database, as `koma serve`
    /// does.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => conn.execute_batch(&format!(
                "BEGIN; {SCHEMA} {OPS} {KEPT_NAMES} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
            ))?,
            1 => conn.execute_batch(&format!(
                "BEGIN; {OPS} {KEPT_NAMES} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
            ))?,
            2 => conn.execute_batch(&format!(
                "BEGIN; {KEPT_NAMES} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
            ))?,
            SCHEMA_VERSION => {}
            _ => return Err(Error::InvalidQuery),
        }
        Ok(Self { conn })
    }

    /// Opens it read-only, as the worker does to read queued changes.
    /// `None` if it does not exist yet.
    pub fn open_read_only(path: &Path) -> Result<Option<Self>, Error> {
        if !path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != SCHEMA_VERSION {
            return Ok(None);
        }
        Ok(Some(Self { conn }))
    }

    /// The key that signs session cookies, made the first time it is asked
    /// for.
    pub fn key(&self) -> Result<[u8; 32], Error> {
        let found: Option<Vec<u8>> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'cookie-key'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(k) = found.and_then(|k| <[u8; 32]>::try_from(k).ok()) {
            return Ok(k);
        }
        let k = random();
        self.conn.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES ('cookie-key', ?1)",
            [&k[..]],
        )?;
        // Another process may have been first: read back what won.
        let k: Vec<u8> =
            self.conn
                .query_row("SELECT value FROM meta WHERE key = 'cookie-key'", [], |r| {
                    r.get(0)
                })?;
        <[u8; 32]>::try_from(k).map_err(|_| Error::InvalidQuery)
    }

    pub fn count(&self) -> Result<i64, Error> {
        self.conn
            .query_row("SELECT count(*) FROM accounts", [], |r| r.get(0))
    }

    /// Makes an account. The first account is the admin, whatever `role`
    /// says; there is never a second admin.
    pub fn create(
        &self,
        name: &str,
        role: Role,
        password: Option<&str>,
    ) -> Result<Account, CreateError> {
        if !valid_name(name) {
            return Err(CreateError::BadName);
        }
        let tx = self.conn.unchecked_transaction()?;
        let first: i64 = tx.query_row("SELECT count(*) = 0 FROM accounts", [], |r| r.get(0))?;
        let role = if first == 1 {
            Role::Admin
        } else if role == Role::Admin {
            Role::Curator
        } else {
            role
        };
        let made = tx.execute(
            "INSERT OR IGNORE INTO accounts (name, role, password, created) VALUES (?1, ?2, ?3, ?4)",
            params![name, role.as_str(), password, now()],
        )?;
        if made == 0 {
            return Err(CreateError::Taken);
        }
        tx.commit()?;
        self.by_name(name)?
            .ok_or_else(|| CreateError::Db("account vanished".into()))
    }

    fn account(r: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
        let role: String = r.get(2)?;
        Ok(Account {
            id: r.get(0)?,
            name: r.get(1)?,
            role: Role::parse(&role).unwrap_or(Role::Reader),
            password: r.get(3)?,
            created: r.get(4)?,
            seen: r.get(5)?,
        })
    }

    pub fn by_name(&self, name: &str) -> Result<Option<Account>, Error> {
        self.conn
            .query_row(
                "SELECT id, name, role, password, created, seen FROM accounts WHERE name = ?1",
                [name],
                Self::account,
            )
            .optional()
    }

    pub fn by_id(&self, id: i64) -> Result<Option<Account>, Error> {
        self.conn
            .query_row(
                "SELECT id, name, role, password, created, seen FROM accounts WHERE id = ?1",
                [id],
                Self::account,
            )
            .optional()
    }

    /// Every account, the admin first, then by name.
    pub fn all(&self) -> Result<Vec<Account>, Error> {
        let mut st = self.conn.prepare(
            "SELECT id, name, role, password, created, seen FROM accounts \
             ORDER BY role <> 'admin', name",
        )?;
        st.query_map([], Self::account)?.collect()
    }

    /// Sets the password and signs out every session but `keep`.
    pub fn set_password(
        &self,
        account: i64,
        password: &str,
        keep: Option<&[u8; 32]>,
    ) -> Result<(), Error> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE accounts SET password = ?2 WHERE id = ?1",
            params![account, password],
        )?;
        tx.execute(
            "DELETE FROM sessions WHERE account = ?1 AND hash IS NOT ?2",
            params![account, keep.map(|k| &k[..])],
        )?;
        tx.execute("DELETE FROM links WHERE account = ?1", [account])?;
        tx.commit()
    }

    /// Sets a reader's or curator's role. The admin's role is changed only
    /// by handing it over. Returns whether it changed.
    pub fn set_role(&self, account: i64, role: Role) -> Result<bool, Error> {
        if role == Role::Admin {
            return Ok(false);
        }
        Ok(self.conn.execute(
            "UPDATE accounts SET role = ?2 WHERE id = ?1 AND role <> 'admin'",
            params![account, role.as_str()],
        )? == 1)
    }

    /// Makes `to` the admin and the admin `from` a curator.
    pub fn hand_over(&self, from: i64, to: i64) -> Result<bool, Error> {
        let tx = self.conn.unchecked_transaction()?;
        let ok = tx.execute(
            "UPDATE accounts SET role = 'curator' WHERE id = ?1 AND role = 'admin'",
            [from],
        )? == 1
            && tx.execute(
                "UPDATE accounts SET role = 'admin' WHERE id = ?1 AND role <> 'admin'",
                [to],
            )? == 1;
        if ok {
            tx.commit()?;
        }
        Ok(ok)
    }

    /// Removes an account with its sessions, links, passkeys and what it
    /// kept. The admin cannot be removed. Its folder stays on disk.
    pub fn remove(&self, account: i64) -> Result<bool, Error> {
        Ok(self.conn.execute(
            "DELETE FROM accounts WHERE id = ?1 AND role <> 'admin'",
            [account],
        )? == 1)
    }

    // Sessions.

    /// A new session; returns its token (the cookie holds it, signed).
    pub fn new_session(
        &self,
        account: i64,
        keep: bool,
        agent: &str,
        addr: &str,
    ) -> Result<[u8; 32], Error> {
        let token = random();
        let t = now();
        self.conn.execute(
            "INSERT INTO sessions (hash, account, keep, created, seen, agent, addr) \
             VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6)",
            params![&hash(&token)[..], account, keep, t, agent, addr],
        )?;
        self.touch_account(account, t)?;
        Ok(token)
    }

    /// The account and session a token belongs to, if it has not expired.
    /// Notes when and from where it was seen (at most every few minutes).
    pub fn session(
        &self,
        token: &[u8; 32],
        addr: &str,
    ) -> Result<Option<(Account, Session)>, Error> {
        let h = hash(token);
        let found = self
            .conn
            .query_row(
                "SELECT a.id, a.name, a.role, a.password, a.created, a.seen, \
                 s.keep, s.created, s.seen, s.agent, s.addr \
                 FROM sessions s JOIN accounts a ON a.id = s.account WHERE s.hash = ?1",
                [&h[..]],
                |r| {
                    Ok((
                        Self::account(r)?,
                        Session {
                            hash: h,
                            keep: r.get(6)?,
                            created: r.get(7)?,
                            seen: r.get(8)?,
                            agent: r.get(9)?,
                            addr: r.get(10)?,
                        },
                    ))
                },
            )
            .optional()?;
        let Some((account, session)) = found else {
            return Ok(None);
        };
        let t = now();
        let life = if session.keep { KEEP_SECS } else { BRIEF_SECS };
        if t - session.seen > life {
            self.conn
                .execute("DELETE FROM sessions WHERE hash = ?1", [&h[..]])?;
            return Ok(None);
        }
        if t - session.seen >= SEEN_EVERY || session.addr != addr {
            self.conn.execute(
                "UPDATE sessions SET seen = ?2, addr = ?3 WHERE hash = ?1",
                params![&h[..], t, addr],
            )?;
            self.touch_account(account.id, t)?;
        }
        Ok(Some((account, session)))
    }

    /// Notes that an account was seen (a proxy sign-in has no session).
    pub fn touch_account(&self, account: i64, t: i64) -> Result<(), Error> {
        self.conn.execute(
            "UPDATE accounts SET seen = ?2 WHERE id = ?1 AND (seen IS NULL OR seen <= ?2 - ?3)",
            params![account, t, SEEN_EVERY],
        )?;
        Ok(())
    }

    /// An account's sessions, the most recently seen first.
    pub fn sessions(&self, account: i64) -> Result<Vec<Session>, Error> {
        let mut st = self.conn.prepare(
            "SELECT hash, keep, created, seen, agent, addr FROM sessions \
             WHERE account = ?1 ORDER BY seen DESC",
        )?;
        st.query_map([account], |r| {
            let h: Vec<u8> = r.get(0)?;
            Ok(Session {
                hash: <[u8; 32]>::try_from(h).unwrap_or_default(),
                keep: r.get(1)?,
                created: r.get(2)?,
                seen: r.get(3)?,
                agent: r.get(4)?,
                addr: r.get(5)?,
            })
        })?
        .collect()
    }

    /// Ends one session of `account`, named by its hash.
    pub fn end_session(&self, account: i64, hash: &[u8; 32]) -> Result<bool, Error> {
        Ok(self.conn.execute(
            "DELETE FROM sessions WHERE account = ?1 AND hash = ?2",
            params![account, &hash[..]],
        )? == 1)
    }

    /// Ends every session of `account` but `keep`.
    pub fn end_other_sessions(&self, account: i64, keep: &[u8; 32]) -> Result<usize, Error> {
        self.conn.execute(
            "DELETE FROM sessions WHERE account = ?1 AND hash <> ?2",
            params![account, &keep[..]],
        )
    }

    // Sign-in links.

    /// A new sign-in link for `account`, replacing any earlier one.
    pub fn new_link(&self, account: i64) -> Result<[u8; 32], Error> {
        let token = random();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM links WHERE account = ?1", [account])?;
        tx.execute(
            "INSERT INTO links (hash, account, expires) VALUES (?1, ?2, ?3)",
            params![&hash(&token)[..], account, now() + LINK_SECS],
        )?;
        tx.commit()?;
        Ok(token)
    }

    /// The account a link is for, if it is still good.
    pub fn link(&self, token: &[u8; 32]) -> Result<Option<Account>, Error> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT account FROM links WHERE hash = ?1 AND expires > ?2",
                params![&hash(token)[..], now()],
                |r| r.get(0),
            )
            .optional()?;
        match id {
            Some(id) => self.by_id(id),
            None => Ok(None),
        }
    }

    /// Whether `account` has a link not used yet.
    pub fn has_link(&self, account: i64) -> Result<bool, Error> {
        self.conn.query_row(
            "SELECT count(*) > 0 FROM links WHERE account = ?1 AND expires > ?2",
            params![account, now()],
            |r| r.get(0),
        )
    }

    // Wrong passwords.

    /// Seconds `name` must still wait before its next try; 0 if none.
    pub fn wait(&self, name: &str) -> Result<i64, Error> {
        let row: Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT count, last FROM failures WHERE name = ?1",
                [name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((count, last)) if count >= MAX_FAILURES => (last + WAIT_SECS - now()).max(0),
            _ => 0,
        })
    }

    pub fn failed(&self, name: &str) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO failures (name, count, last) VALUES (?1, 1, ?2) \
             ON CONFLICT (name) DO UPDATE SET count = count + 1, last = ?2",
            params![name, now()],
        )?;
        Ok(())
    }

    pub fn succeeded(&self, name: &str) -> Result<(), Error> {
        self.conn
            .execute("DELETE FROM failures WHERE name = ?1", [name])?;
        Ok(())
    }

    // Passkeys.

    /// A new challenge for adding a passkey (`account`) or signing in.
    pub fn new_challenge(&self, kind: &str, account: Option<i64>) -> Result<[u8; 32], Error> {
        let c = random();
        let t = now();
        self.conn
            .execute("DELETE FROM challenges WHERE expires < ?1", [t])?;
        self.conn.execute(
            "INSERT INTO challenges (hash, kind, account, expires) VALUES (?1, ?2, ?3, ?4)",
            params![&hash(&c)[..], kind, account, t + CHALLENGE_SECS],
        )?;
        Ok(c)
    }

    /// Uses up a challenge. `Some(account)` if it was good for `kind`.
    pub fn take_challenge(&self, c: &[u8], kind: &str) -> Result<Option<Option<i64>>, Error> {
        let h = hash(c);
        let found: Option<Option<i64>> = self
            .conn
            .query_row(
                "DELETE FROM challenges WHERE hash = ?1 AND kind = ?2 AND expires >= ?3 \
                 RETURNING account",
                params![&h[..], kind, now()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found)
    }

    pub fn add_passkey(&self, p: &Passkey) -> Result<bool, Error> {
        Ok(self.conn.execute(
            "INSERT OR IGNORE INTO passkeys (id, account, name, alg, key, counter, added) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![p.id, p.account, p.name, p.alg, p.key, p.counter, now()],
        )? == 1)
    }

    fn passkey(r: &rusqlite::Row<'_>) -> rusqlite::Result<Passkey> {
        Ok(Passkey {
            id: r.get(0)?,
            account: r.get(1)?,
            name: r.get(2)?,
            alg: r.get(3)?,
            key: r.get(4)?,
            counter: r.get(5)?,
            added: r.get(6)?,
            used: r.get(7)?,
        })
    }

    pub fn passkey_by_id(&self, id: &[u8]) -> Result<Option<Passkey>, Error> {
        self.conn
            .query_row(
                "SELECT id, account, name, alg, key, counter, added, used FROM passkeys WHERE id = ?1",
                [id],
                Self::passkey,
            )
            .optional()
    }

    pub fn passkeys(&self, account: i64) -> Result<Vec<Passkey>, Error> {
        let mut st = self.conn.prepare(
            "SELECT id, account, name, alg, key, counter, added, used FROM passkeys \
             WHERE account = ?1 ORDER BY added",
        )?;
        st.query_map([account], Self::passkey)?.collect()
    }

    pub fn passkey_used(&self, id: &[u8], counter: u32) -> Result<(), Error> {
        self.conn.execute(
            "UPDATE passkeys SET counter = ?2, used = ?3 WHERE id = ?1",
            params![id, counter, now()],
        )?;
        Ok(())
    }

    pub fn rename_passkey(&self, account: i64, id: &[u8], name: &str) -> Result<bool, Error> {
        Ok(self.conn.execute(
            "UPDATE passkeys SET name = ?3 WHERE account = ?1 AND id = ?2",
            params![account, id, name],
        )? == 1)
    }

    pub fn remove_passkey(&self, account: i64, id: &[u8]) -> Result<bool, Error> {
        Ok(self.conn.execute(
            "DELETE FROM passkeys WHERE account = ?1 AND id = ?2",
            params![account, id],
        )? == 1)
    }

    // What an account keeps.

    /// The values `account` keeps for `keys`, those it has.
    pub fn kept(&self, account: i64, keys: &[String]) -> Result<Vec<(String, String)>, Error> {
        let mut st = self
            .conn
            .prepare_cached("SELECT value FROM kept WHERE account = ?1 AND key = ?2")?;
        let mut out = Vec::new();
        for k in keys {
            if let Some(v) = st
                .query_row(params![account, k], |r| r.get::<_, String>(0))
                .optional()?
            {
                out.push((k.clone(), v));
            }
        }
        Ok(out)
    }

    /// Keeps `pairs` for `account`; with `only_new`, only keys it has no
    /// value for (bringing in what a browser kept before accounts).
    /// Returns how many were written. Past [`MAX_KEPT`] keys, new keys are
    /// refused.
    pub fn keep(
        &self,
        account: i64,
        pairs: &[(String, String)],
        only_new: bool,
    ) -> Result<usize, Error> {
        let tx = self.conn.unchecked_transaction()?;
        let mut room: i64 = MAX_KEPT
            - tx.query_row(
                "SELECT count(*) FROM kept WHERE account = ?1",
                [account],
                |r| r.get::<_, i64>(0),
            )?;
        let t = now();
        let mut written = 0;
        {
            let exists = |k: &str| -> Result<bool, Error> {
                tx.query_row(
                    "SELECT count(*) > 0 FROM kept WHERE account = ?1 AND key = ?2",
                    params![account, k],
                    |r| r.get(0),
                )
            };
            for (k, v) in pairs {
                let known = exists(k)?;
                if (known && only_new) || (!known && room <= 0) {
                    continue;
                }
                tx.execute(
                    "INSERT INTO kept (account, key, value, at) VALUES (?1, ?2, ?3, ?4) \
                     ON CONFLICT (account, key) DO UPDATE SET value = ?3, at = ?4",
                    params![account, k, v, t],
                )?;
                if !known {
                    room -= 1;
                }
                written += 1;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    /// How many keys of `account` start with `prefix`.
    pub fn kept_count(&self, account: i64, prefix: &str) -> Result<i64, Error> {
        self.conn.query_row(
            "SELECT count(*) FROM kept WHERE account = ?1 AND substr(key, 1, length(?2)) = ?2",
            params![account, prefix],
            |r| r.get(0),
        )
    }

    /// How many series `account` keeps reader settings for.
    pub fn kept_series(&self, account: i64) -> Result<i64, Error> {
        self.conn.query_row(
            "SELECT count(DISTINCT substr(key, instr(key, ':') + 1)) FROM kept WHERE account = ?1 \
             AND substr(key, 1, instr(key, ':')) IN ('dir:', 'mode:', 'shift:', 'fit:')",
            [account],
            |r| r.get(0),
        )
    }

    /// Forgets `keys` for `account` (marking a book unread).
    pub fn unkeep(&self, account: i64, keys: &[String]) -> Result<(), Error> {
        for k in keys {
            self.conn.execute(
                "DELETE FROM kept WHERE account = ?1 AND key = ?2",
                params![account, k],
            )?;
        }
        Ok(())
    }

    // Changes to the library.

    pub fn queue_op(
        &self,
        account: i64,
        by: &str,
        kind: &str,
        payload: &str,
        title: &str,
    ) -> Result<i64, Error> {
        self.conn.execute(
            "INSERT INTO ops (account, by, kind, payload, title, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![account, by, kind, payload, title, now()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    fn op_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Op> {
        Ok(Op {
            id: r.get(0)?,
            account: r.get(1)?,
            by: r.get(2)?,
            kind: r.get(3)?,
            payload: r.get(4)?,
            title: r.get(5)?,
            control: r.get(6)?,
            created: r.get(7)?,
        })
    }

    pub fn op(&self, id: i64) -> Result<Option<Op>, Error> {
        self.conn
            .query_row(
                "SELECT id, account, by, kind, payload, title, control, created FROM ops WHERE id = ?1",
                [id],
                Self::op_row,
            )
            .optional()
    }

    /// Changes queued after `id`, oldest first.
    pub fn ops_after(&self, id: i64) -> Result<Vec<Op>, Error> {
        let mut st = self.conn.prepare_cached(
            "SELECT id, account, by, kind, payload, title, control, created FROM ops
             WHERE id > ?1 ORDER BY id LIMIT 100",
        )?;
        st.query_map([id], Self::op_row)?.collect()
    }

    /// The most recent changes, newest first.
    pub fn recent_ops(&self, limit: usize) -> Result<Vec<Op>, Error> {
        let mut st = self.conn.prepare_cached(
            "SELECT id, account, by, kind, payload, title, control, created FROM ops
             ORDER BY id DESC LIMIT ?1",
        )?;
        st.query_map([limit as i64], Self::op_row)?.collect()
    }

    /// Asks the worker to pause (`pause`), go on (``) or cancel (`cancel`).
    pub fn set_control(&self, id: i64, control: &str) -> Result<bool, Error> {
        Ok(self.conn.execute(
            "UPDATE ops SET control = ?2 WHERE id = ?1",
            params![id, control],
        )? == 1)
    }

    /// Days the trash keeps things; 0: until emptied. 30 unless set.
    pub fn trash_days(&self) -> Result<i64, Error> {
        let v: Option<Vec<u8>> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'trash-days'", [], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(v.and_then(|v| String::from_utf8(v).ok()?.parse().ok())
            .unwrap_or(30))
    }

    pub fn set_trash_days(&self, days: i64) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES ('trash-days', ?1)
             ON CONFLICT (key) DO UPDATE SET value = ?1",
            [days.to_string().into_bytes()],
        )?;
        Ok(())
    }

    /// Names kept as they are, out of the fix-up queue.
    pub fn kept_names(&self) -> Result<Vec<String>, Error> {
        let mut st = self.conn.prepare_cached("SELECT path FROM kept_names")?;
        st.query_map([], |r| r.get(0))?.collect()
    }

    pub fn keep_name(&self, account: i64, path: &str) -> Result<(), Error> {
        self.conn.execute(
            "INSERT OR IGNORE INTO kept_names (path, account, at) VALUES (?1, ?2, ?3)",
            params![path, account, now()],
        )?;
        Ok(())
    }

    // Library settings.

    pub fn queue_change(&self, c: &SettingChange, account: i64) -> Result<i64, Error> {
        self.conn.execute(
            "INSERT INTO setting_changes (folder, name, kind, direction, colour, account, at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                c.folder,
                c.name,
                c.kind,
                c.direction,
                c.colour,
                account,
                now()
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Changes queued after `id`, oldest first.
    pub fn changes_after(&self, id: i64) -> Result<Vec<SettingChange>, Error> {
        let mut st = self.conn.prepare(
            "SELECT id, folder, name, kind, direction, colour FROM setting_changes \
             WHERE id > ?1 ORDER BY id",
        )?;
        st.query_map([id], |r| {
            Ok(SettingChange {
                id: r.get(0)?,
                folder: r.get(1)?,
                name: r.get(2)?,
                kind: r.get(3)?,
                direction: r.get(4)?,
                colour: r.get(5)?,
            })
        })?
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (Accounts, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "koma-accounts-{}-{}",
            std::process::id(),
            u64::from_le_bytes(random()[..8].try_into().unwrap())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (Accounts::open(&dir.join("accounts.db")).unwrap(), dir)
    }

    #[test]
    fn names() {
        for ok in ["mirelle", "a", "bram.2", "x_y-z", "0day"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "",
            "Mirelle",
            ".hidden",
            "-x",
            "a b",
            "a/b",
            "~a",
            &"a".repeat(33),
            "é",
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn the_first_account_is_the_only_admin() {
        let (a, dir) = db();
        let first = a.create("mirelle", Role::Reader, None).unwrap();
        assert_eq!(first.role, Role::Admin);
        let second = a.create("tamsin", Role::Admin, None).unwrap();
        assert_eq!(second.role, Role::Curator);
        assert_eq!(
            a.create("tamsin", Role::Reader, None),
            Err(CreateError::Taken)
        );
        assert_eq!(
            a.create("Nope", Role::Reader, None),
            Err(CreateError::BadName)
        );
        assert!(!a.set_role(first.id, Role::Reader).unwrap());
        assert!(!a.remove(first.id).unwrap());
        assert!(a.hand_over(first.id, second.id).unwrap());
        assert_eq!(a.by_id(first.id).unwrap().unwrap().role, Role::Curator);
        assert_eq!(a.by_id(second.id).unwrap().unwrap().role, Role::Admin);
        assert!(!a.hand_over(first.id, second.id).unwrap());
        assert!(a.remove(first.id).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sessions_and_links() {
        let (a, dir) = db();
        let m = a.create("mirelle", Role::Reader, Some("x")).unwrap();
        let t1 = a
            .new_session(m.id, true, "Firefox on Linux", "10.0.0.1")
            .unwrap();
        let t2 = a
            .new_session(m.id, false, "Safari on iPhone", "10.0.0.2")
            .unwrap();
        assert_eq!(
            a.session(&t1, "10.0.0.1").unwrap().unwrap().0.name,
            "mirelle"
        );
        assert!(a.session(&[7; 32], "x").unwrap().is_none());
        assert_eq!(a.sessions(m.id).unwrap().len(), 2);
        // A new password signs out the other devices.
        a.set_password(m.id, "y", Some(&hash(&t1))).unwrap();
        assert!(a.session(&t2, "x").unwrap().is_none());
        assert!(a.session(&t1, "x").unwrap().is_some());

        let link = a.new_link(m.id).unwrap();
        assert_eq!(a.link(&link).unwrap().unwrap().id, m.id);
        let again = a.new_link(m.id).unwrap();
        assert!(
            a.link(&link).unwrap().is_none(),
            "a new link replaces the old"
        );
        a.set_password(m.id, "z", None).unwrap();
        assert!(a.link(&again).unwrap().is_none(), "a link works once");
        assert!(a.session(&t1, "x").unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn wrong_passwords_wait() {
        let (a, dir) = db();
        for _ in 0..MAX_FAILURES - 1 {
            a.failed("mirelle").unwrap();
        }
        assert_eq!(a.wait("mirelle").unwrap(), 0);
        a.failed("mirelle").unwrap();
        assert!(a.wait("mirelle").unwrap() > 0);
        a.succeeded("mirelle").unwrap();
        assert_eq!(a.wait("mirelle").unwrap(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn kept_values() {
        let (a, dir) = db();
        let m = a.create("mirelle", Role::Reader, None).unwrap();
        let kv = |k: &str, v: &str| (k.to_owned(), v.to_owned());
        a.keep(m.id, &[kv("pos:1", "4"), kv("seen", "1")], false)
            .unwrap();
        // Bringing in a browser's old values never overwrites.
        assert_eq!(
            a.keep(m.id, &[kv("pos:1", "9"), kv("pos:2", "3")], true)
                .unwrap(),
            1
        );
        let got = a
            .kept(m.id, &["pos:1".into(), "pos:2".into(), "pos:3".into()])
            .unwrap();
        assert_eq!(got, vec![kv("pos:1", "4"), kv("pos:2", "3")]);
        assert_eq!(a.kept_count(m.id, "pos:").unwrap(), 2);
        a.keep(
            m.id,
            &[
                kv("dir:5", "rtl"),
                kv("fit:5", "width"),
                kv("mode:6", "strip"),
            ],
            false,
        )
        .unwrap();
        assert_eq!(a.kept_series(m.id).unwrap(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn challenges_are_used_once() {
        let (a, dir) = db();
        let c = a.new_challenge("get", None).unwrap();
        assert_eq!(a.take_challenge(&c, "create").unwrap(), None);
        assert_eq!(a.take_challenge(&c, "get").unwrap(), Some(None));
        assert_eq!(a.take_challenge(&c, "get").unwrap(), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
