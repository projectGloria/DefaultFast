//! Fuzzy, case-insensitive, prefix-boosted matching. All haystacks are lowercased at index
//! time; per keystroke we lowercase the query once and score without allocating.
use crate::model::Index;

/// Higher is better; 0 = no match. Tiers: exact > prefix > word-boundary substring >
/// inner substring > subsequence (fuzzy, gap-penalized). Within a tier, shorter / earlier wins.
pub fn score(text: &str, query: &str) -> i32 {
    if query.is_empty() {
        return 0;
    }
    if text == query {
        return 100_000;
    }
    if text.starts_with(query) {
        return 90_000 - text.len().min(1000) as i32;
    }
    if let Some(pos) = text.find(query) {
        // `pos > 0` here (prefix handled above), so the previous byte exists.
        let boundary = !text.as_bytes()[pos - 1].is_ascii_alphanumeric();
        return if boundary { 80_000 } else { 60_000 } - pos.min(1000) as i32 - text.len().min(1000) as i32 / 4;
    }
    // Subsequence ("ntpd" -> notepad): must start at a word start and stay compact (at most
    // one skipped char per query char, plus one). Otherwise "vlc" matches
    // "Equalizer APO De(v)ice Se(l)e(c)tor" and fuzzy results drown the real ones.
    let qlen = query.chars().count() as i32;
    if qlen < 2 {
        return 0;
    }
    let first = query.chars().next().unwrap_or_default();
    let mut prev_alnum = false;
    let mut best = 0;
    for (start, c) in text.char_indices() {
        let word_start = !prev_alnum;
        prev_alnum = c.is_alphanumeric();
        if c != first || !word_start {
            continue;
        }
        let mut q = query.chars().skip(1);
        let mut want = q.next();
        let (mut span, mut run, mut best_run) = (1i32, 1i32, 1i32);
        for c in text[start + c.len_utf8()..].chars() {
            let Some(w) = want else { break };
            span += 1;
            if span > qlen * 2 + 1 {
                break;
            }
            if c == w {
                run += 1;
                best_run = best_run.max(run);
                want = q.next();
            } else {
                run = 0;
            }
        }
        if want.is_none() && span <= qlen * 2 + 1 {
            best = best.max(10_000 + best_run * 50 - (span - qlen) * 20);
        }
    }
    best
}

/// Best of the primary field (name / extension) and the secondary search text; primary
/// matches get a bonus so "vlc" ranks the VLC program above a file type mentioning VLC.
fn best(primary: &str, secondary: &str, q: &str) -> i32 {
    let p = score(primary, q);
    let s = score(secondary, q);
    if p > 0 {
        p + 5_000
    } else {
        s
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Program,
    FileType,
    Protocol,
}

#[derive(Default, Debug)]
pub struct Results {
    pub programs: Vec<(usize, i32)>,
    pub file_types: Vec<(usize, i32)>,
    pub protocols: Vec<(usize, i32)>,
}

pub const MAX_PER_GROUP: usize = 40;

/// Lowercases the query and strips a leading dot/space; "." alone stays (matches extensions).
pub fn normalize(query: &str) -> String {
    let q = query.trim().to_lowercase();
    if q.len() > 1 {
        q.trim_start_matches('.').to_owned()
    } else {
        q
    }
}

/// Fills `out` (reused between keystrokes) with the top matches per group.
pub fn run(index: &Index, query: &str, out: &mut Results) {
    out.programs.clear();
    out.file_types.clear();
    out.protocols.clear();
    let q = normalize(query);
    if q.is_empty() {
        return;
    }
    // A query that starts with "." is clearly about file types; skip program noise.
    let wants_ext = query.trim_start().starts_with('.');
    if !wants_ext {
        for (i, p) in index.programs.iter().enumerate() {
            let s = best(&p.search_name, &p.search, &q);
            if s > 0 {
                // Programs that can actually be made default for something rank first.
                out.programs.push((i, s + if p.supports.is_empty() { 0 } else { 2_000 }));
            }
        }
        for (i, p) in index.protocols.iter().enumerate() {
            let s = best(&p.name, &p.search, &q);
            if s > 0 {
                out.protocols.push((i, s));
            }
        }
    }
    for (i, t) in index.file_types.iter().enumerate() {
        let s = best(&t.search_name, &t.search, &q);
        if s > 0 {
            out.file_types.push((i, s));
        }
    }
    for v in [&mut out.programs, &mut out.file_types, &mut out.protocols] {
        // Partial sort: only the top MAX_PER_GROUP need ordering.
        if v.len() > MAX_PER_GROUP {
            v.select_nth_unstable_by(MAX_PER_GROUP, |a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            v.truncate(MAX_PER_GROUP);
        }
        v.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    }
}

/// Group display order: the group whose best hit is strongest goes first.
pub fn group_order(r: &Results) -> [Kind; 3] {
    let top = |v: &Vec<(usize, i32)>| v.first().map(|x| x.1).unwrap_or(i32::MIN);
    let mut kinds = [(Kind::Program, top(&r.programs)), (Kind::FileType, top(&r.file_types)), (Kind::Protocol, top(&r.protocols))];
    kinds.sort_by(|a, b| b.1.cmp(&a.1));
    [kinds[0].0, kinds[1].0, kinds[2].0]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FileType, Program, Protocol};

    #[test]
    fn tiers() {
        assert!(score("bat", "bat") > score("batch file", "bat"));
        assert!(score("batch file", "bat") > score("windows batch file", "bat"));
        assert!(score("windows batch file", "bat") > score("acrobat", "bat"));
        assert!(score("acrobat", "bat") > score("b-a-t", "bat"));
        assert_eq!(score("equalizer apo device selector", "vlc"), 0, "fuzzy must be compact and word-anchored");
        assert!(score("b-a-t", "bat") > 0);
        assert_eq!(score("pdf", "bat"), 0);
        assert_eq!(score("bxxxxxxxxxxxxaxxxxxxxxxt", "bat"), 0, "mostly gaps");
        assert!(score("türkçe 文件", "文件") > 0);
        assert!(score("notepad++", "ntpd") > 0, "subsequence");
        assert!(score("vlc media player", "vlc") > score("video lan client", "vlc"));
    }

    fn index() -> Index {
        let mut vlc = Program { name: "VLC media player".into(), exe: "C:\\VLC\\vlc.exe".into(), ..Default::default() };
        vlc.add_support(".mp4", "VLC.mp4");
        vlc.reindex();
        let mut np = Program { name: "Notepad++".into(), exe: "C:\\npp\\notepad++.exe".into(), ..Default::default() };
        np.reindex();
        let ft = |ext: &str, friendly: &str| FileType {
            ext: ext.into(),
            friendly: friendly.into(),
            search_name: ext[1..].into(),
            search: format!("{} {}", &ext[1..], friendly.to_lowercase()),
        };
        Index {
            programs: vec![np, vlc],
            file_types: vec![ft(".vlc", "VLC playlist"), ft(".pdf", "PDF Document"), ft(".bat", "Windows Batch File")],
            protocols: vec![Protocol { name: "vlc".into(), friendly: "VLC".into(), search: "vlc vlc".into() }],
            complete: true,
        }
    }

    #[test]
    fn grouped_results() {
        let idx = index();
        let mut r = Results::default();
        run(&idx, "VLC", &mut r);
        assert_eq!(r.programs[0].0, 1, "VLC program found case-insensitively");
        assert_eq!(idx.file_types[r.file_types[0].0].ext, ".vlc");
        assert_eq!(r.protocols.len(), 1);

        run(&idx, ".pd", &mut r);
        assert!(r.programs.is_empty(), "dot queries are file-type only");
        assert_eq!(idx.file_types[r.file_types[0].0].ext, ".pdf");

        run(&idx, "notepad", &mut r);
        assert_eq!(idx.programs[r.programs[0].0].name, "Notepad++");

        run(&idx, "zzzz", &mut r);
        assert!(r.programs.is_empty() && r.file_types.is_empty() && r.protocols.is_empty());
    }

    #[test]
    fn exact_extension_beats_friendly_name_match() {
        let idx = index();
        let mut r = Results::default();
        run(&idx, "bat", &mut r);
        assert_eq!(idx.file_types[r.file_types[0].0].ext, ".bat");
        assert_eq!(group_order(&r)[0], Kind::FileType);
    }
}
