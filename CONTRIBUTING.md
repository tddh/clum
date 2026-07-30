# Contributing to yunying

Thanks for your interest in contributing!

## Development Setup

```bash
# Prerequisites: Rust stable (see rust-toolchain.toml), just
cargo install just

# Clone & build
git clone git@github.com:tddh/yunying.git
cd yunying
just check        # Verify compilation
just test         # Run tests
just lint         # Clippy with -D warnings
just fmt-check    # Check formatting
```

## Project Structure

```
crates/
├── yunying-core/    # Shared types (HostConfig, AuditEvent, SessionInfo)
├── yunying-mcp/     # MCP Server — Central Server (HTTP + QUIC) or local stdio mode
├── yunying-cli/     # CLI tool — human PTY passthrough to remote sessions
└── rmux-bridge/     # Bridge daemon — deployed on target Linux hosts
```

## Before Submitting a PR

- [ ] `just fmt` — format code
- [ ] `just lint` — fix all clippy warnings
- [ ] `just test` — all tests pass
- [ ] New features: include tests
- [ ] Documentation: update `docs/TOOLS.md` for new/changed tools, `docs/DEPLOY.md` for config changes

## Commit Style

- Follow [Conventional Commits](https://www.conventionalcommits.org/)
- Examples: `feat: add exec tool`, `fix: frame read error swallowed`, `docs: update DEPLOY.md`

## Questions?

Open an issue or start a discussion.
