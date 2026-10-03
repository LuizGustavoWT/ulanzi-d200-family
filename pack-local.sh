#!/bin/bash
set -e

# Build the plugin and install it straight into OpenDeck's plugin folder.
# No git, no GitHub release — just build + copy, so you can iterate quickly.
#
#   ./pack-local.sh          release build (default)
#   ./pack-local.sh debug    faster debug build

MODE="${1:-release}"

BINARY_NAME="rs-ulanzi-d200-linux"
PLUGIN_UUID="com.glmagalhaes.ulanzi.d200.sdPlugin"
DEST_DIR="$HOME/.var/app/me.amankhanna.opendeck/config/opendeck/plugins/$PLUGIN_UUID"

if [ "$MODE" = "debug" ]; then
    BUILD_DIR="target/debug"
else
    BUILD_DIR="target/release"
fi

echo "🔨 Compilando em modo $MODE..."
cargo build ${MODE:+$([ "$MODE" = release ] && echo --release)}

BIN_SRC="$BUILD_DIR/$BINARY_NAME"
if [ ! -f "$BIN_SRC" ]; then
    echo "❌ Erro: binário não encontrado em $BIN_SRC"
    exit 1
fi

# OpenDeck keeps the plugin running, and Linux refuses to overwrite a binary
# that is currently executing ("Text file busy"). Stop it first.
echo "🛑 Parando o plugin em execução (se houver)..."
pkill -f "$PLUGIN_UUID/$BINARY_NAME" 2>/dev/null || true
pkill -f "$DEST_DIR/$BINARY_NAME" 2>/dev/null || true
# Give the process a moment to release the file.
for _ in $(seq 1 20); do
    if pgrep -f "$BINARY_NAME" >/dev/null 2>&1; then
        sleep 0.1
    else
        break
    fi
done

echo "📦 Instalando em: $DEST_DIR"
mkdir -p "$DEST_DIR/assets" "$DEST_DIR/propertyInspector"

# Copy the freshly built binary over the old one.
cp -f "$BIN_SRC" "$DEST_DIR/$BINARY_NAME"
chmod +x "$DEST_DIR/$BINARY_NAME"

cp src/manifest.json "$DEST_DIR/"
cp config.yaml "$DEST_DIR/"

cp -r src/assets/. "$DEST_DIR/assets/"
if [ -d src/propertyInspector ]; then
    cp -r src/propertyInspector/. "$DEST_DIR/propertyInspector/"
fi

echo "✅ Plugin atualizado localmente!"
echo "🔄 Reinicie o OpenDeck para aplicar (feche e abra de novo)."
echo "   Devices e ações novas exigem restart; mudanças de imagem às vezes não."
