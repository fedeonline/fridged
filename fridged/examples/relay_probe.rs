//! Hardware check of `GpioRelay` on the Pi. Use a pin with **nothing attached**,
//! never the real relay pin: this switches it ON.
//!
//!   relay_probe <bcm-pin> <active-low: true|false> <drop|panic>
//!
//! Opens the pin (must force it OFF), drives it ON, then either drops the relay
//! or panics. Either way the pin must end up an output at the OFF level: check
//! with `pinctrl get <pin>` afterwards.

#[cfg(target_os = "linux")]
fn main() {
    use fridged::relay::{GpioRelay, Relay, RelayState};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let [pin, active_low, mode] = args.as_slice() else {
        eprintln!("usage: relay_probe <bcm-pin> <active-low: true|false> <drop|panic>");
        std::process::exit(64);
    };
    let pin: u8 = pin.parse().expect("pin");
    let active_low: bool = active_low.parse().expect("active-low");

    let mut relay = GpioRelay::open(pin, active_low).expect("open");
    println!("opened: {}", pinctrl(pin));
    relay.set(RelayState::On).expect("set");
    println!("ON:     {}", pinctrl(pin));
    match mode.as_str() {
        "drop" => drop(relay),
        "panic" => panic!("deliberate panic: the hook must turn the relay OFF"),
        other => panic!("unknown mode {other}"),
    }
}

#[cfg(target_os = "linux")]
fn pinctrl(pin: u8) -> String {
    let out = std::process::Command::new("pinctrl")
        .args(["get", &pin.to_string()])
        .output()
        .expect("pinctrl");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("relay_probe needs Linux GPIO");
}
