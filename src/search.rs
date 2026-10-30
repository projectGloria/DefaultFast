//! Allocation-free scoring of pre-normalized strings; result storage is reused.
pub fn normalize_extension(input: &str) -> Option<String> {
    let s = input.trim().trim_start_matches('.');
    if s.is_empty()
        || s.len() > 127
        || s.chars()
            .any(|c| c.is_control() || "\\/:*?\"<>| ".contains(c))
    {
        return None;
    }
    Some(format!(".{}", s.to_lowercase()))
}

pub fn score(text: &str, query: &str) -> i32 {
    if query.is_empty() {
        return 1;
    }
    if text == query {
        return 100_000;
    }
    if text.starts_with(query) {
        return 90_000 - text.len().min(1000) as i32;
    }
    if let Some(p) = text.find(query) {
        let boundary = !text.as_bytes()[p - 1].is_ascii_alphanumeric();
        return (if boundary { 80_000 } else { 60_000 }) - p.min(1000) as i32;
    }
    let mut q = query.chars();
    let mut next = q.next();
    let mut gaps = 0;
    let mut matched = 0;
    for c in text.chars() {
        if Some(c) == next {
            matched += 1;
            next = q.next();
            if next.is_none() {
                return if gaps <= matched * 3 {
                    10_000 + matched * 8 - gaps
                } else {
                    0
                };
            }
        } else {
            gaps += 1;
        }
    }
    0
}

pub fn filter<'a>(
    items: impl Iterator<Item = (usize, &'a str, bool)>,
    query: &str,
    out: &mut Vec<(usize, i32)>,
) {
    out.clear();
    let q = query.trim_start_matches('.');
    for (i, text, recommended) in items {
        let s = score(text, q);
        if s > 0 {
            out.push((i, s + if recommended { 200_000 } else { 0 }));
        }
    }
    out.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ranking() {
        assert!(score("bat", "bat") > score("batch file", "bat"));
        assert!(score("batch file", "bat") > score("windows batch file", "bat"));
        assert!(score("windows batch file", "bat") > score("acrobat", "bat"));
        assert!(score("acrobat", "bat") > score("b a t", "bat"));
        assert_eq!(score("pdf", "bat"), 0);
        assert!(score("türkçe 文件", "文件") > 0);
    }
    #[test]
    fn extensions() {
        assert_eq!(normalize_extension(" BAT ").as_deref(), Some(".bat"));
        assert_eq!(normalize_extension(".PDF").as_deref(), Some(".pdf"));
        for s in ["", ".", "..", "a\\b", "a b", "a/b", "a\0b"] {
            assert_eq!(normalize_extension(s), None);
        }
    }
    #[test]
    fn recommended_only_when_matching() {
        let mut rows = Vec::new();
        filter(
            [(0, "notepad", true), (1, "terminal", false)].into_iter(),
            "terminal",
            &mut rows,
        );
        assert_eq!(rows, vec![(1, 100_000)]);
    }
}
