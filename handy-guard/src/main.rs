//! handy-guard command line.
//!
//!   handy-guard serve [--listen ADDR] [--upstream ADDR] [--rules FILE] [--deadline-ms N]
//!       Run the proxy between Handy and the cleanup model server.
//!       Defaults: listen 127.0.0.1:11436, upstream 127.0.0.1:11435,
//!       rules ~/.config/handy-guard/rules (no rules when the file does not exist).
//!
//!   handy-guard rules-check OUT_DIR [--rules FILE]
//!       Offline check on saved recordings. OUT_DIR is the research/out folder of the
//!       transcribe.cpp study: all.tsv (file, Handy raw text, cleaned text) and
//!       <file>/full.out (the speech model's own decode). Compares Handy's custom-word
//!       output with the exact rules applied to the model's own text.
//!
//!   handy-guard apply [--rules FILE]
//!       Apply the rules to each line of standard input. Prints only the lines that change,
//!       as "before<TAB>after". Use it to test new rules on saved transcripts.
//!
//!   handy-guard eval OUT_DIR [MIN_P]
//!       Research: the confidence guard (put back confident words that cleanup changed).

use handy_guard::rules::{apply, parse_rules, Rule};
use handy_guard::{guard, norm, parse_cli_tokens, serve, Params};
use std::fs;
use std::path::{Path, PathBuf};

const CUSTOM_WORDS: &[&str] = &[
    "claude",
    "anthropic",
    "herdr",
    "tmux",
    "ghostty",
    "taskhive",
    "taskwarrior",
    "neovim",
    "lazyvim",
    "parakeet",
    "handyglow",
];

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn load_rules(args: &[String]) -> Vec<Rule> {
    let path = flag(args, "--rules")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config/handy-guard/rules"));
    match fs::read_to_string(&path) {
        Ok(t) => parse_rules(&t),
        Err(_) => Vec::new(), // No rules file: no rules. DEFAULT_RULES is test data of the author.
    }
}

fn count_custom(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(norm)
        .filter(|w| CUSTOM_WORDS.contains(&w.as_str()))
        .collect()
}

fn rules_check(dir: &Path, rules: &[Rule]) -> std::io::Result<()> {
    let tsv = fs::read_to_string(dir.join("all.tsv"))?;
    let (mut recs, mut handy_ins, mut rule_ins, mut differ) = (0, 0, 0, 0);
    let mut examples = Vec::new();
    for line in tsv.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 2 {
            continue;
        }
        let stem = cols[0].trim_end_matches(".wav");
        let Ok(out) = fs::read_to_string(dir.join(stem).join("full.out")) else {
            continue;
        };
        let Some(model) = out.lines().find_map(|l| l.strip_prefix("text: ")) else {
            continue;
        };
        recs += 1;
        let (ruled, _) = apply(model, rules);
        let base = count_custom(model).len();
        let h = count_custom(cols[1]).len().saturating_sub(base);
        let r = count_custom(&ruled).len().saturating_sub(base);
        handy_ins += h;
        rule_ins += r;
        if h != r {
            differ += 1;
            examples.push((cols[1].to_string(), ruled));
        }
    }
    println!("recordings: {recs}");
    println!("custom words added by Handy's loose matching: {handy_ins}");
    println!("custom words added by the exact rules:        {rule_ins}");
    println!("recordings where the two differ: {differ}\n");
    for (h, r) in examples {
        println!("HANDY: {h}\nRULES: {r}\n");
    }
    Ok(())
}

fn research_eval(dir: &Path, min_p: f32) -> std::io::Result<()> {
    let params = Params { min_p };
    let tsv = fs::read_to_string(dir.join("all.tsv"))?;
    let (mut total, mut changed, mut restored) = (0, 0, 0);
    for line in tsv.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 3 || cols[2].trim().is_empty() {
            continue;
        }
        let stem = cols[0].trim_end_matches(".wav");
        let Ok(out) = fs::read_to_string(dir.join(stem).join("full.out")) else {
            continue;
        };
        let words = parse_cli_tokens(&out);
        if words.is_empty() {
            continue;
        }
        total += 1;
        let (text, r) = guard(&words, cols[2], &params);
        if !r.is_empty() {
            changed += 1;
            restored += r.len();
            println!("CLEANUP: {}\nGUARDED: {text}", cols[2]);
            for x in r {
                println!(
                    "  put back \"{}\" (p={:.3}, replaced by \"{}\")",
                    x.word, x.p, x.replaced_by
                );
            }
            println!();
        }
    }
    println!("min_p {min_p}: {changed} of {total} cleanups changed, {restored} words put back");
    Ok(())
}

fn apply_stdin(rules: &[Rule]) -> std::io::Result<()> {
    use std::io::BufRead;
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        let (out, n) = apply(&line, rules);
        if n > 0 {
            println!("{line}\t{out}");
        }
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("serve") => serve::run(serve::Config {
            listen: flag(&args, "--listen").unwrap_or_else(|| "127.0.0.1:11436".into()),
            upstream: flag(&args, "--upstream").unwrap_or_else(|| "127.0.0.1:11435".into()),
            rules: load_rules(&args),
            log: {
                let d = home().join(".local/state/handy-guard");
                fs::create_dir_all(&d).ok().map(|_| d.join("requests.log"))
            },
            deadline: std::time::Duration::from_millis(
                flag(&args, "--deadline-ms")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(8000),
            ),
            read_timeout: std::time::Duration::from_secs(10),
        }),
        Some("rules-check") if args.len() >= 3 => {
            rules_check(Path::new(&args[2]), &load_rules(&args))
        }
        Some("apply") => apply_stdin(&load_rules(&args)),
        Some("eval") if args.len() >= 3 => research_eval(
            Path::new(&args[2]),
            args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0.97),
        ),
        _ => {
            eprintln!("usage: handy-guard serve [--listen ADDR] [--upstream ADDR] [--rules FILE] [--deadline-ms N] [--deadline-ms N]\n       handy-guard rules-check OUT_DIR [--rules FILE]\n       handy-guard apply [--rules FILE]\n       handy-guard eval OUT_DIR [MIN_P]");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("handy-guard: {e}");
        std::process::exit(1);
    }
}
