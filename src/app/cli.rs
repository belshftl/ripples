// SPDX-FileCopyrightText: 2026 belshftl
// SPDX-License-Identifier: MIT

//! Simple `lexopt` wrapper to parse the options into a struct. Currently also manages default
//! values for options.

use anyhow::{Context, bail};
use lexopt::prelude::*;
use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use crate::debounce::{KeyCode, Mode};

const DEFAULT_MODE: Mode = Mode::Mixed;
const DEFAULT_WINDOW: Duration = Duration::from_millis(20);
const DEFAULT_DEVICE_RESCAN_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct Cli {
    pub mode: Mode,
    pub window: Duration,
    pub keys: Option<Vec<String>>,
    pub exclude_keys: Option<Vec<String>>,
    pub virtual_name: Option<String>,
    pub device_rescan_interval: Duration,
    pub wait_for_device: bool,
    pub device: PathBuf,
}

#[derive(Debug, Clone)]
pub enum CliResult {
    Usage,
    List,
    StablePath(PathBuf),
    Help,
    Version,
    Cli(Cli),
}

pub fn parse() -> anyhow::Result<CliResult> {
    let mut parser = lexopt::Parser::from_env();

    let mut mode = DEFAULT_MODE;
    let mut window = DEFAULT_WINDOW;
    let mut keys = None;
    let mut exclude_keys = None;
    let mut virtual_name = None;
    let mut device_rescan_interval = DEFAULT_DEVICE_RESCAN_INTERVAL;
    let mut wait_for_device = false;
    let mut device = None;

    while let Some(arg) = parser.next()? {
        match arg {
            Short('m') | Long("mode") => {
                mode = match parser.value()?.string()?.as_str() {
                    "eager" => Mode::Eager,
                    "defer" => Mode::Defer,
                    "mixed" => Mode::Mixed,
                    s => bail!("invalid value '{s}' for option -m/--mode"),
                }
            }
            Short('w') | Long("window") => {
                let s = parser.value()?.string()?;
                let Ok(w) = s.parse::<u64>() else {
                    bail!("invalid value '{s}' for option -w/--window");
                };
                window = Duration::from_millis(w);
            }
            Long("keys") => {
                if exclude_keys.is_some() {
                    bail!("at most one of --keys and --exclude-keys may be specified");
                }
                keys = Some(
                    parser
                        .value()?
                        .string()?
                        .split(',')
                        .map(|s| s.trim().to_owned())
                        .collect(),
                );
            }
            Long("exclude-keys") => {
                if keys.is_some() {
                    bail!("at most one of --keys and --exclude-keys may be specified");
                }
                exclude_keys = Some(
                    parser
                        .value()?
                        .string()?
                        .split(',')
                        .map(|s| s.trim().to_owned())
                        .collect(),
                );
            }
            Long("virtual-name") => virtual_name = Some(parser.value()?.string()?),
            Long("device-rescan-interval") => {
                let s = parser.value()?.string()?;
                let Ok(interval) = s.parse::<u64>() else {
                    bail!("invalid value '{s}' for option --device-rescan-interval");
                };
                device_rescan_interval = Duration::from_millis(interval);
            }
            Long("wait-for-device") => wait_for_device = true,
            Short('l') | Long("list") => return Ok(CliResult::List),
            Short('p') | Long("stable-path") => {
                return Ok(CliResult::StablePath(PathBuf::from(parser.value()?)));
            }
            Short('h') | Long("help") => return Ok(CliResult::Help),
            Short('V') | Long("version") => return Ok(CliResult::Version),
            Value(value) => {
                if device.is_some() {
                    bail!("exactly one device must be specified");
                }
                device = Some(PathBuf::from(value));
            }
            _ => return Err(arg.unexpected().into()),
        }
    }

    let Some(device) = device else {
        return Ok(CliResult::Usage);
    };

    Ok(CliResult::Cli(Cli {
        mode,
        window,
        keys,
        exclude_keys,
        virtual_name,
        device_rescan_interval,
        wait_for_device,
        device,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyFilter {
    All,
    Only(HashSet<KeyCode>),
    AllExcept(HashSet<KeyCode>),
}

impl KeyFilter {
    pub fn should_debounce(&self, key: KeyCode) -> bool {
        match self {
            KeyFilter::All => true,
            KeyFilter::Only(set) => set.contains(&key),
            KeyFilter::AllExcept(set) => !set.contains(&key),
        }
    }
}

impl Cli {
    pub fn key_filter(&self) -> anyhow::Result<KeyFilter> {
        if let Some(keys) = &self.keys
            && !keys.is_empty()
        {
            Ok(KeyFilter::Only(parse_keys(keys).context("--keys")?))
        } else if let Some(exclude_keys) = &self.exclude_keys
            && exclude_keys.is_empty()
        {
            Ok(KeyFilter::AllExcept(
                parse_keys(exclude_keys).context("--exclude-keys")?,
            ))
        } else {
            Ok(KeyFilter::All)
        }
    }
}

fn parse_keys(specs: &[String]) -> anyhow::Result<HashSet<KeyCode>> {
    specs.iter().map(|s| parse_key(s.trim())).collect()
}

fn parse_key(spec: &str) -> anyhow::Result<KeyCode> {
    if let Some(hex) = spec.strip_prefix("0x").or_else(|| spec.strip_prefix("0X")) {
        return KeyCode::from_str_radix(hex, 16).with_context(|| format!("bad key code {spec:?}"));
    }
    if !spec.is_empty() && spec.chars().all(|c| c.is_ascii_digit()) {
        return spec
            .parse::<KeyCode>()
            .with_context(|| format!("bad key code {spec:?}"));
    }

    let upper = spec.to_ascii_uppercase();
    keycode_by_name(&upper)
        .or_else(|| keycode_by_name(&format!("KEY_{upper}")))
        .or_else(|| keycode_by_name(&format!("BTN_{upper}")))
        .with_context(|| format!("unknown key {spec:?}"))
}

fn keycode_by_name(name: &str) -> Option<KeyCode> {
    evdev::KeyCode::from_str(name).ok().map(|k| k.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_parse() {
        assert_eq!(parse_key("0x1e").unwrap(), 30);
        assert_eq!(parse_key("30").unwrap(), 30);
        assert_eq!(parse_key("KEY_A").unwrap(), 30);
        assert_eq!(parse_key("key_a").unwrap(), 30);
        assert_eq!(parse_key("A").unwrap(), 30);
        assert_eq!(parse_key("a").unwrap(), 30);
        assert!(parse_key("bad").is_err());
        assert!(parse_key("").is_err());
    }

    #[test]
    fn filter_filters_the_right_keys() {
        let only = KeyFilter::Only(HashSet::from([30]));
        assert!(only.should_debounce(30) && !only.should_debounce(31));
        let except = KeyFilter::AllExcept(HashSet::from([30]));
        assert!(!except.should_debounce(30) && except.should_debounce(31));
        assert!(KeyFilter::All.should_debounce(30));
    }
}
