//! The browser session numbers EVERY read from one counter (craftworks-sdk#166).
//!
//! `Session` is `#[wasm_bindgen]` in a cdylib, so nothing native can build one
//! — which is why this reads the SOURCE: the decision (`Loads::take_id`, and
//! `Refresh::ask` taking its id from the caller) is tested natively in
//! `tests/read_ids.rs`, and what that cannot see is whether the session still
//! ASKS for it. A `refresh.ask(1, ...)` here would put back the collision
//! with every test green.

use std::path::Path;

fn session_src() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/session.rs")).expect("web/src/session.rs")
}

#[test]
fn every_refresh_ask_takes_its_id_from_the_loads_counter() {
    let src = session_src();
    let calls: Vec<usize> = src.match_indices("self.refresh.ask(").map(|(i, _)| i).collect();
    assert!(!calls.is_empty(), "the reader found no refresh.ask call, so it checks nothing");
    for i in calls {
        // The id is the first argument, bound on the line(s) just above.
        let before = &src[i.saturating_sub(300)..i];
        let call = &src[i..src[i..].find(')').map(|e| i + e).unwrap_or(src.len())];
        let arg = call.trim_start_matches("self.refresh.ask(").split(',').next().unwrap_or("").trim();
        let bound_from_counter = before.contains(&format!("let {arg} = self.loads.take_id();"));
        assert!(
            arg == "self.loads.take_id()" || bound_from_counter,
            "a refresh is numbered by `{arg}`, not by the session's one counter: {call}"
        );
    }
}

/// THE CONTROL: the check above refuses a constant id.
#[test]
fn control_the_check_refuses_a_constant() {
    let src = "let lo = 1;\n        if let Some(req) = self.refresh.ask(1, domain, &lo, &hi) {";
    let i = src.find("self.refresh.ask(").unwrap();
    let call = &src[i..];
    let arg = call.trim_start_matches("self.refresh.ask(").split(',').next().unwrap().trim();
    assert_eq!(arg, "1");
    assert!(!src[..i].contains(&format!("let {arg} = self.loads.take_id();")));
}
