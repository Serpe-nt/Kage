//! Measures how far apart the pages of a directory are for the crawler's
//! duplicate detection (spec §13): for every pair of `.html` files, the
//! bits between their fingerprints, once for the main text and once for the
//! whole text, and whether their content hashes are equal.
//!
//! ```text
//! cargo run --package kage-crawl --example near_duplicates -- <directory>
//! ```
//!
//! It is how the threshold `kage_crawl::simhash::NEAR_DUPLICATE_BITS` was
//! chosen, and how to check it against the pages of a site of one's own.

use std::io::Write as _;
use std::path::PathBuf;

use kage_crawl::html;
use kage_crawl::simhash::{content_hash, distance, simhash, words};
use url::Url;

struct Read {
    name: String,
    words: usize,
    main_words: usize,
    whole: Option<u64>,
    main: Option<u64>,
    hash: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("usage: near_duplicates <directory of .html files>")?,
    );
    let base = Url::parse("http://pages.invalid/")?;
    let mut pages = Vec::new();
    let mut names: Vec<PathBuf> = std::fs::read_dir(&directory)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "html")
        })
        .collect();
    names.sort();
    for path in names {
        let text = String::from_utf8_lossy(&std::fs::read(&path)?).into_owned();
        let page = html::read(&text, &base);
        pages.push(Read {
            name: path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default(),
            words: words(&page.text).len(),
            main_words: words(&page.main_text).len(),
            whole: simhash(&page.text),
            main: simhash(&page.main_text),
            hash: content_hash(&page.main_text),
        });
    }

    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "page | words | main words | fingerprint of the main text"
    )?;
    for page in &pages {
        writeln!(
            out,
            "{} | {} | {} | {}",
            page.name,
            page.words,
            page.main_words,
            page.main.map_or_else(
                || "none (too short)".to_owned(),
                |bits| format!("{bits:016x}")
            )
        )?;
    }

    let mut pairs = Vec::new();
    for (index, a) in pages.iter().enumerate() {
        for b in &pages[index + 1..] {
            let main = a.main.zip(b.main).map(|(a, b)| distance(a, b));
            let whole = a.whole.zip(b.whole).map(|(a, b)| distance(a, b));
            pairs.push((main, whole, a.hash == b.hash, &a.name, &b.name));
        }
    }
    pairs.sort();
    writeln!(out)?;
    writeln!(
        out,
        "bits apart (main text) | (whole text) | same content hash | pages"
    )?;
    let show = |bits: Option<u32>| bits.map_or_else(|| "-".to_owned(), |bits| bits.to_string());
    for (main, whole, same, a, b) in &pairs {
        writeln!(
            out,
            "{} | {} | {} | {a} / {b}",
            show(*main),
            show(*whole),
            same
        )?;
    }
    Ok(())
}
