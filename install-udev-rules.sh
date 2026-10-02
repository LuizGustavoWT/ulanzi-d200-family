#!/bin/bash
# Install the udev rule that lets the plugin open the Ulanzi D200/D200H/D200X
# hidraw node without root, and reload the rules so the change applies
# immediately (no unplug/replug required).
set -e

RULE_SRC="$(dirname "$(readlink -f "$0")")/40-opendeck-ulanzi.rules"
RULE_DST="/etc/udev/rules.d/40-opendeck-ulanzi.rules"

if [ ! -f "$RULE_SRC" ]; then
    echo "Error: $RULE_SRC not found" >&2
    exit 1
fi

if [ "$(id -u)" -ne 0 ]; then
    echo "This script needs root. Re-running with sudo..."
    exec sudo "$0" "$@"
fi

echo "Installing $RULE_DST"
install -m 0644 "$RULE_SRC" "$RULE_DST"

echo "Reloading udev rules..."
udevadm control --reload-rules
udevadm trigger --subsystem-match=hidraw --action=add

# Apply the ACL immediately to the nodes that exist right now, so the user does
# not have to unplug and replug the device.
echo "Applying device permissions to currently connected hidraw nodes..."
udevadm trigger --name-match=hidraw0 2>/dev/null || true
for node in /dev/hidraw*; do
    [ -e "$node" ] || continue
    if udevadm info --query=property --name="$node" 2>/dev/null | grep -q 'ID_VENDOR_ID=2207'; then
        setfacl -m "u:$(logname 2>/dev/null || id -un):rw" "$node" 2>/dev/null || true
        chmod 0660 "$node" 2>/dev/null || true
    fi
done

echo
echo "Done. Current permissions:"
for node in /dev/hidraw*; do
    [ -e "$node" ] || continue
    if udevadm info --query=property --name="$node" 2>/dev/null | grep -q 'ID_VENDOR_ID=2207'; then
        ls -l "$node"
    fi
done

echo
echo "If the plugin was already running, restart OpenDeck so it reconnects."
