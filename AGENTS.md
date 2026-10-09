# Agents

`al`, the agent-side CLI of [Assistant Link](https://github.com/assistant-link/assistant-link). Its commands, JSON
output, and exit codes are a public contract that agents depend on; change them only additively.

## Rules

- Comment only what the code cannot show: never restate what the code does.
- Run `make precommit` when you are done and fix everything it reports.
