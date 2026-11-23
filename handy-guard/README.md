# handy-guard

handy-guard makes Handy's custom words more accurate. It is a small proxy between [Handy](https://handy.computer) and the server of the text cleanup model (S1-mini on Ollama).

## The problem

Handy's custom-word feature matches words loosely. On 200 saved recordings, it put a custom word into the text 59 times, and many of these were wrong. The speech model (Parakeet) had heard the correct words with almost 100% confidence:

| The speaker said | Handy wrote |
|---|---|
| to give | Taskhive |
| another agent | Anthropic |
| parked tasks | Parakeet tasks |
| cloud tool | Claude tool |
| max height | tmux height |
| Handy is | HandyGlow |

## How it works

1. Handy's loose custom-word list is empty.
2. Handy sends the cleanup request to handy-guard (port 11436) and not directly to Ollama (port 11435).
3. handy-guard replaces only exact phrases in the text, for example "task hive" with "Taskhive" and "clawed" with "Claude". Then it sends the request on to Ollama and returns the answer.

On the same 200 recordings, the exact rules add 28 custom words, and almost all of them are correct.

## Rules

The rules file is `~/.config/handy-guard/rules`. Each line is one rule: `from words => To`. Case and punctuation do not matter. When the file does not exist, the built-in rules in `src/rules.rs` apply.

## Commands

| Command | What it does |
|---|---|
| `handy-guard serve` | Runs the proxy (127.0.0.1:11436 to 127.0.0.1:11435) |
| `handy-guard rules-check DIR` | Compares Handy's output with the rules on saved recordings |
| `scripts/switch-on.sh` | Starts the service and points Handy at it (Handy restarts) |
| `scripts/switch-off.sh` | Restores Handy's settings from the backup and stops the service |

## Research

The `eval` command and `guard()` in `src/lib.rs` test a second idea: use the token confidence of the speech model to stop cleanup from changing words the model was sure about. The test showed too much noise for daily use, so the proxy does not use it.

Build and test: `cargo build --release` and `cargo test`.
