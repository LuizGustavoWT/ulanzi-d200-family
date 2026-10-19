#!/bin/bash
set -e

# Build the plugin and install it straight into OpenDeck's plugin folder.
# No git, no GitHub release — just build + copy, so you can iterate quickly.
#
#   ./pack-local.sh                 release build into every install (default)
#   ./pack-local.sh native debug    only ~/.config/opendeck, faster debug build
#   ./pack-local.sh macos           only ~/Library/Application Support/opendeck

BINARY_NAME="rs-ulanzi-d200-linux"
PLUGIN_UUID="com.glmagalhaes.ulanzi.d200.sdPlugin"

# OpenDeck keeps its plugins in a different folder per install flavour, and
# each keeps its own copy. Install into whichever ones actually exist so they
# all stay in sync.
#
#   ./pack-local.sh                 -> every existing install (auto)
#   ./pack-local.sh native          -> only ~/.config/opendeck               (Linux)
#   ./pack-local.sh flatpak         -> only ~/.var/app/me.amankhanna.opendeck (Linux)
#   ./pack-local.sh macos           -> only ~/Library/Application Support/opendeck
#   ./pack-local.sh native release  -> target and build mode
TARGETS=()
MODE=""

for arg in "$@"; do
    case "$arg" in
        native|flatpak|macos|all) TARGETS+=("$arg") ;;
        debug|release) MODE="$arg" ;;
        "") ;;
        *) echo "Unknown argument: $arg"; exit 1 ;;
    esac
done

NATIVE_DIR="$HOME/.config/opendeck/plugins/$PLUGIN_UUID"
FLATPAK_DIR="$HOME/.var/app/me.amankhanna.opendeck/config/opendeck/plugins/$PLUGIN_UUID"
MACOS_DIR="$HOME/Library/Application Support/opendeck/plugins/$PLUGIN_UUID"

MODE="${MODE:-release}"

# On macOS there is only ever the one Application Support location; on Linux
# the .deb and the Flatpak each have their own.
if [ ${#TARGETS[@]} -eq 0 ]; then
    if [ "$(uname -s)" = "Darwin" ]; then
        TARGETS=(macos)
    else
        TARGETS=(native flatpak)
        [ -d "$FLATPAK_DIR" ] || TARGETS=(native)
    fi
fi

DEST_DIRS=()
for target in "${TARGETS[@]}"; do
    case "$target" in
        native)  DEST_DIRS+=("$NATIVE_DIR") ;;
        flatpak) DEST_DIRS+=("$FLATPAK_DIR") ;;
        macos)   DEST_DIRS+=("$MACOS_DIR") ;;
    esac
done

if [ "$MODE" = "debug" ]; then
    BUILD_DIR="target/debug"
else
    BUILD_DIR="target/release"
fi

echo "🔨 Compilando em modo $MODE..."
CARGO_FLAGS=""
if [ "$MODE" = "release" ]; then
    CARGO_FLAGS="--release"
fi
cargo build $CARGO_FLAGS

BIN_SRC="$BUILD_DIR/$BINARY_NAME"
if [ ! -f "$BIN_SRC" ]; then
    echo "❌ Erro: binário não encontrado em $BIN_SRC"
    exit 1
fi

# OpenDeck keeps the plugin running, and both Linux and macOS refuse to
# overwrite a binary that is currently executing ("Text file busy"). Stop it
# first.
echo "🛑 Parando o plugin em execução (se houver)..."
pkill -f "$BINARY_NAME" 2>/dev/null || true
for _ in $(seq 1 20); do
    pgrep -f "$BINARY_NAME" >/dev/null 2>&1 || break
    sleep 0.1
done

for DEST_DIR in "${DEST_DIRS[@]}"; do
    echo "📦 Instalando em: $DEST_DIR"
    mkdir -p "$DEST_DIR/assets" "$DEST_DIR/propertyInspector"

    cp -f "$BIN_SRC" "$DEST_DIR/$BINARY_NAME"
    chmod +x "$DEST_DIR/$BINARY_NAME"

    if [ "$(uname -s)" = "Darwin" ]; then
        # Gatekeeper kills an arm64 binary whose signature does not match its
        # contents, and copying can leave a stale one behind. Re-apply the
        # ad-hoc signature the linker would have produced, and drop any
        # quarantine flag inherited from a downloaded archive.
        codesign --force --sign - "$DEST_DIR/$BINARY_NAME" 2>/dev/null || true
        xattr -dr com.apple.quarantine "$DEST_DIR" 2>/dev/null || true
    fi

    cp src/manifest.json "$DEST_DIR/"
    cp config.yaml "$DEST_DIR/"

    cp -r src/assets/. "$DEST_DIR/assets/"
    if [ -d src/propertyInspector ]; then
        cp -r src/propertyInspector/. "$DEST_DIR/propertyInspector/"
    fi
    echo "   -> $("$DEST_DIR/$BINARY_NAME" --version)"
done

echo "✅ Plugin atualizado localmente!"
echo "🔄 Reinicie o OpenDeck para aplicar (feche e abra de novo)."
echo "   O .deb e o Flatpak leem pastas diferentes: reinicie/feche os dois se usar ambos."
echo "   Devices e ações novas exigem restart; mudanças de imagem às vezes não."
