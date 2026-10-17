# fridged

A small Linux daemon that turns an ordinary refrigerator into a temperature-controlled
one (here: a beer fridge) using a Raspberry Pi Zero W, a temperature sensor and a relay,
and publishes its telemetry to Cloud Firestore.

Written in Rust, cross-compiled to a ~1.6 MB static ARMv6 binary that runs in about 3 MB
of RAM.

## What it does

- **Regulates** the temperature with a hysteresis band: compressor ON above `target_max`,
  OFF at or below `target_min`, evaluated every 15 s.
- **Protects the compressor**, whatever the control logic asks for: a minimum OFF time
  before any restart (restarting against residual pressure damages it), a minimum ON
  time (no short-cycling), and a forced rest after a long continuous run.
- **Detects faults**: if the sensor stops answering it falls back to a cautious blind duty
  cycle (SAFE mode); if the compressor keeps running without reaching the band it raises
  an *overrun* fault (door left open, refrigerant loss).
- **Records** one sample every 5 minutes to a local spool and uploads it to Firestore.
  Network outages lose nothing; samples taken before the clock is synchronized are
  re-dated once it is.
- **Can be retuned remotely**: the band can be changed at runtime by writing a document in
  Firestore. The Pi needs no inbound access: it only makes outbound requests.
- **Reports its health** to systemd (watchdog) and optionally to
  [healthchecks.io](https://healthchecks.io) after every upload.

## Hardware

| Part | Notes |
|---|---|
| Raspberry Pi Zero / Zero W | BCM2835, ARMv6. Any Pi should work with the same binary |
| Microchip MCP9808 | I²C temperature sensor, ±0.25 °C. Address `0x18` by default |
| Relay | The one used here is a solid-state relay with a DC input (active high). Electromechanical relay boards are often active low: set `active_low` accordingly |

The sensor goes on I²C bus 1 (header pins 3/5); the relay input on a free GPIO (BCM 26 by
default) and GND.

> The relay switches mains voltage. Wire it with the fridge unplugged, and only if you know
> how to do it safely.

## How it works

```
 control thread (every 15 s, never touches the network)
   MCP9808 ─► controller (hysteresis, SAFE mode, overrun)
                 ─► compressor protections ─► relay GPIO
                 └─► latest state ──┐  single-slot channel, never blocks
                                    ▼
 uploader thread (every 5 min)
   record sample ─► local spool (fsync) ─► Firestore ─► read band settings
                                                     └─► healthchecks.io ping
```

Keeping the relay OFF whenever the daemon is not in control is handled at three levels:
the firmware drives the pin OFF from the first instant of boot (`gpio=26=op,dl` in
`config.txt`), the daemon forces it OFF at startup, on exit and on panic, and systemd runs
`fridged --relay-off` after any stop, including a `SIGKILL`.

The control logic is pure (no hardware, no clock, no I/O), so the whole thing, including
a simulated 24-hour day, is tested on a development machine without any hardware.

## Firestore layout

| Document | Content |
|---|---|
| `devices/{deviceId}/samples/{bootId-uptime}` | one sample every 5 minutes |
| `devices/{deviceId}/status/current` | the latest sample |
| `devices/{deviceId}/settings/current` | optional: `targetMin`, `targetMax` set by you |

The daemon authenticates with a service account (role `roles/datastore.user`); keep the
security rules closed to clients. Writing `settings/current` changes the band within one
upload round; deleting it goes back to the configuration file.

## Build

Prerequisites: stable Rust, a running Docker daemon, and
[`cross`](https://github.com/cross-rs/cross):

```sh
cargo install cross --git https://github.com/cross-rs/cross --locked

./scripts/build-armv6.sh -p fridged
./scripts/verify-armv6.sh target/arm-unknown-linux-musleabihf/release/fridged
```

`verify-armv6.sh` checks that no post-ARMv6 instruction is reachable (QEMU with an
ARM1176 CPU, plus static analysis). The ELF attribute `Tag_CPU_arch: v7` reported on the
binary is a harmless artifact of `ring`'s assembly objects, not a problem.

## Run without hardware

```sh
cargo test -p fridged
cargo run -p fridged -- --config deploy/config.example.toml --simulate -v
```

`--simulate` closes the loop on a simple fridge model and never drives a real relay.

## Install on the Pi

Copy the binary, `deploy/fridged.service`, `deploy/install.sh` and a configuration based
on `deploy/config.example.toml` to the Pi, then:

```sh
sudo ./install.sh fridged config.toml fridged.service [service-account.json]
```

The installer is idempotent. It installs the binary and the systemd unit (dynamic user,
access to the GPIO and I²C devices only), validates the configuration, enables I²C, makes
the firmware hold the relay OFF at boot, keeps the journal across reboots, turns Wi-Fi
power save off (it can leave the Zero W offline until a reboot), and starts the service. The service-account key is
stored root-only and handed to the service through systemd credentials.

Day-to-day:

```sh
systemctl status fridged        # current temperature, compressor state, band
journalctl -u fridged -f        # transitions: compressor ON/OFF, SAFE mode, faults
```

## Configuration

Every key, with its default and its constraints, is documented in
[`deploy/config.example.toml`](deploy/config.example.toml). Unknown keys are rejected and
the whole file is validated at startup (`fridged --check`).

## Repository layout

| Path | Contents |
|---|---|
| `fridged/` | The daemon (library + binary) and a few example tools |
| `spike/` | Toolchain probe (TLS, GPIO, I²C) that shares `Cargo.lock` with the daemon |
| `deploy/` | systemd unit, example configuration, installer |
| `scripts/` | Cross-build, ARMv6 verification, deploy helper |

## License

MIT, see [LICENSE](LICENSE).
