# CLAUDE.md

**Before anything else, read `PROGRESS.md` if it exists**: it holds the current phase,
the next task, decisions already made, and open blockers. It is a local working file,
deliberately kept out of the public repository (it holds details of the actual
installation). Work proceeds in interruptible phases and the state lives there, not in the
conversation.

After each completed task: tick the checkbox in `PROGRESS.md`, add a line to the session
log, and commit it together with the code.

## What this project is

A Rust daemon that regulates the temperature of a refrigerator on a Raspberry Pi Zero W
(ARMv6). It reads an MCP9808 over I²C, drives a relay over GPIO, and publishes to Cloud
Firestore.

## Constraints that must not be violated

- **The control thread never performs network I/O.** If Firestore is unreachable the
  fridge must keep working. It talks to the uploader through a bounded channel with a
  drop-oldest policy (capacity one: it publishes its state every tick, the uploader takes
  the newest once per round).
- **Compressor protections live in the `relay` module (`relay/protection.rs`) and always apply**, whatever the control
  layer asks for: `min_off_time`, `min_on_time`, `max_continuous_on`. Restarting a
  compressor against residual system pressure damages it.
- **`control.rs` stays pure**: no hardware, no `SystemTime`, no I/O. It takes a reading and
  an instant, and returns a command. That is what makes it testable on macOS.
- **The relay must be forced OFF at startup** and on clean exit. `rppal` maps GPIO memory
  directly: after a crash the pin keeps its state, the kernel does not reset it.
- **No async runtime.** Blocking I/O on separate threads: on ARMv6 every extra dependency
  is build time, and `tokio` would buy us nothing here.
- **Never commit credentials.** `service-account.json`, `*.pem` and `*.key` are in
  `.gitignore`.

## Cross-compilation

Target: `arm-unknown-linux-musleabihf` (ARMv6 + hardfloat, static binary).

```sh
./scripts/build-armv6.sh
./scripts/verify-armv6.sh target/arm-unknown-linux-musleabihf/release/<binary>
```

- The `Tag_CPU_arch: v7` ELF attribute is expected and does **not** indicate a problem: it
  is merged down with `ring`'s objects. Do not use it as a verdict.
- The static check in `verify-armv6.sh` is trustworthy **only on unstripped binaries**
  (objdump needs the `$a`/`$d` mapping symbols to tell data from code). On a stripped
  binary, constant tables produce false positives.
- After every dependency or cross-image update, **re-run the verification**.
- Do not export `RUSTFLAGS` in the environment: it would override (not extend) the one in
  `.cargo/config.toml`, losing `-C target-cpu=arm1176jzf-s`.

## Conventions

- **All code, comments and documentation in English.** Conversation with the user may be
  in Italian, but nothing in the repository is.
- `cargo fmt` and `cargo clippy` clean before every commit. For Linux-only code also run
  `cross clippy --target arm-unknown-linux-musleabihf -p fridged --all-targets -- -D warnings`:
  on macOS clippy never sees it.
- Control-logic tests run on macOS without hardware: keep the `TempSensor`, `Relay` and
  `Clock` abstractions and their fake implementations.
- When adding a dependency, check that it cross-compiles for ARMv6 **before** building on
  top of it.
