//! Short previews of a change for approval cards: the changed lines, with
//! a little context, capped so the card stays readable.

/// Lines of context kept around a change.
const CONTEXT: usize = 2;

/// A preview of `old` → `new`: the lines that differ (`-` removed, `+`
/// added) between the common start and end, with line numbers, at most
/// `max_lines` lines.
pub fn preview(old: &str, new: &str, max_lines: usize) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    if a == b {
        return if old == new { "(no change)".into() } else { "(only line endings or the final newline change)".into() };
    }
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let max_suffix = a.len().min(b.len()) - prefix;
    let suffix = a.iter().rev().zip(b.iter().rev()).take(max_suffix).take_while(|(x, y)| x == y).count();
    let start = prefix.saturating_sub(CONTEXT);
    let mut out = vec![format!("@@ line {}", prefix + 1)];
    for line in &a[start..prefix] {
        out.push(format!("  {line}"));
    }
    let removed = &a[prefix..a.len() - suffix];
    let added = &b[prefix..b.len() - suffix];
    for line in removed {
        out.push(format!("- {line}"));
    }
    for line in added {
        out.push(format!("+ {line}"));
    }
    let after = (a.len() - suffix + CONTEXT).min(a.len());
    for line in &a[a.len() - suffix..after] {
        out.push(format!("  {line}"));
    }
    if out.len() > max_lines {
        let hidden = out.len() - max_lines;
        out.truncate(max_lines);
        out.push(format!("… {hidden} more lines (−{} +{} lines in all)", removed.len(), added.len()));
    }
    out.join("\n")
}

/// The first lines of a new file, for a card.
pub fn head(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = lines.iter().take(max_lines).map(|l| format!("+ {l}")).collect();
    if lines.len() > max_lines {
        out.push(format!("… {} more lines", lines.len() - max_lines));
    }
    out.join("\n")
}

/// `1234` → `1.2 KB`.
pub fn size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} bytes"),
        b if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b if b < 1024 * 1024 * 1024 => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
        b => format!("{:.1} GB", b as f64 / (1024.0 * 1024.0 * 1024.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_the_changed_lines_with_context() {
        let old = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "a\nb\nc\nD!\ne\nf\ng\n";
        assert_eq!(preview(old, new, 40), "@@ line 4\n  b\n  c\n- d\n+ D!\n  e\n  f");
    }

    #[test]
    fn additions_at_the_end_and_start() {
        assert_eq!(preview("a\nb\n", "a\nb\nc\n", 40), "@@ line 3\n  a\n  b\n+ c");
        assert_eq!(preview("b\n", "a\nb\n", 40), "@@ line 1\n+ a\n  b");
        assert_eq!(preview("x\n", "x\n", 40), "(no change)");
        assert_eq!(preview("x", "x\n", 40), "(only line endings or the final newline change)");
    }

    #[test]
    fn long_changes_are_capped() {
        let old: String = (0..100).map(|i| format!("{i}\n")).collect();
        let p = preview(&old, "", 10);
        assert_eq!(p.lines().count(), 11);
        assert!(p.ends_with("(−100 +0 lines in all)"), "{p}");
    }

    #[test]
    fn sizes() {
        assert_eq!(size(12), "12 bytes");
        assert_eq!(size(2048), "2.0 KB");
        assert_eq!(head("a\nb\nc", 2), "+ a\n+ b\n… 1 more lines");
    }
}
