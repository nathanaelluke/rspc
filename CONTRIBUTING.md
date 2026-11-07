# Contributing

The workspace has two executables: `rspc` runs the player daemon and launcher;
`crates/rspc-ui` builds the terminal interface. Start with [ARCHITECTURE.md](ARCHITECTURE.md)
for behavior and [CUSTOMIZING.md](CUSTOMIZING.md) for appearance changes.

Use `nix develop` or install Rust, `pkg-config` and the OpenSSL development
libraries. Run the commands in [AGENTS.md](AGENTS.md) before submitting changes.
The process tests need both release executables, and mixed-source tests also need
MPD. They use temporary state and simulated playback, without a Spotify account.

Keep changes focused. Use the existing configuration model rather than adding
another way to express the same setting. Prefer named types and small functions
at protocol boundaries; preserve errors with context rather than inspecting their
display text. Add regression tests for behavior that failed, not just for the
shape of an implementation.

Format with stable rustfmt and run Clippy. Explain behavior changes and relevant
validation in the pull request. Keep upstream copyright notices with derived
code. Please use demo data in screenshots and inspect logs before attaching them.
