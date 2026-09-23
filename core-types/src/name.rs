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

/// A JavaScript module an app carries beside the SDK's `index.js` (its build's
/// `modules` list): one file name, `[A-Za-z0-9_-][A-Za-z0-9_.-]*.js`, so it
/// can never be a path (no `/`, no leading `.`).
pub fn module_ok(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".js") else { return false };
    let mut bytes = stem.bytes();
    let first_ok = bytes.next().is_some_and(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b));
    first_ok && bytes.all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
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
    fn modules() {
        assert!(module_ok("index.js") && module_ok("wrap.js") && module_ok("a.b-c_d.js") && module_ok("_x.js"));
        assert!(!module_ok("") && !module_ok(".js") && !module_ok("../x.js") && !module_ok("a/b.js") && !module_ok(".hidden.js"));
        assert!(!module_ok("x.wasm") && !module_ok("x.js.map") && !module_ok("x y.js"));
    }

    #[test]
    fn domains() {
        assert!(domain_ok("notes.rows", 32) && domain_ok("a", 1));
        assert!(!domain_ok("", 32) && !domain_ok("ab", 1) && !domain_ok("a#b", 32) && !domain_ok("A", 32));
    }
}
