#!/bin/sh
set -eu

REPO="PaulMarkey1/xkeen-sub"
BIN_NAME="xkeen-sub"
DEST="/opt/sbin/${BIN_NAME}"
XRAY_BIN="${XRAY_BIN:-/opt/sbin/xray}"
XRAY_CONFIG_DIR="/opt/etc/xray/configs"
XRAY_ASSET_DIR="/opt/etc/xray/dat"

RED='\033[31m'
CYAN='\033[36m'
RESET='\033[0m'

fail() {
    printf '%b%s%b\n' "$RED" "$*" "$RESET" >&2
    exit 1
}

# Check prerequisites before downloading or replacing xkeen-sub.
if ! command -v xkeen >/dev/null 2>&1; then
    printf '%b%s%b\n' "$RED" "XKeen — отсутствует." "$RESET" >&2
    printf '%b%s%b\n' "$RED" "Установите XKeen, после чего запустите установку xkeen-sub повторно." "$RESET" >&2
    exit 1
fi

if [ ! -x "$XRAY_BIN" ]; then
    printf '%b%s%b\n' "$RED" "Xray — не найден." "$RESET" >&2
    printf '%b%s' "$RED" "Переустановите Xray с помощью команды " >&2
    printf '%b%s%b' "$CYAN" "xkeen -ux" "$RESET" >&2
    printf '%b%s%b\n' "$RED" "." "$RESET" >&2
    exit 1
fi

if [ ! -d "$XRAY_CONFIG_DIR" ]; then
    fail "Каталог конфигурации Xray не найден: $XRAY_CONFIG_DIR"
fi

if [ ! -d "$XRAY_ASSET_DIR" ]; then
    fail "Каталог данных Xray не найден: $XRAY_ASSET_DIR"
fi

# Entware architecture is preferred over uname -m because it reflects the ABI.
ENTWARE_ARCH=""
if command -v opkg >/dev/null 2>&1; then
    ENTWARE_ARCH="$(opkg print-architecture 2>/dev/null | awk '$2 != "all" && $2 != "noarch" { print $2 }' | tail -n 1)"
fi
UNAME_ARCH="$(uname -m)"

case "$ENTWARE_ARCH" in
    aarch64|aarch64-*)
        ASSET="xkeen-sub-aarch64"
        ;;
    mipsel|mipsel-*)
        ASSET="xkeen-sub-mipsel"
        ;;
    mips|mips-*)
        ASSET="xkeen-sub-mips"
        ;;
    "")
        # uname -m reports "mips" for both MIPS endiannesses, so it cannot
        # safely select a MIPS binary when Entware architecture is unknown.
        case "$UNAME_ARCH" in
            aarch64) ASSET="xkeen-sub-aarch64" ;;
            mipsel) ASSET="xkeen-sub-mipsel" ;;
            mips) fail "Невозможно определить endian MIPS без архитектуры Entware (uname: $UNAME_ARCH)." ;;
            *) fail "Архитектура не поддерживается (Entware не определена, uname: $UNAME_ARCH)." ;;
        esac
        ;;
    *)
        fail "Архитектура Entware не поддерживается: $ENTWARE_ARCH (uname: $UNAME_ARCH)."
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

# Inspect every JSON/JSONC file and create the Observatory placeholder only
# after the prerequisite checks above have succeeded.
"$TMP_BIN" install-check || fail "Проверка конфигурации Observatory не пройдена. Установка xkeen-sub отменена."

VERSION="$("$TMP_BIN" --version 2>/dev/null)" || VERSION=""
case "$VERSION" in
    "xkeen-sub "*) ;;
    *) VERSION="не определена (бинарный файл старого релиза)" ;;
esac
echo "Устанавливаемая версия: $VERSION"

mv -f "$TMP_BIN" "$DEST"

echo "Установка завершена: $DEST"
echo "Установлена версия: $VERSION"
echo "Запуск: xkeen-sub"
