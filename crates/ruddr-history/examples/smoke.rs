//! Lists recent sessions from every store on this machine and summarizes
//! the newest one per provider: `cargo run -p ruddr-history --example smoke`.

use ruddr_history::{Event, Provider, Stores, list_sessions, load, unified_diff};

fn main() {
    let stores = Stores::discover();
    let started = std::time::Instant::now();
    let sessions = list_sessions(&stores, 400);
    println!("listed {} sessions in {:?}", sessions.len(), started.elapsed());
    for provider in Provider::ALL {
        let Some(info) = sessions.iter().find(|s| s.provider == provider) else {
            println!("{:8} none", provider.name());
            continue;
        };
        let started = std::time::Instant::now();
        match load(info) {
            Ok(transcript) => {
                let count = |f: fn(&Event) -> bool| transcript.events.iter().filter(|e| f(e)).count();
                let diff = unified_diff(&transcript);
                println!(
                    "{:8} {} | {:.50} | users {} assistant {} tools {} changes {} | diff {} lines | {:?}",
                    provider.name(),
                    &info.id[..info.id.len().min(12)],
                    info.title,
                    count(|e| matches!(e, Event::User { .. })),
                    count(|e| matches!(e, Event::Assistant { .. })),
                    count(|e| matches!(e, Event::ToolCall { .. })),
                    count(|e| matches!(e, Event::FileChange { .. })),
                    diff.lines().count(),
                    started.elapsed()
                );
            }
            Err(error) => println!("{:8} error: {error}", provider.name()),
        }
    }
}
