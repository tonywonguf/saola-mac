# saola for macOS

saola is local voice dictation. You hold one key, you speak, and clean text comes into the app that has focus. No audio and no text goes out of your Mac.

This repository is the macOS version. The Linux version (Wayland, sway) is at [tonywonguf/saola](https://github.com/tonywonguf/saola).

## What it does

saola connects three parts:

1. [Handy](https://handy.computer) records your voice and changes the speech to text with a Parakeet model. Handy also types the result into the app.
2. S1-mini is a small language model (0.6 billion parameters) that cleans the text. It removes "um" and "uh", repairs corrections ("Tuesday, no, Wednesday" becomes "Wednesday") and adds punctuation. Ollama operates S1-mini on the GPU of the Mac.
3. handy-guard is a small local server between Handy and Ollama. It puts your own word rules into the text ("get hub" becomes "GitHub"). If Ollama does not answer in 8 seconds, handy-guard gives back the text without cleanup, so a dictation never stops.

## Install

You need macOS and [Homebrew](https://brew.sh). Use a Mac with Apple silicon (M1 or newer) if you can, because S1-mini then operates on the GPU.

1. Install saola:

   ```sh
   brew install tonywonguf/saola/saola
   ```

2. Install and connect the parts. This step installs Handy and Ollama, downloads S1-mini (484 MB) and starts handy-guard:

   ```sh
   saola setup
   ```

3. If `saola setup` tells you that Handy has no settings yet, open Handy one time:
   1. Give Handy access to the microphone and to Accessibility (System Settings, Privacy and Security). Handy needs Accessibility to type the text.
   2. In Handy, download a Parakeet model and make one dictation.
   3. Quit Handy from the menu bar.
   4. Run `saola setup` again.

4. Do a test of the cleanup without the microphone:

   ```sh
   saola test
   ```

To see a list of the steps first, with no changes, run `saola setup --dry-run`.

## Keys

| Key | Result |
|---|---|
| Option+Shift+Space | Dictation with cleanup by S1-mini |
| Option+Space | Raw dictation, no cleanup |

These are the default keys of Handy. You can change them in the Handy settings. In the Handy settings, you can also select push-to-talk (hold the key) or toggle (push one time to start and one time to stop).

## Commands

| Command | What it does |
|---|---|
| `saola setup` | Installs and connects all parts. You can run it again at any time. |
| `saola status` | Shows the state of Ollama, S1-mini, handy-guard and the Handy settings. |
| `saola test` | Sends one sample sentence through handy-guard and S1-mini. |
| `saola uninstall` | Stops handy-guard, puts back your Handy settings from before saola and removes S1-mini. |

## Word rules

handy-guard reads your rules from `~/.config/handy-guard/rules`. Write one rule on each line, in the form `heard words => Correct Words`. A rule fires only on the exact phrase. After a change, restart handy-guard with `brew services restart saola`.

## Privacy and security

handy-guard and Ollama listen only on 127.0.0.1. handy-guard refuses a request whose Host header is not `localhost` or `127.0.0.1`, so a web page cannot use it. The handy-guard log has counts and times, not your text. `saola setup` makes sure that the SHA-256 value of the S1-mini download is correct before it uses the file.

## Install without Homebrew

1. Install Rust, Ollama and Handy.
2. Build handy-guard: `cargo install --path handy-guard`.
3. Run `bin/saola setup`. Without Homebrew, it starts handy-guard with a LaunchAgent (`~/Library/LaunchAgents/com.tonywonguf.handy-guard.plist`).

## Status

The tests operate on GitHub macOS machines: the handy-guard tests, the settings helper tests, a dry run of `saola setup`, and `brew install` plus `brew test` of the formula. A full dictation on a real Mac is not tested yet. If a step fails on your Mac, open an issue with the output of `saola status`.

## License

Copyright Tony Wong (https://github.com/tonywonguf).

saola-mac uses the [PolyForm Noncommercial License 1.0.0](LICENSE.md). You can use, change and share the code for any noncommercial purpose. You cannot use it for a commercial purpose.

Handy, Ollama, Parakeet and S1-mini have their own licenses. S1-mini uses Apache-2.0 with a name term.
