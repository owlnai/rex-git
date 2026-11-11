//! Parse a unified diff into lines with line numbers, hunk gaps instead of
//! `@@` headers, and the changed span of each edited line.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Ctx,
    Add,
    Del,
}

#[derive(Debug)]
pub struct DLine {
    pub kind: Kind,
    /// The line's number: in the new file for added and context lines, in
    /// the old file for removed ones.
    pub num: u32,
    pub text: String,
    /// The changed part of an edited line, as a char range.
    pub emph: Option<(usize, usize)>,
}

#[derive(Debug)]
pub enum Item {
    /// Unchanged lines left out between hunks, and the function they're in.
    Gap {
        hidden: u32,
        context: String,
    },
    Line(DLine),
    Note(String),
}

#[derive(Default, Debug)]
pub struct Parsed {
    pub items: Vec<Item>,
    pub adds: u32,
    pub dels: u32,
}

impl Parsed {
    pub fn max_num(&self) -> u32 {
        self.items.iter().filter_map(|i| if let Item::Line(l) = i { Some(l.num) } else { None }).max().unwrap_or(0)
    }
}

fn parse_range(s: &str) -> (u32, u32) {
    let s = &s[1..];
    match s.split_once(',') {
        Some((a, b)) => (a.parse().unwrap_or(0), b.parse().unwrap_or(0)),
        None => (s.parse().unwrap_or(0), 1),
    }
}

pub fn parse(text: &str) -> Parsed {
    let mut out = Parsed::default();
    if !text.lines().any(|l| l.starts_with("@@")) {
        // Binary files, mode changes, empty files: show what git said.
        for l in text.lines() {
            if l.starts_with("diff --git") || l.starts_with("index ") || l.starts_with("--- ") || l.starts_with("+++ ")
            {
                continue;
            }
            out.items.push(Item::Note(l.to_string()));
        }
        if out.items.is_empty() {
            out.items.push(Item::Note("no textual changes".into()));
        }
        return out;
    }

    let (mut old, mut new, mut old_end) = (0u32, 0u32, 1u32);
    let mut in_hunk = false;
    for l in text.lines() {
        if let Some(rest) = l.strip_prefix("@@ ") {
            let mut parts = rest.splitn(3, ' ');
            let (a, b) = parse_range(parts.next().unwrap_or("-0"));
            let (c, _) = parse_range(parts.next().unwrap_or("+0"));
            let context = parts.next().unwrap_or("").trim_start_matches("@@").trim().to_string();
            let hidden = a.saturating_sub(old_end);
            if hidden > 0 {
                out.items.push(Item::Gap { hidden, context });
            }
            old = a;
            new = c;
            old_end = a + b;
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        let (kind, body) = match l.chars().next() {
            Some('+') => (Kind::Add, &l[1..]),
            Some('-') => (Kind::Del, &l[1..]),
            Some(' ') => (Kind::Ctx, &l[1..]),
            Some('\\') => {
                out.items.push(Item::Note(l.trim_start_matches("\\ ").to_string()));
                continue;
            }
            _ => (Kind::Ctx, l),
        };
        let num = match kind {
            Kind::Add => {
                new += 1;
                out.adds += 1;
                new - 1
            }
            Kind::Del => {
                old += 1;
                out.dels += 1;
                old - 1
            }
            Kind::Ctx => {
                old += 1;
                new += 1;
                new - 1
            }
        };
        out.items.push(Item::Line(DLine { kind, num, text: body.replace('\t', "    "), emph: None }));
    }
    mark_edits(&mut out.items);
    out
}

/// Pair each run of removed lines with the added lines right after it, and
/// mark the part of each pair that differs.
fn mark_edits(items: &mut [Item]) {
    let kind = |i: &Item| if let Item::Line(l) = i { Some(l.kind) } else { None };
    let mut i = 0;
    while i < items.len() {
        if kind(&items[i]) != Some(Kind::Del) {
            i += 1;
            continue;
        }
        let del_start = i;
        while i < items.len() && kind(&items[i]) == Some(Kind::Del) {
            i += 1;
        }
        let add_start = i;
        while i < items.len() && kind(&items[i]) == Some(Kind::Add) {
            i += 1;
        }
        let pairs = (add_start - del_start).min(i - add_start);
        for k in 0..pairs {
            let (a, b) = (del_start + k, add_start + k);
            let (Item::Line(d), Item::Line(n)) = (&items[a], &items[b]) else { continue };
            if let Some((de, ne)) = changed_span(&d.text, &n.text) {
                if let Item::Line(d) = &mut items[a] {
                    d.emph = Some(de);
                }
                if let Item::Line(n) = &mut items[b] {
                    n.emph = Some(ne);
                }
            }
        }
    }
}

type Span = (usize, usize);

fn changed_span(old: &str, new: &str) -> Option<(Span, Span)> {
    let o: Vec<char> = old.chars().collect();
    let n: Vec<char> = new.chars().collect();
    let prefix = o.iter().zip(&n).take_while(|(a, b)| a == b).count();
    let max_suffix = o.len().min(n.len()) - prefix;
    let suffix = o.iter().rev().zip(n.iter().rev()).take(max_suffix).take_while(|(a, b)| a == b).count();
    // Lines with nothing in common read better without a highlight.
    let common = prefix + suffix;
    if common == 0 || common * 4 < o.len().max(n.len()) {
        return None;
    }
    Some(((prefix, o.len() - suffix), (prefix, n.len() - suffix)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n\
@@ -10,3 +10,3 @@ fn main() {\n ctx\n-let a = 1;\n+let a = 2;\n ctx2\n";

    #[test]
    fn numbers_gaps_and_edits() {
        let p = parse(DIFF);
        assert_eq!((p.adds, p.dels), (1, 1));
        assert!(matches!(&p.items[0], Item::Gap { hidden: 9, context } if context == "fn main() {"));
        let Item::Line(del) = &p.items[2] else { panic!() };
        let Item::Line(add) = &p.items[3] else { panic!() };
        assert_eq!((del.kind, del.num, add.kind, add.num), (Kind::Del, 11, Kind::Add, 11));
        assert_eq!(del.emph, Some((8, 9)));
        assert_eq!(add.emph, Some((8, 9)));
        let Item::Line(last) = &p.items[4] else { panic!() };
        assert_eq!(last.num, 12);
    }

    #[test]
    fn binary_is_a_note() {
        let p = parse("diff --git a/x b/x\nBinary files a/x and b/x differ\n");
        assert!(matches!(&p.items[0], Item::Note(n) if n.starts_with("Binary")));
    }

    #[test]
    fn second_hunk_gets_a_gap() {
        let p = parse("--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-a\n+b\n c\n@@ -10,2 +10,2 @@ fn x()\n d\n-e\n+f\n");
        assert_eq!((p.adds, p.dels), (2, 2));
        assert_eq!(p.items.len(), 7);
        assert!(matches!(&p.items[3], Item::Gap { hidden: 7, context } if context == "fn x()"));
        let Item::Line(first) = &p.items[0] else { panic!() };
        assert_eq!((first.kind, first.num, first.emph), (Kind::Del, 1, None));
    }

    #[test]
    fn no_newline_marker_is_a_note() {
        let p = parse("@@ -1 +1 @@\n-a\n+b\n\\ No newline at end of file\n");
        assert!(matches!(p.items.last(), Some(Item::Note(n)) if n == "No newline at end of file"));
    }

    #[test]
    fn empty_diff_says_so() {
        let p = parse("");
        assert_eq!(p.items.len(), 1);
        assert!(matches!(&p.items[0], Item::Note(n) if n == "no textual changes"));
    }

    #[test]
    fn tabs_become_spaces() {
        let p = parse("@@ -1 +1 @@\n+\tx\n");
        let Item::Line(l) = &p.items[0] else { panic!() };
        assert_eq!(l.text, "    x");
    }
}
