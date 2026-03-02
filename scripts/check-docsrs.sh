#!/bin/bash
set -e

# Simulate docs.rs build for dial9-tokio-telemetry packages
# Usage:
#   ./scripts/check-docsrs.sh              # Run on all workspace packages
#   ./scripts/check-docsrs.sh <pkg_name>   # Run on specific package

# Determine the target to use based on installed nightly targets
TARGET=$(rustup target list --installed --toolchain nightly | head -1)

check_package() {
    local pkg_name=$1
    local pkg_version=$2
    echo "→ Checking docs.rs build for $pkg_name..."
    
    # Because of workspace unification, we need to actually package the individual packages,
    # then attempt to document the package itself to detect failure in the presence of
    # of some bugs.
    cargo package -p "$pkg_name" --allow-dirty
    (cd "target/package/$pkg_name-$pkg_version" && \
        cargo +nightly docs-rs --target "$TARGET")
}

if [ $# -eq 0 ]; then
    # Run on all workspace packages (skip packages with publish = false)
    packages=$(cargo metadata --no-deps --format-version 1 | \
        jq -r '.packages[] | select(.publish == null or (.publish | length) > 0) | "\(.name) \(.version)"')
    
    while IFS= read -r line; do
        pkg_name=$(echo "$line" | cut -d' ' -f1)
        pkg_version=$(echo "$line" | cut -d' ' -f2)
        check_package "$pkg_name" "$pkg_version"
    done <<< "$packages"
else
    # Run on specific package
    pkg_name=$1
    pkg_version=$(cargo metadata --no-deps --format-version 1 | \
        jq -r ".packages[] | select(.name == \"$pkg_name\") | .version")
    
    if [ -z "$pkg_version" ]; then
        echo "Error: Package $pkg_name not found in workspace"
        exit 1
    fi
    
    check_package "$pkg_name" "$pkg_version"
fi

echo "✓ All docs.rs checks passed!"
