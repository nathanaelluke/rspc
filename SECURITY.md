# Private data

rspc stores Spotify credentials and catalog data in `~/.cache/rspc`, and queue
history, pairing state and logs in `~/.local/state/rspc`. Your Soloist key belongs
in `~/.config/rspc/soloist-api-key`. These paths follow the XDG environment
variables. None of that data belongs in the repository.

Debug output can include local paths, server addresses and track metadata. Check
it before posting a report. Use `rspc --demo` when taking public screenshots.
The OAuth client identifier in `src/spotify/credentials.rs` is a public application
identifier, not a user's secret or Soloist key.

Run `python3 scripts/check-release.py` before publishing a tree. It checks Git's
candidate files and staged contents for runtime data, common secret formats and
personal paths, without printing matched values. Add `--history` to inspect
reachable Git blobs as well. This is a local guard against common mistakes, not
a guarantee that an arbitrary file is safe to publish; review unfamiliar binary
assets separately.

If a real credential has been committed, revoke it before sharing the repository.
Removing the file from the current checkout does not remove it from Git history.
