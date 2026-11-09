#!/bin/sh
set -eu

REPO="PaulMarkey1/xkeen-sub"
BIN_NAME="xkeen-sub"
DEST="/opt/sbin/${BIN_NAME}"

fail() {
    echo "Ошибка: $*" >&2
    exit 1
}

# Entware architecture is preferred over uname -m because it reflects the ABI.
ENTWARE_ARCH=""
if command -v opkg >/dev/null 2>&1; then
    ENTWARE_ARCH="$(opkg print-architecture 2>/dev/null | awk '$2 != "all" { print $2 }' | tail -n 1)"
fi
UNAME_ARCH="$(uname -m)"

case "$ENTWARE_ARCH" in
    aarch64-3.10|aarch64-k3.10|aarch64)
        ASSET="xkeen-sub-aarch64"
        ;;
    mipsel-3.4|mipsel)
        ASSET="xkeen-sub-mipsel"
        ;;
    mips-3.4|mips)
        ASSET="xkeen-sub-mips"
        ;;
    *)
        case "$UNAME_ARCH" in
            aarch64) ASSET="xkeen-sub-aarch64" ;;
            mipsel) ASSET="xkeen-sub-mipsel" ;;
            mips) ASSET="xkeen-sub-mips" ;;
            *) fail "Архитектура не поддерживается (Entware: ${ENTWARE_ARCH:-не определена}, uname: $UNAME_ARCH)." ;;
        esac
        ;;
esac

URL="https://github.com/${REPO}/releases/latest/download/${ASSET}"

echo "Архитектура Entware: ${ENTWARE_ARCH:-не определена}"
echo "Архитектура ядра: $UNAME_ARCH"
echo "Загрузка ${ASSET}..."

TMP_FILE="$(mktemp)"
TMP_BIN="/opt/sbin/.${BIN_NAME}.install.$$"
trap 'rm -f "$TMP_FILE" "$TMP_BIN"' EXIT HUP INT TERM

if command -v curl >/dev/null 2>&1; then
    curl -fL "$URL" -o "$TMP_FILE" || fail "Не удалось скачать бинарный файл."
elif command -v wget >/dev/null 2>&1; then
    wget -O "$TMP_FILE" "$URL" || fail "Не удалось скачать бинарный файл."
else
    fail "Для загрузки нужен curl или wget."
fi

[ -s "$TMP_FILE" ] || fail "Загруженный файл пуст."
mkdir -p /opt/sbin
cp "$TMP_FILE" "$TMP_BIN"
chmod 755 "$TMP_BIN"

"$TMP_BIN" --help >/dev/null 2>&1 || fail "Проверка запуска бинарного файла завершилась ошибкой. Текущая установка не заменена."

mv -f "$TMP_BIN" "$DEST"

echo "Установка завершена: $DEST"
echo "Запуск: xkeen-sub"
