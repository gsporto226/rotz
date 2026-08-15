# AGENTS.md

## Commit gate: run the CI checks before committing

The project's CI is defined in `.github/workflows/ci.yml`. Do **not** commit or push
until the local checks below pass — they mirror the exact commands the `lint` and
`test` jobs run. A failed workflow blocks merges and releases.

### Rust checks (mirror the `lint` job)

Run all three, in order:

```sh
cargo fmt --check
cargo clippy --all-features -- -D warnings
cargo clippy --features all-formats -- -D warnings
```

- These must pass with **zero warnings** (`-D warnings` turns warnings into errors).
- Fix formatting with `cargo fmt` before committing.
- Prefer fixing the underlying lint over adding `#[allow(...)]`. Only add an
  `#[allow]` when it is the project's existing convention (see `[lints.clippy]` in
  `Cargo.toml`). Do not change the `[lints.clippy]` policy to silence warnings.

### Tests (mirror the `test` job)

CI tests on `x86_64-unknown-linux-gnu`, `x86_64-apple-darwin` and
`x86_64-pc-windows-msvc`. Run at least on your current platform:

```sh
cargo test --features all-formats
```

### Docs (mirror `docs.yml`, only when `docs/**` changes)

```sh
cd docs && npm ci && npm run build
```

Requires Node.js 22. The docs site is a Docusaurus project; keep `docs/package-lock.json`
in sync if you add dependencies.

### Release notes

`build`/`release` only run on tags (`refs/tags/v*`). The `release` job also runs
`cargo publish --dry-run` before publishing. If you bump `Cargo.toml` version, also
update `CHANGELOG.md` under a matching version header and keep `Cargo.lock` in sync
(run `cargo build` after editing `Cargo.toml`).

## Development

- Edition 2024. Run `cargo fmt` before committing.
- `docs/` is a separate npm/Docusaurus project; Rust changes that touch the docs site
  should still build it (`npm ci && npm run build`).
