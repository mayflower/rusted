mod devices;

use anyhow::{Context, Error, Result};
use clap::Parser;
use core::str::from_utf8;
use devices::DeviceConfig;
use std::fs::write;
use std::io::stderr;
use std::path::Path;
use std::process::{Command, Stdio};
use tokio::runtime::Runtime;
use tracing::{debug, error, info};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

#[derive(Parser, Debug)]
struct Config {
    #[clap(short, long, default_value = "expect_scripts")]
    expect_scripts_dir: String,
    #[clap(long, default_value = "rusted.json")]
    devices: String,
    #[clap(long, default_value = "configs")]
    state_dir: String,
}

fn init_tracing() -> Result<()> {
    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env()?;

    let tracing_subscriber = tracing_subscriber::fmt()
        .with_writer(stderr)
        .without_time()
        .with_env_filter(env_filter)
        .compact()
        .finish();

    tracing::subscriber::set_global_default(tracing_subscriber)
        .context("failed to set global default tracing subscriber")
}

async fn update_device_config_file(
    device_nr: usize,
    device: DeviceConfig,
    expect_scripts_dir: String,
    state_dir: String,
) -> Result<()> {
    let dump_file = device.to_config_dump_path(&state_dir);
    let _ = Path::new(&state_dir).try_exists()?;

    info!("Device {device_nr}: fetching running-config");
    let dump = device.into_filtered_dump(&expect_scripts_dir).await?;

    info!("Device {device_nr}: writing running-config to '{dump_file}'");
    write(dump_file, dump.as_bytes()).map_err(Error::msg)
}

fn commit_and_push(state_dir: &str) -> Result<()> {
    let cmd = Command::new("git")
        .arg("ls-files")
        .arg("--modified")
        .arg("--others")
        .arg("--exclude-standard")
        .current_dir(state_dir)
        .stdout(Stdio::piped())
        .spawn()?;

    let stdout_bytes = cmd.wait_with_output()?.stdout;

    let stdout_str = from_utf8(&stdout_bytes)?;

    for file in stdout_str.lines() {
        info!("commiting changes to file '{state_dir}/{file}'");

        Command::new("git")
            .arg("add")
            .arg(file)
            .current_dir(state_dir)
            .spawn()?
            .wait()?;

        Command::new("git")
            .arg("commit")
            .arg("--message")
            .arg(format!("Update {file}"))
            .current_dir(state_dir)
            .spawn()?
            .wait()?;
    }

    Command::new("git")
        .arg("push")
        .current_dir(state_dir)
        .spawn()?
        .wait()?;

    Ok(())
}

fn main() -> Result<()> {
    init_tracing()?;

    let config = Config::parse();
    debug!("{config:?}");

    let devices = DeviceConfig::read_all_from_file(&config.devices)?;

    info!("fetching configs for {} devices", devices.len());

    let rt = Runtime::new()?;

    let mut tasks = vec![];
    for (i, device) in devices.into_iter().enumerate() {
        let scripts_dir = config.expect_scripts_dir.clone();
        let state_dir = config.state_dir.clone();

        tasks.push(rt.spawn(async move {
            if let Err(e) = update_device_config_file(i + 1, device, scripts_dir, state_dir).await {
                error!("Device {i}: {e:#}");
            }
        }));
    }

    rt.block_on(async { futures::future::join_all(tasks).await });

    commit_and_push(&config.state_dir)
}
