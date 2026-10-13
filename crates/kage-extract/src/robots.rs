//! `robots.txt`: what a site asks of those who read it by machine, and
//! what it says about one address (spec §14).
//!
//! The file is read by the rules of the parser Google published, so that
//! what counts as a rule here is what counts as one there: a missing
//! colon, `disalow`, a byte order mark, `\r` as the end of a line,
//! percent-escapes in either case, a line cut at 16 kibibytes. The verdict
//! follows Google's matcher and what RFC 9309 took from it:
//!
//! - the rules that apply are those of the groups that name one of our
//!   tokens, and only when no group does, those of the groups for `*`;
//! - of the rules that match an address the longest pattern decides, and
//!   `Allow` wins over a `Disallow` of the same length;
//! - `*` in a pattern stands for any run of characters, `$` at its end for
//!   the end of the address;
//! - an `Allow` for `…/index.html` also allows the directory itself;
//! - an address no rule matches is allowed.
//!
//! Both the parser and the matcher are this module's own. The `robotstxt`
//! crate, a port of Google's library that the workspace pinned for this,
//! is not used on a file a site sends: version 0.3.0 fails on a line
//! longer than its limit that holds characters of more than one byte (an
//! index into the middle of one), does not skip a byte order mark in text
//! that is already decoded, and its matcher answers wrongly where a `*`
//! can stand for more than one run (`/*a/b` does not match `/a/a/b`
//! there). It is a test dependency only: on files it reads rightly, its
//! verdict and this module's are compared as a property, and the three
//! faults are tests of this module.
//!
//! A file is parsed once, the verdict says which rule decided, and
//! `Crawl-delay` and `Sitemap:` are read on the way.
//!
//! Nothing here fetches the file or decides what to do with a verdict: who
//! asks for `robots.txt`, how long an answer is kept and whether a
//! disallowed address is left alone are the pipeline's and the policy's.

use url::{Position, Url};

/// The longest line that is read, in bytes; what a line has beyond is
/// ignored. Google's bound: eight times the longest address a browser
/// takes.
pub const MAX_LINE_BYTES: usize = 2083 * 8;

/// The most rules kept of one file; what comes after is counted and left
/// out. Google reads the first 500 kibibytes of a file, which no real file
/// fills with rules.
pub const MAX_RULES: usize = 20_000;

/// The most `Sitemap:` lines kept of one file.
pub const MAX_SITEMAPS: usize = 1000;

/// The longest crawl delay, in seconds: 366 days. A site that asks for more
/// is taken to ask for this much; the bound is there so that a number a
/// site wrote never overflows a time, not to shorten what it asked for.
pub const MAX_CRAWL_DELAY: f64 = 31_622_400.0;

/// One `Allow` or `Disallow` line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    allow: bool,
    /// The pattern, percent-escapes in upper case.
    pattern: String,
}

impl Rule {
    /// The line as it is reported: `Disallow: /private`.
    fn written(&self) -> String {
        format!(
            "{}: {}",
            if self.allow { "Allow" } else { "Disallow" },
            self.pattern
        )
    }
}

/// The rules for one run of `User-agent` lines.
#[derive(Debug, Clone, PartialEq, Default)]
struct Group {
    /// The tokens the group names, in lower case; `*` for everybody.
    agents: Vec<String>,
    rules: Vec<Rule>,
    crawl_delay: Option<f64>,
}

/// A parsed `robots.txt`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Robots {
    groups: Vec<Group>,
    sitemaps: Vec<String>,
    /// Rules left out because the file had more than [`MAX_RULES`].
    rules_dropped: usize,
}

/// What a `robots.txt` says about one address.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// Whether the address may be fetched.
    pub allowed: bool,
    /// The rule that decided, as `Allow: <pattern>` or `Disallow:
    /// <pattern>`; `None` when no rule matched.
    pub matched_rule: Option<String>,
    /// The `Crawl-delay` of the group that applies, in seconds.
    pub crawl_delay: Option<f64>,
}

/// Builds the groups as the parser reads the file.
#[derive(Default)]
struct Builder {
    robots: Robots,
    /// Something other than a `User-agent` line was read since the last
    /// one (a rule, a sitemap, a line Google does not know): the next
    /// `User-agent` line starts a new group.
    seen_rule: bool,
    rules: usize,
}

/// The token of a `User-agent` line: `*` for the line that names
/// everybody, else its leading letters, hyphens and underscores in lower
/// case (`Googlebot/2.1` is `googlebot`).
fn agent_token(value: &str) -> String {
    let value = value.trim();
    let mut chars = value.chars();
    if chars.next() == Some('*') && chars.next().is_none_or(char::is_whitespace) {
        return "*".to_owned();
    }
    value
        .chars()
        .take_while(|c| c.is_ascii_alphabetic() || matches!(c, '-' | '_'))
        .collect::<String>()
        .to_ascii_lowercase()
}

impl Builder {
    fn rule(&mut self, allow: bool, value: &str) {
        let Some(group) = self.robots.groups.last_mut() else {
            // A rule before any `User-agent` line is nobody's.
            return;
        };
        self.seen_rule = true;
        if self.rules >= MAX_RULES {
            self.robots.rules_dropped += 1;
            return;
        }
        self.rules += 1;
        group.rules.push(Rule {
            allow,
            pattern: value.to_owned(),
        });
    }
}

/// The key and the value of a line: `<key>: <value>`, without its
/// comment. A line without a colon is one when it is exactly two words,
/// which is how Google reads a forgotten colon.
fn key_value(line: &str) -> Option<(&str, &str)> {
    let line = line.split('#').next().unwrap_or_default().trim();
    let (key, value) = if let Some((key, value)) = line.split_once(':') {
        (key.trim(), value.trim())
    } else {
        let (key, value) = line.split_once([' ', '\t'])?;
        let value = value.trim();
        if value.is_empty() || value.contains([' ', '\t']) {
            return None;
        }
        (key, value)
    };
    (!key.is_empty()).then_some((key, value))
}

/// A pattern as it is matched: what is not ASCII percent-encoded, and the
/// percent-escapes it has in upper case.
fn escape_pattern(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%'
            && index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit()
        {
            out.push('%');
            out.push(char::from(bytes[index + 1].to_ascii_uppercase()));
            out.push(char::from(bytes[index + 2].to_ascii_uppercase()));
            index += 3;
        } else if byte >= 0x80 {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
            index += 1;
        } else {
            out.push(char::from(byte));
            index += 1;
        }
    }
    out
}

const HEX: [u8; 16] = *b"0123456789ABCDEF";

impl Builder {
    /// Reads one line of the file.
    fn line(&mut self, line: &str) {
        // What a line has beyond the bound is ignored, cut at a character.
        let mut end = line.len().min(MAX_LINE_BYTES);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        let Some((key, value)) = key_value(&line[..end]) else {
            return;
        };
        // Google's keys, by how they begin, and the misspellings it takes.
        let key = key.to_ascii_lowercase();
        let begins = |names: &[&str]| names.iter().any(|name| key.starts_with(name));
        if begins(&["user-agent", "useragent", "user agent"]) {
            self.user_agent(value);
        } else if begins(&["allow"]) {
            self.rule(true, &escape_pattern(value));
        } else if begins(&[
            "disallow",
            "dissallow",
            "dissalow",
            "disalow",
            "diasllow",
            "disallaw",
        ]) {
            self.rule(false, &escape_pattern(value));
        } else if begins(&["sitemap", "site-map"]) {
            self.sitemap(value);
        } else {
            self.unknown(&key, value);
        }
    }

    fn user_agent(&mut self, user_agent: &str) {
        if self.seen_rule || self.robots.groups.is_empty() {
            self.robots.groups.push(Group::default());
            self.seen_rule = false;
        }
        if let Some(group) = self.robots.groups.last_mut() {
            group.agents.push(agent_token(user_agent));
        }
    }

    fn sitemap(&mut self, value: &str) {
        self.seen_rule = true;
        let value = value.trim();
        if !value.is_empty() && self.robots.sitemaps.len() < MAX_SITEMAPS {
            self.robots.sitemaps.push(value.to_owned());
        }
    }

    /// A line Google does not know. One of them is read: `Crawl-delay`.
    fn unknown(&mut self, action: &str, value: &str) {
        self.seen_rule = true;
        if !matches!(action, "crawl-delay" | "crawldelay" | "crawl delay") {
            return;
        }
        let Some(group) = self.robots.groups.last_mut() else {
            return;
        };
        let delay = value
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|delay| delay.is_finite() && *delay >= 0.0)
            .map(|delay| delay.min(MAX_CRAWL_DELAY));
        // The first one a group states is the group's.
        if group.crawl_delay.is_none() {
            group.crawl_delay = delay;
        }
    }
}

/// Whether a pattern matches a path. Both are percent-encoded text; the
/// pattern is anchored at the start of the path, `*` stands for any run of
/// characters and `$` at the end of the pattern for the end of the path.
/// Linear in the lengths of both: each piece between two `*` is looked for
/// once, at the first place it fits, which decides a pattern that has no
/// other wildcard.
fn matches(path: &str, pattern: &str) -> bool {
    let (pattern, to_the_end) = match pattern.strip_suffix('$') {
        Some(rest) => (rest, true),
        None => (pattern, false),
    };
    let mut pieces = pattern.split('*');
    let Some(mut rest) = pieces.next().and_then(|first| path.strip_prefix(first)) else {
        return false;
    };
    let mut pieces = pieces.peekable();
    if pieces.peek().is_none() {
        // No wildcard: a prefix, or with `$` the whole path.
        return !to_the_end || rest.is_empty();
    }
    while let Some(piece) = pieces.next() {
        if pieces.peek().is_none() && to_the_end {
            // The last piece must end the path.
            return rest.ends_with(piece);
        }
        match rest.find(piece) {
            Some(at) => rest = &rest[at + piece.len()..],
            None => return false,
        }
    }
    true
}

/// The best match of one kind (`Allow` or `Disallow`) among some rules:
/// the length of the longest pattern that matches and its rule. The first
/// of two of the same length stays.
#[derive(Debug, Clone, Copy, Default)]
struct Best<'a> {
    length: usize,
    rule: Option<&'a Rule>,
}

impl<'a> Best<'a> {
    fn offer(&mut self, length: usize, rule: &'a Rule) {
        if self.rule.is_none() || length > self.length {
            self.length = length;
            self.rule = Some(rule);
        }
    }

    /// The rule, when it says something: one with an empty pattern says
    /// nothing.
    fn deciding(self) -> Option<&'a Rule> {
        self.rule.filter(|_| self.length > 0)
    }
}

/// The pattern that an `Allow` for a directory's index page also stands
/// for: the directory itself and nothing longer. Google's rule: a last
/// segment that begins with `index.htm`.
fn index_pattern(pattern: &str) -> Option<String> {
    let slash = pattern.rfind('/')?;
    pattern[slash..]
        .starts_with("/index.htm")
        .then(|| format!("{}$", &pattern[..=slash]))
}

/// The part of an address a rule is matched against: its path and query,
/// percent-escapes in upper case.
fn path_of(url: &Url) -> String {
    let path = &url[Position::BeforePath..Position::AfterQuery];
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    // `%2f` and `%2F` are one character to a rule.
    let bytes = path.as_bytes();
    let mut out = String::with_capacity(path.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = bytes[index] == b'%'
            && index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit();
        if escaped {
            out.push('%');
            out.push(char::from(bytes[index + 1].to_ascii_uppercase()));
            out.push(char::from(bytes[index + 2].to_ascii_uppercase()));
            index += 3;
        } else {
            // Not an escape: the character as it is, whatever its length.
            let end = (index + 1..=bytes.len())
                .find(|end| path.is_char_boundary(*end))
                .unwrap_or(bytes.len());
            out.push_str(&path[index..end]);
            index = end;
        }
    }
    out
}

impl Robots {
    /// Parses a `robots.txt`. Nothing here fails: what is no rule is
    /// skipped, and a file that is none at all has no rules.
    pub fn parse(body: &str) -> Self {
        let mut builder = Builder::default();
        // A byte order mark is no part of the first line.
        let body = body.strip_prefix('\u{feff}').unwrap_or(body);
        // `\n`, `\r` and `\r\n` all end a line; the empty line between the
        // two characters of the last says nothing.
        for line in body.split(['\n', '\r']) {
            builder.line(line);
        }
        builder.robots
    }

    /// The addresses of the `Sitemap:` lines, as written, in order.
    pub fn sitemaps(&self) -> &[String] {
        &self.sitemaps
    }

    /// How many rules the file has.
    pub fn rules(&self) -> usize {
        self.groups.iter().map(|group| group.rules.len()).sum()
    }

    /// Rules that were left out because the file had too many.
    pub const fn rules_dropped(&self) -> usize {
        self.rules_dropped
    }

    /// The groups that apply to somebody who answers to `agents`: those
    /// that name one of them, or when none does, those for everybody.
    fn groups_for(&self, agents: &[String]) -> Vec<&Group> {
        let named = |group: &&Group| {
            group.agents.iter().any(|token| {
                token != "*"
                    && !token.is_empty()
                    && agents.iter().any(|agent| agent.eq_ignore_ascii_case(token))
            })
        };
        let own: Vec<&Group> = self.groups.iter().filter(named).collect();
        if own.is_empty() {
            self.groups
                .iter()
                .filter(|group| group.agents.iter().any(|token| token == "*"))
                .collect()
        } else {
            own
        }
    }

    /// What the file says about an address to somebody who answers to the
    /// product tokens `agents` (`*` need not be among them: the rules for
    /// everybody apply when no group names one of the tokens).
    pub fn decide(&self, agents: &[String], url: &Url) -> Decision {
        let path = path_of(url);
        let groups = self.groups_for(agents);
        let (mut allow, mut disallow) = (Best::default(), Best::default());
        for rule in groups.iter().flat_map(|group| &group.rules) {
            if rule.allow {
                if matches(&path, &rule.pattern) {
                    allow.offer(rule.pattern.len(), rule);
                } else if let Some(index) =
                    index_pattern(&rule.pattern).filter(|index| matches(&path, index))
                {
                    allow.offer(index.len(), rule);
                }
            } else if matches(&path, &rule.pattern) {
                disallow.offer(rule.pattern.len(), rule);
            }
        }
        let decided = match (allow.deciding(), disallow.deciding()) {
            (Some(rule), None) | (None, Some(rule)) => Some(rule),
            (Some(allowing), Some(denying)) => Some(if disallow.length > allow.length {
                denying
            } else {
                allowing
            }),
            (None, None) => None,
        };
        Decision {
            allowed: decided.is_none_or(|rule| rule.allow),
            matched_rule: decided.map(Rule::written),
            crawl_delay: groups.iter().find_map(|group| group.crawl_delay),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use robotstxt::DefaultMatcher;
    use robotstxt::matcher::{LongestMatchRobotsMatchStrategy, RobotsMatchStrategy};
    use url::Url;

    use super::{Decision, MAX_RULES, Robots, agent_token, matches, path_of};

    fn us() -> Vec<String> {
        vec!["kage".to_owned()]
    }

    fn url(path: &str) -> Url {
        Url::parse(&format!("https://site.example{path}")).unwrap()
    }

    fn decide(body: &str, path: &str) -> Decision {
        Robots::parse(body).decide(&us(), &url(path))
    }

    fn allowed(body: &str, path: &str) -> bool {
        decide(body, path).allowed
    }

    #[test]
    fn an_address_no_rule_names_is_allowed() {
        for body in [
            "",
            "# nothing here\n",
            "User-agent: *\n",
            "User-agent: *\nDisallow:\n",
            "not a robots file at all",
        ] {
            let decision = decide(body, "/any/page?x=1");
            assert_eq!(
                decision,
                Decision {
                    allowed: true,
                    matched_rule: None,
                    crawl_delay: None
                },
                "{body:?}"
            );
        }
        // A rule before any group is nobody's.
        assert!(allowed("Disallow: /\nUser-agent: *\nAllow: /\n", "/page"));
    }

    #[test]
    fn disallow_and_allow_decide_by_the_longest_pattern() {
        let body = "User-agent: *\nDisallow: /private\nAllow: /private/open\nDisallow: /private/open/secret\n";
        let robots = Robots::parse(body);
        assert_eq!(robots.rules(), 3);
        for (path, allowed, rule) in [
            ("/", true, None),
            ("/public", true, None),
            ("/private", false, Some("Disallow: /private")),
            ("/private/x", false, Some("Disallow: /private")),
            ("/privateer", false, Some("Disallow: /private")),
            ("/private/open", true, Some("Allow: /private/open")),
            ("/private/open/page", true, Some("Allow: /private/open")),
            (
                "/private/open/secret",
                false,
                Some("Disallow: /private/open/secret"),
            ),
            (
                "/private/open/secret/1",
                false,
                Some("Disallow: /private/open/secret"),
            ),
        ] {
            let decision = robots.decide(&us(), &url(path));
            assert_eq!(decision.allowed, allowed, "{path}");
            assert_eq!(decision.matched_rule.as_deref(), rule, "{path}");
        }
    }

    #[test]
    fn allow_wins_over_a_disallow_of_the_same_length() {
        let either = [
            "User-agent: *\nDisallow: /page\nAllow: /page\n",
            "User-agent: *\nAllow: /page\nDisallow: /page\n",
        ];
        for body in either {
            let decision = decide(body, "/page");
            assert!(decision.allowed, "{body}");
            assert_eq!(decision.matched_rule.as_deref(), Some("Allow: /page"));
        }
        // One character longer, and the disallow decides.
        assert!(!allowed(
            "User-agent: *\nAllow: /page\nDisallow: /page/\n",
            "/page/"
        ));
        assert!(allowed(
            "User-agent: *\nAllow: /page/\nDisallow: /page\n",
            "/page/"
        ));
        // The whole site closed, one page open.
        let body = "User-agent: *\nDisallow: /\nAllow: /open\n";
        assert!(!allowed(body, "/"));
        assert!(!allowed(body, "/closed"));
        assert!(allowed(body, "/open"));
    }

    #[test]
    fn wildcards_and_the_end_of_an_address() {
        let body = "User-agent: *\nDisallow: /*.pdf$\nDisallow: /search*\nDisallow: /*/print/\nAllow: /search/help$\nDisallow: /a*b*c\n";
        for (path, expected) in [
            ("/report.pdf", false),
            ("/files/report.pdf", false),
            ("/report.pdf?download=1", true),
            ("/report.pdfx", true),
            ("/search", false),
            ("/search?q=x", false),
            ("/searching", false),
            ("/search/help", true),
            ("/search/help/more", false),
            ("/news/print/", false),
            ("/news/print/3", false),
            ("/print/", true),
            ("/news/print", true),
            ("/abc", false),
            ("/a-x-b-y-c-z", false),
            ("/acb", true),
        ] {
            assert_eq!(allowed(body, path), expected, "{path}");
        }
        // A `$` that does not end a pattern is a character.
        assert!(!allowed("User-agent: *\nDisallow: /a$b\n", "/a$b"));
        assert!(allowed("User-agent: *\nDisallow: /a$b\n", "/a"));
        // The pattern itself, piece by piece.
        for (path, pattern, expected) in [
            ("/", "/", true),
            ("/abc", "/", true),
            ("/", "/abc", false),
            ("/google/robotstxt/tree/master", "/*/*/tree/master", true),
            (
                "/google/robotstxt/tree/master/index.html",
                "/*/*/tree/master",
                true,
            ),
            ("/google/robotstxt/tree/master", "/*/*/tree/master$", true),
            (
                "/google/robotstxt/tree/master/abc",
                "/*/*/tree/master$",
                false,
            ),
            ("/google/robotstxt/tree/abc", "/*/*/tree/master", false),
            ("/x", "", true),
            ("/x", "$", false),
            ("/x", "*", true),
            ("/x", "*$", true),
            ("/x/y", "/**/y$", true),
            ("/ab/ab", "/*ab$", true),
            ("/ab/abc", "/*ab$", false),
        ] {
            assert_eq!(matches(path, pattern), expected, "{path} against {pattern}");
        }
    }

    #[test]
    fn the_group_that_names_us_applies_and_only_then_the_one_for_everybody() {
        let body = "User-agent: *\nDisallow: /all\nCrawl-delay: 10\n\nUser-agent: Googlebot\nUser-agent: Kage/1.0\nDisallow: /ours\nCrawl-delay: 2.5\n\nUser-agent: other\nDisallow: /\n";
        let robots = Robots::parse(body);
        let ours = robots.decide(&us(), &url("/ours/page"));
        assert!(!ours.allowed);
        assert_eq!(ours.crawl_delay, Some(2.5));
        // What everybody is told does not apply to somebody who is named.
        assert!(robots.decide(&us(), &url("/all")).allowed);
        // Somebody nobody names is everybody.
        let stranger = vec!["stranger".to_owned()];
        assert!(!robots.decide(&stranger, &url("/all")).allowed);
        assert!(robots.decide(&stranger, &url("/ours")).allowed);
        assert_eq!(robots.decide(&stranger, &url("/")).crawl_delay, Some(10.0));
        // A group for us without a rule for the address allows it.
        assert!(allowed(
            "User-agent: kage\nDisallow:\n\nUser-agent: *\nDisallow: /\n",
            "/page"
        ));
        // Nobody to answer to: the rules for everybody.
        assert!(!robots.decide(&[], &url("/all")).allowed);
        // Two groups for us are one.
        let twice = "User-agent: kage\nDisallow: /a\n\nUser-agent: x\nDisallow: /\n\nUser-agent: kage\nDisallow: /b\n";
        assert!(!allowed(twice, "/a") && !allowed(twice, "/b") && allowed(twice, "/c"));
        for (line, token) in [
            ("*", "*"),
            (" * ", "*"),
            ("* all of you", "*"),
            ("*bot", ""),
            ("Googlebot/2.1", "googlebot"),
            ("My-Bot_2", "my-bot_"),
            ("", ""),
        ] {
            assert_eq!(agent_token(line), token, "{line:?}");
        }
    }

    #[test]
    fn a_crawl_delay_is_the_groups_and_what_is_none_is_none() {
        for (value, delay) in [
            ("5", Some(5.0)),
            ("0.5", Some(0.5)),
            ("0", Some(0.0)),
            ("3600", Some(3600.0)),
            ("86401", Some(86_401.0)),
            // What would overflow a time is a year and a day.
            ("31622400", Some(31_622_400.0)),
            ("31622401", Some(31_622_400.0)),
            ("1e300", Some(31_622_400.0)),
            ("-1", None),
            ("soon", None),
            ("NaN", None),
            ("inf", None),
        ] {
            let body = format!("User-agent: *\nCrawl-delay: {value}\nDisallow: /x\n");
            assert_eq!(decide(&body, "/").crawl_delay, delay, "{value}");
        }
        // The first one a group states; one before any group is nobody's.
        assert_eq!(
            decide(
                "Crawl-delay: 9\nUser-agent: *\nCrawl-delay: 1\nCrawl-delay: 2\n",
                "/"
            )
            .crawl_delay,
            Some(1.0)
        );
        // Like every line that is no `User-agent` line, it ends a run of
        // them: the delay is the first group's, the rule the second's.
        let body = "User-agent: a\nCrawl-delay: 5\nUser-agent: kage\nDisallow: /x\n";
        let decision = decide(body, "/x");
        assert!(!decision.allowed);
        assert_eq!(decision.crawl_delay, None);
        let together = "User-agent: a\nUser-agent: kage\nCrawl-delay: 5\nDisallow: /x\n";
        assert_eq!(decide(together, "/").crawl_delay, Some(5.0));
    }

    #[test]
    fn sitemaps_are_named_wherever_they_stand() {
        let robots = Robots::parse(
            "Sitemap: https://site.example/sitemap.xml\nUser-agent: *\nDisallow: /x\nsitemap:  https://site.example/more.xml.gz \nSite-map: /relative.xml\nSitemap:\n",
        );
        assert_eq!(
            robots.sitemaps(),
            [
                "https://site.example/sitemap.xml",
                "https://site.example/more.xml.gz",
                "/relative.xml"
            ]
        );
        assert_eq!(Robots::parse("").sitemaps(), Vec::<String>::new());
    }

    #[test]
    fn what_google_reads_as_a_rule_is_one() {
        // A byte order mark, a missing colon, a typo, a comment, mixed
        // line endings, keys in any case.
        let body = "\u{feff}USER-AGENT: *\r\nDisalow /typo # a comment\rdisallow: /lower\nALLOW: /lower/ok\n";
        assert!(!allowed(body, "/typo"));
        assert!(!allowed(body, "/lower"));
        assert!(allowed(body, "/lower/ok"));
        // Percent-escapes are one character in either case, and what is
        // not ASCII is matched escaped.
        let escaped = "User-agent: *\nDisallow: /caf%c3%a9\nDisallow: /straße\n";
        assert!(!allowed(escaped, "/caf%C3%A9"));
        assert!(!allowed(escaped, "/caf%c3%a9/menu"));
        assert!(!allowed(escaped, "/café"));
        assert!(!allowed(escaped, "/stra%C3%9Fe"));
        assert!(allowed(escaped, "/cafe"));
        assert_eq!(path_of(&url("/a%2fb?q=%e9#frag")), "/a%2Fb?q=%E9");
        assert_eq!(path_of(&url("")), "/");
        assert_eq!(path_of(&url("/100%")), "/100%");
        // An allow for the index page allows the directory.
        let index = "User-agent: *\nDisallow: /\nAllow: /shop/index.html\n";
        assert!(allowed(index, "/shop/index.html"));
        assert!(allowed(index, "/shop/"));
        assert!(!allowed(index, "/shop/other"));
        assert!(!allowed(index, "/shop"));
        assert_eq!(
            decide(index, "/shop/").matched_rule.as_deref(),
            Some("Allow: /shop/index.html")
        );
        let htm = "User-agent: *\nDisallow: /\nAllow: /index.htm\n";
        assert!(allowed(htm, "/") && !allowed(htm, "/x"));
    }

    #[test]
    fn a_file_with_too_many_rules_is_cut_and_says_so() {
        let mut body = String::from("User-agent: *\n");
        for number in 0..MAX_RULES + 7 {
            writeln!(body, "Disallow: /p{number}$").unwrap();
        }
        let robots = Robots::parse(&body);
        assert_eq!(robots.rules(), MAX_RULES);
        assert_eq!(robots.rules_dropped(), 7);
        assert!(!robots.decide(&us(), &url("/p0")).allowed);
        assert!(
            !robots
                .decide(&us(), &url(&format!("/p{}", MAX_RULES - 1)))
                .allowed
        );
        // The rule that was the first too many is not there.
        assert!(
            robots
                .decide(&us(), &url(&format!("/p{MAX_RULES}")))
                .allowed
        );
    }

    /// A small generator: the same cases every time.
    struct Draw(u64);

    impl Draw {
        fn below(&mut self, bound: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            usize::try_from((self.0 >> 33) % u64::try_from(bound.max(1)).unwrap()).unwrap()
        }

        fn text(&mut self, alphabet: &[&str], most: usize) -> String {
            (0..self.below(most + 1))
                .map(|_| alphabet[self.below(alphabet.len())])
                .collect()
        }
    }

    #[test]
    fn a_pattern_matches_what_its_meaning_says_it_matches() {
        // The meaning of a pattern, said another way: a regular expression
        // anchored at the start, `*` any run of characters, `$` at the end
        // the end, everything else itself.
        let meaning = |pattern: &str| {
            let (body, end) = match pattern.strip_suffix('$') {
                Some(body) => (body, "$"),
                None => (pattern, ""),
            };
            let pieces: Vec<String> = body.split('*').map(regex::escape).collect();
            regex::Regex::new(&format!("^{}{end}", pieces.join(".*"))).unwrap()
        };
        let mut draw = Draw(11);
        let pieces = ["a", "b", "/", "*", "$", "ab", ".", "?", "%2F"];
        let plain = ["a", "b", "/", "ab", ".", "?", "%2F", "$"];
        let mut differ = 0_usize;
        for _ in 0..20_000 {
            let path = format!("/{}", draw.text(&plain, 8));
            let pattern = draw.text(&pieces, 6);
            let ours = matches(&path, &pattern);
            assert_eq!(
                ours,
                meaning(&pattern).is_match(&path),
                "{path:?} against {pattern:?}"
            );
            if ours != LongestMatchRobotsMatchStrategy::matches(&path, &pattern) {
                differ += 1;
            }
        }
        // Why the crate's matcher is not the one used: where a `*` can
        // stand for more than one run it loses the later ones. A site that
        // says `Disallow: /*a/b` means `/a/a/b` too.
        assert!(matches("/a/a/b", "/*a/b"));
        assert!(!LongestMatchRobotsMatchStrategy::matches("/a/a/b", "/*a/b"));
        assert!(differ > 0, "the crate's matcher agreed on every case");
    }

    #[test]
    fn what_the_crate_misreads_is_read() {
        // A byte order mark in text that is already decoded: the crate
        // takes it for part of the first key and loses the group.
        let marked = "\u{feff}User-agent: *\nDisallow: /x\n";
        assert!(!allowed(marked, "/x"));
        assert!(DefaultMatcher::default().one_agent_allowed_by_robots(
            marked,
            "kage",
            "https://site.example/x"
        ));
        // A line longer than the bound, of characters of two bytes, is cut
        // at a character (the crate cuts inside one and fails; the log of
        // that is with the milestone's evidence). What follows it is read.
        let wide = format!(
            "User-agent: *\nDisallow: /{}\nDisallow: /after\n",
            "é".repeat(20_000)
        );
        let robots = Robots::parse(&wide);
        assert_eq!(robots.rules(), 2);
        assert!(!robots.decide(&us(), &url("/after")).allowed);
        assert!(robots.decide(&us(), &url("/before")).allowed);
        // The cut line is a rule as far as it was read: 16 kibibytes.
        let cut = "é".repeat(super::MAX_LINE_BYTES / 2);
        assert!(!robots.decide(&us(), &url(&format!("/{cut}"))).allowed);
    }

    #[test]
    fn a_verdict_is_the_one_googles_matcher_gives() {
        let mut draw = Draw(29);
        let agents = ["*", "kage", "other", "Kage", "*"];
        let paths = [
            "/",
            "/a",
            "/a/b",
            "/b",
            "/a.pdf",
            "/index.html",
            "/a/index.html",
            "/ab",
        ];
        let patterns = [
            "",
            "/",
            "/a",
            "/a/",
            "/b",
            "/*.pdf$",
            "/a*",
            "/a/index.html",
            "/index.htm",
            "/*b",
            "/ab$",
        ];
        for round in 0..4000 {
            let mut body = String::new();
            for _ in 0..=draw.below(4) {
                for _ in 0..=draw.below(2) {
                    writeln!(body, "User-agent: {}", agents[draw.below(agents.len())]).unwrap();
                }
                for _ in 0..draw.below(5) {
                    let kind = ["Allow", "Disallow", "Disallow", "Crawl-delay"][draw.below(4)];
                    let value = if kind == "Crawl-delay" {
                        "3"
                    } else {
                        patterns[draw.below(patterns.len())]
                    };
                    writeln!(body, "{kind}: {value}").unwrap();
                }
            }
            let robots = Robots::parse(&body);
            for path in paths {
                let address = format!("https://site.example{path}");
                let theirs =
                    DefaultMatcher::default().one_agent_allowed_by_robots(&body, "kage", &address);
                let ours = robots.decide(&us(), &Url::parse(&address).unwrap());
                assert_eq!(ours.allowed, theirs, "round {round}, {path} under\n{body}");
            }
        }
    }

    #[test]
    fn nothing_a_site_sends_makes_the_parser_fail() {
        let mut draw = Draw(5);
        let pieces = [
            "User-agent",
            "user agent",
            ":",
            " ",
            "\t",
            "\n",
            "\r\n",
            "\r",
            "*",
            "$",
            "#",
            "/",
            "%",
            "%2",
            "%zz",
            "é",
            "\u{feff}",
            "Disallow",
            "Allow",
            "Sitemap",
            "Crawl-delay",
            "\0",
            "ß",
            "a",
            "9",
            "🙂",
            "::",
            "",
        ];
        for _ in 0..5000 {
            let body = draw.text(&pieces, 40);
            let robots = Robots::parse(&body);
            let _ = robots.decide(&us(), &url("/a%2Fb/ß?x=1"));
        }
        // A line longer than any parser keeps, of characters of two bytes.
        let wide = format!(
            "User-agent: *\nDisallow: /{}\nAllow: /x\n",
            "é".repeat(20_000)
        );
        let _ = Robots::parse(&wide);
        // A long line, and a long file of nothing.
        let long = format!("User-agent: *\nDisallow: /{}\n", "x".repeat(200_000));
        assert!(Robots::parse(&long).decide(&us(), &url("/y")).allowed);
        let _ = Robots::parse(&"\n".repeat(100_000));
    }
}
