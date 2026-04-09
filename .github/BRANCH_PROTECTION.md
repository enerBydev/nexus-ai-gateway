# Branch Protection Rules

## Main Branch Protection

The `main` branch is protected with the following rules:

### Required Status Checks
- All CI checks must pass before merging
- At least 1 approval required from code owners

### Restrictions
- Force push disabled
- Deletion restricted
- Merge commits prohibited

### Branch Protection Policy

### 1. Pull Request Requirements

To merge changes to the main branch, all of the following requirements must be met:

1. **Pull Request Required**: All changes must be made through a pull request
2. **CI Checks**: All GitHub Actions workflows must pass
3. **Code Review**: At least one approval from a code owner is required
4. **No Conflicts**: Branch must be up to date with main before merging

### 2. Branch Management

- **Main Branch**: Protected, no direct pushes allowed
- **Feature Branches**: Use for development work
- **Release Branches**: For version-specific releases (optional)

### 3. Merge Strategy

- **Squash and Merge**: Preferred for clean history
- **Rebase and Merge**: For maintaining linear history
- **Create Merge Commit**: For preserving all commits

### 4. Version Management

Version bumps should follow semantic versioning:
- **Major**: Breaking changes
- **Minor**: New features (backward compatible)
- **Patch**: Bug fixes (backward compatible)

Use the Taskfile commands for version management:
- `task bump-major` - for major version changes
- `task bump-minor` - for minor feature additions
- `task bump-patch` - for bug fixes

### 5. Commit Message Format

All commit messages must follow conventional commit format:
- `feat:` - New features
- `fix:` - Bug fixes
- `chore:` - Maintenance tasks
- `docs:` - Documentation changes
- `refactor:` - Code refactoring
- `test:` - Adding or updating tests
- `ci:` - CI configuration changes
- `perf:` - Performance improvements
- `style:` - Code style changes
- `build:` - Build system changes
- `revert:` - Reverting previous commits

The pre-commit hook will validate this format automatically.