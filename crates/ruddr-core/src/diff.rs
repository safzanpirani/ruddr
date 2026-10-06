//! omp's numbered edit diff, rebuilt as unified-diff hunks. The omp adapter
//! uses it for live edits and the history reader for past ones.

/// Hunks from omp's numbered edit diff, which records real line numbers.
/// Each row is a prefix (` `, `+`, or `-`), a padded line number, `|`, and
/// the text. Context and removed rows carry the old file's number, added
/// rows the new file's; a row whose text is `...` marks elided lines. A
/// jump in the old numbering starts a new hunk.
pub fn numbered_diff(text: &str) -> String {
    struct Hunk {
        old_start: i64,
        new_start: i64,
        old_count: i64,
        new_count: i64,
        body: String,
    }
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut delta = 0i64;
    let mut next_old: Option<i64> = None;
    for row in text.lines() {
        let mut chars = row.chars();
        let Some(prefix @ (' ' | '+' | '-')) = chars.next() else { continue };
        let Some((number, line)) = chars.as_str().split_once('|') else {
            continue;
        };
        let Ok(number) = number.trim().parse::<i64>() else { continue };
        if prefix == ' ' && line == "..." {
            next_old = None;
            continue;
        }
        let (old, new) = match prefix {
            '+' => (number - delta, number),
            _ => (number, number + delta),
        };
        if next_old != Some(old) || hunks.is_empty() {
            hunks.push(Hunk {
                old_start: old,
                new_start: new,
                old_count: 0,
                new_count: 0,
                body: String::new(),
            });
        }
        let hunk = hunks.last_mut().expect("a hunk was just pushed");
        match prefix {
            '+' => {
                hunk.new_count += 1;
                delta += 1;
                next_old = Some(old);
            }
            '-' => {
                hunk.old_count += 1;
                delta -= 1;
                next_old = Some(old + 1);
            }
            _ => {
                hunk.old_count += 1;
                hunk.new_count += 1;
                next_old = Some(old + 1);
            }
        }
        hunk.body.push(prefix);
        hunk.body.push_str(line);
        hunk.body.push('\n');
    }
    // An empty side starts at the line before it, as in `diff -u`.
    let start = |start: i64, count: i64| if count == 0 { start - 1 } else { start }.max(0);
    hunks
        .iter()
        .map(|h| {
            format!(
                "@@ -{},{} +{},{} @@\n{}",
                start(h.old_start, h.old_count),
                h.old_count,
                start(h.new_start, h.new_count),
                h.new_count,
                h.body
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_diff_rebuilds_hunks_with_real_line_numbers() {
        // omp's editDiffString output: line 5 changed, 11-12 removed, two
        // lines inserted after old line 22.
        let omp = " 3|line3\n 4|line4\n-5|line5\n+5|CHANGED5\n 6|line6\n 7|line7\n 9|line9\n 10|line10\n\
                   -11|line11\n-12|line12\n 13|line13\n 14|line14\n 21|line21\n 22|line22\n+21|NEW-A\n+22|NEW-B\n\
                   \x2023|line23\n 24|line24\n";
        assert_eq!(
            numbered_diff(omp),
            "@@ -3,5 +3,5 @@\n line3\n line4\n-line5\n+CHANGED5\n line6\n line7\n\
             @@ -9,6 +9,4 @@\n line9\n line10\n-line11\n-line12\n line13\n line14\n\
             @@ -21,4 +19,6 @@\n line21\n line22\n+NEW-A\n+NEW-B\n line23\n line24\n"
        );
        // Elision rows split hunks and never reach the output.
        assert_eq!(
            numbered_diff("   1|...\n 112|a\n+113|b\n 113|c\n 114|..."),
            "@@ -112,2 +112,3 @@\n a\n+b\n c\n"
        );
        assert_eq!(numbered_diff("+1|only\n"), "@@ -0,0 +1,1 @@\n+only\n");
        assert_eq!(numbered_diff("no rows here"), "");
    }
}
