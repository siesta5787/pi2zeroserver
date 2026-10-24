//! Which version is newer, for the system and for apps.
//!
//! Versions are dotted numbers compared part by part (0.50 < 0.51 < 0.100 <
//! 1.0; missing parts count as 0), optionally with a pre-release suffix after
//! "-" that comes before the plain version (0.51-test1 < 0.51-test2 < 0.51;
//! letters compare as text, digits as numbers). "+..." build information is
//! ignored. System versions used to be dates (2026.10.24.1323-gbb2746b): any
//! such version counts as older than every numbered one, so boxes still on a
//! date version take the first numbered release.

use std::cmp::Ordering;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Chunk {
    Number(u64),
    Text(String),
}

/// "test12rc" → [Text("test"), Number(12), Text("rc")]
fn chunks(s: &str) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        let digits = c.is_ascii_digit();
        let end = rest.find(|ch: char| ch.is_ascii_digit() != digits).unwrap_or(rest.len());
        let (part, tail) = rest.split_at(end);
        out.push(if digits {
            Chunk::Number(part.parse().unwrap_or(u64::MAX))
        } else {
            Chunk::Text(part.to_owned())
        });
        rest = tail;
    }
    out
}

struct Parsed {
    /// A date-style version from before numbered releases
    legacy: bool,
    numbers: Vec<u64>,
    /// None for a plain version
    pre: Option<Vec<Chunk>>,
}

fn parse(v: &str) -> Parsed {
    let v = v.trim().trim_start_matches('v');
    let v = v.split('+').next().unwrap_or(v);
    let (main, suffix) = v.split_once('-').map_or((v, None), |(m, s)| (m, Some(s)));
    let numbers: Vec<u64> = main.split('.').map(|n| n.parse().unwrap_or(0)).collect();
    let legacy = numbers.first().is_some_and(|&n| n >= 1000);
    Parsed {
        legacy,
        numbers,
        // Date versions' suffix was the commit ("gbb2746b"), not a pre-release
        pre: suffix.filter(|_| !legacy).map(chunks),
    }
}

pub fn compare(a: &str, b: &str) -> Ordering {
    let (a, b) = (parse(a), parse(b));
    let numbered = (!a.legacy).cmp(&!b.legacy);
    let len = a.numbers.len().max(b.numbers.len());
    let part = |p: &Parsed, i: usize| p.numbers.get(i).copied().unwrap_or(0);
    let numbers =
        (0..len).map(|i| part(&a, i).cmp(&part(&b, i))).find(|o| o.is_ne()).unwrap_or(Ordering::Equal);
    let pre = match (&a.pre, &b.pre) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => x.cmp(y),
    };
    numbered.then(numbers).then(pre)
}

/// `candidate` is newer than `current`
pub fn newer(candidate: &str, current: &str) -> bool {
    compare(candidate, current) == Ordering::Greater
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_versions() {
        assert!(newer("0.51", "0.50"));
        assert!(newer("0.100", "0.99"));
        assert!(newer("1.0", "0.100"));
        assert!(!newer("0.50", "0.50") && !newer("0.50.0", "0.50") && !newer("0.50", "0.50.0"));
        assert!(newer("v0.51", "0.50"));
        assert!(newer("0.18.10", "0.18.9"));
        assert!(!newer("0.9.0", "0.18.6"));
    }

    #[test]
    fn test_versions_come_before_the_release() {
        assert!(newer("0.51-test1", "0.50"));
        assert!(newer("0.51-test2", "0.51-test1"));
        assert!(newer("0.51-test10", "0.51-test9"));
        assert!(newer("0.51", "0.51-test2"));
        assert!(newer("0.20.0-rc", "0.19.0") && newer("0.20.0", "0.20.0-rc"));
        assert!(newer("0.50", "0.50-dirty"));
        assert!(!newer("0.51+build7", "0.51"));
    }

    #[test]
    fn dates_are_older_than_any_number() {
        assert!(newer("0.50", "2026.10.24.1323-gbb2746b"));
        assert!(!newer("2026.10.24.1323-gbb2746b", "0.50"));
        assert!(newer("2026.10.25.0100-gaaa", "2026.10.24.1323-gbb2746b"));
        assert!(!newer("2026.10.19.2244-g77c", "2026.10.19.2244-g77c-dirty"));
    }
}
