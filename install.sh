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

package_installed() {
    opkg list-installed 2>/dev/null | awk -v package="$1" '$1 == package { found=1 } END { exit !found }'
}

cron_ready() {
    command -v crontab >/dev/null 2>&1 || return 1
    if ! command -v cron >/dev/null 2>&1 && ! command -v crond >/dev/null 2>&1; then
        return 1
    fi
    for script in /opt/etc/init.d/S10*cron* /opt/etc/init.d/s10*cron*; do
        [ -x "$script" ] && return 0
    done
    return 1
}

ensure_required_packages() {
    command -v opkg >/dev/null 2>&1 || fail "Не найден opkg. Сначала установите Entware — без пакетного менеджера невозможно автоматически установить зависимости."

    # Entware opkg downloads packages through wget. Without it, opkg cannot
    # install the missing packages itself.
    command -v wget >/dev/null 2>&1 || fail "Не найден wget, необходимый пакетному менеджеру Entware. Восстановите wget/Entware и повторите установку."

    MISSING=""
    package_installed curl || MISSING="$MISSING curl"
    package_installed ca-bundle || MISSING="$MISSING ca-bundle"
    if ! cron_ready; then
        MISSING="$MISSING cron"
    fi

    if [ -n "$MISSING" ]; then
        echo "Устанавливаем необходимые пакеты Entware:$MISSING"
        opkg update || fail "Не удалось обновить список пакетов Entware. Проверьте подключение и работу wget."
        # opkg installs missing dependencies of these packages automatically.
        # Deliberately do not run a system-wide upgrade.
        opkg install $MISSING || fail "Не удалось установить необходимые пакеты:$MISSING"
    else
        echo "Все необходимые пакеты Entware уже установлены."
    fi

    package_installed curl || fail "Пакет curl не установлен."
    package_installed ca-bundle || fail "Пакет ca-bundle не установлен."
    cron_ready || fail "Служба cron установлена не полностью: не найдены crontab, cron/crond или сценарий запуска в /opt/etc/init.d."
    command -v curl >/dev/null 2>&1 || fail "Пакет curl установлен, но команда curl недоступна."
    if ! curl --version >/dev/null 2>&1; then
        echo "Предупреждение: curl установлен, но не запускается. При загрузке будет предпринята попытка использовать wget."
    fi
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

# Check and install missing Entware dependencies only after platform
# prerequisites have been confirmed, and before downloading/replacing the app.
ensure_required_packages

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

if command -v curl >/dev/null 2>&1 && curl -fL "$URL" -o "$TMP_FILE"; then
    :
elif command -v wget >/dev/null 2>&1; then
    echo "curl не сработал, пробуем wget..."
    wget -O "$TMP_FILE" "$URL" || fail "Не удалось скачать бинарный файл через curl и wget."
else
    fail "Не удалось скачать бинарный файл через curl; wget недоступен."
fi

[ -s "$TMP_FILE" ] || fail "Загруженный файл пуст."
mkdir -p /opt/sbin
cp "$TMP_FILE" "$TMP_BIN"
chmod 755 "$TMP_BIN"

"$TMP_BIN" --help >/dev/null 2>&1 || fail "Проверка запуска бинарного файла завершилась ошибкой. Текущая установка не заменена."

# Review Xray balancers and Observatory settings before replacing the app.
# Any accepted config changes are backed up and validated before being saved.
"$TMP_BIN" install-check || fail "Проверка конфигурации Xray не пройдена. Установка xkeen-sub отменена."

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
