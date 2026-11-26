# Oak

Oak is a coding agent app for Windows and Linux, written in Rust. [SPEC.md](SPEC.md) describes
how it behaves, in full (the repository's layout, building and testing, every part of the app):
read the parts your change touches, and keep them true with it. Don't edit README.md.

```sh
cargo build && cargo clippy --all-targets
```

Don't add or run tests unless the work is complex enough to need them (`cargo test`, or
`cargo test <name>` for the ones it touches).

## When you're finished

1. **If you changed Oak** (this repository): build and install it with
   `../oak/scripts/install-app.sh`. It builds a release in `target/install/` and installs it as
   `target/release/oak.exe` here, even while that Oak runs (the running exe is renamed out of
   the way); the new one takes over when Oak restarts.
2. **If you changed a plugin** (`../oak/plugins/`): build and install it with
   `../oak/scripts/build.sh --release --install <plugin> ...`, which copies it into Oak's
   plugins folder; it loads when Oak next starts.
3. **Commit and push.** Unless the user asks to keep changes local, commit the work for the task
   and push it to the branch's upstream (`origin/master` here; `origin/main` in `../oak`). If the
   remote has advanced, rebase on it, resolve conflicts and rerun the affected checks before
   pushing. Never force-push or include unrelated edits. Confirm the push succeeded; if it's
   blocked, say what remains local and why.
