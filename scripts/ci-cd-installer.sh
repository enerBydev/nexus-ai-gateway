#!/bin/bash
# ═══════════════════════════════════════════════════════════════════════════════
# CI/CD PIPELINE INSTALLER FOR RUST/DIOXUS PROJECTS
# ═══════════════════════════════════════════════════════════════════════════════
#
# Replicates the complete CI/CD infrastructure from NEXUS-AI-Gateway
# to any Rust/Dioxus project.
#
# Features:
#   • GitHub Actions workflows (CI + Auto-Version)
#   • Portable Git hooks (pre-commit, commit-msg, post-commit, pre-push)
#   • Version management scripts (bump, increment, auto-version)
#   • Task runner configuration (Taskfile.yaml)
#   • Branch protection documentation
#
# Usage: ./ci-cd-installer.sh [OPTIONS]
#
# Options:
#   -p, --project-name NAME    Override project name (default: from Cargo.toml)
#   -u, --git-user USER        Git commit author (default: git config user.name)
#   -e, --git-email EMAIL      Git commit email (default: git config user.email)
#   -o, --output DIR           Output directory (default: current directory)
#   --dry-run                  Preview changes without writing files
#   --force                    Overwrite existing files without prompting
#   --update                   Update existing (preserve VERSION, CHANGELOG)
#   --no-hooks                 Skip git hooks installation
#   --no-taskfile              Skip Taskfile.yaml generation
#   -h, --help                 Show this help message
#
# Author: Claude Code (NEXUS-AI-Gateway CI/CD Pipeline)
# License: MIT
# ═══════════════════════════════════════════════════════════════════════════════

set -e

# ─────────────────────────────────────────────────────────────────────────────────
# CONFIGURATION
# ─────────────────────────────────────────────────────────────────────────────────

SCRIPT_VERSION="1.0.0"
SCRIPT_NAME="$(basename "$0")"

# Default values (will be auto-detected)
PROJECT_NAME=""
BINARY_NAME=""
GIT_USER=""
GIT_EMAIL=""
OUTPUT_DIR="."
PRESET="auto"  # rust, dioxus, hybrid, auto

# Flags
DRY_RUN=false
FORCE=false
UPDATE=false
NO_HOOKS=false
NO_TASKFILE=false

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

# ─────────────────────────────────────────────────────────────────────────────────
# UTILITY FUNCTIONS
# ─────────────────────────────────────────────────────────────────────────────────

print_header() {
    echo ""
    echo -e "${CYAN}╔══════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${CYAN}║        CI/CD PIPELINE INSTALLER FOR RUST/DIOXUS PROJECTS         ║${NC}"
    echo -e "${CYAN}║                        v${SCRIPT_VERSION}                                      ║${NC}"
    echo -e "${CYAN}╚══════════════════════════════════════════════════════════════════╝${NC}"
    echo ""
}

print_usage() {
    cat << EOF
Usage: $SCRIPT_NAME [OPTIONS]

Options:
  -p, --project-name NAME    Override project name (default: from Cargo.toml)
  -u, --git-user USER        Git commit author (default: git config user.name)
  -e, --git-email EMAIL      Git commit email (default: git config user.email)
  -o, --output DIR           Output directory (default: current directory)
  --dry-run                  Preview changes without writing files
  --force                    Overwrite existing files without prompting
  --update                   Update existing (preserve VERSION, CHANGELOG)
  --no-hooks                 Skip git hooks installation
  --no-taskfile              Skip Taskfile.yaml generation
  -h, --help                 Show this help message
  --preset <PRESET>      Choose project preset: rust, dioxus, hybrid, auto (default: auto)

Examples:
  # Basic installation (interactive)
  $SCRIPT_NAME

  # Non-interactive with auto-detection
  $SCRIPT_NAME --force

  # Preview changes
  $SCRIPT_NAME --dry-run

  # Custom project settings

  # With preset selection
  $SCRIPT_NAME --preset dioxus --force
  $SCRIPT_NAME --preset hybrid
  $SCRIPT_NAME -p my-app -u "Developer" -e "dev@example.com"

EOF
}

log_info() {
    echo -e "${BLUE}ℹ${NC} $1"
}

log_success() {
    echo -e "${GREEN}✅${NC} $1"
}

log_warning() {
    echo -e "${YELLOW}⚠${NC} $1"
}

log_error() {
    echo -e "${RED}❌${NC} $1"
}

log_dry_run() {
    echo -e "${CYAN}[DRY-RUN]${NC} Would create: $1"
}

confirm() {
    local prompt="$1"
    local default="${2:-n}"

    if [ "$FORCE" = true ]; then
        return 0
    fi

    local choice
    read -p "$prompt [$default] " choice
    choice="${choice:-$default}"
    [ "$choice" = "y" ] || [ "$choice" = "Y" ]
}

# ─────────────────────────────────────────────────────────────────────────────────
# DETECTION FUNCTIONS
# ─────────────────────────────────────────────────────────────────────────────────

detect_project_name() {
    if [ -n "$PROJECT_NAME" ]; then
        BINARY_NAME="$PROJECT_NAME"
        return
    fi

    if [ -f "$OUTPUT_DIR/Cargo.toml" ]; then
        # Extract project name from Cargo.toml
        PROJECT_NAME=$(grep -m1 '^name\s*=' "$OUTPUT_DIR/Cargo.toml" | sed 's/name\s*=\s*"\(.*\)"/\1/' | tr -d '"')
        BINARY_NAME="$PROJECT_NAME"
    else
        log_error "Cargo.toml not found. Use -p to specify project name."
        exit 1
    fi
}

detect_git_config() {
    if [ -z "$GIT_USER" ]; then
        GIT_USER=$(git config --global user.name 2>/dev/null || echo "Developer")
    fi
    if [ -z "$GIT_EMAIL" ]; then
        GIT_EMAIL=$(git config --global user.email 2>/dev/null || echo "dev@example.com")
    fi
}

detect_preset() {
    local dir="${1:-.}"

    # Check for Dioxus.toml in current directory
    if [ -f "$dir/Dioxus.toml" ]; then
        echo "dioxus"
        return
    fi

    # Check for workspace with backend/frontend directories
    if [ -f "$dir/Cargo.toml" ]; then
        if grep -q "\[workspace\]" "$dir/Cargo.toml" 2>/dev/null; then
            if [ -d "$dir/backend" ] && [ -d "$dir/frontend" ]; then
                echo "hybrid"
                return
            fi
        fi
    fi

    # Check if there's a frontend subdirectory with Dioxus.toml
    if [ -d "$dir/frontend" ] && [ -f "$dir/frontend/Dioxus.toml" ]; then
        echo "hybrid"
        return
    fi

    # Default to rust
    echo "rust"
}

check_prerequisites() {
    local errors=0

    # Check git
    if ! command -v git >/dev/null 2>&1; then
        log_error "git is required but not installed."
        errors=$((errors + 1))
    fi

    # Check if in a git repository
    if ! git rev-parse --git-dir >/dev/null 2>&1; then
        log_error "Not a git repository. Run 'git init' first."
        errors=$((errors + 1))
    fi

    # Check Cargo.toml
    if [ ! -f "$OUTPUT_DIR/Cargo.toml" ]; then
        log_error "Cargo.toml not found in $OUTPUT_DIR"
        log_info "This installer is designed for Rust projects."
        errors=$((errors + 1))
    fi

    if [ $errors -gt 0 ]; then
        exit 1
    fi
}

# ─────────────────────────────────────────────────────────────────────────────────
# FILE GENERATION FUNCTIONS
# ─────────────────────────────────────────────────────────────────────────────────

write_file() {
    local target="$1"
    local content="$2"
    local mode="${3:-644}"

    if [ "$DRY_RUN" = true ]; then
        log_dry_run "$target"
        return 0
    fi

    # Check if file exists
    if [ -f "$target" ]; then
        if [ "$FORCE" != true ] && [ "$UPDATE" != true ]; then
            if ! confirm "File exists: $target. Overwrite?"; then
                log_warning "Skipped: $target"
                return 0
            fi
        fi
        # Backup existing file
        cp "$target" "${target}.bak"
        log_info "Backed up: ${target}.bak"
    fi

    # Create directory if needed
    local dir
    dir=$(dirname "$target")
    if [ ! -d "$dir" ]; then
        mkdir -p "$dir"
    fi

    # Write file
    echo "$content" > "$target"
    chmod "$mode" "$target"
    log_success "Created: $target"
}

make_executable() {
    local target="$1"
    if [ "$DRY_RUN" = true ]; then
        return 0
    fi
    chmod +x "$target"
}

# ─────────────────────────────────────────────────────────────────────────────────
# WORKFLOW GENERATORS
# ─────────────────────────────────────────────────────────────────────────────────

generate_ci_workflow() {
    cat << 'EOF'
name: CI/CD Pipeline

on:
  push:
    branches: [ main ]
  pull_request:
    branches: [ main ]

env:
  CARGO_TERM_COLOR: always

jobs:
  security-scan:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
          key: ${{ runner.os }}-cargo-audit-${{ hashFiles('**/Cargo.lock') }}
      - name: Install cargo-audit
        run: cargo install cargo-audit
      - name: Run security audit
        run: cargo audit

  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-cargo-test-${{ hashFiles('**/Cargo.lock') }}
          restore-keys: |
            ${{ runner.os }}-cargo-test-
      - name: Run tests
        run: cargo test

  lint:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy
      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-cargo-clippy-${{ hashFiles('**/Cargo.lock') }}
          restore-keys: |
            ${{ runner.os }}-cargo-clippy-
      - name: Run clippy
        run: cargo clippy -- -D warnings

  format:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
      - name: Check formatting
        run: cargo fmt -- --check

  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-cargo-build-${{ hashFiles('**/Cargo.lock') }}
          restore-keys: |
            ${{ runner.os }}-cargo-build-
      - name: Build release
        run: cargo build --release

  release:
    needs: [security-scan, test, lint, format, build]
    runs-on: ubuntu-latest
    if: github.ref == 'refs/heads/main'
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - name: Get version
        id: get_version
        run: |
          VERSION=$(cat VERSION 2>/dev/null || grep '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
          TAG="v$VERSION"
          echo "version=$VERSION" >> $GITHUB_OUTPUT
          echo "tag=$TAG" >> $GITHUB_OUTPUT
      - name: Check if tag exists
        id: check_tag
        run: |
          if git rev-parse "${{ steps.get_version.outputs.tag }}" >/dev/null 2>&1; then
            echo "exists=true" >> $GITHUB_OUTPUT
            echo "✅ Tag ${{ steps.get_version.outputs.tag }} exists — release handled by auto-version.yml"
          else
            echo "exists=false" >> $GITHUB_OUTPUT
            echo "ℹ️ Tag ${{ steps.get_version.outputs.tag }} not found — auto-version.yml will handle release"
          fi
      - name: CI Pipeline Summary
        run: |
          echo "## CI/CD Pipeline Results" >> $GITHUB_STEP_SUMMARY
          echo "" >> $GITHUB_STEP_SUMMARY
          echo "| Check | Status |" >> $GITHUB_STEP_SUMMARY
          echo "|:------|:------:|" >> $GITHUB_STEP_SUMMARY
          echo "| Security Scan | ✅ |" >> $GITHUB_STEP_SUMMARY
          echo "| Tests | ✅ |" >> $GITHUB_STEP_SUMMARY
          echo "| Lint (Clippy) | ✅ |" >> $GITHUB_STEP_SUMMARY
          echo "| Format | ✅ |" >> $GITHUB_STEP_SUMMARY
          echo "| Build | ✅ |" >> $GITHUB_STEP_SUMMARY
          echo "" >> $GITHUB_STEP_SUMMARY
          echo "**Version**: ${{ steps.get_version.outputs.version }}" >> $GITHUB_STEP_SUMMARY
          echo "**Tag exists**: ${{ steps.check_tag.outputs.exists }}" >> $GITHUB_STEP_SUMMARY
EOF
}

generate_auto_version_workflow() {
    local user="$1"
    local email="$2"
    local binary="$3"

    cat << EOF
name: Auto Version & Release

on:
  workflow_run:
    workflows: ["CI/CD Pipeline"]
    types: [completed]
    branches: [main]

permissions:
  contents: write

jobs:
  auto-version:
    runs-on: ubuntu-latest
    if: \${{ github.event.workflow_run.conclusion == 'success' && github.event.workflow_run.event == 'push' }}
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
          token: \${{ secrets.GITHUB_TOKEN }}

      - name: Configure git as repo owner
        run: |
          git config user.name "$user"
          git config user.email "$email"

      - name: Analyze commits for version bump
        id: analyze
        run: |
          CURRENT_VERSION=\$(cat VERSION 2>/dev/null || grep '^version' Cargo.toml | sed 's/version = "\\(.*\\)"/\\1/')
          CURRENT_TAG="v\${CURRENT_VERSION}"

          if ! git rev-parse "\$CURRENT_TAG" >/dev/null 2>&1; then
            echo "bump=none" >> \$GITHUB_OUTPUT
            exit 0
          fi

          COMMITS=\$(git log "\${CURRENT_TAG}..HEAD" --oneline 2>/dev/null)
          TOTAL=\$(echo "\$COMMITS" | grep -c '.' || true)

          if [ "\$TOTAL" -eq 0 ]; then
            echo "bump=none" >> \$GITHUB_OUTPUT
            exit 0
          fi

          FEAT=\$(echo "\$COMMITS" | grep -cE '^[a-f0-9]+ feat' || true)
          FIX=\$(echo "\$COMMITS" | grep -cE '^[a-f0-9]+ fix' || true)
          REFACTOR=\$(echo "\$COMMITS" | grep -cE '^[a-f0-9]+ refactor' || true)
          PERF=\$(echo "\$COMMITS" | grep -cE '^[a-f0-9]+ perf' || true)
          BREAKING=\$(echo "\$COMMITS" | grep -cE '!:' || true)

          if [ "\$BREAKING" -gt 0 ] || [ "\$FEAT" -gt 0 ]; then
            BUMP="minor"
          elif [ "\$FIX" -gt 0 ] || [ "\$REFACTOR" -gt 0 ] || [ "\$PERF" -gt 0 ]; then
            BUMP="patch"
          else
            BUMP="none"
          fi

          echo "bump=\$BUMP" >> \$GITHUB_OUTPUT
          echo "current=\$CURRENT_VERSION" >> \$GITHUB_OUTPUT

      - name: Calculate new version
        id: version
        if: steps.analyze.outputs.bump != 'none' && steps.analyze.outputs.bump != ''
        run: |
          BUMP=\${{ steps.analyze.outputs.bump }}
          CURRENT=\${{ steps.analyze.outputs.current }}

          MAJOR=\$(echo "\$CURRENT" | cut -d. -f1)
          MINOR=\$(echo "\$CURRENT" | cut -d. -f2)
          PATCH=\$(echo "\$CURRENT" | cut -d. -f3)

          case \$BUMP in
            minor) NEW="\$MAJOR.\$((MINOR + 1)).0" ;;
            patch) NEW="\$MAJOR.\$MINOR.\$((PATCH + 1))" ;;
          esac

          echo "new=\$NEW" >> \$GITHUB_OUTPUT
          echo "tag=v\$NEW" >> \$GITHUB_OUTPUT

      - name: Install Rust
        if: steps.version.outputs.new != ''
        uses: dtolnay/rust-toolchain@stable

      - name: Cache cargo
        if: steps.version.outputs.new != ''
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: \${{ runner.os }}-cargo-release-\${{ hashFiles('**/Cargo.lock') }}

      - name: Apply version bump
        if: steps.version.outputs.new != ''
        run: |
          NEW=\${{ steps.version.outputs.new }}
          echo "\$NEW" > VERSION
          sed -i "s/^version = \\".*\\"/version = \\"\$NEW\\"/" Cargo.toml
          [ -f src/lib.rs ] && sed -i "s/pub const VERSION: &str = \\".*\\";/pub const VERSION: &str = \\"\$NEW\\";/" src/lib.rs

          if [ -f CHANGELOG.md ]; then
            TODAY=\$(date +%Y-%m-%d)
            sed -i "s/## \\[Unreleased\\]/## [Unreleased]\\n\\n### Added\\n\\n### Changed\\n\\n### Fixed\\n\\n---\\n\\n## [\$NEW] - \$TODAY/" CHANGELOG.md
          fi

          git add VERSION Cargo.toml src/lib.rs CHANGELOG.md
          git commit -m "chore: bump version to \$NEW [skip ci]"
          git tag -a "v\$NEW" -m "Release v\$NEW"
          git push origin main --tags

      - name: Build release binary
        if: steps.version.outputs.new != ''
        run: cargo build --release

      - name: Create GitHub Release
        if: steps.version.outputs.new != ''
        uses: softprops/action-gh-release@v2
        with:
          tag_name: \${{ steps.version.outputs.tag }}
          name: "\${{ steps.version.outputs.tag }}"
          files: target/release/${binary}
          draft: false
          prerelease: false
EOF
}

# ─────────────────────────────────────────────────────────────────────────────────
# PRESET-SPECIFIC WORKFLOW GENERATORS
# ─────────────────────────────────────────────────────────────────────────────────

generate_rust_workflow() {
    # Standard Rust workflow - same as existing generate_ci_workflow
    generate_ci_workflow
}

generate_dioxus_workflow() {
    cat << 'EOF'
name: CI/CD Pipeline

on:
  push:
    branches: [ main ]
  pull_request:
    branches: [ main ]

env:
  CARGO_TERM_COLOR: always

jobs:
  security-scan:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
          key: ${{ runner.os }}-cargo-audit-${{ hashFiles('**/Cargo.lock') }}
      - name: Install cargo-audit
        run: cargo install cargo-audit
      - name: Run security audit
        run: cargo audit

  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-cargo-test-${{ hashFiles('**/Cargo.lock') }}
          restore-keys: |
            ${{ runner.os }}-cargo-test-
      - name: Run tests
        run: cargo test --all-features

  lint:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy
      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-cargo-clippy-${{ hashFiles('**/Cargo.lock') }}
          restore-keys: |
            ${{ runner.os }}-cargo-clippy-
      - name: Run clippy
        run: cargo clippy -- -D warnings

  format:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
      - name: Check formatting
        run: cargo fmt -- --check

  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          targets: wasm32-unknown-unknown
      - name: Install dioxus-cli
        run: cargo install dioxus-cli
      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
            target/dx
          key: ${{ runner.os }}-dioxus-${{ hashFiles('**/Cargo.lock') }}
          restore-keys: |
            ${{ runner.os }}-dioxus-
      - name: Build release
        run: dx build --release
      - name: Archive dist folder
        uses: actions/upload-artifact@v4
        with:
          name: dist
          path: dist/

  release:
    needs: [security-scan, test, lint, format, build]
    runs-on: ubuntu-latest
    if: github.ref == 'refs/heads/main'
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - name: Get version
        id: get_version
        run: |
          VERSION=$(cat VERSION 2>/dev/null || grep '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
          TAG="v$VERSION"
          echo "version=$VERSION" >> $GITHUB_OUTPUT
          echo "tag=$TAG" >> $GITHUB_OUTPUT
      - name: Check if tag exists
        id: check_tag
        run: |
          if git rev-parse "${{ steps.get_version.outputs.tag }}" >/dev/null 2>&1; then
            echo "exists=true" >> $GITHUB_OUTPUT
            echo "Tag ${{ steps.get_version.outputs.tag }} exists"
          else
            echo "exists=false" >> $GITHUB_OUTPUT
            echo "Tag ${{ steps.get_version.outputs.tag }} not found"
          fi
      - name: CI Pipeline Summary
        run: |
          echo "## CI/CD Pipeline Results" >> $GITHUB_STEP_SUMMARY
          echo "" >> $GITHUB_STEP_SUMMARY
          echo "| Check | Status |" >> $GITHUB_STEP_SUMMARY
          echo "|:------|:------:|" >> $GITHUB_STEP_SUMMARY
          echo "| Security Scan | |" >> $GITHUB_STEP_SUMMARY
          echo "| Tests | |" >> $GITHUB_STEP_SUMMARY
          echo "| Lint (Clippy) | |" >> $GITHUB_STEP_SUMMARY
          echo "| Format | |" >> $GITHUB_STEP_SUMMARY
          echo "| Build | |" >> $GITHUB_STEP_SUMMARY
          echo "" >> $GITHUB_STEP_SUMMARY
          echo "**Version**: ${{ steps.get_version.outputs.version }}" >> $GITHUB_STEP_SUMMARY
          echo "**Tag exists**: ${{ steps.check_tag.outputs.exists }}" >> $GITHUB_STEP_SUMMARY
EOF
}

generate_hybrid_workflow() {
    cat << 'EOF'
name: CI/CD Pipeline

on:
  push:
    branches: [ main ]
  pull_request:
    branches: [ main ]

env:
  CARGO_TERM_COLOR: always

jobs:
  security-scan:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
          key: ${{ runner.os }}-cargo-audit-${{ hashFiles('**/Cargo.lock') }}
      - name: Install cargo-audit
        run: cargo install cargo-audit
      - name: Run security audit
        run: cargo audit

  test-backend:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-backend-test-${{ hashFiles('**/Cargo.lock') }}
          restore-keys: |
            ${{ runner.os }}-backend-test-
      - name: Test backend
        run: cargo test -p backend --all-features

  test-frontend:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-frontend-test-${{ hashFiles('**/Cargo.lock') }}
          restore-keys: |
            ${{ runner.os }}-frontend-test-
      - name: Test frontend
        run: cargo test -p frontend --all-features

  lint:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy
      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-clippy-${{ hashFiles('**/Cargo.lock') }}
          restore-keys: |
            ${{ runner.os }}-clippy-
      - name: Run clippy
        run: cargo clippy --workspace -- -D warnings

  format:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
      - name: Check formatting
        run: cargo fmt -- --check

  build-backend:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-backend-build-${{ hashFiles('**/Cargo.lock') }}
          restore-keys: |
            ${{ runner.os }}-backend-build-
      - name: Build backend
        run: cargo build --release -p backend
      - name: Archive backend binary
        uses: actions/upload-artifact@v4
        with:
          name: backend-binary
          path: target/release/backend

  build-frontend:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          targets: wasm32-unknown-unknown
      - name: Install dioxus-cli
        run: cargo install dioxus-cli
      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
            target/dx
            frontend/target
          key: ${{ runner.os }}-frontend-build-${{ hashFiles('**/Cargo.lock') }}
          restore-keys: |
            ${{ runner.os }}-frontend-build-
      - name: Build frontend
        run: cd frontend && dx build --release
      - name: Archive frontend dist
        uses: actions/upload-artifact@v4
        with:
          name: frontend-dist
          path: frontend/dist/

  combined-release:
    needs: [security-scan, test-backend, test-frontend, lint, format, build-backend, build-frontend]
    runs-on: ubuntu-latest
    if: github.ref == 'refs/heads/main'
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - name: Get version
        id: get_version
        run: |
          VERSION=$(cat VERSION 2>/dev/null || grep '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
          TAG="v$VERSION"
          echo "version=$VERSION" >> $GITHUB_OUTPUT
          echo "tag=$TAG" >> $GITHUB_OUTPUT
      - name: Check if tag exists
        id: check_tag
        run: |
          if git rev-parse "${{ steps.get_version.outputs.tag }}" >/dev/null 2>&1; then
            echo "exists=true" >> $GITHUB_OUTPUT
            echo "Tag ${{ steps.get_version.outputs.tag }} exists"
          else
            echo "exists=false" >> $GITHUB_OUTPUT
            echo "Tag ${{ steps.get_version.outputs.tag }} not found"
          fi
      - name: Download artifacts
        uses: actions/download-artifact@v4
        with:
          path: artifacts
      - name: Create release archive
        run: |
          mkdir -p release/${{ steps.get_version.outputs.version }}
          cp artifacts/backend-binary/backend release/${{ steps.get_version.outputs.version }}/ 2>/dev/null || true
          cp -r artifacts/frontend-dist/dist release/${{ steps.get_version.outputs.version }}/ 2>/dev/null || true
          tar -czf release-${{ steps.get_version.outputs.version }}.tar.gz -C release ${{ steps.get_version.outputs.version }}
      - name: Upload combined release archive
        uses: actions/upload-artifact@v4
        with:
          name: release-${{ steps.get_version.outputs.version }}
          path: release-${{ steps.get_version.outputs.version }}.tar.gz
      - name: CI Pipeline Summary
        run: |
          echo "## CI/CD Pipeline Results" >> $GITHUB_STEP_SUMMARY
          echo "" >> $GITHUB_STEP_SUMMARY
          echo "| Check | Status |" >> $GITHUB_STEP_SUMMARY
          echo "|:------|:------:|" >> $GITHUB_STEP_SUMMARY
          echo "| Security Scan | |" >> $GITHUB_STEP_SUMMARY
          echo "| Backend Tests | |" >> $GITHUB_STEP_SUMMARY
          echo "| Frontend Tests | |" >> $GITHUB_STEP_SUMMARY
          echo "| Lint (Clippy) | |" >> $GITHUB_STEP_SUMMARY
          echo "| Format | |" >> $GITHUB_STEP_SUMMARY
          echo "| Backend Build | |" >> $GITHUB_STEP_SUMMARY
          echo "| Frontend Build | |" >> $GITHUB_STEP_SUMMARY
          echo "" >> $GITHUB_STEP_SUMMARY
          echo "**Version**: ${{ steps.get_version.outputs.version }}" >> $GITHUB_STEP_SUMMARY
          echo "**Tag exists**: ${{ steps.check_tag.outputs.exists }}" >> $GITHUB_STEP_SUMMARY
EOF
}

# ─────────────────────────────────────────────────────────────────────────────────
# HOOK GENERATORS
# ─────────────────────────────────────────────────────────────────────────────────

generate_pre_commit_hook() {
    cat << 'EOF'
#!/bin/sh
# Pre-commit hook for code quality checks

echo "🔍 Running pre-commit checks..."

# Check for secrets (basic check)
if git diff --cached --name-only | xargs grep -l "API_KEY\\|SECRET\\|PASSWORD\\|PRIVATE_KEY" 2>/dev/null; then
    echo ""
    echo "⚠️  Warning: Potential secrets detected in staged files"
    echo "   Please review before committing"
    echo ""
fi

# Check for large files (>1MB)
LARGE_FILES=$(git diff --cached --name-only | xargs -I {} sh -c 'if [ -f "{}" ]; then stat -c%s "{}" 2>/dev/null; fi' | awk '$1 > 1048576 {print}')
if [ -n "$LARGE_FILES" ]; then
    echo ""
    echo "⚠️  Warning: Large files detected (>1MB)"
    echo "   Consider using Git LFS for large binaries"
    echo ""
fi

# Check if there are Rust files staged
RUST_FILES=$(git diff --cached --name-only -- '*.rs')
if [ -n "$RUST_FILES" ]; then
    echo ""
    echo "🔧 Checking Rust code quality..."

    # Check formatting
    echo "  Running cargo fmt --check..."
    if ! cargo fmt --check 2>/dev/null; then
        echo ""
        echo "❌ Code formatting issues detected!"
        echo "   Run: cargo fmt"
        exit 1
    fi
    echo "  ✅ Format check passed"

    # Check clippy
    echo "  Running cargo clippy..."
    if ! cargo clippy -- -D warnings 2>/dev/null; then
        echo ""
        echo "❌ Clippy lint errors detected!"
        echo "   Run: cargo clippy --fix"
        exit 1
    fi
    echo "  ✅ Clippy check passed"
fi

echo ""
echo "✅ Pre-commit checks passed"
exit 0
EOF
}

generate_commit_msg_hook() {
    cat << 'EOF'
#!/bin/sh
# Commit-msg hook for conventional commits validation

COMMIT_MSG_FILE=$1
COMMIT_MSG=$(head -n 1 "$COMMIT_MSG_FILE")

# Skip empty commits
if [ -z "$COMMIT_MSG" ]; then
    exit 0
fi

# Skip merge commits
if echo "$COMMIT_MSG" | grep -qE "^Merge (branch|tag) "; then
    exit 0
fi

echo "🔍 Validating commit message format..."

# Check if it matches conventional commit format
if ! echo "$COMMIT_MSG" | grep -qE "^(feat|fix|chore|docs|refactor|test|ci|perf|style|build|revert)(\(.+\))?!?: .+"; then
    echo ""
    echo "❌ Commit message does not follow conventional commit format!"
    echo ""
    echo "Required format: <type>(<scope>): <description>"
    echo ""
    echo "Types: feat, fix, chore, docs, refactor, test, ci, perf, style, build, revert"
    echo ""
    echo "Examples:"
    echo "  feat: add new authentication feature"
    echo "  fix(proxy): handle timeout edge case"
    echo "  chore: update dependencies"
    echo ""
    echo "Your message: $COMMIT_MSG"
    echo ""
    exit 1
fi

echo "✅ Valid conventional commit format"

# Reminder for version bump
if echo "$COMMIT_MSG" | grep -qE "^(feat|fix|refactor|perf)(\(.+))?!?: "; then
    echo ""
    echo "💡 Reminder: This commit type may require a version bump"
    echo "   feat: → task bump-minor"
    echo "   fix: → task bump-patch"
    echo "   refactor/perf: → task bump-patch"
    echo ""
fi

exit 0
EOF
}

generate_post_commit_hook() {
    cat << 'EOF'
#!/bin/sh
# Post-commit hook — Analyze commit and suggest version bump if needed

# Skip if this is a version bump commit itself (avoid recursion)
COMMIT_MSG=$(git log -1 --format=%s)
if echo "$COMMIT_MSG" | grep -qE "^chore: bump version to"; then
    exit 0
fi

# Only analyze if auto-version.sh exists
if [ -x scripts/auto-version.sh ]; then
    scripts/auto-version.sh --dry-run 2>/dev/null
fi

exit 0
EOF
}

generate_pre_push_hook() {
    cat << 'EOF'
#!/bin/sh
# Pre-push hook for validation before pushing to remote

remote="$1"
url="$2"

# Read the ref being pushed
while read local_ref local_sha remote_ref remote_sha; do
    # Only validate pushes to main
    if [ "$remote_ref" = "refs/heads/main" ]; then
        echo "🔍 Validating before push to main..."
        echo ""

        # 1. Version sync check
        V_FILE=$(cat VERSION 2>/dev/null)
        V_CARGO=$(grep '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
        V_LIB=$(grep 'pub const VERSION' src/lib.rs 2>/dev/null | sed 's/.*"\(.*\)".*/\1/')

        if [ "$V_FILE" != "$V_CARGO" ] || [ "$V_FILE" != "$V_LIB" ]; then
            echo "❌ Version mismatch detected!"
            echo "   VERSION file: $V_FILE"
            echo "   Cargo.toml: $V_CARGO"
            echo "   src/lib.rs: $V_LIB"
            echo ""
            echo "   Run: task bump-patch (or bump-minor/bump-major)"
            exit 1
        fi
        echo "  ✅ Version sync: $V_FILE"

        # 2. Tag validation
        TAG="v$V_FILE"
        if ! git rev-parse "$TAG" >/dev/null 2>&1; then
            echo "  ⚠️  No git tag for $TAG — consider running: task release"
        else
            echo "  ✅ Tag $TAG exists"
        fi

        # 3. Run tests
        echo ""
        echo "  Running cargo test..."
        if ! cargo test --quiet 2>/dev/null; then
            echo "❌ Tests failed! Push blocked."
            exit 1
        fi
        echo "  ✅ Tests passed"

        # 4. Run clippy
        echo "  Running cargo clippy..."
        if ! cargo clippy -- -D warnings 2>/dev/null; then
            echo "❌ Clippy errors found! Push blocked."
            exit 1
        fi
        echo "  ✅ Clippy passed"

        # 5. Security audit (non-blocking)
        if command -v cargo-audit >/dev/null 2>&1; then
            echo "  Running cargo audit..."
            if ! cargo audit --quiet 2>/dev/null; then
                echo "⚠️  Security vulnerabilities found! Review with: cargo audit"
                echo "   (Not blocking push — fix when possible)"
            else
                echo "  ✅ Security audit passed"
            fi
        fi

        echo ""
        echo "✅ Pre-push validation passed"
    fi
done

exit 0
EOF
}

# ─────────────────────────────────────────────────────────────────────────────────
# SCRIPT GENERATORS
# ─────────────────────────────────────────────────────────────────────────────────

generate_bump_version_script() {
    cat << 'EOF'
#!/bin/bash
# bump-version.sh - Bumps version across all project files
# Usage: ./scripts/bump-version.sh <new_version>

set -e

if [ $# -ne 1 ]; then
    echo "Usage: $0 <new_version>"
    echo "Example: $0 0.6.0"
    exit 1
fi

NEW_VERSION=$1

# Validate version format
if ! echo "$NEW_VERSION" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$'; then
    echo "❌ Invalid version format: $NEW_VERSION"
    echo "   Expected format: X.Y.Z (e.g., 0.6.0)"
    exit 1
fi

echo "📦 Bumping version to $NEW_VERSION"

# Get current version
CURRENT_VERSION=$(cat VERSION 2>/dev/null || grep '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
echo "   Current: $CURRENT_VERSION"

if [ "$CURRENT_VERSION" = "$NEW_VERSION" ]; then
    echo "⚠️  Version is already $NEW_VERSION"
    exit 0
fi

# 1. Update VERSION file
echo "$NEW_VERSION" > VERSION
echo "  ✅ VERSION updated"

# 2. Update Cargo.toml
sed -i "s/^version = \".*\"/version = \"$NEW_VERSION\"/" Cargo.toml
echo "  ✅ Cargo.toml updated"

# 3. Update src/lib.rs
if [ -f src/lib.rs ]; then
    sed -i "s/pub const VERSION: &str = \".*\";/pub const VERSION: &str = \"$NEW_VERSION\";/" src/lib.rs
    echo "  ✅ src/lib.rs updated"
fi

# 4. Update CHANGELOG.md
if [ -f CHANGELOG.md ]; then
    TODAY=$(date +%Y-%m-%d)
    sed -i "s/## \[Unreleased\]/## [Unreleased]\n\n### Added\n\n### Changed\n\n### Fixed\n\n---\n\n## [$NEW_VERSION] - $TODAY/" CHANGELOG.md
    echo "  ✅ CHANGELOG.md updated ([$NEW_VERSION] - $TODAY)"
fi

# 5. Verify all sources match
V1=$(cat VERSION)
V2=$(grep '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
V3=$(grep 'pub const VERSION' src/lib.rs 2>/dev/null | sed 's/.*"\(.*\)".*/\1/')

echo ""
if [ "$V1" = "$NEW_VERSION" ] && [ "$V2" = "$NEW_VERSION" ] && [ "$V3" = "$NEW_VERSION" ]; then
    echo "✅ All 3 sources in sync: $NEW_VERSION"
else
    echo "⚠️  Version mismatch detected!"
    echo "   VERSION file: $V1"
    echo "   Cargo.toml: $V2"
    echo "   src/lib.rs: $V3"
    exit 1
fi

echo ""
echo "🚀 Next steps:"
echo "   1. Review changes: git diff"
echo "   2. Commit: git commit -am 'chore: bump version to $NEW_VERSION'"
echo "   3. Push: git push"
echo "   4. Create release: task release"
EOF
}

generate_increment_version_script() {
    cat << 'EOF'
#!/bin/bash
# increment-version.sh - Calculates next version
# Usage: ./scripts/increment-version.sh patch|minor|major

set -e

if [ $# -ne 1 ]; then
    echo "Usage: $0 patch|minor|major"
    exit 1
fi

VERSION_TYPE=$1

# Get current version
if [ -f "VERSION" ]; then
    CURRENT_VERSION=$(cat VERSION)
else
    CURRENT_VERSION=$(grep '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
fi

# Parse version components
MAJOR=$(echo $CURRENT_VERSION | cut -d. -f1)
MINOR=$(echo $CURRENT_VERSION | cut -d. -f2)
PATCH=$(echo $CURRENT_VERSION | cut -d. -f3)

case $VERSION_TYPE in
    major)
        NEW_VERSION="$((MAJOR + 1)).0.0"
        ;;
    minor)
        NEW_VERSION="$MAJOR.$((MINOR + 1)).0"
        ;;
    patch)
        NEW_VERSION="$MAJOR.$MINOR.$((PATCH + 1))"
        ;;
    *)
        echo "Invalid version type. Use: major, minor, or patch"
        exit 1
        ;;
esac

echo $NEW_VERSION
EOF
}

generate_auto_version_script() {
    cat << 'EOF'
#!/bin/bash
# auto-version.sh — Automatic version management based on conventional commits

set -e

PROJECT_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$PROJECT_ROOT"

MODE="${1:---dry-run}"

# Get current version
CURRENT_VERSION=$(cat VERSION 2>/dev/null || grep '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
CURRENT_TAG="v${CURRENT_VERSION}"

# Check if tag exists
if ! git rev-parse "$CURRENT_TAG" >/dev/null 2>&1; then
    echo "⚠️  Tag $CURRENT_TAG not found. Using all commits."
    RANGE="HEAD"
else
    RANGE="${CURRENT_TAG}..HEAD"
fi

# Count commit types since last tag
COMMITS=$(git log "$RANGE" --oneline 2>/dev/null)
TOTAL=$(echo "$COMMITS" | grep -c '.' 2>/dev/null || echo 0)

if [ "$TOTAL" -eq 0 ]; then
    echo "✅ No new commits since $CURRENT_TAG — no bump needed"
    exit 0
fi

# Analyze commit types
FEAT_COUNT=$(echo "$COMMITS" | grep -cE '^[a-f0-9]+ feat' || true)
FIX_COUNT=$(echo "$COMMITS" | grep -cE '^[a-f0-9]+ fix' || true)
REFACTOR_COUNT=$(echo "$COMMITS" | grep -cE '^[a-f0-9]+ refactor' || true)
PERF_COUNT=$(echo "$COMMITS" | grep -cE '^[a-f0-9]+ perf' || true)
BREAKING_COUNT=$(echo "$COMMITS" | grep -cE '!:' || true)

CHORE_COUNT=$(echo "$COMMITS" | grep -cE '^[a-f0-9]+ chore' || true)
DOCS_COUNT=$(echo "$COMMITS" | grep -cE '^[a-f0-9]+ docs' || true)
CI_COUNT=$(echo "$COMMITS" | grep -cE '^[a-f0-9]+ ci' || true)
TEST_COUNT=$(echo "$COMMITS" | grep -cE '^[a-f0-9]+ test' || true)

BUMP_COMMITS=$((FEAT_COUNT + FIX_COUNT + REFACTOR_COUNT + PERF_COUNT))
NO_BUMP_COMMITS=$((CHORE_COUNT + DOCS_COUNT + CI_COUNT + TEST_COUNT))

# Determine bump level
BUMP="none"
if [ "$BREAKING_COUNT" -gt 0 ] || [ "$FEAT_COUNT" -gt 0 ]; then
    BUMP="minor"
elif [ "$FIX_COUNT" -gt 0 ] || [ "$REFACTOR_COUNT" -gt 0 ] || [ "$PERF_COUNT" -gt 0 ]; then
    BUMP="patch"
fi

# Calculate new version
MAJOR=$(echo "$CURRENT_VERSION" | cut -d. -f1)
MINOR=$(echo "$CURRENT_VERSION" | cut -d. -f2)
PATCH=$(echo "$CURRENT_VERSION" | cut -d. -f3)

case $BUMP in
    minor)
        NEW_VERSION="$MAJOR.$((MINOR + 1)).0"
        ;;
    patch)
        NEW_VERSION="$MAJOR.$MINOR.$((PATCH + 1))"
        ;;
    none)
        NEW_VERSION="$CURRENT_VERSION"
        ;;
esac

# Display analysis
echo "┌─────────────────────────────────────────────────┐"
echo "│          📊 AUTO-VERSION ANALYSIS               │"
echo "├─────────────────────────────────────────────────┤"
echo "│ Current version: $CURRENT_VERSION"
echo "│ Commits since $CURRENT_TAG: $TOTAL"
echo "├─────────────────────────────────────────────────┤"
echo "│ BUMP-TRIGGERING:"
[ "$FEAT_COUNT" -gt 0 ] && echo "│   feat: $FEAT_COUNT → MINOR"
[ "$FIX_COUNT" -gt 0 ] && echo "│   fix: $FIX_COUNT → PATCH"
[ "$REFACTOR_COUNT" -gt 0 ] && echo "│   refactor: $REFACTOR_COUNT → PATCH"
[ "$PERF_COUNT" -gt 0 ] && echo "│   perf: $PERF_COUNT → PATCH"
[ "$BREAKING_COUNT" -gt 0 ] && echo "│   BREAKING: $BREAKING_COUNT → MINOR"
echo "│ NO-BUMP:"
[ "$CHORE_COUNT" -gt 0 ] && echo "│   chore: $CHORE_COUNT"
[ "$DOCS_COUNT" -gt 0 ] && echo "│   docs: $DOCS_COUNT"
[ "$CI_COUNT" -gt 0 ] && echo "│   ci: $CI_COUNT"
[ "$TEST_COUNT" -gt 0 ] && echo "│   test: $TEST_COUNT"
echo "├─────────────────────────────────────────────────┤"

if [ "$BUMP" = "none" ]; then
    echo "│ ✅ RESULT: No bump needed (only chore/docs/ci)  │"
    echo "└─────────────────────────────────────────────────┘"
    exit 0
fi

echo "│ 🎯 RESULT: $BUMP bump → $NEW_VERSION"
echo "└─────────────────────────────────────────────────┘"

if [ "$MODE" = "--apply" ]; then
    echo ""
    echo "🚀 Applying version bump..."
    ./scripts/bump-version.sh "$NEW_VERSION"

    echo ""
    echo "📦 Creating version commit..."
    git add VERSION Cargo.toml src/lib.rs CHANGELOG.md 2>/dev/null
    git commit -m "chore: bump version to $NEW_VERSION" --no-verify

    echo ""
    echo "🏷️  Creating tag v$NEW_VERSION..."
    git tag -a "v$NEW_VERSION" -m "Release v$NEW_VERSION"

    echo ""
    echo "✅ Auto-version applied: $CURRENT_VERSION → $NEW_VERSION"
    echo "   Run: git push origin main --tags"
elif [ "$MODE" = "--dry-run" ]; then
    echo ""
    echo "ℹ️  Dry run — no changes made"
    echo "   To apply: ./scripts/auto-version.sh --apply"
fi
EOF
}

generate_setup_hooks_script() {
    cat << 'EOF'
#!/bin/bash
# setup-hooks.sh — Configure git to use portable hooks from scripts/hooks/

set -e

PROJECT_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
HOOKS_DIR="$PROJECT_ROOT/scripts/hooks"

if [ ! -d "$HOOKS_DIR" ]; then
    echo "❌ Hooks directory not found: $HOOKS_DIR"
    exit 1
fi

# Make hooks executable
chmod +x "$HOOKS_DIR"/*

# Configure git to use hooks from scripts/hooks/
git config core.hooksPath "$HOOKS_DIR"

echo "✅ Git hooks configured from: $HOOKS_DIR"
echo ""
echo "Installed hooks:"
ls -la "$HOOKS_DIR"
EOF
}

# ─────────────────────────────────────────────────────────────────────────────────
# TASKFILE GENERATOR
# ─────────────────────────────────────────────────────────────────────────────────

generate_taskfile() {
    local binary="$1"

    cat << EOF
version: '3'

vars:
  BINARY_NAME: ${binary}
  INSTALL_DIR: '{{.HOME}}/.cargo/bin'
  BUILD_DIR: target/release

tasks:
  default:
    desc: Show available tasks
    cmds:
      - task --list

  build:
    desc: Build release binary
    cmds:
      - cargo build --release
    sources:
      - src/**/*.rs
      - Cargo.toml
      - Cargo.lock
    generates:
      - '{{.BUILD_DIR}}/{{.BINARY_NAME}}'

  build-dev:
    desc: Build debug binary
    cmds:
      - cargo build

  install:
    desc: Build and install to ~/.cargo/bin
    deps: [build]
    cmds:
      - cp {{.BUILD_DIR}}/{{.BINARY_NAME}} {{.INSTALL_DIR}}/{{.BINARY_NAME}}
      - chmod +x {{.INSTALL_DIR}}/{{.BINARY_NAME}}
      - echo "✅ Installed to {{.INSTALL_DIR}}/{{.BINARY_NAME}}"

  run:
    desc: Run in development mode
    cmds:
      - cargo run

  test:
    desc: Run all tests
    cmds:
      - cargo test

  test-verbose:
    desc: Run tests with verbose output
    cmds:
      - cargo test -- --nocapture --test-threads=1

  fmt:
    desc: Format code
    cmds:
      - cargo fmt

  fmt-check:
    desc: Check code formatting
    cmds:
      - cargo fmt -- --check

  lint:
    desc: Run clippy linter
    cmds:
      - cargo clippy -- -D warnings

  lint-fix:
    desc: Run clippy with auto-fix
    cmds:
      - cargo clippy --fix --allow-dirty --allow-staged

  check:
    desc: Run all checks (fmt, lint, test)
    cmds:
      - task: fmt-check
      - task: lint
      - task: test

  clean:
    desc: Clean build artifacts
    cmds:
      - cargo clean
      - echo "✅ Cleaned build artifacts"

  audit:
    desc: Check for security vulnerabilities
    cmds:
      - cargo audit

  # SETUP
  setup:
    desc: Full project setup (build, install, hooks)
    cmds:
      - task: install
      - task: setup-hooks
      - echo "✅ Project ready!"

  setup-hooks:
    desc: Configure git to use portable hooks from scripts/hooks/
    cmds:
      - bash scripts/setup-hooks.sh

  # VERSION MANAGEMENT
  version:
    desc: Show current version
    cmds:
      - echo "Cargo.toml: \$(grep '^version' Cargo.toml)"
      - '[ -f VERSION ] && echo "VERSION file: \$(cat VERSION)" || echo "VERSION file: not found"'
      - git describe --tags --always 2>/dev/null || echo "Git tag: none"

  bump-patch:
    desc: Bump PATCH version (0.5.0 → 0.5.1)
    cmds:
      - ./scripts/bump-version.sh \$(./scripts/increment-version.sh patch)

  bump-minor:
    desc: Bump MINOR version (0.5.0 → 0.6.0)
    cmds:
      - ./scripts/bump-version.sh \$(./scripts/increment-version.sh minor)

  bump-major:
    desc: Bump MAJOR version (0.5.0 → 1.0.0)
    cmds:
      - ./scripts/bump-version.sh \$(./scripts/increment-version.sh major)

  auto-version:
    desc: Auto-detect version bump from commits and apply
    cmds:
      - bash scripts/auto-version.sh --apply

  auto-version-dry:
    desc: Preview auto-version bump without applying
    cmds:
      - bash scripts/auto-version.sh --dry-run

  version-check:
    desc: Validate version sync across VERSION, Cargo.toml, lib.rs
    cmds:
      - |
        V1=\$(cat VERSION)
        V2=\$(grep '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
        V3=\$(grep 'pub const VERSION' src/lib.rs | sed 's/.*"\(.*\)".*/\1/')
        TAG=\$(git describe --tags --abbrev=0 2>/dev/null || echo "none")
        echo "VERSION file: \$V1"
        echo "Cargo.toml: \$V2"
        echo "src/lib.rs: \$V3"
        echo "Git tag: \$TAG"
        if [ "\$V1" = "\$V2" ] && [ "\$V1" = "\$V3" ]; then
          echo "✅ All 3 sources in sync"
        else
          echo "❌ VERSION MISMATCH"
          exit 1
        fi

  release:
    desc: Create and push a release tag
    cmds:
      - |
        VERSION=\$(cat VERSION 2>/dev/null || grep '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
        TAG="v\$VERSION"
        echo "Creating release for version \$VERSION"
        echo "Tag: \$TAG"
        if git rev-parse "\$TAG" >/dev/null 2>&1; then
          echo "Tag \$TAG already exists"
          echo "To create a new release, bump the version first:"
          echo "  task bump-patch  # or bump-minor, bump-major"
          exit 1
        fi
        git tag -a "\$TAG" -m "Release \$VERSION"
        git push origin "\$TAG"
        echo "✅ Release tag \$TAG created and pushed"
    sources:
      - VERSION
      - Cargo.toml

  full-release:
    desc: One-command release (auto-bump + commit + tag + push)
    cmds:
      - task: auto-version
      - git push origin main --tags
      - echo "✅ Release pushed. GitHub Actions will build and publish."
EOF
}

# ─────────────────────────────────────────────────────────────────────────────────
# PRESET-SPECIFIC TASKFILE GENERATORS
# ─────────────────────────────────────────────────────────────────────────────────

generate_rust_taskfile() {
    # Standard Rust Taskfile - same as existing generate_taskfile
    generate_taskfile "$1"
}

generate_dioxus_taskfile() {
    local binary="$1"

    cat << 'EOF'
version: '3'

vars:
  BINARY_NAME: ${binary}
  INSTALL_DIR: '{{.HOME}}/.cargo/bin'
  BUILD_DIR: target/release

tasks:
  default:
    desc: Show available tasks
    cmds:
      - task --list

  dev:
    desc: Run Dioxus development server
    cmds:
      - dx serve

  build:
    desc: Build release with Dioxus
    cmds:
      - dx build --release

  build-ssg:
    desc: Build for static site generation (SSG)
    cmds:
      - dx build --release --ssg

  build-dev:
    desc: Build debug binary
    cmds:
      - cargo build

  install:
    desc: Build and install to ~/.cargo/bin
    deps: [build]
    cmds:
      - cp {{.BUILD_DIR}}/{{.BINARY_NAME}} {{.INSTALL_DIR}}/{{.BINARY_NAME}}
      - chmod +x {{.INSTALL_DIR}}/{{.BINARY_NAME}}
      - echo "Installed to {{.INSTALL_DIR}}/{{.BINARY_NAME}}"

  run:
    desc: Run in development mode
    cmds:
      - cargo run

  test:
    desc: Run all tests with all features
    cmds:
      - cargo test --all-features

  test-verbose:
    desc: Run tests with verbose output
    cmds:
      - cargo test -- --nocapture --test-threads=1

  fmt:
    desc: Format code
    cmds:
      - cargo fmt

  fmt-check:
    desc: Check code formatting
    cmds:
      - cargo fmt -- --check

  lint:
    desc: Run clippy linter
    cmds:
      - cargo clippy -- -D warnings

  lint-fix:
    desc: Run clippy with auto-fix
    cmds:
      - cargo clippy --fix --allow-dirty --allow-staged

  check:
    desc: Run all checks (fmt, lint, test)
    cmds:
      - task: fmt-check
      - task: lint
      - task: test

  clean:
    desc: Clean build artifacts including dist/
    cmds:
      - cargo clean
      - rm -rf dist/
      - rm -rf target/dx/
      - echo "Cleaned build artifacts"

  audit:
    desc: Check for security vulnerabilities
    cmds:
      - cargo audit

  setup:
    desc: Full project setup (build, install, hooks)
    cmds:
      - task: install
      - task: setup-hooks
      - echo "Project ready!"

  setup-hooks:
    desc: Configure git to use portable hooks from scripts/hooks/
    cmds:
      - bash scripts/setup-hooks.sh

  version:
    desc: Show current version
    cmds:
      - echo "Cargo.toml: $(grep '^version' Cargo.toml)"
      - '[ -f VERSION ] && echo "VERSION file: $(cat VERSION)" || echo "VERSION file: not found"'
      - git describe --tags --always 2>/dev/null || echo "Git tag: none"

  bump-patch:
    desc: Bump PATCH version (0.5.0 -> 0.5.1)
    cmds:
      - ./scripts/bump-version.sh $(./scripts/increment-version.sh patch)

  bump-minor:
    desc: Bump MINOR version (0.5.0 -> 0.6.0)
    cmds:
      - ./scripts/bump-version.sh $(./scripts/increment-version.sh minor)

  bump-major:
    desc: Bump MAJOR version (0.5.0 -> 1.0.0)
    cmds:
      - ./scripts/bump-version.sh $(./scripts/increment-version.sh major)

  auto-version:
    desc: Auto-detect version bump from commits and apply
    cmds:
      - bash scripts/auto-version.sh --apply

  auto-version-dry:
    desc: Preview auto-version bump without applying
    cmds:
      - bash scripts/auto-version.sh --dry-run

  version-check:
    desc: Validate version sync across VERSION, Cargo.toml, lib.rs
    cmds:
      - |
        V1=$(cat VERSION)
        V2=$(grep '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
        V3=$(grep 'pub const VERSION' src/lib.rs | sed 's/.*"\(.*\)".*/\1/')
        TAG=$(git describe --tags --abbrev=0 2>/dev/null || echo "none")
        echo "VERSION file: $V1"
        echo "Cargo.toml: $V2"
        echo "src/lib.rs: $V3"
        echo "Git tag: $TAG"
        if [ "$V1" = "$V2" ] && [ "$V1" = "$V3" ]; then
          echo "All 3 sources in sync"
        else
          echo "VERSION MISMATCH"
          exit 1
        fi

  release:
    desc: Create and push a release tag
    cmds:
      - |
        VERSION=$(cat VERSION 2>/dev/null || grep '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
        TAG="v$VERSION"
        echo "Creating release for version $VERSION"
        echo "Tag: $TAG"
        if git rev-parse "$TAG" >/dev/null 2>&1; then
          echo "Tag $TAG already exists"
          echo "To create a new release, bump the version first:"
          echo "  task bump-patch # or bump-minor, bump-major"
          exit 1
        fi
        git tag -a "$TAG" -m "Release $VERSION"
        git push origin "$TAG"
        echo "Release tag $TAG created and pushed"
    sources:
      - VERSION
      - Cargo.toml

  full-release:
    desc: One-command release (auto-bump + commit + tag + push)
    cmds:
      - task: auto-version
      - git push origin main --tags
      - echo "Release pushed. GitHub Actions will build and publish."
EOF
}

generate_hybrid_taskfile() {
    local binary="$1"

    cat << 'EOF'
version: '3'

vars:
  BINARY_NAME: ${binary}
  INSTALL_DIR: '{{.HOME}}/.cargo/bin'

tasks:
  default:
    desc: Show available tasks
    cmds:
      - task --list

  backend:dev:
    desc: Run backend in development mode
    cmds:
      - cargo run -p backend

  backend:build:
    desc: Build backend release binary
    cmds:
      - cargo build --release -p backend

  backend:test:
    desc: Run backend tests with all features
    cmds:
      - cargo test -p backend --all-features

  frontend:dev:
    desc: Run Dioxus frontend development server
    cmds:
      - cd frontend && dx serve

  frontend:build:
    desc: Build Dioxus frontend for release
    cmds:
      - cd frontend && dx build --release

  frontend:test:
    desc: Run frontend tests
    cmds:
      - cargo test -p frontend --all-features

  dev:
    desc: Run both backend and frontend (use two terminals)
    cmds:
      - echo "Backend will run on one terminal, Frontend on another"
      - echo "Run: task backend:dev (in terminal 1)"
      - echo "Run: task frontend:dev (in terminal 2)"

  build:
    desc: Build both backend and frontend
    cmds:
      - task: backend:build
      - task: frontend:build
      - echo "Build complete: backend binary + frontend dist/"

  test:
    desc: Run all tests (backend + frontend)
    cmds:
      - cargo test --workspace --all-features

  test-verbose:
    desc: Run tests with verbose output
    cmds:
      - cargo test --workspace --all-features -- --nocapture --test-threads=1

  fmt:
    desc: Format all workspace code
    cmds:
      - cargo fmt

  fmt-check:
    desc: Check formatting
    cmds:
      - cargo fmt -- --check

  lint:
    desc: Run clippy on entire workspace
    cmds:
      - cargo clippy --workspace -- -D warnings

  lint-fix:
    desc: Run clippy with auto-fix
    cmds:
      - cargo clippy --fix --allow-dirty --allow-staged

  check:
    desc: Run all checks (fmt, lint, test)
    cmds:
      - task: fmt-check
      - task: lint
      - task: test

  clean:
    desc: Clean all build artifacts including frontend dist
    cmds:
      - cargo clean
      - rm -rf frontend/dist/
      - rm -rf frontend/target/dx/
      - echo "Cleaned all build artifacts"

  audit:
    desc: Check for security vulnerabilities
    cmds:
      - cargo audit

  setup:
    desc: Full project setup (build, install, hooks)
    cmds:
      - task: setup-hooks
      - echo "Project ready!"

  setup-hooks:
    desc: Configure git to use portable hooks from scripts/hooks/
    cmds:
      - bash scripts/setup-hooks.sh

  version:
    desc: Show current version
    cmds:
      - echo "Cargo.toml: $(grep '^version' Cargo.toml)"
      - '[ -f VERSION ] && echo "VERSION file: $(cat VERSION)" || echo "VERSION file: not found"'
      - git describe --tags --always 2>/dev/null || echo "Git tag: none"

  bump-patch:
    desc: Bump PATCH version (0.5.0 -> 0.5.1)
    cmds:
      - ./scripts/bump-version.sh $(./scripts/increment-version.sh patch)

  bump-minor:
    desc: Bump MINOR version (0.5.0 -> 0.6.0)
    cmds:
      - ./scripts/bump-version.sh $(./scripts/increment-version.sh minor)

  bump-major:
    desc: Bump MAJOR version (0.5.0 -> 1.0.0)
    cmds:
      - ./scripts/bump-version.sh $(./scripts/increment-version.sh major)

  auto-version:
    desc: Auto-detect version bump from commits and apply
    cmds:
      - bash scripts/auto-version.sh --apply

  auto-version-dry:
    desc: Preview auto-version bump without applying
    cmds:
      - bash scripts/auto-version.sh --dry-run

  version-check:
    desc: Validate version sync across VERSION, Cargo.toml, lib.rs
    cmds:
      - |
        V1=$(cat VERSION)
        V2=$(grep '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
        V3=$(grep 'pub const VERSION' backend/src/lib.rs 2>/dev/null | sed 's/.*"\(.*\)".*/\1/')
        TAG=$(git describe --tags --abbrev=0 2>/dev/null || echo "none")
        echo "VERSION file: $V1"
        echo "Cargo.toml: $V2"
        echo "backend/src/lib.rs: $V3"
        echo "Git tag: $TAG"
        if [ "$V1" = "$V2" ] && [ "$V1" = "$V3" ]; then
          echo "All 3 sources in sync"
        else
          echo "VERSION MISMATCH"
          exit 1
        fi

  release:
    desc: Create combined release archive (backend binary + frontend dist)
    cmds:
      - |
        VERSION=$(cat VERSION 2>/dev/null || grep '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
        TAG="v$VERSION"
        echo "Creating release for version $VERSION"
        echo "Tag: $TAG"
        if git rev-parse "$TAG" >/dev/null 2>&1; then
          echo "Tag $TAG already exists"
          echo "To create a new release, bump the version first:"
          echo "  task bump-patch # or bump-minor, bump-major"
          exit 1
        fi
        task build
        mkdir -p release/$VERSION
        cp target/release/backend release/$VERSION/ 2>/dev/null || echo "Warning: backend binary not found"
        cp -r frontend/dist release/$VERSION/ 2>/dev/null || echo "Warning: frontend dist not found"
        tar -czf release-$VERSION.tar.gz -C release $VERSION
        echo "Created release archive: release-$VERSION.tar.gz"
        echo "Contains: backend binary + dist/ folder"
        git tag -a "$TAG" -m "Release $VERSION"
        git push origin "$TAG"
        echo "Release tag $TAG created and pushed"
    sources:
      - VERSION
      - Cargo.toml

  full-release:
    desc: One-command release (auto-bump + build + tag + push)
    cmds:
      - task: auto-version
      - git push origin main --tags
      - echo "Release pushed. GitHub Actions will build and publish."
EOF
}

# ─────────────────────────────────────────────────────────────────────────────────
# BRANCH PROTECTION DOCUMENTATION
# ─────────────────────────────────────────────────────────────────────────────────

generate_branch_protection_doc() {
    cat << 'EOF'
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
EOF
}

# ─────────────────────────────────────────────────────────────────────────────────
# MAIN INSTALLATION LOGIC
# ─────────────────────────────────────────────────────────────────────────────────

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            -p|--project-name)
                PROJECT_NAME="$2"
                shift 2
                ;;
            -u|--git-user)
                GIT_USER="$2"
                shift 2
                ;;
            -e|--git-email)
                GIT_EMAIL="$2"
                shift 2
                ;;
            -o|--output)
                OUTPUT_DIR="$2"
                shift 2
                ;;
            --dry-run)
                DRY_RUN=true
                shift
                ;;
            --force)
                FORCE=true
                shift
                ;;
            --update)
                UPDATE=true
                shift
                ;;
            --no-hooks)
                NO_HOOKS=true
                shift
                ;;
            --no-taskfile)
                NO_TASKFILE=true
                shift
                ;;
 --preset)
 PRESET="$2"
 shift 2
 ;;
            -h|--help)
                print_usage
                exit 0
                ;;
            *)
                log_error "Unknown option: $1"
                print_usage
                exit 1
                ;;
        esac
    done
}

install_files() {
    log_info "Project: $PROJECT_NAME"
    log_info "Binary: $BINARY_NAME"
    log_info "Git User: $GIT_USER"
    log_info "Git Email: $GIT_EMAIL"
    log_info "Output: $OUTPUT_DIR"
    echo ""

    # Detect preset if auto
    if [ "$PRESET" = "auto" ]; then
        PRESET=$(detect_preset)
        log_info "Auto-detected preset: $PRESET"
    fi
    log_info "Using preset: $PRESET"
    echo ""

    # Create VERSION file if it doesn't exist
    if [ ! -f "$OUTPUT_DIR/VERSION" ] && [ "$UPDATE" != true ]; then
        local version
        version=$(grep '^version' "$OUTPUT_DIR/Cargo.toml" | sed 's/version = "\(.*\)"/\1/')
        write_file "$OUTPUT_DIR/VERSION" "$version"
    fi

    # Create src/lib.rs with VERSION constant if it doesn't exist
    if [ ! -f "$OUTPUT_DIR/src/lib.rs" ] && [ "$UPDATE" != true ]; then
        local version
        version=$(cat "$OUTPUT_DIR/VERSION" 2>/dev/null || echo "0.1.0")
        local lib_content="//! $PROJECT_NAME
//!
//! Project description

/// Current version of the crate
pub const VERSION: &str = \"$version\";
"
        write_file "$OUTPUT_DIR/src/lib.rs" "$lib_content"
    fi

    # GitHub Workflows - preset specific
    log_info "Creating GitHub Actions workflows..."
    case "$PRESET" in
        rust)
            write_file "$OUTPUT_DIR/.github/workflows/ci.yml" "$(generate_rust_workflow)"
            ;;
        dioxus)
            write_file "$OUTPUT_DIR/.github/workflows/ci.yml" "$(generate_dioxus_workflow)"
            ;;
        hybrid)
            write_file "$OUTPUT_DIR/.github/workflows/ci.yml" "$(generate_hybrid_workflow)"
            ;;
    esac
    write_file "$OUTPUT_DIR/.github/workflows/auto-version.yml" "$(generate_auto_version_workflow "$GIT_USER" "$GIT_EMAIL" "$BINARY_NAME")"

    # Git Hooks
    if [ "$NO_HOOKS" != true ]; then
        log_info "Creating git hooks..."
        write_file "$OUTPUT_DIR/scripts/hooks/pre-commit" "$(generate_pre_commit_hook)" 755
        write_file "$OUTPUT_DIR/scripts/hooks/commit-msg" "$(generate_commit_msg_hook)" 755
        write_file "$OUTPUT_DIR/scripts/hooks/post-commit" "$(generate_post_commit_hook)" 755
        write_file "$OUTPUT_DIR/scripts/hooks/pre-push" "$(generate_pre_push_hook)" 755
    fi

    # Version Scripts
    log_info "Creating version management scripts..."
    write_file "$OUTPUT_DIR/scripts/bump-version.sh" "$(generate_bump_version_script)" 755
    write_file "$OUTPUT_DIR/scripts/increment-version.sh" "$(generate_increment_version_script)" 755
    write_file "$OUTPUT_DIR/scripts/auto-version.sh" "$(generate_auto_version_script)" 755
    write_file "$OUTPUT_DIR/scripts/setup-hooks.sh" "$(generate_setup_hooks_script)" 755


    # Taskfile - preset specific
    if [ "$NO_TASKFILE" != true ]; then
        log_info "Creating Taskfile.yaml..."
        case "$PRESET" in
            rust)
                write_file "$OUTPUT_DIR/Taskfile.yaml" "$(generate_rust_taskfile "$BINARY_NAME")"
                ;;
            dioxus)
                write_file "$OUTPUT_DIR/Taskfile.yaml" "$(generate_dioxus_taskfile "$BINARY_NAME")"
                ;;
            hybrid)
                write_file "$OUTPUT_DIR/Taskfile.yaml" "$(generate_hybrid_taskfile "$BINARY_NAME")"
                ;;
        esac
    fi

    # Branch Protection Documentation
    log_info "Creating branch protection documentation..."
    write_file "$OUTPUT_DIR/.github/BRANCH_PROTECTION.md" "$(generate_branch_protection_doc)"

    # Make scripts executable
    if [ "$DRY_RUN" != true ]; then
        make_executable "$OUTPUT_DIR/scripts/hooks/pre-commit"
        make_executable "$OUTPUT_DIR/scripts/hooks/commit-msg"
        make_executable "$OUTPUT_DIR/scripts/hooks/post-commit"
        make_executable "$OUTPUT_DIR/scripts/hooks/pre-push"
        make_executable "$OUTPUT_DIR/scripts/bump-version.sh"
        make_executable "$OUTPUT_DIR/scripts/increment-version.sh"
        make_executable "$OUTPUT_DIR/scripts/auto-version.sh"
        make_executable "$OUTPUT_DIR/scripts/setup-hooks.sh"
    fi
}

print_next_steps() {
    echo ""
    echo -e "${GREEN}╔══════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${GREEN}║                    ✅ INSTALLATION COMPLETE                      ║${NC}"
    echo -e "${GREEN}╚══════════════════════════════════════════════════════════════════╝${NC}"
    echo ""
    echo "Next steps:"
    echo ""
    echo "  1. Configure git hooks:"
    echo "     $ ./scripts/setup-hooks.sh"
    echo ""
    echo "  2. Or use Task (if installed):"
    echo "     $ task setup-hooks"
    echo ""
    echo "  3. Verify version sync:"
    echo "     $ task version-check"
    echo ""
    echo "  4. Commit the new CI/CD files:"
    echo "     $ git add ."
    echo "     $ git commit -m 'chore: add CI/CD pipeline'"
    echo ""
    echo "  5. Push to trigger CI:"
    echo "     $ git push origin main"
    echo ""
    echo "For more information, see:"
    echo "  • .github/BRANCH_PROTECTION.md"
    echo "  • Taskfile.yaml (task --list)"
    echo ""
}

# ─────────────────────────────────────────────────────────────────────────────────
# ENTRY POINT
# ─────────────────────────────────────────────────────────────────────────────────

main() {
    print_header
validate_preset() {
    case "$PRESET" in
        rust|dioxus|hybrid|auto)
            # valid preset
            ;;
        *)
            log_error "Invalid preset: $PRESET"
            log_info "Valid presets: rust, dioxus, hybrid, auto (default)"
            exit 1
            ;;
    esac
}

parse_args "$@"
 validate_preset
    check_prerequisites
    detect_project_name
    detect_git_config

    if [ "$DRY_RUN" = true ]; then
        log_warning "DRY RUN MODE — No files will be modified"
        echo ""
    fi

    install_files
    print_next_steps
}

main "$@"
