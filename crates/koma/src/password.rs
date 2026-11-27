//! Passwords: argon2id with its default settings (19 MiB, 2 passes, one
//! lane, the OWASP minimum), stored as a PHC string with its own salt. A
//! hash takes tens of milliseconds and that memory, so it runs on a
//! blocking thread, one sign-in at a time per thread.

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};

/// Shortest password accepted.
pub const MIN_LEN: usize = 10;
/// Longest: argon2 takes any length, but nobody types more.
pub const MAX_LEN: usize = 1024;

/// Why a new password is refused, in words for the form.
pub fn check_new(password: &str, again: &str) -> Result<(), &'static str> {
    if password != again {
        Err("The two passwords are not the same.")
    } else if password.chars().count() < MIN_LEN {
        Err("A password needs at least 10 characters.")
    } else if password.len() > MAX_LEN {
        Err("That password is too long.")
    } else {
        Ok(())
    }
}

/// The PHC string of `password` with a fresh random salt.
pub fn hash(password: &str) -> String {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .expect("argon2 with default settings cannot fail")
}

/// Whether `password` matches the stored `phc`.
pub fn verify(password: &str, phc: &str) -> bool {
    password.len() <= MAX_LEN
        && PasswordHash::new(phc).is_ok_and(|h| {
            Argon2::default()
                .verify_password(password.as_bytes(), &h)
                .is_ok()
        })
}

/// A hash to check against when a name has no account, so that a wrong
/// name takes as long as a wrong password.
pub fn decoy() -> &'static str {
    static DECOY: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| hash("decoy-password"));
    &DECOY
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_and_verifies() {
        let h = hash("starfall moonforge");
        assert!(h.starts_with("$argon2id$"));
        assert!(verify("starfall moonforge", &h));
        assert!(!verify("starfall moonforgE", &h));
        assert!(!verify("starfall moonforge", "not a hash"));
        assert_ne!(h, hash("starfall moonforge"), "each hash has its own salt");
    }

    #[test]
    fn new_passwords() {
        assert!(check_new("0123456789", "0123456789").is_ok());
        assert!(check_new("012345678", "012345678").is_err());
        assert!(check_new("0123456789", "0123456780").is_err());
        assert!(check_new("ééééééééééé", "ééééééééééé").is_ok());
    }
}
