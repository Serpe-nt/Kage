//! Measures the main content against the fixtures (spec §14).
//!
//! ```text
//! cargo run --package kage-extract --example main_content
//! ```
//!
//! Every fixture under `tests/fixtures/extract/` is a page with the text a
//! careful reader would copy out of it, marked by hand (`expected.txt`).
//! For each page this prints how much of what was found is that text
//! (precision), how much of that text was found (recall), and their
//! harmonic mean (F1), word for word in order, for two readings of the
//! page:
//!
//! - `furniture`: the page reader's main text (`kage_crawl::html`), which
//!   leaves out navigation, headers, footers and side bars where the markup
//!   names them;
//! - `content`: this crate's main content.
//!
//! A page given as an argument is read instead, and its main content is
//! printed as text, or as markdown with `--markdown`. `--diff <fixture>`
//! prints the lines marked in a fixture that were not found, and the
//! blocks found that were not marked.

// An example that reads the repository's own fixtures: a file that is not
// there is a mistake in the checkout.
#![allow(clippy::unwrap_used, clippy::print_stdout)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use kage_extract::{Document, main_content};
use url::Url;

/// How many words two texts share, in order: the length of their longest
/// common subsequence.
fn shared(a: &[&str], b: &[&str]) -> usize {
    let mut previous = vec![0_usize; b.len() + 1];
    let mut current = vec![0_usize; b.len() + 1];
    for word in a {
        for (index, other) in b.iter().enumerate() {
            current[index + 1] = if word == other {
                previous[index] + 1
            } else {
                current[index].max(previous[index + 1])
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

/// Precision, recall and F1 in percent, to one decimal.
fn judge(found: &str, expected: &str) -> (f64, f64, f64) {
    let found: Vec<&str> = found.split_whitespace().collect();
    let expected: Vec<&str> = expected.split_whitespace().collect();
    if found.is_empty() && expected.is_empty() {
        return (100.0, 100.0, 100.0);
    }
    #[allow(clippy::cast_precision_loss)] // Counts of words.
    let ratio = |part: usize, whole: usize| {
        if whole == 0 {
            0.0
        } else {
            part as f64 / whole as f64
        }
    };
    let both = shared(&found, &expected);
    let (precision, recall) = (ratio(both, found.len()), ratio(both, expected.len()));
    let f1 = if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    };
    (precision * 100.0, recall * 100.0, f1 * 100.0)
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/extract")
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let first = arguments.next();
    if first.as_deref() == Some("--diff") {
        let directory = fixtures().join(arguments.next().unwrap());
        let html = std::fs::read_to_string(directory.join("page.html")).unwrap();
        let expected = std::fs::read_to_string(directory.join("expected.txt")).unwrap();
        let document = Document::parse(&html, &Url::parse("https://page.example/").unwrap());
        let found = main_content(&document).text;
        let squeeze = |line: &str| line.split_whitespace().collect::<Vec<_>>().join(" ");
        let found_lines: Vec<String> = found
            .lines()
            .map(squeeze)
            .filter(|line| !line.is_empty())
            .collect();
        let marked: Vec<String> = expected
            .lines()
            .map(squeeze)
            .filter(|line| !line.is_empty())
            .collect();
        let all_found = found_lines.join(" ");
        let all_marked = marked.join(" ");
        println!("== marked and not found");
        for line in marked
            .iter()
            .filter(|line| !all_found.contains(line.as_str()))
        {
            println!("- {}", line.chars().take(110).collect::<String>());
        }
        println!("== found and not marked");
        for line in found_lines
            .iter()
            .filter(|line| !all_marked.contains(line.as_str()))
        {
            println!("+ {}", line.chars().take(110).collect::<String>());
        }
        return;
    }
    if let Some(page) = first {
        let html = std::fs::read_to_string(&page).unwrap();
        let document = Document::parse(&html, &Url::parse("https://page.example/").unwrap());
        let content = main_content(&document);
        if arguments.next().as_deref() == Some("--markdown") {
            println!("{}", content.markdown);
        } else {
            println!("{}", content.text);
        }
        return;
    }
    let mut names: Vec<String> = std::fs::read_dir(fixtures())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            ["page.html", "expected.txt", "about.json"]
                .iter()
                .all(|file| entry.path().join(file).is_file())
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    println!(
        "page | words marked | furniture: words, precision, recall, F1 | content: words, precision, recall, F1"
    );
    let mut sums = [0.0_f64; 2];
    for name in &names {
        let directory = fixtures().join(name);
        let html = std::fs::read_to_string(directory.join("page.html")).unwrap();
        let expected = std::fs::read_to_string(directory.join("expected.txt")).unwrap();
        let about: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(directory.join("about.json")).unwrap())
                .unwrap();
        let url = Url::parse(about["url"].as_str().unwrap()).unwrap();
        let document = Document::parse(&html, &url);
        let furniture = document.page().main_text;
        let content = main_content(&document).text;
        let mut row = format!("{name} | {}", expected.split_whitespace().count());
        for (index, found) in [&furniture, &content].into_iter().enumerate() {
            let (precision, recall, f1) = judge(found, &expected);
            sums[index] += f1;
            write!(
                row,
                " | {}, {precision:.1}, {recall:.1}, {f1:.1}",
                found.split_whitespace().count()
            )
            .unwrap();
        }
        println!("{row}");
    }
    #[allow(clippy::cast_precision_loss)] // A handful of pages.
    let pages = names.len().max(1) as f64;
    println!(
        "mean F1 over {} pages | | furniture {:.1} | content {:.1}",
        names.len(),
        sums[0] / pages,
        sums[1] / pages
    );
}
