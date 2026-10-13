//! Extraction on the fixture pages (spec §14, §21).
//!
//! `tests/fixtures/extract/` holds pages of ten kinds, each written to the
//! markup of a real family of pages with invented names and generated
//! text, and for each the text a careful reader would copy out of it,
//! marked by hand (`expected.txt`), and everything extraction returns for
//! it (`golden.json`).
//!
//! - The golden files are compared byte for byte. After a change that is
//!   meant to move them, write them again and read the difference:
//!
//!   ```text
//!   just goldens
//!   ```
//!
//!   (`KAGE_UPDATE_GOLDENS=1 cargo test -p kage-extract --test fixtures`).
//! - The main content is held to the marked text: no page may fall under
//!   the precision and recall it has today.
//! - The crawl and the extraction agree on what a reader sees: the text of
//!   the main content is text of the page as the crawl reads it, in the
//!   same order, and two pages are near-duplicates by the one exactly when
//!   they are by the other.
//!
//! `tests/fixtures/extract-heldout/` holds three more pages of three other
//! kinds, written after the rules of the main content were, by somebody
//! who had not seen them. **Nobody tunes on these.** They say what the
//! rules do on a page they were not written for: each has to be read to an
//! F1 of 90 at least, and a rule that is changed to move one of them makes
//! it a page of the first kind, to be replaced here by a new one.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use kage_crawl::simhash::{near_duplicate, simhash};
use kage_extract::{Document, Options, extract};
use url::Url;

fn directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/extract")
}

/// The pages nobody tunes on.
fn held_out() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/extract-heldout")
}

/// A fixture page: its name, its markup and the address it was loaded
/// from.
struct Fixture {
    name: String,
    html: String,
    url: Url,
    expected: String,
    thin: bool,
}

fn fixtures() -> Vec<Fixture> {
    fixtures_in(&directory())
}

fn fixtures_in(directory: &Path) -> Vec<Fixture> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let read = |file: &str| {
                std::fs::read_to_string(directory.join(&name).join(file))
                    .unwrap_or_else(|error| panic!("{name}/{file}: {error}"))
            };
            let about: serde_json::Value = serde_json::from_str(&read("about.json")).unwrap();
            Fixture {
                html: read("page.html"),
                url: Url::parse(about["url"].as_str().unwrap()).unwrap(),
                expected: read("expected.txt"),
                thin: about["thin"].as_bool().unwrap(),
                name,
            }
        })
        .collect()
}

/// How many words two texts share, in order.
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

/// Precision and recall of a text against the marked one, in percent.
fn judge(found: &str, expected: &str) -> (f64, f64) {
    let found: Vec<&str> = found.split_whitespace().collect();
    let expected: Vec<&str> = expected.split_whitespace().collect();
    if found.is_empty() && expected.is_empty() {
        return (100.0, 100.0);
    }
    #[allow(clippy::cast_precision_loss)] // Counts of words.
    let percent = |part: usize, whole: usize| {
        if whole == 0 {
            0.0
        } else {
            part as f64 * 100.0 / whole as f64
        }
    };
    let both = shared(&found, &expected);
    (percent(both, found.len()), percent(both, expected.len()))
}

#[test]
fn there_are_ten_pages_of_ten_kinds() {
    let names: Vec<String> = fixtures().into_iter().map(|fixture| fixture.name).collect();
    assert_eq!(
        names,
        [
            "blog-post",
            "docs-page",
            "forum-thread",
            "legacy-table-layout",
            "listing-page",
            "news-article",
            "product-page",
            "recipe-page",
            "reference-article",
            "spa-shell",
        ]
    );
}

#[test]
fn every_page_extracts_to_its_golden_file() {
    let update = std::env::var("KAGE_UPDATE_GOLDENS").is_ok_and(|value| value == "1");
    let mut differ = Vec::new();
    for fixture in fixtures() {
        let document = Document::parse(&fixture.html, &fixture.url);
        let extracted = extract(&document, &Options::default());
        let mut written = serde_json::to_string_pretty(&extracted).unwrap();
        written.push('\n');
        let golden = directory().join(&fixture.name).join("golden.json");
        if update {
            std::fs::write(&golden, &written).unwrap();
            continue;
        }
        let kept = std::fs::read_to_string(&golden).unwrap_or_default();
        if kept != written {
            differ.push(fixture.name);
        }
        // What was written reads back as what was extracted.
        let read: kage_extract::Extract = serde_json::from_str(&written).unwrap();
        assert_eq!(read, extracted);
    }
    assert!(
        differ.is_empty(),
        "extraction no longer gives the golden file of {differ:?}; when that is meant, run \
         `just goldens` and read the difference"
    );
}

#[test]
fn the_main_content_is_the_text_a_reader_would_copy_out() {
    // The lowest precision and recall a page may have, in percent: one
    // point under what it has, so that a rule that costs a page a
    // paragraph fails here and names the page.
    let floors = [
        ("blog-post", 99.0, 98.0),
        ("docs-page", 99.0, 99.0),
        ("forum-thread", 98.0, 97.0),
        ("legacy-table-layout", 97.0, 99.0),
        ("listing-page", 82.0, 94.0),
        ("news-article", 97.0, 99.0),
        ("product-page", 91.0, 99.0),
        ("recipe-page", 93.0, 97.0),
        ("reference-article", 83.0, 98.0),
        ("spa-shell", 100.0, 100.0),
    ];
    for fixture in fixtures() {
        let document = Document::parse(&fixture.html, &fixture.url);
        let extracted = extract(&document, &Options::default());
        let (precision, recall) = judge(&extracted.text, &fixture.expected);
        let (_, least_precision, least_recall) = floors
            .iter()
            .find(|(name, _, _)| *name == fixture.name)
            .unwrap_or_else(|| panic!("{} has no floor", fixture.name));
        assert!(
            precision >= *least_precision && recall >= *least_recall,
            "{}: precision {precision:.1} (at least {least_precision}), recall {recall:.1} (at \
             least {least_recall})",
            fixture.name
        );
        // A page is thin exactly when it gave its reader nothing.
        assert_eq!(extracted.thin_content, fixture.thin, "{}", fixture.name);
        assert_eq!(
            extracted.word_count,
            u64::try_from(extracted.text.split_whitespace().count()).unwrap(),
            "{}",
            fixture.name
        );
        // The markdown says what the text says.
        let markdown = extracted.markdown.unwrap();
        assert_eq!(
            markdown.is_empty(),
            extracted.text.is_empty(),
            "{}",
            fixture.name
        );
    }
}

#[test]
fn pages_nobody_tuned_on_are_read_as_a_reader_would_read_them() {
    let pages = fixtures_in(&held_out());
    let names: Vec<&str> = pages.iter().map(|page| page.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "essay-hashed-classes",
            "government-notice",
            "question-and-answers"
        ]
    );
    for page in &pages {
        let document = Document::parse(&page.html, &page.url);
        let extracted = extract(&document, &Options::default());
        let (precision, recall) = judge(&extracted.text, &page.expected);
        let f1 = 2.0 * precision * recall / (precision + recall);
        assert!(
            f1 >= 90.0,
            "{}: F1 {f1:.1} (precision {precision:.1}, recall {recall:.1}); this page is not \
             tuned on: say so in place of changing a rule for it",
            page.name
        );
        assert_eq!(extracted.thin_content, page.thin, "{}", page.name);
    }
}

/// The characters of a text that are no white space and no zero-width
/// space.
fn inked(text: &str) -> Vec<char> {
    text.chars()
        .filter(|c| !c.is_whitespace() && !matches!(c, '\u{200b}' | '\u{feff}'))
        .collect()
}

#[test]
fn the_extraction_shows_nothing_the_crawl_does_not_see() {
    for fixture in fixtures() {
        let document = Document::parse(&fixture.html, &fixture.url);
        let page = document.page();
        let extracted = extract(&document, &Options::default());
        // Every character of the main content is a character of the
        // page's text as the crawl reads it, in the same order.
        let whole = inked(&page.text);
        let mut at = 0;
        for (position, character) in inked(&extracted.text).into_iter().enumerate() {
            match whole[at..].iter().position(|other| *other == character) {
                Some(found) => at += found + 1,
                None => panic!(
                    "{}: character {position} of the main content ({character:?}) is not in \
                     the page's text after what came before it",
                    fixture.name
                ),
            }
        }
        // And so is every link: one the crawl sees, hidden ones left out.
        let seen: Vec<String> = page
            .links
            .iter()
            .filter(|link| !link.hidden)
            .map(|link| link.url.to_string())
            .collect();
        let listed: Vec<String> = extracted
            .links
            .unwrap()
            .into_iter()
            .map(|link| link.url)
            .collect();
        assert_eq!(listed, seen, "{}", fixture.name);
        assert_eq!(
            extracted.title.unwrap_or_default(),
            page.title,
            "{}",
            fixture.name
        );
    }
}

#[test]
fn two_pages_are_near_duplicates_by_the_extraction_when_they_are_by_the_crawl() {
    let read = |html: &str, url: &Url| {
        let document = Document::parse(html, url);
        let crawl = simhash(&document.page().main_text);
        let extraction = simhash(&extract(&document, &Options::default()).text);
        (crawl, extraction)
    };
    let pages = fixtures();
    let prints: Vec<(String, Option<u64>, Option<u64>)> = pages
        .iter()
        .map(|fixture| {
            let (crawl, extraction) = read(&fixture.html, &fixture.url);
            (fixture.name.clone(), crawl, extraction)
        })
        .collect();
    // No two of the ten are alike, by either reading.
    for (index, (name, crawl, extraction)) in prints.iter().enumerate() {
        for (other, other_crawl, other_extraction) in &prints[index + 1..] {
            let near = |a: &Option<u64>, b: &Option<u64>| match (a, b) {
                (Some(a), Some(b)) => near_duplicate(*a, *b),
                _ => false,
            };
            assert!(!near(crawl, other_crawl), "{name} and {other} by the crawl");
            assert!(
                !near(extraction, other_extraction),
                "{name} and {other} by the extraction"
            );
        }
    }
    // The same article under another address, with one word of its text
    // changed and other furniture around it, is a near-duplicate by both.
    for name in ["news-article", "blog-post", "reference-article"] {
        let fixture = pages.iter().find(|fixture| fixture.name == name).unwrap();
        let word = fixture
            .expected
            .lines()
            .skip(3)
            .flat_map(str::split_whitespace)
            .find(|word| word.len() >= 8 && word.chars().all(char::is_alphabetic))
            .unwrap();
        let changed = fixture
            .html
            .replacen(word, "otherwise", 1)
            .replacen("<body", "<body data-copy=\"1\"", 1)
            .replacen(
                "</body>",
                "<div><p>Printed again elsewhere.</p></div></body>",
                1,
            );
        assert_ne!(changed, fixture.html);
        let (crawl, extraction) = read(&fixture.html, &fixture.url);
        let (copy_crawl, copy_extraction) = read(&changed, &fixture.url);
        assert!(
            near_duplicate(crawl.unwrap(), copy_crawl.unwrap()),
            "{name} and its copy by the crawl"
        );
        assert!(
            near_duplicate(extraction.unwrap(), copy_extraction.unwrap()),
            "{name} and its copy by the extraction"
        );
    }
    // A page that says nothing has no fingerprint by either.
    let shell = prints
        .iter()
        .find(|(name, _, _)| name == "spa-shell")
        .unwrap();
    assert_eq!((shell.1, shell.2), (None, None));
}
