//! Command-line entry point and intentionally small argument surface.

#![deny(unsafe_code)]

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;

use bigcp_core::{
    CopyOptions, DeviceClass, TuneOptions, VerifyOptions, load_report, run_copy,
    run_standalone_verify,
};
use bigcp_tui::{
    PlainObserver, print_report_summary, run_dashboard_with_color, show_report, stdout_is_terminal,
};
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "bigcp",
    version,
    about = "Reliable, high-throughput local and UNC tree copy for Windows 11",
    disable_help_subcommand = true,
    override_usage = "bigcp [OPTIONS] <SOURCE> <DESTINATION>\n       bigcp verify <SOURCE> <DESTINATION>\n       bigcp report [--plain] <FILE>",
    after_help = "Examples:\n  bigcp C:\\source D:\\backup\n  bigcp C:\\source D:\\backup --dry-run\n  bigcp C:\\source D:\\backup --verify --quiet\n  bigcp \\\\server\\share\\source D:\\backup --accept-remote-paths\n  bigcp verify C:\\source D:\\backup\n  bigcp report C:\\audit\\run.report.json"
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
    // Keep this as `Option` so subcommand rejection can see an explicit
    // `--replace=true` too, not only `--replace=false`.
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

    /// Accept timestamp/metadata degradation on FAT or exFAT destinations.
    #[arg(long)]
    accept_degraded_filesystem: bool,

    /// Accept UNC/WSL semantic and remote-durability limitations.
    #[arg(long)]
    accept_remote_paths: bool,

    /// Continue without prompting when the destination drive uses the slower
    /// Windows "Quick removal" write-cache policy.
    #[arg(long)]
    accept_write_cache_policy: bool,

    /// Device class (auto|nvme|sata-ssd|usb-ssd|hdd|unknown), or "SRC,DST".
    #[arg(long, value_parser = parse_profiles, value_name = "CLASS[,CLASS]")]
    profile: Option<(DeviceClass, DeviceClass)>,

    /// Comma-separated overrides: chunk, threads, mem, large-threshold,
    /// checkpoint-threshold, same-spindle-burst (sizes accept KiB/MiB/GiB).
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
            let _ = writeln!(std::io::stderr().lock(), "bigcp: {message}");
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
            let mut stdout = std::io::stdout().lock();
            serde_json::to_writer_pretty(&mut stdout, &summary)
                .map_err(|error| (6, format!("write verify result: {error}")))?;
            writeln!(stdout).map_err(|error| (6, format!("write verify result: {error}")))?;
            Ok(if summary.failed == 0 { 0 } else { 2 })
        }
        Some(Command::Report { file, plain }) => {
            reject_copy_only_flags(&cli.flags)?;
            let report = load_report(&file).map_err(|error| (5, error.to_string()))?;
            if plain || !stdout_is_terminal() {
                print_report_summary(&report)
                    .map_err(|error| (6, format!("write report summary: {error}")))?;
            } else {
                // The browser honors the NO_COLOR convention like the copy
                // dashboard does (README documents the convention globally).
                let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
                show_report(&report, !no_color)
                    .map_err(|error| (6, format!("report browser: {error}")))?;
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
            let acceptance = confirm_preflight_warnings(
                &source,
                &destination,
                PreflightWarningOptions {
                    interactive: std::io::stdin().is_terminal()
                        && stdout_is_terminal()
                        && !cli.flags.plain
                        && !cli.flags.quiet
                        && !cli.flags.dry_run,
                    accepted: PreflightAcceptance {
                        degraded_filesystem: cli.flags.accept_degraded_filesystem,
                        remote_paths: cli.flags.accept_remote_paths,
                        write_cache_policy: cli.flags.accept_write_cache_policy,
                    },
                    dry_run: cli.flags.dry_run,
                },
            )?;
            // Honor the NO_COLOR convention alongside --no-color (PLAN §11).
            // Color selection is independent of the dashboard/plain choice.
            let no_color = cli.flags.no_color
                || std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
            let display_mode = copy_display_mode(&cli.flags, no_color, stdout_is_terminal());
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
            options.accept_degraded_filesystem = acceptance.degraded_filesystem;
            options.accept_remote_paths = acceptance.remote_paths;
            options.state_dir = cli.flags.state_dir;
            options.log_path = cli.flags.log;
            options.report_path = cli.flags.report;
            options.tune = cli.flags.tune.unwrap_or_default();
            if let Some((source_profile, destination_profile)) = cli.flags.profile {
                options.source_profile = source_profile;
                options.destination_profile = destination_profile;
            }

            // Graceful Ctrl+C for every copy mode: the first request cancels
            // between bounded work units (exit 3), a second falls through to
            // default termination. If installation fails, Ctrl+C keeps its
            // default hard-kill behavior, which the abort-and-rerun contract
            // already covers.
            let console_cancel = bigcp_win::install_cancel_handler()
                .ok()
                .map(|()| bigcp_win::cancel_requested as fn() -> bool);
            let report = match display_mode {
                CopyDisplayMode::Plain { quiet } => {
                    let observer = PlainObserver::new(quiet, console_cancel);
                    run_copy(&options, &observer)
                }
                CopyDisplayMode::Dashboard { color_enabled } => {
                    run_dashboard_with_color(options, color_enabled, console_cancel)
                }
            }
            .map_err(|error| (exit_for_error(&error), error.to_string()))?;
            print_report_summary(&report)
                .map_err(|error| (6, format!("write copy summary: {error}")))?;
            Ok(u8::try_from(report.run.exit).unwrap_or(6))
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CopyDisplayMode {
    Dashboard { color_enabled: bool },
    Plain { quiet: bool },
}

fn copy_display_mode(
    flags: &CopyFlags,
    color_disabled: bool,
    stdout_terminal: bool,
) -> CopyDisplayMode {
    if flags.plain || flags.quiet || !stdout_terminal {
        CopyDisplayMode::Plain { quiet: flags.quiet }
    } else {
        CopyDisplayMode::Dashboard {
            color_enabled: !color_disabled,
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
        || flags.accept_degraded_filesystem
        || flags.accept_remote_paths
        || flags.accept_write_cache_policy
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
            "copy flags are not accepted by verify or report subcommands \
             (subcommand-specific flags go after the subcommand, e.g. \
             bigcp report FILE --plain)"
                .to_owned(),
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
            "same-spindle-burst" => tune.same_spindle_burst_bytes = Some(parse_size_usize(raw)?),
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

#[derive(Clone, Copy)]
struct PreflightAcceptance {
    degraded_filesystem: bool,
    remote_paths: bool,
    /// CLI-only: silences the advisory Quick-removal prompt. Core does not
    /// re-gate on this because a slow write-cache policy is a performance
    /// notice, not a fidelity or safety loss (VISION line 11).
    write_cache_policy: bool,
}

#[derive(Clone, Copy)]
struct PreflightWarningOptions {
    interactive: bool,
    accepted: PreflightAcceptance,
    dry_run: bool,
}

/// Emits all known pre-copy filesystem/endpoint/device warnings and prompts
/// at most once. FAT-family fidelity loss and remote-path limitations require
/// explicit acceptance; Quick-removal remains a warning with an interactive
/// opt-out. Detection failures stay silent and the core repeats every required
/// acceptance gate before mutation (ADRs 0032/0035/0037).
fn confirm_preflight_warnings(
    source: &std::path::Path,
    destination: &std::path::Path,
    options: PreflightWarningOptions,
) -> Result<PreflightAcceptance, (u8, String)> {
    // `GetVolumePathNameW` is most reliable with an absolute path. Resolve
    // lexically here so a relative, not-yet-created FAT destination can still
    // receive its one interactive acceptance instead of reaching the core
    // gate without a way to answer it.
    let source_probe = std::path::absolute(source).unwrap_or_else(|_| source.to_path_buf());
    let source_volume = bigcp_win::probe_volume(&source_probe).ok();
    let mut probe = std::path::absolute(destination).unwrap_or_else(|_| destination.to_path_buf());
    let destination_volume = loop {
        match bigcp_win::probe_volume(&probe) {
            Ok(volume) => break volume,
            Err(_) => {
                if !probe.pop() {
                    return Ok(options.accepted);
                }
            }
        }
    };
    let degraded = destination_volume.filesystem.is_fat_family();
    if degraded {
        let source_name = source_volume
            .as_ref()
            .map_or("source", |volume| volume.filesystem.name());
        writeln!(
            std::io::stderr().lock(),
            "Preflight notice - reduced filesystem fidelity: copying from {source_name} to {}.\n  Creation and last-write times use the destination's coarser, range-limited representation;\n  only READONLY, HIDDEN, SYSTEM, and ARCHIVE attributes are representable. Named streams,\n  EAs, sparse layout, EFS state, ACLs, and reparse points cannot be preserved. Reparse\n  objects fail without being followed.{}\n  To confirm this limitation noninteractively next time, pass --accept-degraded-filesystem.",
            destination_volume.filesystem.name(),
            if destination_volume.filesystem.maximum_file_size().is_some() {
                " FAT files larger than 4 GiB minus 1 byte fail before writing."
            } else {
                ""
            }
        )
        .map_err(|error| (6, format!("write preflight warning: {error}")))?;
    }
    let source_endpoint = source_volume
        .as_ref()
        .map_or(bigcp_win::EndpointKind::Local, |volume| volume.endpoint);
    let remote = source_endpoint.is_remote() || destination_volume.endpoint.is_remote();
    let wsl = source_endpoint == bigcp_win::EndpointKind::Wsl
        || destination_volume.endpoint == bigcp_win::EndpointKind::Wsl;
    if remote {
        writeln!(
            std::io::stderr().lock(),
            "Preflight notice - remote path: source is {}; destination is {}.\n  Network or WSL disconnects can interrupt open handles, and server-side cache durability and\n  optional metadata support cannot be inferred from local disk IOCTLs. bigcp keeps bounded I/O,\n  atomic-or-rerunnable publication, verification, and no retries, but cannot make the remote\n  server durable.{}\n  To confirm this limitation noninteractively next time, pass --accept-remote-paths.",
            source_endpoint.name(),
            destination_volume.endpoint.name(),
            if wsl {
                "\n  WSL access crosses Windows/Linux 9P translation. Linux uid/gid/mode/xattrs and special\n  files are not representable by this Win32 engine; unsupported reparse objects fail, and\n  case-colliding names fail when the destination is case-insensitive. Microsoft recommends\n  native Linux tools for sustained Linux-filesystem workloads because \\\\wsl.localhost I/O is slower."
            } else {
                ""
            }
        )
        .map_err(|error| (6, format!("write preflight warning: {error}")))?;
    }
    let device = bigcp_win::profile_device(&destination_volume);
    let quick_removal = device.write_cache_enabled == Some(false);
    if quick_removal {
        writeln!(
            std::io::stderr().lock(),
            "Preflight notice - destination performance: write caching is disabled (Windows \
         'Quick removal' policy).\n  Copies with many small files run several times slower this way (~3.4x \
         measured).\n  To speed it up: Device Manager > the drive > Policies > 'Better \
         performance', and check\n  'Enable write caching on the device'. Leave 'Turn off \
         Windows write-cache buffer flushing'\n  UNCHECKED — that setting risks filesystem \
         corruption on power loss, which a re-run cannot\n  repair. With caching on, always \
         use Safely Remove Hardware before unplugging.\n  To continue without this prompt \
         next time, pass --accept-write-cache-policy."
        )
        .map_err(|error| (6, format!("write preflight warning: {error}")))?;
    }
    // The Quick-removal notice always prints, but its interactive opt-out is
    // suppressed once the user has explicitly accepted the policy.
    let prompt_for_write_cache = quick_removal && !options.accepted.write_cache_policy;

    let needs_degradation_confirmation =
        degraded && !options.accepted.degraded_filesystem && !options.dry_run;
    let needs_remote_confirmation = remote && !options.accepted.remote_paths && !options.dry_run;
    let needs_required_confirmation = needs_degradation_confirmation || needs_remote_confirmation;
    if needs_required_confirmation && !options.interactive {
        let required = if needs_degradation_confirmation && needs_remote_confirmation {
            "--accept-degraded-filesystem and --accept-remote-paths"
        } else if needs_degradation_confirmation {
            "--accept-degraded-filesystem"
        } else {
            "--accept-remote-paths"
        };
        return Err((
            5,
            format!(
                "copy limitations require confirmation, but no interactive prompt is available; pass {required} after reviewing the warning"
            ),
        ));
    }
    if options.interactive && (needs_required_confirmation || prompt_for_write_cache) {
        let mut stderr = std::io::stderr().lock();
        if needs_required_confirmation {
            write!(
                stderr,
                "Continue with the acknowledged copy limitations? [y/N] "
            )
            .map_err(|error| (6, format!("write preflight prompt: {error}")))?;
        } else {
            write!(stderr, "Continue with the current drive policy? [Y/n] ")
                .map_err(|error| (6, format!("write preflight prompt: {error}")))?;
        }
        stderr
            .flush()
            .map_err(|error| (6, format!("flush preflight prompt: {error}")))?;
        drop(stderr);
        let mut answer = String::new();
        let read = std::io::stdin().read_line(&mut answer);
        let accepted = read.is_ok() && prompt_answer_accepted(&answer, needs_required_confirmation);
        if !accepted {
            return Err((
                5,
                "aborted before any copying; no destination-tree changes were made".to_owned(),
            ));
        }
        return Ok(PreflightAcceptance {
            degraded_filesystem: options.accepted.degraded_filesystem
                || needs_degradation_confirmation,
            remote_paths: options.accepted.remote_paths || needs_remote_confirmation,
            write_cache_policy: options.accepted.write_cache_policy || prompt_for_write_cache,
        });
    }
    Ok(options.accepted)
}

fn prompt_answer_accepted(answer: &str, degradation_confirmation: bool) -> bool {
    let normalized = answer.trim().to_ascii_lowercase();
    if degradation_confirmation {
        matches!(normalized.as_str(), "y" | "yes")
    } else {
        !normalized.starts_with('n')
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Cli, Command, CopyDisplayMode, copy_display_mode, execute, exit_for_error, parse_profiles,
        parse_size, parse_tune, prompt_answer_accepted,
    };
    use bigcp_core::{BigcpError, DeviceClass};
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
    fn help_shows_copy_examples_without_internal_implementation_notes() {
        let mut command = <Cli as clap::CommandFactory>::command();
        let mut help = Vec::new();
        assert!(command.write_long_help(&mut help).is_ok());
        let help = String::from_utf8_lossy(&help);
        assert!(help.contains("Examples:"));
        assert!(help.contains("bigcp [OPTIONS] <SOURCE> <DESTINATION>"));
        assert!(!help.contains("subcommand rejection"));
    }

    #[test]
    fn no_color_keeps_dashboard_while_quiet_really_is_summary_only() {
        let no_color = Cli::try_parse_from(["bigcp", "source", "destination", "--no-color"]);
        assert!(no_color.is_ok());
        let Some(no_color) = no_color.ok() else {
            return;
        };
        assert_eq!(
            copy_display_mode(&no_color.flags, true, true),
            CopyDisplayMode::Dashboard {
                color_enabled: false
            }
        );

        let quiet = Cli::try_parse_from(["bigcp", "source", "destination", "--quiet"]);
        assert!(quiet.is_ok());
        let Some(quiet) = quiet.ok() else {
            return;
        };
        assert_eq!(
            copy_display_mode(&quiet.flags, false, true),
            CopyDisplayMode::Plain { quiet: true }
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

    #[test]
    fn same_spindle_burst_tuning_parses_binary_size() {
        let parsed = Cli::try_parse_from([
            "bigcp",
            "source",
            "destination",
            "--tune",
            "same-spindle-burst=128MiB",
        ]);
        assert!(parsed.is_ok_and(|value| {
            value
                .flags
                .tune
                .and_then(|tune| tune.same_spindle_burst_bytes)
                == Some(128 * 1024 * 1024)
        }));
    }

    #[test]
    fn degraded_filesystem_acceptance_is_copy_only_and_explicit() {
        let copy = Cli::try_parse_from([
            "bigcp",
            "source",
            "destination",
            "--accept-degraded-filesystem",
        ]);
        assert!(copy.is_ok_and(|value| value.flags.accept_degraded_filesystem));

        let verify = Cli::try_parse_from([
            "bigcp",
            "--accept-degraded-filesystem",
            "verify",
            "source",
            "destination",
        ]);
        assert!(verify.is_ok());
        let Some(verify) = verify.ok() else {
            return;
        };
        assert!(execute(verify).is_err_and(|(code, message)| {
            code == 5 && message.contains("copy flags are not accepted")
        }));
    }

    #[test]
    fn remote_path_acceptance_is_copy_only_and_explicit() {
        let copy = Cli::try_parse_from([
            "bigcp",
            r"\\server\share\source",
            r"C:\destination",
            "--accept-remote-paths",
        ]);
        assert!(copy.is_ok_and(|value| value.flags.accept_remote_paths));

        let verify = Cli::try_parse_from([
            "bigcp",
            "verify",
            "source",
            "destination",
            "--accept-remote-paths",
        ]);
        assert!(verify.is_err());
    }

    #[test]
    fn sizes_parse_binary_suffixes_case_insensitively_and_reject_nonsense() {
        assert_eq!(parse_size("4096"), Ok(4096));
        assert_eq!(parse_size("64KiB"), Ok(64 * 1024));
        assert_eq!(parse_size("10mib"), Ok(10 * 1024 * 1024));
        assert_eq!(parse_size("2GiB"), Ok(2 * 1024 * 1024 * 1024));
        assert!(parse_size("0").is_err());
        assert!(parse_size("").is_err());
        assert!(parse_size("12MB").is_err());
        // Overflow in the multiplier must fail, not wrap.
        assert!(parse_size("99999999999999999999GiB").is_err());
        assert!(parse_size(&format!("{}GiB", u64::MAX)).is_err());
    }

    #[test]
    fn profile_classes_fan_out_and_reject_unknown_or_extra_entries() {
        assert_eq!(
            parse_profiles("hdd"),
            Ok((DeviceClass::Hdd, DeviceClass::Hdd))
        );
        assert_eq!(
            parse_profiles("nvme,usb-ssd"),
            Ok((DeviceClass::Nvme, DeviceClass::UsbSsd))
        );
        assert!(parse_profiles("ssd").is_err());
        assert!(parse_profiles("nvme,hdd,auto").is_err());
        // A trailing comma is an empty second entry, not a silent single.
        assert!(parse_profiles("nvme,").is_err());
    }

    #[test]
    fn tune_items_require_known_keys_and_key_value_form() {
        assert!(parse_tune("streams=2").is_err());
        assert!(parse_tune("chunk").is_err());
        let parsed = parse_tune("chunk=1MiB,threads=4");
        assert!(parsed.as_ref().is_ok_and(|tune| {
            tune.chunk_bytes == Some(1024 * 1024) && tune.threads == Some(4)
        }));
    }

    #[test]
    fn library_errors_map_to_preflight_or_internal_exit_codes() {
        assert_eq!(exit_for_error(&BigcpError::Locked("x".to_owned())), 5);
        assert_eq!(exit_for_error(&BigcpError::Invalid("x".to_owned())), 5);
        assert_eq!(exit_for_error(&BigcpError::Audit("x".to_owned())), 6);
        assert_eq!(exit_for_error(&BigcpError::Invariant("x".to_owned())), 6);
        assert_eq!(exit_for_error(&BigcpError::Format("x".to_owned())), 6);
    }

    #[test]
    fn redirected_stdout_always_selects_plain_progress() {
        let parsed = Cli::try_parse_from(["bigcp", "source", "destination"]);
        assert!(parsed.is_ok());
        let Some(parsed) = parsed.ok() else {
            return;
        };
        assert_eq!(
            copy_display_mode(&parsed.flags, false, false),
            CopyDisplayMode::Plain { quiet: false }
        );
    }

    /// Completeness guard: every copy flag must be listed both here (with a
    /// sample invocation) and in `reject_copy_only_flags`. Adding a flag to
    /// `CopyFlags` without updating the rejection list makes this test fail
    /// with a "not accepted" mismatch; forgetting the sample fails the
    /// missing-list assertion.
    #[test]
    fn every_copy_flag_is_rejected_before_a_subcommand() {
        let samples: &[(&str, &[&str])] = &[
            ("dry-run", &["--dry-run"]),
            ("verify", &["--verify"]),
            ("include-system", &["--include-system"]),
            ("skip-cloud", &["--skip-cloud"]),
            ("replace", &["--replace=false"]),
            ("flush", &["--flush"]),
            ("no-sparse", &["--no-sparse"]),
            ("analyze", &["--analyze"]),
            ("raw-reparse", &["--raw-reparse"]),
            ("fresh", &["--fresh"]),
            (
                "accept-degraded-filesystem",
                &["--accept-degraded-filesystem"],
            ),
            ("accept-remote-paths", &["--accept-remote-paths"]),
            (
                "accept-write-cache-policy",
                &["--accept-write-cache-policy"],
            ),
            ("profile", &["--profile", "hdd"]),
            ("tune", &["--tune", "chunk=1MiB"]),
            ("state-dir", &["--state-dir", "state"]),
            ("log", &["--log", "log.jsonl"]),
            ("report", &["--report", "report.json"]),
            ("plain", &["--plain"]),
            ("no-color", &["--no-color"]),
            ("quiet", &["--quiet"]),
        ];
        let command = <Cli as clap::CommandFactory>::command();
        let mut missing = Vec::new();
        for argument in command.get_arguments() {
            let Some(long) = argument.get_long() else {
                continue;
            };
            if long == "help" || long == "version" {
                continue;
            }
            let Some((_, invocation)) = samples.iter().find(|(name, _)| *name == long) else {
                missing.push(long.to_owned());
                continue;
            };
            let mut argv = vec!["bigcp"];
            argv.extend_from_slice(invocation);
            argv.extend_from_slice(&["verify", "source", "destination"]);
            let parsed = Cli::try_parse_from(&argv);
            assert!(parsed.is_ok(), "{long}: flag failed to parse before verify");
            let Some(parsed) = parsed.ok() else {
                return;
            };
            let result = execute(parsed);
            assert!(
                result
                    .as_ref()
                    .err()
                    .is_some_and(|(code, message)| *code == 5 && message.contains("not accepted")),
                "--{long} was not rejected before the verify subcommand: {result:?}"
            );
        }
        assert!(
            missing.is_empty(),
            "add sample invocations and a rejection-list entry for: {missing:?}"
        );
    }

    #[test]
    fn degradation_prompt_defaults_no_while_quick_removal_defaults_yes() {
        assert!(!prompt_answer_accepted("", true));
        assert!(!prompt_answer_accepted("maybe", true));
        assert!(prompt_answer_accepted("YES", true));

        assert!(prompt_answer_accepted("", false));
        assert!(!prompt_answer_accepted("no", false));
    }
}
