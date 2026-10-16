#!/usr/bin/env bash
# Installs or updates fridged on the Raspberry Pi. Run on the Pi, as root:
#
#   sudo ./install.sh fridged config.example.toml fridged.service [service-account.json]
#
# Idempotent: an existing /etc/fridged/config.toml is never overwritten, and
# the key is only installed when given. Changes to /boot/firmware/config.txt
# need a reboot to take effect; the script says so.
set -euo pipefail

BIN=${1:?usage: install.sh <fridged> <config.example.toml> <fridged.service> [key.json]}
EXAMPLE=${2:?missing config.example.toml}
UNIT=${3:?missing fridged.service}
KEY=${4:-}

[[ $EUID -eq 0 ]] || { echo "run as root (sudo)" >&2; exit 1; }

CONFIG=/etc/fridged/config.toml
BOOT=/boot/firmware/config.txt
reboot_needed=no

echo "==> binary"
install -m 0755 "$BIN" /usr/local/bin/fridged.new
mv /usr/local/bin/fridged.new /usr/local/bin/fridged

echo "==> configuration"
install -d -m 0755 /etc/fridged
if [[ ! -f $CONFIG ]]; then
    install -m 0644 "$EXAMPLE" "$CONFIG"
    echo "    created $CONFIG from the example: review it"
fi
if [[ -n $KEY ]]; then
    install -m 0600 -o root -g root "$KEY" /etc/fridged/service-account.json
    echo "    service-account key installed (root-only, 0600)"
fi
/usr/local/bin/fridged --config "$CONFIG" --check

echo "==> I2C"
if ! grep -qE '^dtparam=i2c_arm=on' "$BOOT"; then
    raspi-config nonint do_i2c 0
    reboot_needed=yes
fi

echo "==> relay OFF from the first instant of boot"
# The firmware drives the relay pin to its OFF level before Linux even starts,
# so the compressor cannot switch on while fridged is not yet running.
pin=$(sed -nE 's/^[[:space:]]*gpio_pin[[:space:]]*=[[:space:]]*([0-9]+).*/\1/p' "$CONFIG" | head -1)
active_low=$(sed -nE 's/^[[:space:]]*active_low[[:space:]]*=[[:space:]]*(true|false).*/\1/p' "$CONFIG" | head -1)
pin=${pin:-26}
level=dl; [[ ${active_low:-false} == true ]] && level=dh
line="gpio=${pin}=op,${level}"
if ! grep -qxF "$line" "$BOOT"; then
    # Drop a previous fridged line, if the pin or polarity changed.
    sed -i '/# fridged relay OFF at boot/,+1d' "$BOOT"
    printf '# fridged relay OFF at boot\n%s\n' "$line" >> "$BOOT"
    echo "    added '$line' to $BOOT"
    reboot_needed=yes
fi

echo "==> service"
install -m 0644 "$UNIT" /etc/systemd/system/fridged.service
systemctl daemon-reload
# A hand-started instance would fight the service over the GPIO.
pkill -TERM -x fridged 2>/dev/null && sleep 2 || true
systemctl enable fridged.service
systemctl restart fridged.service
sleep 3
systemctl --no-pager --lines=5 status fridged.service || true

if [[ $reboot_needed == yes ]]; then
    echo
    echo "Boot configuration changed: reboot when convenient (sudo reboot)."
fi
