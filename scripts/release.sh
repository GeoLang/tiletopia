#!/usr/bin/env bash
set -euo pipefail

# TileTopia Ecosystem Release Script
# Tags and releases all ecosystem repositories at the given version.
#
# Usage: ./scripts/release.sh <version>
# Example: ./scripts/release.sh 0.1.0
#
# Prerequisites:
#   - gh CLI authenticated
#   - All repos cloned at ~/src/<name>
#   - Working tree clean in all repos

VERSION="${1:-}"
if [[ -z "$VERSION" ]]; then
    echo "Usage: $0 <version>"
    echo "Example: $0 0.1.0"
    exit 1
fi

TAG="v${VERSION}"
SRC_DIR="${TILETOPIA_SRC_DIR:-$HOME/src}"

# All ecosystem repositories in dependency order
# (libraries first, then apps that depend on them)
REPOS=(
    # Core libraries (no internal deps)
    projicio
    fenestra
    topoi
    fluvius
    panoptes
    jung

    # Mid-level libraries
    nubis
    terrano
    geokode
    geodukt
    geogit

    # Routing (depends on nothing else in ecosystem)
    itinera

    # Tile server
    ptolemy

    # Main platform
    tiletopia

    # Frontend
    viewtopia
)

echo "╔══════════════════════════════════════════════════╗"
echo "║  TileTopia Ecosystem Release v${VERSION}        ║"
echo "╚══════════════════════════════════════════════════╝"
echo ""

# Pre-flight checks
echo "▸ Pre-flight checks..."
for repo in "${REPOS[@]}"; do
    dir="$SRC_DIR/$repo"
    if [[ ! -d "$dir" ]]; then
        echo "  ✗ $repo: directory not found at $dir"
        exit 1
    fi
    if [[ -n "$(git -C "$dir" status --porcelain)" ]]; then
        echo "  ✗ $repo: working tree not clean"
        exit 1
    fi
    echo "  ✓ $repo"
done
echo ""

# Version bump
echo "▸ Bumping versions to $VERSION..."
for repo in "${REPOS[@]}"; do
    dir="$SRC_DIR/$repo"
    if [[ -f "$dir/Cargo.toml" ]]; then
        sed -i "s/^version = \".*\"/version = \"$VERSION\"/" "$dir/Cargo.toml"
    fi
    if [[ -f "$dir/package.json" ]]; then
        sed -i "s/\"version\": \".*\"/\"version\": \"$VERSION\"/" "$dir/package.json"
    fi
    echo "  ✓ $repo → $VERSION"
done
echo ""

# Build check (Rust repos only)
echo "▸ Running build checks..."
for repo in "${REPOS[@]}"; do
    dir="$SRC_DIR/$repo"
    if [[ -f "$dir/Cargo.toml" ]]; then
        if ! cargo check --manifest-path "$dir/Cargo.toml" 2>/dev/null; then
            echo "  ✗ $repo: cargo check failed"
            exit 1
        fi
        echo "  ✓ $repo"
    fi
done
echo ""

# Commit, tag, and push
echo "▸ Committing and tagging..."
for repo in "${REPOS[@]}"; do
    dir="$SRC_DIR/$repo"
    pushd "$dir" > /dev/null
    if [[ -n "$(git status --porcelain)" ]]; then
        git add -A
        git commit -m "release: v${VERSION}"
    fi
    git tag -a "$TAG" -m "Release $TAG"
    git push origin master --tags
    echo "  ✓ $repo → $TAG pushed"
    popd > /dev/null
done
echo ""

# Create GitHub releases (triggers release.yml workflows)
echo "▸ Creating GitHub releases..."
for repo in "${REPOS[@]}"; do
    dir="$SRC_DIR/$repo"
    pushd "$dir" > /dev/null
    gh release create "$TAG" \
        --title "$TAG" \
        --generate-notes \
        --latest 2>/dev/null || echo "  ⚠ $repo: release may already exist"
    echo "  ✓ $repo"
    popd > /dev/null
done
echo ""

echo "╔══════════════════════════════════════════════════╗"
echo "║  Release $TAG complete!                         ║"
echo "║  CI workflows will build cross-platform binaries║"
echo "╚══════════════════════════════════════════════════╝"
