# al

`al` is the agent-side command-line tool for [Assistant Link](https://github.com/assistant-link/assistant-link): a
single Rust binary that pairs an agent host with an Assistant Link and keeps it connected. It is a thin client of the
Assistant Link Protocol ([ALP v1](https://github.com/assistant-link/backend/blob/main/docs/alp/v1.md)); agents can also
speak ALP directly.

| Command          | What it does                                                                                  |
| ---------------- | --------------------------------------------------------------------------------------------- |
| `al pair <code>` | Exchange a pairing code from the app for the Link key.                                        |
| `al status`      | Check once that the service accepts a connection with the Link key.                           |
| `al wait`        | Hold the connection open, so the Link shows online, until interrupted. Reconnects on its own. |

The service's [`install.md`](https://assistant-link.fly.dev/install.md) walks an agent through installing `al`,
pairing, and storing the key.

## Usage

### The Link key

`al` keeps no state on the host. `al pair` hands the Link key to you, and you store it wherever suits the host (the
macOS Keychain, the Linux Secret Service, a secret manager, or a file only your user can read) and supply it to
`al status` and `al wait` on every run:

- `--key-file <path>`: read the key from a file, ignoring surrounding whitespace; or
- `AL_LINK_KEY`: the key itself, when no `--key-file` is given.

`al` never takes the key itself as an argument, since arguments show up in `ps` and shell history. Because the key
travels with each command, one host user can run any number of Links side by side, each with its own key.

### Pairing

```sh
mkdir -p -m 700 ~/.config/assistant-link
al pair K7QM-2XPA --key-file ~/.config/assistant-link/hermes.key
```

`--key-file` creates a new file, readable only by you, holding the key, and leaves the key out of the output, so it
does not end up in an agent's transcript. It refuses a path that already exists, before sending the code. Without
`--key-file`, the key is printed as `link_key`:

```sh
al pair K7QM-2XPA
```

### Checking and staying online

```sh
al status --key-file ~/.config/assistant-link/hermes.key
AL_LINK_KEY="$(cat ~/.config/assistant-link/hermes.key)" al wait
```

`al status` connects, waits for the service to accept the connection, and closes it, so it never leaves the Link
online. `al wait` holds the connection until SIGINT or SIGTERM, reconnecting after network drops with a backoff of up
to 30 s. It reads the key once at startup.

### Service URL

Every command uses `--server`, else `AL_SERVER`, else `https://assistant-link.fly.dev`. `al` does not remember which
service issued a key: use the service named in the pairing text.

### Output

Every command prints JSON by default; add `--human` for text. The exit code signals the outcome either way.

- `al pair` prints one object on standard output: `{"server", "link_id", "assistant_name", "link_key"}`, without
  `link_key` when `--key-file` is given.
- `al status` prints one object on standard output: `{"server", "link_id", "assistant_name", "check", "error"}`.
  `check` is `accepted`, `rejected`, `unreachable`, `no_key`, or `unexpected`. `link_id` and `assistant_name` come from
  the service and are present only when the check is `accepted`; `error` is present for any other check.
- `al wait` keeps standard output for events from the user (none are defined yet) and writes its status to standard
  error as JSON lines: `{"status": "connected", "server", "link_id", "assistant_name"}`, `{"status": "reconnecting", "delay_ms", "error"}`, and a final
  `{"status": "stopped", "error"?}`.

Failures carry `{"error": {"code", "message"}}`. Codes are the service's own (`invalid_code`, `expired_code`,
`invalid_params`, `unauthorized`) or `al`'s: `no_key`, `key_file_exists`, `unreachable`, `connection_lost`,
`account_deleted`, `unexpected`.

### Exit codes

| Code | Meaning                                                                                  |
| ---- | ---------------------------------------------------------------------------------------- |
| 0    | Success                                                                                  |
| 1    | Unexpected error, such as a service that does not speak ALP                              |
| 2    | Usage error, including a `--key-file` path that already exists for `al pair`             |
| 3    | No Link key supplied: set `AL_LINK_KEY` or pass `--key-file`                             |
| 4    | Rejected by the service: invalid or expired code, or a Link key it does not accept       |
| 5    | Service unreachable, including `5xx` responses                                           |

A rejected key can also mean the key was sent to a service other than the one that issued it; the message names the
service URL that was used.

## Install

Each [release](https://github.com/assistant-link/cli/releases) has a self-contained `al` binary per platform, packed
as `al-<target>.tar.gz` with `al` at the root of the archive. The Linux binaries are statically linked against musl
and use rustls, so they need no system libraries; to reach the service the host still needs a CA certificate bundle.
The macOS binaries run on macOS 11 or later.

| Platform              | `<target>`                   |
| --------------------- | ---------------------------- |
| macOS, Apple Silicon  | `aarch64-apple-darwin`       |
| macOS, Intel          | `x86_64-apple-darwin`        |
| Linux, x86_64         | `x86_64-unknown-linux-musl`  |
| Linux, arm64          | `aarch64-unknown-linux-musl` |

```sh
TARGET=aarch64-apple-darwin   # the target for this host, from the table
mkdir -p ~/.local/bin
curl -fsSL "https://github.com/assistant-link/cli/releases/latest/download/al-$TARGET.tar.gz" | tar -xz -C ~/.local/bin al
~/.local/bin/al --version
```

Each archive has a [build provenance attestation](https://docs.github.com/actions/security-for-github-actions/using-artifact-attestations)
showing it was built by this repository's release workflow. To check one, download the archive and verify it with the
GitHub CLI:

```sh
curl -fsSLO "https://github.com/assistant-link/cli/releases/latest/download/al-$TARGET.tar.gz"
gh attestation verify "al-$TARGET.tar.gz" --repo assistant-link/cli
```

Each release also has `SHA256SUMS`, which catches a corrupted download but not a tampered release, since it is published
alongside the archives. With the archive and `SHA256SUMS` in one directory:

```sh
grep " al-$TARGET.tar.gz\$" SHA256SUMS | shasum -a 256 -c -   # or `sha256sum -c -` on Linux
```

On any other platform, build it from source with Rust installed:

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

## Releasing

`.github/workflows/release-workflow.yml` builds the release binaries for the four targets above, checks that each
runs `al --version` (on Linux in an empty `FROM scratch` container; on macOS also that it targets macOS 11), and packs
them as `al-<target>.tar.gz`. It runs:

- on every pull request to `main`, and on manual dispatch: it builds and checks the binaries and keeps the archives as
  workflow artifacts, without publishing, so a change that breaks a release build fails before it merges.
- on a pushed `v*` tag: it checks the tag first, and fails before building anything unless the tag is `v` followed by
  `Cargo.toml`'s `version` and points to a commit on `main`. After the four builds pass, it attests each archive's
  build provenance and publishes the GitHub release for the tag with the archives and `SHA256SUMS`.

The release becomes `releases/latest`, which `install.md` downloads from, only when its version is the highest
released so far, so a patch for an older version does not replace a newer one. A version with a `-` suffix
(`0.2.0-rc.1`) is published as a pre-release, which `releases/latest` never serves.

To cut a release, maintainers:

1. Merge a pull request that sets `version` in `Cargo.toml` (and `Cargo.lock`, which `cargo build` updates).
2. Tag that commit on `main` and push the tag:

   ```sh
   git switch main && git pull
   git tag v0.2.0
   git push origin v0.2.0
   ```

3. Check that the workflow passed and the release lists `al-<target>.tar.gz` for every target and `SHA256SUMS`.

The release is created as a draft and published only after every asset is uploaded. If the publish job fails, re-run
the failed jobs: it picks up the same draft. It refuses to touch a release that is already published. If a check or a
build fails, fix it on `main`, then delete the tag (`git push origin :v0.2.0`) and tag again, or release the next
version.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Licensed under the [MIT License](LICENSE).
