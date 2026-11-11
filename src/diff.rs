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
    let s = s.get(1..).unwrap_or("");
    match s.split_once(',') {
        Some((a, b)) => (a.parse().unwrap_or(0), b.parse().unwrap_or(0)),
        None => (s.parse().unwrap_or(0), 1),
    }
}

/// A line of a merge's combined diff, which has one marker per parent: it's
/// added when any parent lacks it, removed when it's gone from the result.
fn combined_line(l: &str, cols: usize) -> (Kind, &str) {
    let Some(marks) = l.get(..cols) else { return (Kind::Ctx, l) };
    let kind = if marks.contains('+') {
        Kind::Add
    } else if marks.contains('-') {
        Kind::Del
    } else {
        Kind::Ctx
    };
    (kind, &l[cols..])
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

    // `cols` is how many marker columns a line has: 1, or one per parent in a
    // merge's combined diff, whose headers are `@@@ -a,b -a,b +c,d @@@`.
    let (mut old, mut new, mut end, mut cols) = (0u32, 0u32, 1u32, 1usize);
    let mut in_hunk = false;
    for l in text.lines() {
        let ats = l.chars().take_while(|&c| c == '@').count();
        if ats >= 2
            && let Some(rest) = l[ats..].strip_prefix(' ')
        {
            cols = ats - 1;
            let mut parts = rest.splitn(cols + 2, ' ');
            let (a, b) = parse_range(parts.next().unwrap_or("-0"));
            let mut parts = parts.skip(cols - 1);
            let (c, d) = parse_range(parts.next().unwrap_or("+0"));
            let context = parts.next().unwrap_or("").trim_start_matches('@').trim().to_string();
            // A combined diff has several old sides, so its gaps are counted on the new one.
            let (start, len) = if cols == 1 { (a, b) } else { (c, d) };
            let hidden = start.saturating_sub(end);
            if hidden > 0 {
                out.items.push(Item::Gap { hidden, context });
            }
            old = a;
            new = c;
            end = start + len;
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if l.starts_with('\\') {
            out.items.push(Item::Note(l.trim_start_matches("\\ ").to_string()));
            continue;
        }
        let (kind, body) = if cols > 1 {
            combined_line(l, cols)
        } else {
            match l.chars().next() {
                Some('+') => (Kind::Add, &l[1..]),
                Some('-') => (Kind::Del, &l[1..]),
                Some(' ') => (Kind::Ctx, &l[1..]),
                _ => (Kind::Ctx, l),
            }
        };
        let num = match kind {
            Kind::Add => {
                new += 1;
                out.adds += 1;
                new - 1
            }
            // A combined diff has no single old file to number a removed line in.
            Kind::Del if cols > 1 => {
                out.dels += 1;
                new
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

    const COMBINED: &str = "diff --cc f\nindex 797be14,0c02ccc..0000000\n--- a/f\n+++ b/f\n\
@@@ -1,3 -1,3 +1,7 @@@\n  a\n++<<<<<<< HEAD\n +Y\n++=======\n+ X\n++>>>>>>> other\n  c\n";

    #[test]
    fn combined_diff_of_a_conflict() {
        let p = parse(COMBINED);
        assert_eq!((p.adds, p.dels), (5, 0));
        assert_eq!(p.items.len(), 7);
        let lines: Vec<(Kind, u32, &str)> = p
            .items
            .iter()
            .filter_map(|i| if let Item::Line(l) = i { Some((l.kind, l.num, l.text.as_str())) } else { None })
            .collect();
        assert_eq!(lines[0], (Kind::Ctx, 1, "a"));
        assert_eq!(lines[1], (Kind::Add, 2, "<<<<<<< HEAD"));
        assert_eq!(lines[2], (Kind::Add, 3, "Y"));
        assert_eq!(lines[4], (Kind::Add, 5, "X"));
        assert_eq!(lines[6], (Kind::Ctx, 7, "c"));
    }

    #[test]
    fn malformed_hunk_header_does_not_panic() {
        let p = parse("@@ \n+a\n");
        assert_eq!(p.adds, 1);
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
