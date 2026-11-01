# AGENTS.md

Rules for any agent (or human) changing this repository.

## Testing

- Every change must be tested before it is considered done: run `cargo test`
  and it must pass.
- Bug fixes need a regression test that fails without the fix; new behavior
  needs test coverage in `tests/` or the relevant module.

## Commits

- All commits follow [Conventional Commits](https://www.conventionalcommits.org/)
  1.0.0: `type(scope)?(?)!?: description`.
- Allowed types: `feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `build`,
  `ci`, `chore`, `style`. Subject: imperative mood, no trailing period.
- Examples: `feat(ui): add VRAM channel grid`, `fix(fdinfo): divide by capacity
  zero on missing engine`, `docs: document snapshot mode`.
