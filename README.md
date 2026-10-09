# al

`al` is the agent-side command-line tool for [Assistant Link](https://github.com/assistant-link/assistant-link): a
single Rust binary that pairs an agent host with an Assistant Link and keeps it connected. It is a thin client of the
Assistant Link Protocol ([ALP v1](https://github.com/assistant-link/backend/blob/main/docs/alp/v1.md)); agents can also
speak ALP directly.

| Command          | What it does                                                                            |
| ---------------- | --------------------------------------------------------------------------------------- |
| `al pair <code>` | Exchange a pairing code for a Link token and store it.                                  |
| `al status`      | Check that the stored Link is paired and that the service accepts it.                   |
| `al wait`        | Hold the connection open, so the Link shows online, until interrupted. Reconnects on its own. |

Every command prints JSON by default; add `--human` for text. `--server` (or `AL_SERVER`) picks the service and
defaults to `https://assistant-link.fly.dev`. The service's [`install.md`](https://assistant-link.fly.dev/install.md)
walks an agent through installing and pairing, including the exit codes.

## Install

```sh
cargo install --git https://github.com/assistant-link/cli al
```

## Development prerequisites

- [mise](https://mise.jdx.dev). This repository's `mise.toml` pins Rust, and `mise install` sets up rustup if it is
  missing.

The pinned Rust is the one preinstalled on GitHub Actions runners. `rust-toolchain.toml` pins the same version
with `rustfmt` and `clippy`, so `cargo` in this directory uses it even without mise, and `Cargo.toml` sets it as
`rust-version`. `make lint` fails if the three differ.

## Setup

Install the pinned toolchain:

```sh
mise trust
mise install
```

## Building and running

Run these from the repository root:

| Target           | What it does                                                                      |
| ---------------- | --------------------------------------------------------------------------------- |
| `make build`     | Build `al` into `target/debug/al`.                                                |
| `make test`      | Run the tests.                                                                    |
| `make run`       | Run `al` with `ARGS`, for example `make run ARGS="--help"`.                       |
| `make toolchain` | Check that `mise.toml`, `rust-toolchain.toml` and `Cargo.toml` use the same Rust. |
| `make lint`      | `make toolchain`, then `rustfmt` and `clippy` checks.                             |
| `make format`    | Reformat Rust sources in place with `rustfmt`.                                    |
| `make precommit` | The quality gate: `lint`, then `test`.                                            |
| `make clean`     | Remove `target/`.                                                                 |

## Quality gate

```sh
make precommit
```

`make precommit` is the single quality gate. Run it before every commit. It runs, in order:

- `make toolchain`: the three Rust versions match
- `cargo fmt --check`: formatting (fix with `make format`)
- `cargo clippy --all-targets -- -D warnings`: compiler and Clippy warnings, in tests too
- `cargo test`: the tests

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Licensed under the [MIT License](LICENSE).
