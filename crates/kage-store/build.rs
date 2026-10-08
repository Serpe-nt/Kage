//! Rebuilds the crate when a migration changes: `sqlx::migrate!` embeds the
//! files at compile time, and cargo does not track that directory itself.

// A build script talks to cargo through stdout.
#![allow(clippy::print_stdout)]

fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
