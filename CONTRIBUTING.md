# Contributing to NEXUS-AI-Gateway

Thanks for your interest in contributing!

## Development Setup

- Rust stable (1.85+)
- `cargo build` - Build the project
- `cargo test` - Run tests

## Testing Requirements

Before submitting:

```bash
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

## PR Process

1. Fork and branch from `main`
2. Use conventional commits (see below)
3. Pre-commit hooks run automatically
4. Ensure CI passes before requesting review

## Code Style

- `cargo fmt` - Enforced formatting
- `cargo clippy -- -D warnings` - Linting (warnings as errors)

## Commit Message Format

Use conventional commits:

- `feat:` - New feature
- `fix:` - Bug fix
- `refactor:` - Code refactoring
- `docs:` - Documentation
- `test:` - Tests
- `chore:` - Maintenance
- `perf:` - Performance
- `ci:` - CI/CD

Example: `feat: add retry logic for rate limits`
