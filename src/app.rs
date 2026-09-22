//! THE APP NAMESPACE (the forest ruling): a person has ONE tree, divided by
//! app. Every name an app uses — a domain, or a watch key (`domain` or
//! `domain#parent`) — is RELATIVE to it and stored as `<app>.<name>`, so an app
//! has no name for another app's data and cannot write it. Another app's data
//! is READ through the absolute `@app/name` form (`db.other(app)`); it is
//! never written. `.` separates the app from the name: an app id has none.
//!
//! Pure functions, so the rules are tested natively (tests/app_namespace.rs);
//! the web Session applies them at every name that crosses into it.

use crate::db::DbError;

/// Is `app` an app id: 1–32 of a-z 0-9 _ - (no `.`, no `@`, no `/`).
pub fn check(app: &str) -> Result<(), DbError> {
    let ok = (1..=32).contains(&app.len()) && app.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b));
    if ok { Ok(()) } else { Err(DbError::Refused(format!("app id `{app}` must be 1–32 of a-z 0-9 _ -"))) }
}

/// A name this app READS, as the tree stores it: its own `<app>.name`, another
/// app's through `@other/name`, or — with no app — the name as given (data
/// from before apps, readable, never written).
pub fn read(app: Option<&str>, name: &str) -> Result<String, DbError> {
    if let Some(abs) = name.strip_prefix('@') {
        let (other, rest) = abs.split_once('/').ok_or_else(|| DbError::Refused(format!("`{name}` is not `@app/name`")))?;
        check(other)?;
        return Ok(format!("{other}.{rest}"));
    }
    Ok(match app {
        Some(a) => format!("{a}.{name}"),
        None => name.to_string(),
    })
}

/// A name this app WRITES: only its own. Another app's (`@other/…`) is refused
/// by name, and so is every write by a session with no app.
pub fn write(app: Option<&str>, name: &str) -> Result<String, DbError> {
    if name.starts_with('@') {
        return Err(DbError::Refused(format!("`{name}` is another app's: an app writes only its own")));
    }
    match app {
        Some(a) => Ok(format!("{a}.{name}")),
        None => Err(DbError::Refused(
            "no app: open the session with { app } before writing — a write with no app would land outside every app's space".into(),
        )),
    }
}

/// A stored name back to what this app calls it; `None` for another app's.
pub fn own(app: Option<&str>, stored: &str) -> Option<String> {
    match app {
        Some(a) => stored.strip_prefix(a).and_then(|r| r.strip_prefix('.')).map(str::to_string),
        None => Some(stored.to_string()),
    }
}
