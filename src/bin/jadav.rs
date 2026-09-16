//! `jadav` — a standalone, self-hosted CalDAV server daemon.
//!
//! Thin wrapper over [`jamail::jadav::run`], mirroring `jamaild.rs`: every
//! piece of logic lives in the library crate so it can be unit-tested and
//! reuse the crate's `pub(crate)` helpers.
//!
//! ```text
//! jadav [--config <path>] [serve]
//! jadav [--config <path>] check-config
//! jadav [--config <path>] health
//! jadav [--config <path>] import-caldav <base-url> --calendar <slug> [...]
//! ```

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(err) = jamail::jadav::run(args) {
        eprintln!("jadav: {:?}", err);
        std::process::exit(1);
    }
}
