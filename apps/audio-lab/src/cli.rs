//! Command grammar for the audio lab. Parsing does not enumerate devices or open streams.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::profile::{PlaybackMode, SavedPlayback, validate_gain};

#[derive(Clone, Debug, PartialEq)]
pub enum ProbeRoute {
    Indexes { source: usize, output: usize },
    Profile(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub enum LoopRoute {
    Indexes {
        source: usize,
        output: usize,
        playback: SavedPlayback,
    },
    Profile(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Devices,
    Probe(ProbeRoute),
    Loopback {
        route: LoopRoute,
        seconds: u64,
    },
    ProfileInit {
        path: PathBuf,
        source_index: usize,
        output_index: usize,
        playback: SavedPlayback,
    },
    ProfileInspect(PathBuf),
    ProfilePreflight(PathBuf),
}

struct ParsedArgs {
    positionals: Vec<String>,
    flags: BTreeMap<String, String>,
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let Some(command) = args.first() else {
        return Err(usage());
    };
    match command.as_str() {
        "devices" => parse_devices(&args[1..]),
        "probe" => parse_probe(&args[1..]),
        "loopback" => parse_loopback(&args[1..]),
        "profile" => parse_profile_command(&args[1..]),
        _ => Err(usage()),
    }
}

pub fn usage() -> String {
    "\
usage: rustlanbus-audio-lab devices
       rustlanbus-audio-lab profile init PATH --source N --output N [--play remote|local|mix|mute] [--local-gain 0..1] [--remote-gain 0..1] [--buffer-periods 1..8]
       rustlanbus-audio-lab profile inspect PATH
       rustlanbus-audio-lab profile preflight PATH
       rustlanbus-audio-lab probe --source N --output N
       rustlanbus-audio-lab probe --profile PATH
       rustlanbus-audio-lab loopback --source N --output N --seconds N [--play remote|local|mix|mute] [--local-gain 0..1] [--remote-gain 0..1] [--buffer-periods 1..8]
       rustlanbus-audio-lab loopback --profile PATH --seconds N
A profile stores the opaque source id, source kind, opaque output id, playback mode, gains, and buffer-period count.
Friendly names in the file are display hints. Enumeration indexes are not identity.
Inspect and preflight start no audio. Preflight reports catalog eligibility and leaves period negotiation to a later open.
Probe opens the selected streams and does not start them. Loopback runs the existing bounded self-loop for --seconds.
The profile restores route and playback choices for the command that names the file. Peer identity, bilateral consent, automatic startup, driver installation, and automatic device reopening stay outside the file."
        .into()
}

fn parse_devices(args: &[String]) -> Result<Command, String> {
    let parsed = parse_flags(args, &[])?;
    if !parsed.positionals.is_empty() || !parsed.flags.is_empty() {
        return Err("devices takes no arguments".into());
    }
    Ok(Command::Devices)
}

fn parse_probe(args: &[String]) -> Result<Command, String> {
    let parsed = parse_flags(args, &["--source", "--output", "--profile"])?;
    reject_positionals(&parsed)?;
    Ok(Command::Probe(route_without_playback(&parsed)?))
}

fn parse_loopback(args: &[String]) -> Result<Command, String> {
    let parsed = parse_flags(
        args,
        &[
            "--source",
            "--output",
            "--profile",
            "--seconds",
            "--play",
            "--local-gain",
            "--remote-gain",
            "--buffer-periods",
        ],
    )?;
    reject_positionals(&parsed)?;
    let seconds = required(&parsed, "--seconds").and_then(parse_seconds)?;
    let route = if parsed.flags.contains_key("--profile") {
        reject_saved_route_overrides(&parsed)?;
        LoopRoute::Profile(profile_path(&parsed)?)
    } else {
        let (source, output) = index_pair(&parsed)?;
        LoopRoute::Indexes {
            source,
            output,
            playback: playback_from_flags(&parsed)?,
        }
    };
    Ok(Command::Loopback { route, seconds })
}

fn parse_profile_command(args: &[String]) -> Result<Command, String> {
    let Some(action) = args.first() else {
        return Err(usage());
    };
    match action.as_str() {
        "init" => parse_init(&args[1..]),
        "inspect" => path_command(&args[1..]).map(Command::ProfileInspect),
        "preflight" => path_command(&args[1..]).map(Command::ProfilePreflight),
        _ => Err(usage()),
    }
}

fn parse_init(args: &[String]) -> Result<Command, String> {
    let parsed = parse_flags(
        args,
        &[
            "--source",
            "--output",
            "--play",
            "--local-gain",
            "--remote-gain",
            "--buffer-periods",
        ],
    )?;
    let path = one_path(&parsed.positionals)?;
    let (source_index, output_index) = index_pair(&parsed)?;
    Ok(Command::ProfileInit {
        path,
        source_index,
        output_index,
        playback: playback_from_flags(&parsed)?,
    })
}

fn path_command(args: &[String]) -> Result<PathBuf, String> {
    let parsed = parse_flags(args, &[])?;
    one_path(&parsed.positionals)
}

fn route_without_playback(parsed: &ParsedArgs) -> Result<ProbeRoute, String> {
    if parsed.flags.contains_key("--profile") {
        reject_saved_route_overrides(parsed)?;
        Ok(ProbeRoute::Profile(profile_path(parsed)?))
    } else {
        let (source, output) = index_pair(parsed)?;
        Ok(ProbeRoute::Indexes { source, output })
    }
}

fn reject_saved_route_overrides(parsed: &ParsedArgs) -> Result<(), String> {
    for flag in [
        "--source",
        "--output",
        "--play",
        "--local-gain",
        "--remote-gain",
        "--buffer-periods",
    ] {
        if parsed.flags.contains_key(flag) {
            return Err(format!(
                "--profile conflicts with {flag}; remove the extra route choice"
            ));
        }
    }
    Ok(())
}

fn index_pair(parsed: &ParsedArgs) -> Result<(usize, usize), String> {
    Ok((
        required(parsed, "--source").and_then(|text| parse_index("--source", text))?,
        required(parsed, "--output").and_then(|text| parse_index("--output", text))?,
    ))
}

fn playback_from_flags(parsed: &ParsedArgs) -> Result<SavedPlayback, String> {
    let mut playback = SavedPlayback::remote_defaults();
    if let Some(value) = parsed.flags.get("--play") {
        playback.mode = PlaybackMode::parse(value)?;
    }
    if let Some(value) = parsed.flags.get("--local-gain") {
        playback.local_gain = parse_gain("--local-gain", value)?;
    }
    if let Some(value) = parsed.flags.get("--remote-gain") {
        playback.remote_gain = parse_gain("--remote-gain", value)?;
    }
    if let Some(value) = parsed.flags.get("--buffer-periods") {
        playback.buffer_periods = parse_periods(value)?;
    }
    Ok(playback)
}

fn profile_path(parsed: &ParsedArgs) -> Result<PathBuf, String> {
    let text = required(parsed, "--profile")?;
    if text.is_empty() {
        return Err("profile path is empty".into());
    }
    Ok(PathBuf::from(text))
}

fn required<'a>(parsed: &'a ParsedArgs, flag: &str) -> Result<&'a str, String> {
    parsed
        .flags
        .get(flag)
        .map(String::as_str)
        .ok_or_else(|| format!("missing {flag}"))
}

fn one_path(positionals: &[String]) -> Result<PathBuf, String> {
    match positionals {
        [path] if path.is_empty() => Err("profile path is empty".into()),
        [path] => Ok(PathBuf::from(path)),
        [] => Err("missing profile path".into()),
        _ => Err("use one profile path".into()),
    }
}

fn reject_positionals(parsed: &ParsedArgs) -> Result<(), String> {
    if let Some(extra) = parsed.positionals.first() {
        Err(format!("unexpected argument {extra}"))
    } else {
        Ok(())
    }
}

fn parse_index(flag: &str, text: &str) -> Result<usize, String> {
    text.parse::<usize>()
        .map_err(|_| format!("{flag} must be an enumeration index"))
}

fn parse_seconds(text: &str) -> Result<u64, String> {
    let seconds = text.parse::<u64>().map_err(|_| seconds_error())?;
    if seconds == 0 || seconds > 300 {
        return Err(seconds_error());
    }
    Ok(seconds)
}

fn seconds_error() -> String {
    "seconds must be 1..300 for this bounded experiment".into()
}

fn parse_gain(flag: &str, text: &str) -> Result<f32, String> {
    let gain = text
        .parse::<f32>()
        .map_err(|_| format!("{flag} must be finite in 0..1"))?;
    validate_gain(flag, gain)?;
    Ok(gain)
}

fn parse_periods(text: &str) -> Result<u8, String> {
    let value = text.parse::<u16>().map_err(|_| periods_error())?;
    u8::try_from(value)
        .ok()
        .filter(|periods| (1..=8).contains(periods))
        .ok_or_else(periods_error)
}

fn periods_error() -> String {
    "buffer periods must be 1..8".into()
}

fn parse_flags(args: &[String], allowed: &[&str]) -> Result<ParsedArgs, String> {
    let mut positionals = Vec::new();
    let mut flags = BTreeMap::new();
    let mut index = 0;
    while index < args.len() {
        let token = &args[index];
        if token.starts_with("--") {
            if !allowed.iter().any(|flag| *flag == token) {
                let message = if token.contains('=') {
                    format!("unknown argument {token}; pass the value as the next argument")
                } else {
                    format!("unknown argument {token}")
                };
                return Err(message);
            }
            if flags.contains_key(token) {
                return Err(format!("duplicate {token}"));
            }
            let Some(value) = args.get(index + 1) else {
                return Err(format!("missing value for {token}"));
            };
            if value.starts_with("--") {
                return Err(format!("missing value for {token}"));
            }
            flags.insert(token.to_owned(), value.clone());
            index += 2;
        } else {
            positionals.push(token.clone());
            index += 1;
        }
    }
    Ok(ParsedArgs { positionals, flags })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn legacy_commands_parse_with_remote_monitoring_defaults() {
        assert_eq!(parse(&args(&["devices"])).unwrap(), Command::Devices);
        assert_eq!(
            parse(&args(&["probe", "--source", "1", "--output", "2"])).unwrap(),
            Command::Probe(ProbeRoute::Indexes {
                source: 1,
                output: 2
            })
        );
        assert_eq!(
            parse(&args(&[
                "loopback",
                "--output",
                "4",
                "--source",
                "3",
                "--seconds",
                "10"
            ]))
            .unwrap(),
            Command::Loopback {
                route: LoopRoute::Indexes {
                    source: 3,
                    output: 4,
                    playback: SavedPlayback::remote_defaults(),
                },
                seconds: 10,
            }
        );
        let mixed = parse(&args(&[
            "loopback",
            "--source",
            "3",
            "--output",
            "4",
            "--seconds",
            "10",
            "--play",
            "mix",
            "--local-gain",
            "0.5",
            "--remote-gain",
            "0.5",
            "--buffer-periods",
            "2",
        ]))
        .unwrap();
        match mixed {
            Command::Loopback {
                route: LoopRoute::Indexes { playback, .. },
                seconds,
            } => {
                assert_eq!(seconds, 10);
                assert_eq!(playback.mode, PlaybackMode::Mix);
                assert_eq!(playback.local_gain, 0.5);
                assert_eq!(playback.remote_gain, 0.5);
                assert!(playback.mode.original_monitoring());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn profile_commands_reject_duplicate_and_conflicting_choices() {
        assert_eq!(
            parse(&args(&["profile", "inspect", "route.json"])).unwrap(),
            Command::ProfileInspect(PathBuf::from("route.json"))
        );
        assert_eq!(
            parse(&args(&["profile", "preflight", "route.json"])).unwrap(),
            Command::ProfilePreflight(PathBuf::from("route.json"))
        );
        match parse(&args(&[
            "profile",
            "init",
            "route.json",
            "--source",
            "1",
            "--output",
            "2",
        ]))
        .unwrap()
        {
            Command::ProfileInit { playback, .. } => {
                assert_eq!(playback, SavedPlayback::remote_defaults());
                assert!(!playback.mode.original_monitoring());
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            parse(&args(&["probe", "--profile", "route.json"])).unwrap(),
            Command::Probe(ProbeRoute::Profile(_))
        ));
        assert!(matches!(
            parse(&args(&[
                "loopback",
                "--profile",
                "route.json",
                "--seconds",
                "5"
            ]))
            .unwrap(),
            Command::Loopback {
                route: LoopRoute::Profile(_),
                seconds: 5,
            }
        ));

        let rejected = [
            args(&[
                "probe",
                "--profile",
                "route.json",
                "--source",
                "1",
                "--output",
                "2",
            ]),
            args(&[
                "loopback",
                "--profile",
                "route.json",
                "--seconds",
                "1",
                "--play",
                "local",
            ]),
            args(&["probe", "--source", "1", "--source", "2", "--output", "3"]),
            args(&["profile", "inspect", "route.json", "--source", "1"]),
            args(&[
                "loopback",
                "--source",
                "1",
                "--output",
                "2",
                "--seconds",
                "0",
            ]),
            args(&["probe", "--source", "1"]),
        ];
        let expected = [
            "--profile conflicts with --source",
            "--profile conflicts with --play",
            "duplicate --source",
            "unknown argument --source",
            "seconds must be 1..300",
            "missing --output",
        ];
        for (command, needle) in rejected.iter().zip(expected) {
            let error = parse(command).expect_err(&command.join(" "));
            assert!(
                error.contains(needle),
                "error `{error}` did not contain `{needle}`"
            );
        }
    }
}
