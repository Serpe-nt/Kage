//! Embeds the fingerprint fixtures of the http path into the crate.
//!
//! `tests/fixtures/fingerprints/<profile>-<os>.json` holds what each mapped
//! emulation profile must look like on the wire. The self-test compares
//! against these values at run time (`kage selftest http`), in an image
//! that has no repository, so they are compiled in. The fixtures of browser
//! captures live in a subdirectory and are read by tests only.

// A build script talks to cargo through stdout.
#![allow(clippy::print_stdout)]

use std::error::Error;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{env, fs};

fn main() -> Result<(), Box<dyn Error>> {
    let fixtures =
        Path::new(&env::var("CARGO_MANIFEST_DIR")?).join("../../tests/fixtures/fingerprints");
    println!("cargo:rerun-if-changed={}", fixtures.display());

    let mut files: Vec<PathBuf> = Vec::new();
    // Without the directory (a build from the crate alone) nothing is
    // embedded and the self-test reports that it has nothing to compare
    // with.
    if let Ok(entries) = fs::read_dir(&fixtures) {
        for entry in entries {
            let path = entry?.path();
            if path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "json")
            {
                files.push(path.canonicalize()?);
            }
        }
    }
    files.sort();

    let mut code = String::from(
        "/// The fingerprint fixtures: file stem and content.\n\
         pub(crate) const FIXTURES: &[(&str, &str)] = &[\n",
    );
    for path in &files {
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or("a fixture file name is not UTF-8")?;
        let path = path.to_str().ok_or("a fixture path is not UTF-8")?;
        writeln!(code, "    ({stem:?}, include_str!({path:?})),")?;
    }
    code.push_str("];\n");
    fs::write(
        Path::new(&env::var("OUT_DIR")?).join("fingerprints.rs"),
        code,
    )?;
    Ok(())
}
