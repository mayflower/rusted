use anyhow::{bail, Context, Result};
use core::str::from_utf8;
use regex::Regex;
use serde::Deserialize;
use std::fs::read_to_string;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, warn};

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    #[serde(default)]
    trim_lines_head: usize,
    #[serde(default)]
    trim_lines_tail: usize,
    #[serde(default)]
    filter_patterns: Vec<String>,
    #[serde(default)]
    replace_patterns: Vec<(String, String)>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    host: String,
    model: String,
    user: String,
    password_file: String,
    cipher: Option<String>,
    kexalgorithm: Option<String>,
    hostkeyalgorithm: Option<String>,
    #[serde(default)]
    extra_expect_params: Vec<String>,
    filter_config: Option<Filter>,
}

impl DeviceConfig {
    pub fn read_all_from_file(config_path: &str) -> Result<Vec<Self>> {
        let config_json = read_to_string(config_path)?;

        let configs: Vec<DeviceConfig> = serde_json::from_str(&config_json)
            .with_context(|| format!("failed to deserialize JSON from '{config_path}'"))?;

        Ok(configs)
    }

    pub fn to_config_dump_path(&self, state_dir: &str) -> String {
        format!("{state_dir}/{}", self.host)
    }

    pub async fn into_filtered_dump(self, scripts_dir: &str) -> Result<String> {
        let script_path = format!("{scripts_dir}/{}.exp", &self.model);
        let maybe_filter = self.to_filter()?;
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
        let dump = match maybe_filter {
            None => stdout_str.to_owned(),
            Some(filter) => filter(stdout_str),
        };

        Ok(dump)
    }

    fn to_filter(&self) -> Result<Option<impl Fn(&str) -> String>> {
        // clone filter_config for use in the returned closure
        let filter_config = match self.filter_config.as_ref() {
            None => return Ok(None),
            Some(f) => f.clone(),
        };

        // compiling regexes from user-provided patterns may fail
        // so we compile them outside the filter closure
        let filter_regexes = filter_config
            .filter_patterns
            .iter()
            .map(|regex| {
                Regex::new(regex).with_context(|| format!("failed to compile regex: '{regex}'"))
            })
            .collect::<Result<Vec<_>>>()?;

        let replacements = filter_config
            .replace_patterns
            .into_iter()
            .map(|(regex, replacement)| {
                Regex::new(&regex)
                    .with_context(|| format!("failed to compile regex: '{regex}'"))
                    .map(|re| (re, replacement))
            })
            .collect::<Result<Vec<_>>>()?;

        let filter = move |raw: &str| -> String {
            let lines = raw
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
                .get(..lines.len() - filter_config.trim_lines_tail)
                .map(|lines| lines.join("\n"))
                .unwrap_or_else(|| {
                    warn!("no lines remain after trimming");
                    "".to_owned()
                })
        };

        Ok(Some(filter))
    }

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
