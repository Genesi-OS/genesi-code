#!/usr/bin/env bash

set -e

resolve_self() {
    if command -v readlink >/dev/null 2>&1; then
        readlink -f "$0" 2>/dev/null && return
    fi
    realpath "$0"
}

is_virtual_machine() {
    if command -v systemd-detect-virt >/dev/null 2>&1 && systemd-detect-virt --quiet; then
        return 0
    fi

    for path in \
        /sys/class/dmi/id/product_name \
        /sys/class/dmi/id/sys_vendor \
        /sys/class/dmi/id/board_vendor; do
        if [[ -r "$path" ]] && grep -qiE 'virtualbox|vmware|qemu|kvm|hyper-v|microsoft corporation|parallels|bhyve' "$path"; then
            return 0
        fi
    done

    return 1
}

APP_PATH="$(resolve_self)"
APP_DIR="$(dirname "$APP_PATH")"
APP_NAME="$(basename "$APP_PATH")"
BIN_PATH="${APP_DIR}/${APP_NAME}.bin"
XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
USER_FLAGS=""

if [[ -f "$XDG_CONFIG_HOME/${APP_NAME}-flags.conf" ]]; then
    USER_FLAGS="$(grep -v '^#' "$XDG_CONFIG_HOME/${APP_NAME}-flags.conf" || true)"
fi

if [[ -z "${GENESI_CODE_DISABLE_VM_GL_FALLBACK:-}" ]] && is_virtual_machine; then
    export WGPU_BACKEND="${WGPU_BACKEND:-gl}"
    export LIBGL_ALWAYS_SOFTWARE="${LIBGL_ALWAYS_SOFTWARE:-1}"
fi

exec "$BIN_PATH" $USER_FLAGS "$@"
