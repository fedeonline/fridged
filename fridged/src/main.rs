//! `fridged` entry point: parses the command line, sets up logging, loads the
//! configuration, wires sensor, relay and controller, and runs the loop.
//!
//! Two threads: the control loop runs on the main thread and never does disk
//! or network I/O for telemetry; it hands each sample to the uploader thread
//! through a single-slot channel (capacity one, drop-oldest): it publishes its
//! state every tick, the uploader records the newest one every
//! `upload_interval`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use fridged::channel::{self, Publisher, Subscriber};
use fridged::cli::{self, Args, Command};
use fridged::clock::SystemClock;
use fridged::config::Config;
use fridged::control::{Band, Params};
use fridged::daemon::{Daemon, SensorOpener};
use fridged::firestore::auth::ServiceAccount;
use fridged::firestore::client::Client;
use fridged::healthcheck::HealthchecksIo;
use fridged::relay::dry_run::DryRunRelay;
use fridged::relay::{self, Limits, Relay};
use fridged::sample::Sample;
use fridged::sensor::{self, Celsius, TempSensor};
use fridged::settings;
use fridged::signals::Shutdown;
use fridged::sim::SimFridge;
use fridged::systemd::Notifier;
use fridged::timestamp::SystemTimeSource;
use fridged::uploader::{Retention, Uploader};
use fridged::{Error, error, logging};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args = match Command::from_env() {
        Ok(Command::Run(args)) => args,
        Ok(Command::Help) => {
            println!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(Command::Version) => {
            println!("fridged {VERSION}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("fridged: {e}\n\n{}", cli::USAGE);
            return ExitCode::from(error::EX_USAGE);
        }
    };

    logging::init(args.verbosity);
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("{}", error::chain(&e));
            ExitCode::from(e.exit_code())
        }
    }
}

fn run(args: &Args) -> fridged::Result<()> {
    log::info!("fridged {VERSION} starting");
    // Installed first: a stop request during startup must not be lost.
    let shutdown = Shutdown::install().map_err(Error::Signals)?;
    let config = Config::load(&args.config)?;

    let c = &config.control;
    let r = &config.relay;
    log::info!(
        "config {}: band {:.1}–{:.1} °C, relay GPIO {}{}",
        args.config.display(),
        c.target_min,
        c.target_max,
        r.gpio_pin,
        if r.active_low { " (active low)" } else { "" },
    );
    if config.firestore.project_id.is_none() {
        log::warn!("firestore.project_id not set: uploading disabled, samples stay in the spool");
    }

    if args.check {
        println!("{}: valid", args.config.display());
        return Ok(());
    }
    if args.relay_off {
        // Opening forces the pin OFF; dropping keeps it an output at OFF.
        drop(relay::open_gpio(r)?);
        log::info!("relay OFF");
        return Ok(());
    }
    if args.once {
        let mut sensor = open_sensor(args, &config)?;
        let t = sensor.read()?;
        log::info!("temperature {t}");
        println!("{t}");
        return Ok(());
    }

    let (relay, open_sensor): (Box<dyn Relay>, SensorOpener) = if args.simulate {
        // The simulated fridge is both the sensor and the relay. The real
        // relay is never driven from simulated temperatures.
        log::info!("simulation: simulated fridge, the GPIO is not driven");
        let fridge = SimFridge::new(mid_band(&config), Rc::new(SystemClock));
        let sensor = fridge.clone();
        (
            Box::new(fridge),
            Box::new(move || Ok(Box::new(sensor.clone()) as Box<dyn TempSensor>)),
        )
    } else {
        let relay: Box<dyn Relay> = if args.dry_run {
            log::info!("dry run: relay commands are logged, the GPIO is not driven");
            Box::new(DryRunRelay::default())
        } else {
            relay::open_gpio(r)?
        };
        let sensor_config = config.sensor.clone();
        let opener: SensorOpener = Box::new(move || sensor::open_mcp9808(&sensor_config));
        (relay, opener)
    };

    // The band: the last one set remotely, if saved, else the config file's.
    let spool_path = spool_path(args, &config);
    let settings_path = settings::path_next_to(&spool_path);
    let config_band = Band {
        min: config.control.target_min,
        max: config.control.target_max,
    };
    let band = match settings::load(&settings_path) {
        Some(saved) => {
            log::info!("band {saved} from the saved remote settings (config: {config_band})");
            saved
        }
        None => config_band,
    };
    let mut params = Params::from(&config.control);
    params.target_min = band.min;
    params.target_max = band.max;

    let (band_tx, band_rx) = channel::latest::<Band>();
    let mut daemon = Daemon::new(
        SystemClock,
        params,
        Limits::from(&config.relay),
        relay,
        open_sensor,
    )?
    .with_settings(band_rx);

    let (tx, rx) = channel::latest::<Sample>();
    let band_sync = BandSync {
        path: settings_path,
        default: config_band,
        applied: band,
        publisher: band_tx,
    };
    let uploader = start_uploader(args, &config, spool_path, rx, band_sync)?;
    let time = SystemTimeSource::new();
    let systemd = Notifier::from_env();
    if let Some(w) = systemd.watchdog_interval() {
        if w < config.sensor.read_interval * 2 {
            log::warn!(
                "WatchdogSec {} s is shorter than two ticks: systemd may kill a healthy daemon",
                w.as_secs()
            );
        }
    }
    systemd.ready();
    let result = daemon.run(config.sensor.read_interval, &shutdown, |tick| {
        tx.publish(Sample::new(tick, time.stamp()));
        systemd.watchdog();
        systemd.status(&status_line(tick));
    });
    systemd.stopping();
    // The relay is already OFF here. Closing the channel stops the uploader,
    // after the round in progress, if any.
    drop(tx);
    if uploader.join().is_err() {
        log::error!("uploader thread panicked");
    }
    result?;
    Ok(())
}

/// One line for `systemctl status`.
fn status_line(tick: &fridged::daemon::Tick) -> String {
    let temp = tick
        .temperature
        .map_or_else(|| "no reading".to_owned(), |t| t.to_string());
    let mode = match tick.mode {
        fridged::control::ControlState::Normal => "",
        fridged::control::ControlState::Safe { .. } => ", SAFE mode",
    };
    let fault = if tick.overrun_fault {
        ", OVERRUN FAULT"
    } else {
        ""
    };
    format!(
        "{temp}, compressor {}, band {}{mode}{fault}",
        tick.relay, tick.band
    )
}

/// Simulated data stays apart from the real fridge's: its own spool (and
/// saved settings, next to it) and its own device id.
fn spool_path(args: &Args, config: &Config) -> PathBuf {
    if args.simulate {
        std::env::temp_dir().join("fridged-sim-spool.jsonl")
    } else {
        config.spool.path.clone()
    }
}

/// What the uploader needs to keep the band in sync with the settings document.
struct BandSync {
    path: PathBuf,
    default: Band,
    applied: Band,
    publisher: Publisher<Band>,
}

fn start_uploader(
    args: &Args,
    config: &Config,
    spool_path: PathBuf,
    rx: Subscriber<Sample>,
    band: BandSync,
) -> fridged::Result<JoinHandle<()>> {
    let f = &config.firestore;
    let device_id = if args.simulate {
        format!("{}-sim", f.device_id)
    } else {
        f.device_id.clone()
    };
    let remote = f.project_id.as_deref().and_then(|project| {
        let client = ServiceAccount::load(&f.credentials_path)
            .and_then(|account| Client::new(&account, project, &f.database, &device_id));
        match client {
            Ok(c) => {
                log::info!(
                    "uploading to Firestore {project}/{}, devices/{device_id}",
                    f.database
                );
                Some(c)
            }
            Err(e) => {
                log::error!(
                    "uploading disabled: {} ({}). Under systemd the key belongs in \
                     /etc/fridged/service-account.json",
                    fridged::error::chain(&e),
                    f.credentials_path.display()
                );
                None
            }
        }
    });

    let days = u64::from(config.spool.max_spool_days);
    let max_age = Duration::from_secs(days * 86_400);
    let per_sample = f.upload_interval.as_secs().max(1);
    let retention = Retention {
        max_age,
        max_samples: usize::try_from(max_age.as_secs() / per_sample).unwrap_or(usize::MAX),
    };
    log::info!("spool {}", spool_path.display());
    let uploading = remote.is_some();
    let mut uploader = Uploader::new(spool_path, remote, f.upload_interval, retention);
    if uploading {
        log::info!("band settings read from devices/{device_id}/settings/current every round");
        uploader = uploader.with_settings(band.path, band.default, band.applied, band.publisher);
    }
    if let Some(url) = &config.healthcheck.ping_url {
        if args.simulate {
            // Simulated pings would hide a real outage of the Pi.
            log::info!("healthcheck disabled in simulation");
        } else if !uploading {
            log::warn!("healthcheck.ping_url set but uploading is disabled: no pings");
        } else {
            let host = url.split('/').nth(2).unwrap_or("?");
            log::info!("healthcheck pings to {host} after every upload round");
            uploader = uploader.with_heartbeat(Box::new(HealthchecksIo::new(url)));
        }
    }
    thread::Builder::new()
        .name("uploader".to_owned())
        .spawn(move || uploader.run(rx, &SystemTimeSource::new(), std::time::Instant::now()))
        .map_err(Error::Uploader)
}

/// The sensor for `--once`.
fn open_sensor(args: &Args, config: &Config) -> fridged::Result<Box<dyn TempSensor>> {
    if args.simulate {
        return Ok(Box::new(SimFridge::new(
            mid_band(config),
            Rc::new(SystemClock),
        )));
    }
    let s = &config.sensor;
    let sensor = sensor::open_mcp9808(s)?;
    log::info!("MCP9808 on /dev/i2c-{} at {:#04x}", s.i2c_bus, s.address);
    Ok(sensor)
}

fn mid_band(config: &Config) -> Celsius {
    let c = &config.control;
    Celsius((c.target_min + c.target_max) / 2.0)
}
