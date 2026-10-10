# Contributing

Thanks for helping with Assistant Link. This repository is one part of the project; the
[assistant-link](https://github.com/assistant-link/assistant-link) repository explains how the parts fit together and
how to set up a workspace with all of them ([getting started](https://github.com/assistant-link/assistant-link/blob/main/docs/getting-started.md)).

## Proposing a change

- For a bug, open an issue with what you did, what you expected, and what happened.
- For a new feature or a change to a public contract (the Assistant Link Protocol, `al`'s commands and output), open an
  issue first so we can agree on the approach before you write the code. Changes are proposed with
  [OpenSpec](https://github.com/Fission-AI/OpenSpec) in the assistant-link repository's `openspec/`, including changes
  to a single repository.
- For a security issue, do not open an issue. Follow
  [SECURITY.md](https://github.com/assistant-link/assistant-link/blob/main/SECURITY.md).

## Pull requests

See [how to make changes](https://github.com/assistant-link/assistant-link/blob/main/docs/engineering/how-to-make-changes.md) for how a change moves through the repositories.

- Keep each pull request to one change, and explain why it is needed.
- `make precommit` is the quality gate. CI runs it on every pull request.
- Pull requests are squash-merged, so the pull request title becomes the commit message on `main`.

## License

By contributing, you agree that your contributions are licensed under the [MIT License](LICENSE). The Assistant Link
name and app icon are not covered by it; see
[TRADEMARK.md](https://github.com/assistant-link/assistant-link/blob/main/TRADEMARK.md).
