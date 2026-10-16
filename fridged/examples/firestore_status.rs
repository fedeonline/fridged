//! Reads a device's status document from Firestore. Read-only.
//!
//!   firestore_status <service-account.json> [device-id] [database]

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use fridged::firestore::auth::ServiceAccount;
use fridged::firestore::client::Client;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(key) = args.first() else {
        eprintln!("usage: firestore_status <service-account.json> [device-id] [database]");
        std::process::exit(64);
    };
    let device = args.get(1).map_or("fridge", String::as_str);
    let database = args.get(2).map_or("(default)", String::as_str);

    let account = ServiceAccount::load(Path::new(key)).expect("service account");
    let mut client = Client::new(&account, &account.project_id, database, device).expect("client");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    match client.get(&format!("devices/{device}/status/current"), now) {
        Ok(doc) => {
            let f = &doc["fields"];
            let s = |k: &str| f[k]["stringValue"].as_str().unwrap_or("?").to_owned();
            println!(
                "{device}: {} temperature {} band {}–{} relay {} ({}, {}) mode {} overrun {} | updated {}",
                f["time"]["timestampValue"].as_str().unwrap_or("?"),
                f["temperature"]["doubleValue"],
                f["targetMin"]["doubleValue"],
                f["targetMax"]["doubleValue"],
                s("relay"),
                s("wanted"),
                s("reason"),
                s("mode"),
                f["overrunFault"]["booleanValue"],
                doc["updateTime"].as_str().unwrap_or("?"),
            );
        }
        Err(e) => {
            eprintln!("{}", fridged::error::chain(&e));
            std::process::exit(1);
        }
    }
}
