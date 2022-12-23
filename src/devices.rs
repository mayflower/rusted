use anyhow::{bail, Context, Result};
use core::str::from_utf8;
use regex::Regex;
use serde::Deserialize;
use std::fs::read_to_string;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, warn};

/// configuration for filtering output from the expect script for this device
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    /// number of lines to remove from the start
    #[serde(default)]
    trim_lines_head: usize,
    /// number of lines to remove from the end
    #[serde(default)]
    trim_lines_tail: usize,
    /// all lines that match one of these patterns are removed from the output
    #[serde(default)]
    filter_patterns: Vec<String>,
    /// all occurrences of the pattern (first tuple element) in a line are replaced
    /// with the second element of the tuple; for each tuple
    #[serde(default)]
    replace_patterns: Vec<(String, String)>,
}

/// all device specific parameters as well as an optional [`Filter`]
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    /// host address (IP or hostname}
    host: String,
    /// device model; used to identify which expect script to use
    model: String,
    /// SSH user
    user: String,
    /// file that contains the SSH password
    password_file: String,
    /// SSH cipher
    cipher: Option<String>,
    /// SSH keyexchange algorithm
    kexalgorithm: Option<String>,
    /// SSH hostkey algorithm
    hostkeyalgorithm: Option<String>,
    /// any additional parameter that should be passed to the model's expect script
    #[serde(default)]
    extra_expect_params: Vec<String>,
    /// optional filter configuration to apply to the expect output
    filter_config: Option<Filter>,
}

impl DeviceConfig {
    /// reads the JSON configuration from `config_path` and deserializes it
    /// into a `Vec<DeviceConfig>` using `serde_json::from_str()`
    pub fn read_all_from_file(config_path: &str) -> Result<Vec<Self>> {
        let config_json = read_to_string(config_path)?;

        let configs: Vec<DeviceConfig> = serde_json::from_str(&config_json)
            .with_context(|| format!("failed to deserialize JSON from '{config_path}'"))?;

        Ok(configs)
    }

    /// constructs the path to the file into which the (filtered) device config dump
    /// shall be written
    pub fn to_config_dump_path(&self, state_dir: &str) -> String {
        format!("{state_dir}/{}", self.host)
    }

    /// executes the device's expect script and returns its filtered output
    pub async fn into_filtered_dump(self, scripts_dir: &str) -> Result<String> {
        // path to the expect script for this model; assumed to
        let script_path = format!("{scripts_dir}/{}.exp", &self.model);

        // get optional filter closure for this device
        let maybe_filter = self.to_filter()?;

        // expect script with parameters
        let mut cmd = Command::new(&script_path);
        cmd.args(self.into_expect_args())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        debug!("running script: {cmd:?}");

        let child = cmd
            .spawn()
            .with_context(|| format!("failed to run expect script {script_path}"))?;
        let output = child.wait_with_output().await?;

        if !output.status.success() {
            bail!(
                "expect script failed {script_path}:\n{}",
                from_utf8(&output.stderr).unwrap_or("<invalid utf8>")
            );
        }

        let stdout_str = from_utf8(&output.stdout)?;

        // if a filter is configured, apply it to the expect output
        let dump = match maybe_filter {
            None => stdout_str.to_owned(),
            Some(filter) => filter(stdout_str),
        };

        Ok(dump)
    }

    /// returns a closure that filters the input str as defined in the [`Filter`] of this device
    fn to_filter(&self) -> Result<Option<impl Fn(&str) -> String>> {
        let filter_config = match self.filter_config.as_ref() {
            // no filter is configured for this device
            None => return Ok(None),
            // clone filter_config for use in the returned closure
            Some(f) => f.clone(),
        };

        // compiling regexes from user-provided patterns may fail
        // so we compile them outside the filter closure

        // regexes for lines that will be removed
        let filter_regexes = filter_config
            .filter_patterns
            .iter()
            .map(|regex| {
                Regex::new(regex).with_context(|| format!("failed to compile regex: '{regex}'"))
            })
            .collect::<Result<Vec<_>>>()?;

        // regexes and corresponding replacements as tuples
        let replacements = filter_config
            .replace_patterns
            .into_iter()
            .map(|(regex, replacement)| {
                Regex::new(&regex)
                    .with_context(|| format!("failed to compile regex: '{regex}'"))
                    .map(|re| (re, replacement))
            })
            .collect::<Result<Vec<_>>>()?;

        // the filter closure
        let filter = move |raw: &str| -> String {
            let lines = raw
                // split input into lines
                .lines()
                // trim head
                .skip(filter_config.trim_lines_head)
                // replace patterns from `replacement_patterns`
                .map(|line| {
                    let mut line = line.to_owned();
                    for &(ref regex, ref replacement) in &replacements {
                        line = regex.replace_all(&line, replacement).to_string();
                    }
                    line
                })
                // remove lines containing any of `filter_patterns`
                .filter(|line| !filter_regexes.iter().any(|re| re.is_match(line)))
                // remove trailing whitespace
                .map(|line| line.trim_end().to_owned())
                .collect::<Vec<String>>();

            lines
                // trim tail
                .get(..lines.len() - filter_config.trim_lines_tail)
                // concatenate all lines to a single string again
                .map(|lines| lines.join("\n"))
                // print a warning if the resulting string is empty
                .unwrap_or_else(|| {
                    warn!("no lines remain after trimming");
                    "".to_owned()
                })
        };

        Ok(Some(filter))
    }

    /// generate a Vec of all parameters that need to be passed to the device's expect script
    fn into_expect_args(self) -> Vec<String> {
        vec![self.user, self.password_file, self.host]
            .into_iter()
            .chain(
                vec![self.kexalgorithm, self.cipher, self.hostkeyalgorithm]
                    .into_iter()
                    .flatten(),
            )
            .chain(self.extra_expect_params)
            .collect()
    }
}
