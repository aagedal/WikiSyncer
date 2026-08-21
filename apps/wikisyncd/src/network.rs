//! Best-effort operating-system metered-network detection.
//!
//! Detection is deliberately tri-state. Callers may defer automatic network work
//! when [`MeteredNetworkState::Metered`] is reported, but must not treat an unknown
//! result as proof that the current network is unmetered.

/// Whether the operating system reports the current active network as metered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MeteredNetworkState {
    /// Every relevant active connection reports a metered cost.
    Metered,
    /// Every relevant active connection reports an unmetered cost.
    Unmetered,
    /// No reliable single cost could be determined.
    Unknown,
}

/// How a metered-network probe reached its result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MeteredNetworkProbeOutcome {
    /// The operating system reported one consistent cost for active connections.
    Reported,
    /// The operating system could not determine the cost or no connection was active.
    Indeterminate,
    /// Relevant active connections had conflicting or partially unknown costs.
    Ambiguous,
    /// This target has no implemented reliable detection mechanism.
    Unsupported,
    /// The operating-system query utility was not available.
    CommandUnavailable,
    /// The operating-system query exceeded its execution deadline.
    TimedOut,
    /// The operating-system query could not start, run, or exit successfully.
    CommandFailed,
    /// The query produced malformed, non-UTF-8, or excessive output.
    InvalidOutput,
}

/// Fail-visible result of a metered-network probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MeteredNetworkStatus {
    /// Policy-facing tri-state network cost.
    pub state: MeteredNetworkState,
    /// Probe outcome that explains whether `state` is authoritative.
    pub outcome: MeteredNetworkProbeOutcome,
}

impl MeteredNetworkStatus {
    const fn unknown(outcome: MeteredNetworkProbeOutcome) -> Self {
        Self {
            state: MeteredNetworkState::Unknown,
            outcome,
        }
    }
}

/// Detects whether the current network is metered without contacting the network.
///
/// Linux queries NetworkManager through a bounded `nmcli` child process. Other
/// targets return [`MeteredNetworkState::Unknown`] rather than guessing. Failures
/// are represented by [`MeteredNetworkStatus::outcome`] and do not themselves imply
/// that synchronization must be blocked.
#[must_use]
pub fn detect_metered_network() -> MeteredNetworkStatus {
    #[cfg(target_os = "linux")]
    {
        linux::detect()
    }

    #[cfg(not(target_os = "linux"))]
    {
        MeteredNetworkStatus::unknown(MeteredNetworkProbeOutcome::Unsupported)
    }
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy)]
enum ReportedCost {
    Metered,
    Unmetered,
    Unknown,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Default)]
struct DeviceReport {
    state: Option<u32>,
    cost: Option<ReportedCost>,
}

#[cfg(any(target_os = "linux", test))]
fn parse_nmcli_output(output: &str) -> Option<MeteredNetworkStatus> {
    let mut devices = Vec::new();
    let mut device = DeviceReport::default();
    let mut has_fields = false;

    for line in output.lines() {
        if line.is_empty() {
            if has_fields {
                devices.push(device);
                device = DeviceReport::default();
                has_fields = false;
            }
            continue;
        }
        let (field, value) = line.split_once(':')?;
        match field {
            "GENERAL.STATE" => {
                if device.state.is_some() {
                    if has_fields {
                        devices.push(device);
                    }
                    device = DeviceReport::default();
                }
                let digits = value
                    .trim_start()
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>();
                if digits.is_empty() {
                    return None;
                }
                device.state = Some(digits.parse().ok()?);
                has_fields = true;
            }
            "GENERAL.METERED" => {
                if device.cost.is_some() {
                    return None;
                }
                device.cost = Some(match value.trim() {
                    "yes" | "guess-yes" => ReportedCost::Metered,
                    "no" | "guess-no" => ReportedCost::Unmetered,
                    "unknown" => ReportedCost::Unknown,
                    _ => return None,
                });
                has_fields = true;
            }
            _ => return None,
        }
    }
    if has_fields {
        devices.push(device);
    }

    let mut metered = 0_usize;
    let mut unmetered = 0_usize;
    let mut unknown = 0_usize;
    for device in devices {
        let (Some(state), Some(cost)) = (device.state, device.cost) else {
            return None;
        };
        if state != 100 {
            continue;
        }
        match cost {
            ReportedCost::Metered => metered += 1,
            ReportedCost::Unmetered => unmetered += 1,
            ReportedCost::Unknown => unknown += 1,
        }
    }

    Some(if metered == 0 && unmetered == 0 {
        MeteredNetworkStatus::unknown(MeteredNetworkProbeOutcome::Indeterminate)
    } else if unknown > 0 || (metered > 0 && unmetered > 0) {
        MeteredNetworkStatus::unknown(MeteredNetworkProbeOutcome::Ambiguous)
    } else if metered > 0 {
        MeteredNetworkStatus {
            state: MeteredNetworkState::Metered,
            outcome: MeteredNetworkProbeOutcome::Reported,
        }
    } else {
        MeteredNetworkStatus {
            state: MeteredNetworkState::Unmetered,
            outcome: MeteredNetworkProbeOutcome::Reported,
        }
    })
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{self, Read};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{MeteredNetworkProbeOutcome, MeteredNetworkStatus, parse_nmcli_output};

    const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
    const POLL_INTERVAL: Duration = Duration::from_millis(10);
    const MAX_OUTPUT_BYTES: u64 = 64 * 1024;

    pub(super) fn detect() -> MeteredNetworkStatus {
        let mut child = match Command::new("nmcli")
            .args([
                "--terse",
                "--fields",
                "GENERAL.STATE,GENERAL.METERED",
                "device",
                "show",
            ])
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return MeteredNetworkStatus::unknown(
                    MeteredNetworkProbeOutcome::CommandUnavailable,
                );
            }
            Err(_) => {
                return MeteredNetworkStatus::unknown(MeteredNetworkProbeOutcome::CommandFailed);
            }
        };

        let Some(stdout) = child.stdout.take() else {
            terminate(&mut child);
            return MeteredNetworkStatus::unknown(MeteredNetworkProbeOutcome::CommandFailed);
        };
        let reader = match thread::Builder::new()
            .name("wikisyncd-nmcli-output".to_owned())
            .spawn(move || {
                let mut output = Vec::new();
                stdout
                    .take(MAX_OUTPUT_BYTES + 1)
                    .read_to_end(&mut output)
                    .map(|_| output)
            }) {
            Ok(reader) => reader,
            Err(_) => {
                terminate(&mut child);
                return MeteredNetworkStatus::unknown(MeteredNetworkProbeOutcome::CommandFailed);
            }
        };

        let deadline = Instant::now() + PROBE_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
                Ok(None) => {
                    terminate(&mut child);
                    break None;
                }
                Err(_) => {
                    terminate(&mut child);
                    return MeteredNetworkStatus::unknown(
                        MeteredNetworkProbeOutcome::CommandFailed,
                    );
                }
            }
        };

        let output = match reader.join() {
            Ok(Ok(output)) => output,
            Ok(Err(_)) | Err(_) => {
                return MeteredNetworkStatus::unknown(MeteredNetworkProbeOutcome::CommandFailed);
            }
        };
        let Some(status) = status else {
            return MeteredNetworkStatus::unknown(MeteredNetworkProbeOutcome::TimedOut);
        };
        interpret(status, &output)
    }

    fn interpret(status: ExitStatus, output: &[u8]) -> MeteredNetworkStatus {
        if !status.success() {
            return MeteredNetworkStatus::unknown(MeteredNetworkProbeOutcome::CommandFailed);
        }
        if output.len() > usize::try_from(MAX_OUTPUT_BYTES).unwrap_or(usize::MAX) {
            return MeteredNetworkStatus::unknown(MeteredNetworkProbeOutcome::InvalidOutput);
        }
        let Ok(output) = std::str::from_utf8(output) else {
            return MeteredNetworkStatus::unknown(MeteredNetworkProbeOutcome::InvalidOutput);
        };
        parse_nmcli_output(output).unwrap_or_else(|| {
            MeteredNetworkStatus::unknown(MeteredNetworkProbeOutcome::InvalidOutput)
        })
    }

    fn terminate(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_and_guessed_costs_have_the_same_policy_meaning() {
        for value in ["yes", "guess-yes"] {
            assert_eq!(
                parse_nmcli_output(&format!(
                    "GENERAL.STATE:100 (connected)\nGENERAL.METERED:{value}\n"
                )),
                Some(MeteredNetworkStatus {
                    state: MeteredNetworkState::Metered,
                    outcome: MeteredNetworkProbeOutcome::Reported,
                })
            );
        }
        for value in ["no", "guess-no"] {
            assert_eq!(
                parse_nmcli_output(&format!(
                    "GENERAL.STATE:100 (connected)\nGENERAL.METERED:{value}\n"
                )),
                Some(MeteredNetworkStatus {
                    state: MeteredNetworkState::Unmetered,
                    outcome: MeteredNetworkProbeOutcome::Reported,
                })
            );
        }
    }

    #[test]
    fn inactive_devices_do_not_make_an_active_report_ambiguous() {
        let output = "GENERAL.STATE:100 (connected)\nGENERAL.METERED:no\n\n\
                      GENERAL.STATE:30 (disconnected)\nGENERAL.METERED:unknown\n";
        assert_eq!(
            parse_nmcli_output(output),
            Some(MeteredNetworkStatus {
                state: MeteredNetworkState::Unmetered,
                outcome: MeteredNetworkProbeOutcome::Reported,
            })
        );
    }

    #[test]
    fn unknown_or_missing_active_cost_is_fail_visible() {
        assert_eq!(
            parse_nmcli_output("GENERAL.STATE:100 (connected)\nGENERAL.METERED:unknown\n"),
            Some(MeteredNetworkStatus::unknown(
                MeteredNetworkProbeOutcome::Indeterminate
            ))
        );
        assert_eq!(
            parse_nmcli_output("GENERAL.STATE:30 (disconnected)\nGENERAL.METERED:no\n"),
            Some(MeteredNetworkStatus::unknown(
                MeteredNetworkProbeOutcome::Indeterminate
            ))
        );
    }

    #[test]
    fn conflicting_or_partially_unknown_active_costs_are_ambiguous() {
        for second in ["no", "unknown"] {
            let output = format!(
                "GENERAL.STATE:100 (connected)\nGENERAL.METERED:yes\n\n\
                 GENERAL.STATE:100 (connected)\nGENERAL.METERED:{second}\n"
            );
            assert_eq!(
                parse_nmcli_output(&output),
                Some(MeteredNetworkStatus::unknown(
                    MeteredNetworkProbeOutcome::Ambiguous
                ))
            );
        }
    }

    #[test]
    fn malformed_or_unrecognized_reports_are_rejected() {
        assert_eq!(
            parse_nmcli_output("GENERAL.STATE:connected\nGENERAL.METERED:no\n"),
            None
        );
        assert_eq!(
            parse_nmcli_output("GENERAL.STATE:100 (connected)\nGENERAL.METERED:maybe\n"),
            None
        );
        assert_eq!(parse_nmcli_output("GENERAL.STATE:100 (connected)\n"), None);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn unsupported_targets_do_not_guess() {
        assert_eq!(
            detect_metered_network(),
            MeteredNetworkStatus::unknown(MeteredNetworkProbeOutcome::Unsupported)
        );
    }
}
