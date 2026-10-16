//! Reads or changes a device's band in Firestore, like the console does.
//!
//!   firestore_settings <service-account.json> <device-id> get
//!   firestore_settings <service-account.json> <device-id> set <min> <max>
//!   firestore_settings <service-account.json> <device-id> clear
//!
//! The device picks the change up at its next upload round. `clear` deletes
//! the settings document: the device goes back to its config file's band.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use fridged::control::Band;
use fridged::firestore::auth::ServiceAccount;
use fridged::firestore::client::Client;
use fridged::settings;

const USAGE: &str =
    "usage: firestore_settings <key.json> <device-id> get | set <min> <max> | clear";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(key), Some(device), Some(command)) = (args.first(), args.get(1), args.get(2)) else {
        eprintln!("{USAGE}");
        std::process::exit(64);
    };
    let account = ServiceAccount::load(Path::new(key)).expect("service account");
    let mut client =
        Client::new(&account, &account.project_id, "(default)", device).expect("client");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let result = match command.as_str() {
        "get" => client.read_settings(now).map(|doc| match doc {
            None => println!("{device}: no settings, the config file's band applies"),
            Some(doc) => match settings::parse(&doc) {
                Ok(band) => println!("{device}: band {band}"),
                Err(e) => println!("{device}: INVALID settings, ignored by the device: {e}"),
            },
        }),
        "set" => {
            let number = |i: usize| -> f64 {
                args.get(i).and_then(|s| s.parse().ok()).unwrap_or_else(|| {
                    eprintln!("{USAGE}");
                    std::process::exit(64)
                })
            };
            // Written even if invalid, to test how the device reacts.
            let band = Band {
                min: number(3),
                max: number(4),
            };
            if let Err(e) = Band::new(band.min, band.max) {
                println!("warning: the device will ignore this band: {e}");
            }
            client
                .write_settings(band, now)
                .map(|()| println!("{device}: band set to {band}"))
        }
        "clear" => client
            .clear_settings(now)
            .map(|()| println!("{device}: settings removed")),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(64);
        }
    };
    if let Err(e) = result {
        eprintln!("{}", fridged::error::chain(&e));
        std::process::exit(1);
    }
}
