#!/bin/sh
set -eu

REPO="PaulMarkey1/xkeen-sub"
BIN_NAME="xkeen-sub"
DEST="/opt/bin/${BIN_NAME}"

fail() {
    echo "Ошибка: $*" >&2
    exit 1
}

# Сначала определяем архитектуру Entware: она точнее uname -m на роутерах.
ENTWARE_ARCH=""
if command -v opkg >/dev/null 2>&1; then
    ENTWARE_ARCH="$(opkg print-architecture 2>/dev/null | awk '$2 != "all" { print $2 }' | tail -n 1)"
fi

case "$ENTWARE_ARCH" in
    aarch64-3.10|aarch64-k3.10|aarch64)
        ASSET="xkeen-sub-aarch64"
        ;;
    armv7sf-k3.2|armv7-3.2|armv7l)
        ASSET="xkeen-sub-armv7"
        ;;
    mipsel-3.4|mipsel)
        ASSET="xkeen-sub-mipsel"
        ;;
    x86_64-3.2|x86_64)
        ASSET="xkeen-sub-x86_64"
        ;;
    *)
        case "$(uname -m)" in
            aarch64) ASSET="xkeen-sub-aarch64" ;;
            armv7l|armv7) ASSET="xkeen-sub-armv7" ;;
            mipsel) ASSET="xkeen-sub-mipsel" ;;
            x86_64) ASSET="xkeen-sub-x86_64" ;;
            *) fail "Архитектура не поддерживается (Entware: ${ENTWARE_ARCH:-не определена}, uname: $(uname -m))." ;;
        esac
        ;;
esac

URL="https://github.com/${REPO}/releases/latest/download/${ASSET}"

echo "Архитектура Entware: ${ENTWARE_ARCH:-не определена}"
echo "Загрузка ${ASSET}..."

TMP_FILE="$(mktemp)"
trap 'rm -f "$TMP_FILE"' EXIT HUP INT TERM

if command -v curl >/dev/null 2>&1; then
    curl -fL "$URL" -o "$TMP_FILE" || fail "Не удалось скачать бинарный файл."
elif command -v wget >/dev/null 2>&1; then
    wget -O "$TMP_FILE" "$URL" || fail "Не удалось скачать бинарный файл."
else
    fail "Для загрузки нужен curl или wget."
fi

[ -s "$TMP_FILE" ] || fail "Загруженный файл пуст."
mkdir -p "$(dirname "$DEST")"
cp "$TMP_FILE" "$DEST"
chmod 755 "$DEST"

"$DEST" --help >/dev/null 2>&1 || {
    echo "Файл установлен, но проверка запуска с --help завершилась ошибкой." >&2
    echo "Проверьте совместимость бинарного файла с системой роутера." >&2
    exit 1
}

echo "Установка завершена: $DEST"
echo "Запуск: xkeen-sub"
