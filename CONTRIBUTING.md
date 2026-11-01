# Contributing to xtop

Bug reports, hardware reports and pull requests are welcome.

## Building and checking

```sh
cargo build
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs the same commands and checks the minimum supported Rust version (1.88).

## Ground rules

- **Stay standalone.** xtop reads kernel interfaces (sysfs, `/proc`, DRM
  ioctls) directly. It does not shell out to other tools or link GPU
  runtimes, and its only crates are `ratatui` and `libc`. Discuss in an issue
  first before adding a dependency.
- **Stay unprivileged.** Every feature should work without root. A metric that
  only root can read should degrade gracefully and say why.
- **Cite the source.** When you add a metric, note in a comment which kernel
  file or uapi struct it comes from and its units.
- **Hardware reports help most.** If xtop shows something wrong on your GPU,
  open a bug with the output of `xtop --list` and `xtop --snapshot`.

## License

xtop is licensed under the GNU General Public License v2.0 only
(`GPL-2.0-only`). By contributing you agree that your contributions are
licensed under the same terms.
