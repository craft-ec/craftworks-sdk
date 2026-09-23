//! Names: the app id rule and the domain name rule, stated once.

/// The longest app id.
pub const MAX_APP: usize = 32;

/// An app id: 1–[`MAX_APP`] of `a-z 0-9 _ -` (no `.`, which separates an app
/// from its names, and no `@` or `/`).
pub fn app_ok(app: &str) -> bool {
    (1..=MAX_APP).contains(&app.len()) && app.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
}

/// A domain name of at most `max` bytes: 1–`max` of `a-z 0-9 _ - .`. The bound
/// differs by where the name sits (as an app writes it, or as stored with its
/// app's prefix); the characters do not.
pub fn domain_ok(name: &str, max: usize) -> bool {
    (1..=max).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-.".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_ids() {
        assert!(app_ok("notes") && app_ok("a_b-9") && app_ok(&"a".repeat(32)));
        assert!(!app_ok("") && !app_ok(&"a".repeat(33)) && !app_ok("a.b") && !app_ok("A") && !app_ok("a/b") && !app_ok("@a"));
    }

    #[test]
    fn domains() {
        assert!(domain_ok("notes.rows", 32) && domain_ok("a", 1));
        assert!(!domain_ok("", 32) && !domain_ok("ab", 1) && !domain_ok("a#b", 32) && !domain_ok("A", 32));
    }
}
