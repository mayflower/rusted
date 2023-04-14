mod devices;

use anyhow::{anyhow, bail, Context, Error, Result};
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

/// rusted config, command line parameter parsing is done using `clap_derive`
#[derive(Parser, Debug)]
struct Config {
    /// directory that contains expect scripts for each device model
    #[clap(short, long, default_value = "expect_scripts")]
    expect_scripts_dir: String,
    /// definition of all devices in JSON format
    #[clap(long, default_value = "rusted.json")]
    devices: String,
    /// location of the git repository containing the fetched device configurations
    #[clap(long, default_value = "configs")]
    state_dir: String,
    /// disable pushing to the default remote repository after committing
    #[clap(long)]
    no_push: bool,
}

/// initializes the tracing subscriber, sets the default log level to `INFO`.
/// this default configuration may the be overridden using the `RUST_LOG` environment variable.
/// (e.g. `RUST_LOG=debug` or `RUST_LOG=rusted=debug`)
fn init_tracing() -> Result<()> {
    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env()?;

    // format for use with journald
    let tracing_subscriber = tracing_subscriber::fmt()
        .with_writer(stderr)
        .without_time()
        .with_env_filter(env_filter)
        .compact()
        .finish();

    tracing::subscriber::set_global_default(tracing_subscriber)
        .context("failed to set global default tracing subscriber")
}

///
async fn update_device_config_file(
    device_nr: usize,
    device: DeviceConfig,
    expect_scripts_dir: String,
    state_dir: String,
) -> Result<()> {
    // construct path to config dump file for this device
    let dump_file = device.to_config_dump_path(&state_dir);

    // check if `state_dir` actually exists
    if !Path::new(&state_dir).try_exists()? {
        bail!("state_dir {state_dir} does not exist");
    }

    info!("Device {device_nr}: fetching running-config");
    // consume device to acquire its filtered config dump
    let dump = device.into_filtered_dump(&expect_scripts_dir).await?;

    info!("Device {device_nr}: writing running-config to '{dump_file}'");
    // write filtered config dump to previously constructed file location
    write(dump_file, dump.as_bytes()).map_err(Error::msg)
}

/// invokes `git` with the specified `subcmd` and further `args` in `workdir`
/// `with_output` controls whether or not the result contains Some(stdout) of the
/// invoked command or None
fn git_subcommand(
    subcmd: &str,
    workdir: &str,
    args: &[&str],
    with_output: bool,
) -> Result<Option<String>> {
    let child = Command::new("git")
        .arg(subcmd)
        .args(args)
        .current_dir(workdir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("git command failed")?;

    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "git ls-files failed:\nstderr:\n{}\nstdout:\n{}",
            from_utf8(&output.stderr).unwrap_or("<invalid utf8>"),
            from_utf8(&output.stdout).unwrap_or("<invalid utf8>")
        )
    }

    if with_output {
        let stdout_str = from_utf8(&output.stdout)?;
        Ok(Some(stdout_str.to_owned()))
    } else {
        Ok(None)
    }
}

/// iterates over all modified and added files in `state_dir`,
/// then creates one commit per file and optionally pushes changes
/// to the default remote for the current branch
fn update_git_repo(state_dir: &str, no_push: bool) -> Result<()> {
    let changed_files = git_subcommand(
        "ls-files",
        state_dir,
        vec!["--modified", "--others", "--exclude-standard"].as_ref(),
        true,
    )
    .context("failed to list changed files")?
    .ok_or_else(|| anyhow!("this should never happen"))?;

    for file in changed_files.lines() {
        info!("commiting changes to file '{state_dir}/{file}'");

        git_subcommand("add", state_dir, vec![file].as_ref(), false)?;
        git_subcommand(
            "commit",
            state_dir,
            vec!["--message", format!("Update {file}").as_ref()].as_ref(),
            false,
        )?;
    }

    if !no_push {
        git_subcommand("push", state_dir, &[], false)?;
    }

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
    // spawns a task on the runtime `rt` for each configured device.
    // an index is assigned to each task to identify async log output
    for (i, device) in devices.into_iter().enumerate() {
        let scripts_dir = config.expect_scripts_dir.clone();
        let state_dir = config.state_dir.clone();

        tasks.push(rt.spawn(async move {
            let idx = i + 1; // devices start at 1 :)
            update_device_config_file(idx, device, scripts_dir, state_dir)
                .await
                .map_err(|e| {
                    // log error when it occurs
                    error!("Device {idx}: {e:#}");
                    e
                })
        }));
    }

    let tasks_failed: bool = rt
        .block_on(async { futures::future::join_all(tasks).await })
        .iter()
        .any(|res| match res {
            Ok(Err(_)) => true, // contained error was already logged above
            Err(e) => {
                error!("async task failed: {e}");
                true
            }
            _ => false,
        });

    update_git_repo(&config.state_dir, config.no_push)?;

    // TODO maybe exit before updating the git repo if any task failed?
    if tasks_failed {
        bail!("not all tasks succeeded")
    }

    Ok(())
}
