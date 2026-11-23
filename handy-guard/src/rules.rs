//! Exact phrase rules for custom words: "task hive" -> "Taskhive".
//!
//! Handy's own custom-word feature matches loosely (fuzzy), and on 200 of the author's recordings it
//! put a custom word in place of correct words 59 times ("to give" -> "Taskhive", "another
//! agent" -> "Anthropic", "parked" -> "Parakeet"). These rules match whole words exactly
//! (case and punctuation ignored), so a rule only fires on the phrase it names.
//!
//! Rules file: one rule per line, `from words => To`. Lines that start with `#` are comments.
//! A rule that starts with `^ ` fires only at the start of a sentence (the first word of a
//! line, or the first word after `.`, `!`, `?` or `:`). Filler words such as "uh" or "yeah"
//! before it do not count. A rule that maps a phrase to itself ('at scale => at scale') keeps
//! that phrase, because longer rules win. When the heard word starts with a capital letter,
//! the replacement starts with a capital letter too.

use crate::norm;

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub from: Vec<String>,
    pub to: String,
    /// Fires only at the start of a sentence.
    pub anchored: bool,
}

/// Words that can come before a sentence-start rule without breaking it.
const FILLERS: &[&str] = &[
    "uh", "um", "ah", "oh", "er", "hmm", "yeah", "yes", "ok", "okay", "so", "and", "but", "well",
    "alright",
];

/// Built-in rules, used when there is no rules file.
pub const DEFAULT_RULES: &str = "\
# Custom words of the author's setup. Only exact phrases; no loose matching.
task hive => Taskhive
task-hive => Taskhive
taskhive => Taskhive
handy glow => HandyGlow
handyglow => HandyGlow
clawed => Claude
claude code => Claude Code
cloud code => Claude Code
clawed code => Claude Code
cloud agent view => Claude agent view
clawed agent view => Claude agent view
cloud setup => Claude setup
cloud set up => Claude setup
cloud model => Claude model
cloud models => Claude models
cloud advisor => Claude advisor
cloud session => Claude session
cloud sessions => Claude sessions
cloud max => Claude Max
cloud plan => Claude plan
cloud agent => Claude agent
cloud agents => Claude agents
t mux => tmux
tea mux => tmux
ghost tea => Ghostty
ghosty => Ghostty
task warrior => Taskwarrior
neo vim => Neovim
lazy vim => LazyVim
herder => herdr
anthropic => Anthropic
claw setup => Claude setup
claw mem => claude-mem
clawed mem => claude-mem

# Parakeet often hears 'skill' as 'scale'.
# In the author's speech 'scale' almost always means 'skill'. These phrases keep the real word.
scale => skill
scales => skills
at scale => at scale
to scale => to scale
scale up => scale up
scale down => scale down
scale out => scale out
scale well => scale well
scales well => scales well
scale better => scale better
scale of => scale of
scale factor => scale factor
large scale => large scale
small scale => small scale
full scale => full scale
gray scale => gray scale
grey scale => grey scale
severity scale => severity scale
sliding scale => sliding scale
time scale => time scale
on a scale => on a scale
scale the => scale the
scale it => scale it
scale them => scale them
scale horizontally => scale horizontally
scale vertically => scale vertically

# Parakeet hears 'accept' as 'except', mostly as the first word ('Except all five').
^ except => accept
except for => except for
except when => except when
except if => except if
except where => except where
except maybe => except maybe
hive except => hive accept
except all => accept all
except merge => accept merge
ahead except => ahead accept
accepting criteria => acceptance criteria
accepting criterion => acceptance criterion
exception criteria => acceptance criteria
exceptions criteria => acceptance criteria

# Other names that Parakeet hears wrong.
wig on lan => wake on LAN
wig on land => wake on LAN
wig on => wake on
wake on land => wake on LAN
wake on nan => wake on LAN
wake on lan => wake on LAN
heiku => Haiku
high cruise => Haiku
bitward => Bitwarden
logo models => local models
quen => Qwen
zem browser => Zen browser
";

pub fn parse_rules(text: &str) -> Vec<Rule> {
    let mut rules: Vec<Rule> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let (a, b) = l.split_once("=>")?;
            // "^ word" and "^word" both anchor the rule to the start of a sentence.
            let (a, anchored) = match a.trim_start().strip_prefix('^') {
                Some(rest) => (rest, true),
                None => (a, false),
            };
            let from: Vec<String> = a
                .split_whitespace()
                .map(norm)
                .filter(|w| !w.is_empty())
                .collect();
            let to = b.trim().to_string();
            (!from.is_empty() && !to.is_empty()).then_some(Rule { from, to, anchored })
        })
        .collect();
    // Longer phrases first, so "claude code" wins over a shorter rule.
    rules.sort_by(|a, b| b.from.len().cmp(&a.from.len()));
    // The same phrase twice ("task-hive" and "taskhive" read the same): the first one stays.
    let mut kept: Vec<Rule> = Vec::new();
    for r in rules {
        match kept.iter().find(|k| k.from == r.from && k.anchored == r.anchored) {
            Some(k) if k.to != r.to => eprintln!(
                "handy-guard: rule \"{}\" is given twice with different results (\"{}\", \"{}\"); the first one is used",
                r.from.join(" "),
                k.to,
                r.to
            ),
            Some(_) => {}
            None => kept.push(r),
        }
    }
    kept
}

/// Split a word into (leading punctuation, core, trailing punctuation).
fn split_punct(w: &str) -> (&str, &str, &str) {
    let start = w.find(|c: char| c.is_alphanumeric()).unwrap_or(w.len());
    let end = w
        .rfind(|c: char| c.is_alphanumeric())
        .map(|i| i + w[i..].chars().next().unwrap().len_utf8())
        .unwrap_or(start);
    (&w[..start], &w[start..end], &w[end..])
}

/// True when word `i` starts a sentence: nothing but filler words stand between it and the
/// start of the line or the last `.`, `!`, `?` or `:`.
fn sentence_start(words: &[&str], normed: &[String], i: usize) -> bool {
    let mut j = i;
    while j > 0 {
        j -= 1;
        if split_punct(words[j]).2.contains(['.', '!', '?', ':']) {
            return true;
        }
        if !normed[j].is_empty() && !FILLERS.contains(&normed[j].as_str()) {
            return false;
        }
    }
    true
}

/// Give `to` a capital first letter when the heard word had one. An all-caps heard phrase
/// ("CLOUD CODE") gets the replacement as written, with a capital first letter. A single heard
/// letter ("T mux") does not count as a capital word, so "tmux" stays "tmux".
fn keep_case(heard: &str, to: &str) -> String {
    let heard_upper =
        heard.chars().count() > 1 && heard.chars().next().is_some_and(char::is_uppercase);
    let mut c = to.chars();
    match c.next() {
        Some(f) if heard_upper && f.is_lowercase() => f.to_uppercase().chain(c).collect(),
        _ => to.to_string(),
    }
}

/// Apply the rules to text. Returns the new text and the number of replacements.
pub fn apply(text: &str, rules: &[Rule]) -> (String, usize) {
    let mut count = 0;
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            let before = count;
            let normed: Vec<String> = words.iter().map(|w| norm(split_punct(w).1)).collect();
            let mut out: Vec<String> = Vec::new();
            let mut i = 0;
            'outer: while i < words.len() {
                for r in rules {
                    let n = r.from.len();
                    if i + n <= words.len()
                        && normed[i..i + n] == r.from[..]
                        && (!r.anchored || sentence_start(&words, &normed, i))
                        // A phrase does not continue across the end of a sentence.
                        && !words[i..i + n - 1].iter().any(|w| split_punct(w).2.contains(['.', '!', '?', ':']))
                    {
                        let to_words: Vec<&str> = r.to.split(' ').collect();
                        let replaced = if n > 1 && to_words.len() == n {
                            // Same number of words: replace word by word, so inner commas stay.
                            (0..n)
                                .map(|k| {
                                    let (lead, core, trail) = split_punct(words[i + k]);
                                    format!("{lead}{}{trail}", keep_case(core, to_words[k]))
                                })
                                .collect::<Vec<_>>()
                                .join(" ")
                        } else {
                            let (lead, core, _) = split_punct(words[i]);
                            let trail = split_punct(words[i + n - 1]).2;
                            format!("{lead}{}{trail}", keep_case(core, &r.to))
                        };
                        // A rule that only fixes case must not change a word that is already right.
                        let original = words[i..i + n].join(" ");
                        if replaced != original {
                            count += 1;
                        }
                        out.push(replaced);
                        i += n;
                        continue 'outer;
                    }
                }
                out.push(words[i].to_string());
                i += 1;
            }
            // Only a line that changed gets its spaces collapsed; other lines stay as they were.
            if count == before {
                line.to_string()
            } else {
                out.join(" ")
            }
        })
        .collect();
    (lines.join("\n"), count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Vec<Rule> {
        parse_rules(DEFAULT_RULES)
    }

    #[test]
    fn joins_split_custom_words() {
        let (t, n) = apply("can you add a task hive? and open Claude code.", &rules());
        assert_eq!(t, "can you add a Taskhive? and open Claude Code.");
        assert_eq!(n, 2);
    }

    #[test]
    fn leaves_normal_words_alone() {
        let text = "Is there any way to give the ability to another agent, I parked it in the cloud backup, max height";
        let (t, n) = apply(text, &rules());
        assert_eq!(t, text);
        assert_eq!(n, 0);
    }

    #[test]
    fn skill_heard_as_scale() {
        let r = rules();
        assert_eq!(
            apply("Is there a scale for the cargo CLI?", &r).0,
            "Is there a skill for the cargo CLI?"
        );
        assert_eq!(
            apply("it is doing the scale test. Scales are good", &r).0,
            "it is doing the skill test. Skills are good"
        );
        let real = "it runs at scale, so scale up on a scale of 1 to 10";
        assert_eq!(apply(real, &r), (real.to_string(), 0));
    }

    #[test]
    fn accept_heard_as_except() {
        let r = rules();
        assert_eq!(apply("Except.", &r).0, "Accept.");
        assert_eq!(apply("Uh except all five.", &r).0, "Uh accept all five.");
        assert_eq!(
            apply("Except it looks good.", &r).0,
            "Accept it looks good."
        );
        assert_eq!(apply("Ah hive except.", &r).0, "Ah hive accept.");
        assert_eq!(apply("Go ahead, except.", &r).0, "Go ahead, accept.");
        assert_eq!(
            apply("[Context: general]\nExcept, also about that inbox.", &r).0,
            "[Context: general]\nAccept, also about that inbox."
        );
        assert_eq!(
            apply("all good. Yeah, except that task", &r).0,
            "all good. Yeah, accept that task"
        );
        assert_eq!(
            apply("all those accepting criteria", &r).0,
            "all those acceptance criteria"
        );
    }

    #[test]
    fn no_match_across_sentence_end() {
        let r = rules();
        assert_eq!(
            apply("It is in the cloud. Agents run there.", &r).0,
            "It is in the cloud. Agents run there."
        );
        assert_eq!(apply("we scale the service", &r).0, "we scale the service");
    }

    #[test]
    fn matches_across_double_spaces_and_tabs() {
        let r = rules();
        assert_eq!(
            apply("add a task  hive now", &r),
            ("add a Taskhive now".to_string(), 1)
        );
        assert_eq!(apply("add a task\thive now", &r).0, "add a Taskhive now");
        // A line without a match keeps its spaces.
        let plain = "two  spaces\tand a tab";
        assert_eq!(apply(plain, &r), (plain.to_string(), 0));
    }

    #[test]
    fn caret_without_space_still_anchors() {
        let r = parse_rules("^except => accept\nexcept for => except for");
        assert!(r[r.len() - 1].anchored || r[0].anchored);
        assert_eq!(apply("Except it works", &r).0, "Accept it works");
        assert_eq!(
            apply("it works except it fails", &r).0,
            "it works except it fails"
        );
    }

    #[test]
    fn duplicate_rules_keep_the_first() {
        // "task-hive" and "taskhive" read the same, so DEFAULT_RULES has one rule for them.
        let r = rules();
        assert_eq!(r.iter().filter(|x| x.from == ["taskhive"]).count(), 1);
        let r = parse_rules("foo bar => First\nfoo  bar => Second");
        assert_eq!(r.len(), 1);
        assert_eq!(apply("a foo bar b", &r).0, "a First b");
    }

    #[test]
    fn case_of_the_heard_words() {
        let r = rules();
        // All caps: the replacement as written, first letter capital.
        assert_eq!(apply("open CLOUD CODE now", &r).0, "open Claude Code now");
        assert_eq!(apply("TASK HIVE is good", &r).0, "Taskhive is good");
        // One heard letter does not make "tmux" a sentence start.
        assert_eq!(apply("use T mux here", &r).0, "use tmux here");
        assert_eq!(apply("T mux is good", &r).0, "tmux is good");
        assert_eq!(apply("Tea mux is good", &r).0, "Tmux is good");
    }

    #[test]
    fn keeps_real_except() {
        let r = rules();
        for t in [
            "everything works except the paste",
            "Except for the logs, it is done",
            "it is fine, except when it hangs",
        ] {
            assert_eq!(apply(t, &r), (t.to_string(), 0), "{t}");
        }
    }

    #[test]
    fn keeps_punctuation_and_lines() {
        let (t, _) = apply(
            "[Styling: semi-formal]\n\"Clawed agent view,\" he said",
            &rules(),
        );
        assert_eq!(t, "[Styling: semi-formal]\n\"Claude agent view,\" he said");
    }
}
