//! handy-guard: compare the raw transcript (with token confidences) and the cleaned text, and
//! put back confident content words that the cleanup step removed or replaced.
//!
//! The rules are deliberately narrow. Cleanup is allowed to:
//! - change case, punctuation and number format,
//! - remove filler words, repeated words and false starts,
//! - expand or contract short forms ("you're" and "you are").
//!
//! It is not allowed to remove or replace a content word that the speech model was sure about
//! (lowest token probability at or above the threshold). Such a word is put back.

pub mod rules;
pub mod serve;

/// One word of the raw transcript with the lowest probability of its tokens.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub text: String,
    pub p: f32,
}

/// One change that the guard undid.
#[derive(Debug, Clone, PartialEq)]
pub struct Restore {
    pub word: String,
    pub p: f32,
    /// The cleaned words that replaced it (empty when cleanup removed it).
    pub replaced_by: String,
}

#[derive(Debug, Clone)]
pub struct Params {
    /// A word needs at least this probability to be protected.
    pub min_p: f32,
}

impl Default for Params {
    fn default() -> Self {
        Params { min_p: 0.97 }
    }
}

const FILLERS: &[&str] = &[
    "uh",
    "um",
    "ah",
    "oh",
    "er",
    "erm",
    "hmm",
    "mm",
    "huh",
    "like",
    "yeah",
    "okay",
    "ok",
    "so",
    "well",
    "just",
    "actually",
    "basically",
    "really",
    "kind",
    "sort",
    "know",
    "mean",
];

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "if", "then", "of", "to", "in", "on", "at", "for",
    "with", "by", "from", "as", "is", "are", "was", "were", "be", "been", "am", "do", "does",
    "did", "have", "has", "had", "it", "its", "this", "that", "these", "those", "there", "here",
    "i", "you", "he", "she", "we", "they", "me", "my", "your", "our", "their", "them", "him",
    "her", "us", "what", "which", "who", "how", "about", "not", "no", "can", "will", "would",
    "should", "could", "also", "too", "very", "some", "any", "all", "more", "into", "up", "out",
    "over", "go", "get", "got", "want", "think", "wait", "let", "lets", "let's", "i'm", "it's",
    "that's", "don't", "dont", "i'll", "you're",
];

const NUMBER_WORDS: &[&str] = &[
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    "eleven", "twelve", "twenty", "thirty", "forty", "fifty", "hundred", "thousand", "point",
    "percent", "first", "second", "third",
];

/// Lower case, letters, digits and apostrophes only.
pub fn norm(w: &str) -> String {
    w.chars()
        .filter(|c| c.is_alphanumeric() || *c == '\'')
        .flat_map(|c| c.to_lowercase())
        .collect::<String>()
        .trim_matches('\'')
        .to_string()
}

fn is_content(n: &str) -> bool {
    n.chars().count() >= 3
        && !FILLERS.contains(&n)
        && !STOPWORDS.contains(&n)
        && !NUMBER_WORDS.contains(&n)
        && !n.chars().all(|c| c.is_ascii_digit())
}

/// Parse the `tokens:` block of `transcribe-cli` output into words.
/// A token that starts with a space starts a new word.
pub fn parse_cli_tokens(out: &str) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    let mut on = false;
    for line in out.lines() {
        if line.starts_with("tokens:") {
            on = true;
            continue;
        }
        if !on {
            continue;
        }
        if !line.starts_with("  [") {
            break;
        }
        let Some(pos) = line.find("p=") else { continue };
        let rest = &line[pos + 2..];
        let Some(sp) = rest.find(' ') else { continue };
        let p: f32 = rest[..sp].parse().unwrap_or(1.0);
        let tok = &rest[sp + 1..];
        if let Some(t) = tok.strip_prefix(' ') {
            words.push(Word {
                text: t.to_string(),
                p,
            });
        } else if let Some(last) = words.last_mut() {
            last.text.push_str(tok);
            last.p = last.p.min(p);
        } else {
            words.push(Word {
                text: tok.to_string(),
                p,
            });
        }
    }
    words.retain(|w| !norm(&w.text).is_empty() || !w.text.trim().is_empty());
    words
}

/// Longest common subsequence alignment on normalized words.
/// Returns pairs (raw index, cleaned index) of matched words, in order.
fn align(raw: &[String], clean: &[String]) -> Vec<(usize, usize)> {
    let (n, m) = (raw.len(), clean.len());
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if !raw[i].is_empty() && raw[i] == clean[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut pairs) = (0, 0, Vec::new());
    while i < n && j < m {
        if !raw[i].is_empty() && raw[i] == clean[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}

/// Is raw word `k` part of a disfluency: a repeat of a nearby word, or a false start that the
/// speaker restarted ("I want I think": the span starts with the same word as what follows it)?
fn disfluent(raw: &[String], span: (usize, usize), k: usize) -> bool {
    let w = &raw[k];
    let near = |d: isize| -> bool {
        let idx = k as isize + d;
        idx >= 0 && (idx as usize) < raw.len() && &raw[idx as usize] == w
    };
    if near(-1) || near(1) || near(-2) || near(2) {
        return true;
    }
    let (s, e) = span;
    if e < raw.len() && raw[s] == raw[e] {
        return true;
    }
    // A span whose first word appears again within the next few raw words is a restart.
    raw[e..raw.len().min(e + 4)].contains(&raw[s])
}

/// Apply the guard. Returns the guarded text and the list of words it put back.
pub fn guard(raw: &[Word], cleaned: &str, params: &Params) -> (String, Vec<Restore>) {
    let raw_n: Vec<String> = raw.iter().map(|w| norm(&w.text)).collect();
    let clean_words: Vec<&str> = cleaned.split_whitespace().collect();
    let clean_n: Vec<String> = clean_words.iter().map(|w| norm(w)).collect();
    let pairs = align(&raw_n, &clean_n);

    let mut out: Vec<String> = Vec::new();
    let mut restores = Vec::new();
    let (mut ri, mut ci) = (0usize, 0usize);
    let mut anchors = pairs.clone();
    anchors.push((raw.len(), clean_words.len()));
    for (ra, ca) in anchors {
        // Gap: raw[ri..ra] was removed or replaced by clean[ci..ca].
        let gap_clean: Vec<&str> = clean_words[ci..ca].to_vec();
        let gap_clean_n: Vec<&String> = clean_n[ci..ca].iter().collect();
        let joined_clean: String = gap_clean_n
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("");
        let mut put_back: Vec<(usize, String)> = Vec::new();
        for k in ri..ra {
            let n = &raw_n[k];
            if raw[k].p < params.min_p || !is_content(n) {
                continue;
            }
            if disfluent(&raw_n, (ri, ra), k) {
                continue;
            }
            // Same word in another form (case, a joined or split word, a contraction).
            if joined_clean.contains(n.as_str())
                || gap_clean_n
                    .iter()
                    .any(|c| c.contains(n.as_str()) || n.contains(c.as_str()) && c.len() >= 3)
            {
                continue;
            }
            // Number format change ("one" -> "1") is allowed; digits in the replacement mean that.
            if gap_clean
                .iter()
                .any(|c| c.chars().any(|ch| ch.is_ascii_digit()))
            {
                continue;
            }
            put_back.push((
                k,
                raw[k]
                    .text
                    .trim_end_matches(|c: char| c == ',' || c == '.' || c == '?' || c == '!')
                    .to_string(),
            ));
            restores.push(Restore {
                word: raw[k].text.clone(),
                p: raw[k].p,
                replaced_by: gap_clean.join(" "),
            });
        }
        // Keep the cleaned words of the gap, and put the protected raw words in front of them.
        // When cleanup replaced a protected word, the raw word wins and the replacement is dropped.
        if !put_back.is_empty() && !gap_clean.is_empty() && put_back.len() == ra - ri {
            for (_, w) in &put_back {
                out.push(w.clone());
            }
            if let Some(last) = gap_clean.last() {
                // Keep trailing punctuation of the replaced span.
                let tail: String = last
                    .chars()
                    .rev()
                    .take_while(|c| c.is_ascii_punctuation())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                if let Some(o) = out.last_mut() {
                    o.push_str(&tail);
                }
            }
        } else {
            for (_, w) in &put_back {
                out.push(w.clone());
            }
            out.extend(gap_clean.iter().map(|s| s.to_string()));
        }
        if ca < clean_words.len() {
            out.push(clean_words[ca].to_string());
        }
        ri = ra + 1;
        ci = ca + 1;
    }
    (out.join(" "), restores)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str, p: f32) -> Vec<Word> {
        s.split_whitespace()
            .map(|w| Word {
                text: w.to_string(),
                p,
            })
            .collect()
    }

    #[test]
    fn keeps_punctuation_and_contractions() {
        let raw = words("This is on top of what you're implementing", 0.99);
        let (out, r) = guard(
            &raw,
            "This is on top of what you are implementing.",
            &Params::default(),
        );
        assert_eq!(out, "This is on top of what you are implementing.");
        assert!(r.is_empty());
    }

    #[test]
    fn allows_repeats_and_false_starts() {
        let raw = words("Wait, I want I think I know my password", 0.99);
        let (out, r) = guard(
            &raw,
            "Wait, I think I know my password.",
            &Params::default(),
        );
        assert_eq!(out, "Wait, I think I know my password.");
        assert!(r.is_empty());
        let raw = words("Parakeet Parakeet talking", 0.99);
        let (_, r) = guard(&raw, "Parakeet talking", &Params::default());
        assert!(r.is_empty());
    }

    #[test]
    fn allows_number_format() {
        let raw = words("Go ahead and add one and two", 0.99);
        let (out, r) = guard(&raw, "Go ahead and add 1 and 2.", &Params::default());
        assert_eq!(out, "Go ahead and add 1 and 2.");
        assert!(r.is_empty());
    }

    #[test]
    fn puts_back_a_removed_confident_word() {
        let raw = words("move forward with transcribe cpp for that", 0.99);
        let (out, r) = guard(
            &raw,
            "move forward with transcribe for that.",
            &Params::default(),
        );
        assert_eq!(out, "move forward with transcribe cpp for that.");
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn leaves_low_confidence_words_to_cleanup() {
        let mut raw = words("move forward with transcribe cpp for that", 0.99);
        raw[4].p = 0.80;
        let (out, r) = guard(
            &raw,
            "move forward with transcribe for that.",
            &Params::default(),
        );
        assert_eq!(out, "move forward with transcribe for that.");
        assert!(r.is_empty());
    }

    #[test]
    fn replaced_confident_word_wins() {
        let raw = words("open the hive board now", 0.99);
        let (out, r) = guard(&raw, "open the hype board now.", &Params::default());
        assert_eq!(out, "open the hive board now.");
        assert_eq!(r[0].replaced_by, "hype");
    }

    #[test]
    fn parses_cli_tokens() {
        let out = "text: x\ntokens: 3\n  [   0.00 ->    0.08] p=0.972  U\n  [   0.08 ->    0.24] p=0.994 h\n  [   0.32 ->    0.48] p=0.992  yeah\n  realtime: 6x\n";
        let w = parse_cli_tokens(out);
        assert_eq!(
            w,
            vec![
                Word {
                    text: "Uh".into(),
                    p: 0.972
                },
                Word {
                    text: "yeah".into(),
                    p: 0.992
                }
            ]
        );
    }
}
