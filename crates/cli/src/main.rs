//! Command-line entry point and intentionally small argument surface.

#![deny(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;

use bigcp_core::{
    CopyOptions, DeviceClass, TuneOptions, VerifyOptions, load_report, run_copy,
    run_standalone_verify,
};
use bigcp_tui::{
    PlainObserver, print_report_summary, run_dashboard, show_report, stdout_is_terminal,
};
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "bigcp",
    version,
    about = "Reliable, high-throughput local NTFS/ReFS tree copy for Windows 11",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Source tree for the default copy command.
    source: Option<PathBuf>,

    /// Destination tree for the default copy command.
    destination: Option<PathBuf>,

    #[command(flatten)]
    flags: CopyFlags,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Fully verifies source and destination trees by reading both.
    Verify {
        /// Source tree.
        source: PathBuf,
        /// Destination tree.
        destination: PathBuf,
    },
    /// Opens a saved JSON report.
    Report {
        /// Report file.
        file: PathBuf,
        /// Print the summary instead of opening a full-screen browser.
        #[arg(long)]
        plain: bool,
    },
}

#[derive(Debug, Args)]
struct CopyFlags {
    /// Enumerate and classify only; make no destination-tree writes.
    #[arg(long)]
    dry_run: bool,

    /// Read back files copied during this run and compare xxh3-128 digests.
    #[arg(long)]
    verify: bool,

    /// Include root-level operating-system artifacts.
    #[arg(long)]
    include_system: bool,

    /// Exclude cloud placeholders instead of hydrating them.
    #[arg(long)]
    skip_cloud: bool,

    /// Replace differing destination files (default true).
    ///
    /// `Option` so subcommand rejection can see an *explicit* `--replace=true`
    /// too, not only `--replace=false`.
    #[arg(
        long,
        num_args = 0..=1,
        default_missing_value = "true",
        require_equals = true
    )]
    replace: Option<bool>,

    /// Flush each committed file after rename and metadata.
    #[arg(long)]
    flush: bool,

    /// Expand sparse files instead of preserving sparse allocation.
    #[arg(long)]
    no_sparse: bool,

    /// Collect bounded live-run insight (size-class timings, slowest files,
    /// finer stat samples) into the log and report.
    #[arg(long)]
    analyze: bool,

    /// Copy unknown reparse buffers verbatim at the user's risk.
    #[arg(long)]
    raw_reparse: bool,

    /// Ignore prior checkpoints and start new partials.
    #[arg(long)]
    fresh: bool,

    /// Device class (auto|nvme|sata-ssd|usb-ssd|hdd|unknown), or "SRC,DST".
    #[arg(long, value_parser = parse_profiles, value_name = "CLASS[,CLASS]")]
    profile: Option<(DeviceClass, DeviceClass)>,

    /// Comma-separated overrides: chunk, threads, mem,
    /// large-threshold, checkpoint-threshold (sizes accept KiB/MiB/GiB).
    #[arg(long, value_parser = parse_tune, value_name = "KEY=VALUE,...")]
    tune: Option<TuneOptions>,

    /// Override state directory.
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// Override JSONL log path.
    #[arg(long)]
    log: Option<PathBuf>,

    /// Override JSON report path.
    #[arg(long)]
    report: Option<PathBuf>,

    /// Use line-oriented output.
    #[arg(long)]
    plain: bool,

    /// Disable terminal colors.
    #[arg(long)]
    no_color: bool,

    /// Print only the final summary.
    #[arg(long)]
    quiet: bool,
}

fn main() -> ExitCode {
    // clap's default usage-error exit code is 2, which collides with the
    // contract's "completed with failures" (PLAN section 10.1). Help/version
    // exit 0; every usage or value-parse failure exits 5, so scripts can
    // trust that 2 always means a real run finished with failed objects.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                0
            } else {
                5
            };
            let _ = error.print();
            return ExitCode::from(code);
        }
    };
    match execute(cli) {
        Ok(code) => ExitCode::from(code),
        Err((code, message)) => {
            eprintln!("bigcp: {message}");
            ExitCode::from(code)
        }
    }
}

fn execute(cli: Cli) -> Result<u8, (u8, String)> {
    match cli.command {
        Some(Command::Verify {
            source,
            destination,
        }) => {
            reject_copy_only_flags(&cli.flags)?;
            let summary = run_standalone_verify(&VerifyOptions {
                source,
                destination,
            })
            .map_err(|error| (5, error.to_string()))?;
            serde_json::to_writer_pretty(std::io::stdout(), &summary)
                .map_err(|error| (6, format!("write verify result: {error}")))?;
            println!();
            Ok(if summary.failed == 0 { 0 } else { 2 })
        }
        Some(Command::Report { file, plain }) => {
            reject_copy_only_flags(&cli.flags)?;
            let report = load_report(&file).map_err(|error| (5, error.to_string()))?;
            if plain || !stdout_is_terminal() {
                print_report_summary(&report);
            } else {
                show_report(&report).map_err(|error| (6, format!("report browser: {error}")))?;
            }
            Ok(0)
        }
        None => {
            let source = cli
                .source
                .ok_or_else(|| (5, "copy requires SRC and DST; see --help".to_owned()))?;
            let destination = cli
                .destination
                .ok_or_else(|| (5, "copy requires SRC and DST; see --help".to_owned()))?;
            warn_quick_removal_destination(
                &destination,
                stdout_is_terminal() && !cli.flags.plain && !cli.flags.quiet && !cli.flags.dry_run,
            )?;
            let mut options = CopyOptions::new(source, destination);
            options.dry_run = cli.flags.dry_run;
            options.verify = cli.flags.verify;
            options.include_system = cli.flags.include_system;
            options.skip_cloud = cli.flags.skip_cloud;
            options.replace = cli.flags.replace.unwrap_or(true);
            options.flush = cli.flags.flush;
            options.no_sparse = cli.flags.no_sparse;
            options.analyze = cli.flags.analyze;
            options.raw_reparse = cli.flags.raw_reparse;
            options.fresh = cli.flags.fresh;
            options.state_dir = cli.flags.state_dir;
            options.log_path = cli.flags.log;
            options.report_path = cli.flags.report;
            options.tune = cli.flags.tune.unwrap_or_default();
            if let Some((source_profile, destination_profile)) = cli.flags.profile {
                options.source_profile = source_profile;
                options.destination_profile = destination_profile;
            }

            // Honor the NO_COLOR convention alongside --no-color (PLAN §11).
            let no_color = cli.flags.no_color
                || std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
            let report = if cli.flags.plain || no_color || !stdout_is_terminal() {
                let observer = PlainObserver::new(cli.flags.quiet);
                run_copy(&options, &observer)
            } else {
                run_dashboard(options)
            }
            .map_err(|error| (exit_for_error(&error), error.to_string()))?;
            print_report_summary(&report);
            Ok(u8::try_from(report.run.exit).unwrap_or(6))
        }
    }
}

fn reject_copy_only_flags(flags: &CopyFlags) -> Result<(), (u8, String)> {
    let used = flags.dry_run
        || flags.verify
        || flags.include_system
        || flags.skip_cloud
        || flags.replace.is_some()
        || flags.flush
        || flags.no_sparse
        || flags.analyze
        || flags.raw_reparse
        || flags.fresh
        || flags.profile.is_some()
        || flags.tune.is_some()
        || flags.state_dir.is_some()
        || flags.log.is_some()
        || flags.report.is_some()
        || flags.plain
        || flags.no_color
        || flags.quiet;
    if used {
        Err((
            5,
            "copy flags are not accepted by verify or report subcommands".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn parse_profiles(value: &str) -> Result<(DeviceClass, DeviceClass), String> {
    let values = value.split(',').collect::<Vec<_>>();
    match values.as_slice() {
        [one] => {
            let class = parse_device_class(one)?;
            Ok((class, class))
        }
        [source, destination] => Ok((
            parse_device_class(source)?,
            parse_device_class(destination)?,
        )),
        _ => Err("profile expects one class or source,destination".to_owned()),
    }
}

fn parse_device_class(value: &str) -> Result<DeviceClass, String> {
    match value {
        "auto" => Ok(DeviceClass::Auto),
        "nvme" => Ok(DeviceClass::Nvme),
        "sata-ssd" => Ok(DeviceClass::SataSsd),
        "usb-ssd" => Ok(DeviceClass::UsbSsd),
        "hdd" => Ok(DeviceClass::Hdd),
        "unknown" => Ok(DeviceClass::Unknown),
        _ => Err(format!("unknown device class: {value}")),
    }
}

fn parse_tune(value: &str) -> Result<TuneOptions, String> {
    let mut tune = TuneOptions::default();
    for item in value.split(',').filter(|item| !item.is_empty()) {
        let (key, raw) = item
            .split_once('=')
            .ok_or_else(|| format!("tune item lacks '=': {item}"))?;
        match key {
            "chunk" => tune.chunk_bytes = Some(parse_size_usize(raw)?),
            "threads" => tune.threads = Some(parse_positive(raw, key)?),
            "mem" => tune.memory_bytes = Some(parse_size_usize(raw)?),
            "large-threshold" => tune.large_threshold = Some(parse_size(raw)?),
            "checkpoint-threshold" => tune.checkpoint_threshold = Some(parse_size(raw)?),
            _ => return Err(format!("unknown tune key: {key}")),
        }
    }
    Ok(tune)
}

fn parse_positive<T>(value: &str, key: &str) -> Result<T, String>
where
    T: FromStr + PartialEq + Default,
    T::Err: std::fmt::Display,
{
    let parsed = value
        .parse::<T>()
        .map_err(|error| format!("invalid {key}: {error}"))?;
    if parsed == T::default() {
        return Err(format!("{key} must be positive"));
    }
    Ok(parsed)
}

fn parse_size_usize(value: &str) -> Result<usize, String> {
    usize::try_from(parse_size(value)?).map_err(|_| "size exceeds address space".to_owned())
}

fn parse_size(value: &str) -> Result<u64, String> {
    let lower = value.to_ascii_lowercase();
    let (number, multiplier) = if let Some(number) = lower.strip_suffix("kib") {
        (number, 1024_u64)
    } else if let Some(number) = lower.strip_suffix("mib") {
        (number, 1024_u64.pow(2))
    } else if let Some(number) = lower.strip_suffix("gib") {
        (number, 1024_u64.pow(3))
    } else {
        (lower.as_str(), 1)
    };
    let number = number
        .parse::<u64>()
        .map_err(|error| format!("invalid size: {error}"))?;
    number
        .checked_mul(multiplier)
        .filter(|size| *size > 0)
        .ok_or_else(|| "size must be positive and fit u64".to_owned())
}

fn exit_for_error(error: &bigcp_core::BigcpError) -> u8 {
    match error {
        bigcp_core::BigcpError::Locked(_)
        | bigcp_core::BigcpError::Invalid(_)
        | bigcp_core::BigcpError::Io { .. } => 5,
        bigcp_core::BigcpError::Audit(_)
        | bigcp_core::BigcpError::Invariant(_)
        | bigcp_core::BigcpError::Format(_) => 6,
    }
}

/// Warns — and in an interactive terminal, confirms — before copying onto a
/// destination whose device write cache is disabled (the Quick-removal
/// policy), which was measured at ~3.4x slower for metadata-heavy small-file
/// workloads (BENCHMARKS.md 2026-07-29; ADR 0032). Detection failures stay
/// silent, non-interactive runs only warn, and bigcp never changes the
/// policy itself.
fn warn_quick_removal_destination(
    destination: &std::path::Path,
    interactive: bool,
) -> Result<(), (u8, String)> {
    let mut probe = destination.to_path_buf();
    let volume = loop {
        match bigcp_win::probe_volume(&probe) {
            Ok(volume) => break volume,
            Err(_) => {
                if !probe.pop() {
                    return Ok(());
                }
            }
        }
    };
    let device = bigcp_win::profile_device(&volume);
    if device.write_cache_enabled != Some(false) {
        return Ok(());
    }
    eprintln!(
        "warning: the destination drive has write caching disabled (Windows 'Quick removal' \
         policy).\n  Copies with many small files run several times slower this way (~3.4x \
         measured).\n  To speed it up: Device Manager > the drive > Policies > 'Better \
         performance', and check\n  'Enable write caching on the device'. Leave 'Turn off \
         Windows write-cache buffer flushing'\n  UNCHECKED — that setting risks filesystem \
         corruption on power loss, which a re-run cannot\n  repair. With caching on, always \
         use Safely Remove Hardware before unplugging."
    );
    if interactive {
        eprint!("Continue with the current policy? [Y/n] ");
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_ok()
            && answer.trim_start().to_ascii_lowercase().starts_with('n')
        {
            return Err((
                5,
                "aborted before any copying: adjust the drive policy and run again".to_owned(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command, execute};
    use clap::Parser;

    #[test]
    fn grammar_has_copy_and_one_standalone_verify_command() {
        let copy = Cli::try_parse_from(["bigcp", "source", "destination", "--verify"]);
        assert!(copy.is_ok());
        assert!(copy.ok().is_some_and(|value| value.command.is_none()));

        let verify = Cli::try_parse_from(["bigcp", "verify", "source", "destination"]);
        assert!(verify.is_ok());
        assert!(
            verify
                .ok()
                .is_some_and(|value| matches!(value.command, Some(Command::Verify { .. })))
        );
    }

    #[test]
    fn standalone_verify_rejects_copy_flags_before_io() {
        let parsed = Cli::try_parse_from(["bigcp", "--dry-run", "verify", "source", "destination"]);
        assert!(parsed.is_ok());
        let Some(parsed) = parsed.ok() else {
            return;
        };
        let result = execute(parsed);
        assert!(result.is_err());
        assert!(result.err().is_some_and(|(code, message)| {
            code == 5 && message.contains("copy flags are not accepted")
        }));
    }

    #[test]
    fn zero_tuning_values_are_rejected() {
        let parsed = Cli::try_parse_from(["bigcp", "source", "destination", "--tune", "threads=0"]);
        assert!(parsed.is_err());
    }

    #[test]
    fn removed_nonfunctional_stream_tuning_is_rejected() {
        let parsed = Cli::try_parse_from(["bigcp", "source", "destination", "--tune", "streams=2"]);
        assert!(parsed.is_err());
    }
}
