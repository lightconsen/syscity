#!/bin/bash
# Syscity Build Script
# Builds web terminal, cleans Rust artifacts, and builds release binary
#
# Usage:
#   ./build.sh              Build frontend + backend (full, with cloud features)
#   ./build.sh --front      Build frontend only
#   ./build.sh --nocloud    Full build without the cloud Cargo feature
#   (--front --nocloud together is rejected: the flag only applies to the full build)

set -e  # Exit on error

FRONT_ONLY=false
CLOUD=true
for arg in "$@"; do
  case "$arg" in
    --front) FRONT_ONLY=true ;;
    --nocloud) CLOUD=false ;;
    -h|--help)
      echo "Usage:"
      echo "  ./build.sh              Build frontend + backend (full, with cloud features)"
      echo "  ./build.sh --front      Build frontend only"
      echo "  ./build.sh --nocloud    Build without the cloud Cargo feature"
      exit 0
      ;;
    *)
      echo "Unknown option: $arg" >&2
      exit 1
      ;;
  esac
done

if [ "$FRONT_ONLY" = true ] && [ "$CLOUD" = false ]; then
  echo "--nocloud 只在完整构建时生效，不能与 --front 组合" >&2
  echo "   （前端构建不涉及 Rust features；要去掉 --front 再试）" >&2
  exit 1
fi

echo "🚀 Starting Syscity build..."

# Navigate to project root (in case script is run from scripts/)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."

# Build web frontend
echo "📦 Building web frontend..."
cd web
pnpm install --frozen-lockfile
pnpm build
cd ..

if [ "$FRONT_ONLY" = true ]; then
  echo "✅ Frontend build complete!"
  echo "📦 Web bundle: ./dist/"
echo "   Frontend is served from dist/ at runtime — no binary rebuild needed for frontend changes."
  exit 0
fi

# Clean Rust build artifacts
echo "🧹 Cleaning Rust build artifacts..."
cargo clean

# Build release binary (frontend served from dist/ at runtime)
echo "🔨 Building release binary..."
CARGO_ARGS=(build --release)
if [ "$CLOUD" = true ]; then
  echo "   (with cloud features)"
  CARGO_ARGS+=(--features cloud)
else
  echo "   (without cloud features)"
fi
cargo "${CARGO_ARGS[@]}"

echo "✅ Build complete!"
echo "📍 Binary location: ./target/release/syscity"
echo ""
echo "Run with:"
echo "  SYSCITY_BASE_URL=\"<openai-compatible base url>\" \\"
echo "  SYSCITY_API_KEY=\"<api key>\" \\"
echo "  SYSCITY_MODEL=\"<model>\" \\"
echo "  ./target/release/syscity start --foreground"
echo ""
echo "  Tips: SYSCITY_MODEL_PROVIDER / SYSCITY_IS_ANTHROPIC / SYSCITY_CONFIG"
echo "  are honored too; full setup in docs/getting-started.md"
