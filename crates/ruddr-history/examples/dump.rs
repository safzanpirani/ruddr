//! Prints the unified diff of the newest session from one provider:
//! `cargo run -p ruddr-history --example dump -- codex`.

use ruddr_history::{Stores, list_sessions, load, unified_diff};

fn main() {
    let want = std::env::args().nth(1).unwrap_or_else(|| "codex".into());
    let sessions = list_sessions(&Stores::discover(), 400);
    let info = sessions
        .iter()
        .find(|s| s.provider.name() == want)
        .expect("no session for that provider");
    print!("{}", unified_diff(&load(info).expect("load")));
}
