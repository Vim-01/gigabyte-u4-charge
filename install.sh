#!/usr/bin/env bash
set -euo pipefail

if [[ "${EUID}" -ne 0 ]]; then
    echo "Ошибка / Error: run with sudo: sudo $0" >&2
    exit 1
fi

BUILD_USER="${SUDO_USER:-nobody}"
if [[ "${BUILD_USER}" == "root" ]]; then
    echo "Error: do not run makepkg as pure root; run via sudo from a non-root user." >&2
    exit 1
fi

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DAEMON_DIR="${REPO_DIR}/daemon"
KERNEL_DIR="${REPO_DIR}/kernel"
SYSTEMD_DIR="${REPO_DIR}/systemd"

echo "==> 1. Building u4-charge (Rust release) as ${BUILD_USER}..."
runuser -u "${BUILD_USER}" -- cargo build --release --manifest-path "${DAEMON_DIR}/Cargo.toml"
BIN_SRC="${DAEMON_DIR}/target/release/u4-charge"

echo "==> 2. Building DKMS package clevo-drivers-u4-dkms as ${BUILD_USER}..."
runuser -u "${BUILD_USER}" -- bash -c "cd \"${KERNEL_DIR}\" && makepkg -f --noconfirm"

shopt -s nullglob
PKGS=("${KERNEL_DIR}"/clevo-drivers-u4-dkms-*.pkg.tar.zst)
shopt -u nullglob
if [[ "${#PKGS[@]}" -eq 0 ]]; then
    echo "Error: compiled DKMS package not found in ${KERNEL_DIR}" >&2
    exit 1
fi
PKG_FILE="${PKGS[-1]}"

echo "==> 3. Stopping active daemons and unloading old kernel modules..."
if systemctl is-active --quiet u4-charge.service; then
    systemctl stop u4-charge.service
fi

TCCD_SHOULD_START=0
if systemctl is-active --quiet tccd.service || systemctl is-enabled --quiet tccd.service 2>/dev/null; then
    TCCD_SHOULD_START=1
fi
if systemctl is-active --quiet tccd.service; then
    systemctl stop tccd.service
fi

unload_if_loaded() {
    local mod="$1"
    if [[ -d "/sys/module/${mod}" ]]; then
        modprobe -r "${mod}"
    fi
}

if [[ -d /sys/module/tuxedo_keyboard/holders ]]; then
    for holder_path in /sys/module/tuxedo_keyboard/holders/*; do
        [[ -e "${holder_path}" ]] || continue
        unload_if_loaded "$(basename "${holder_path}")"
    done
fi

for mod in tuxedo_io clevo_acpi clevo_wmi uniwill_wmi tuxedo_keyboard tuxedo_compatibility_check; do
    unload_if_loaded "${mod}"
done

if [[ -d /sys/module/tuxedo_keyboard ]]; then
    echo "Error: failed to unload old tuxedo_keyboard module from memory!" >&2
    exit 1
fi

echo "==> 4. Installing ${PKG_FILE##*/}..."
installed_pkgs=$'\n'"$(pacman -Qq)"$'\n'
for conflict_pkg in clevo-drivers-dkms-git clevo-drivers-dkms tuxedo-drivers-dkms tuxedo-keyboard-dkms tuxedo-keyboard-ite-dkms; do
    if [[ "${installed_pkgs}" == *$'\n'"${conflict_pkg}"$'\n'* ]]; then
        pacman -Rdd --noconfirm "${conflict_pkg}"
    fi
done
pacman -U --noconfirm "${PKG_FILE}"

echo "==> 5. Loading patched kernel modules and verifying sysfs..."
modprobe tuxedo_compatibility_check || true
modprobe tuxedo_keyboard
modprobe clevo_acpi || true
modprobe clevo_wmi || true
modprobe tuxedo_io || true

if [[ "${TCCD_SHOULD_START}" -eq 1 ]]; then
    systemctl start tccd.service
fi

if [[ ! -f /sys/class/power_supply/BAT0/charge_control_end_threshold ]]; then
    echo "Error: /sys/class/power_supply/BAT0/charge_control_end_threshold missing!" >&2
    exit 1
fi

if [[ -e /sys/devices/platform/tuxedo_keyboard/ec_acpi_test ]]; then
    echo "Error: stale debug sysfs attribute ec_acpi_test still loaded!" >&2
    exit 1
fi

if [[ ! -r /sys/devices/platform/tuxedo_keyboard/ec_live_state ]]; then
    echo "Error: read-only sysfs attribute ec_live_state missing!" >&2
    exit 1
fi

echo "==> 6. Installing binary, configuration, and systemd units..."
install -Dm755 "${BIN_SRC}" /usr/local/bin/u4-charge

if [[ ! -f /etc/u4-charge.conf ]]; then
    install -Dm644 "${SYSTEMD_DIR}/u4-charge.conf" /etc/u4-charge.conf
else
    echo "    Preserving existing /etc/u4-charge.conf."
fi

install -Dm644 "${SYSTEMD_DIR}/u4-charge.service" /etc/systemd/system/u4-charge.service
install -Dm644 "${SYSTEMD_DIR}/u4-charge-notify.service" /etc/systemd/user/u4-charge-notify.service

systemctl daemon-reload
systemctl enable --now u4-charge.service
systemctl --global enable u4-charge-notify.service

SUDO_UID_VAL="$(id -u "${BUILD_USER}")"
if [[ -d "/run/user/${SUDO_UID_VAL}" && -S "/run/user/${SUDO_UID_VAL}/bus" ]]; then
    runuser -u "${BUILD_USER}" -- env \
        XDG_RUNTIME_DIR="/run/user/${SUDO_UID_VAL}" \
        DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/${SUDO_UID_VAL}/bus" \
        systemctl --user daemon-reload || true
    runuser -u "${BUILD_USER}" -- env \
        XDG_RUNTIME_DIR="/run/user/${SUDO_UID_VAL}" \
        DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/${SUDO_UID_VAL}/bus" \
        systemctl --user start --no-block u4-charge-notify.service || true
fi

echo "==> Done:"
sleep 1
/usr/local/bin/u4-charge status
