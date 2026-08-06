// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors

#[derive(Debug, PartialEq, Eq)]
pub enum LineResult {
    Ok,
    Response(String),
    Error(String),
    /// Serial-poll `pad` and return its status byte.
    SerialPoll {
        pad: u8,
    },
    /// Group execute trigger to `pad`.
    Trigger {
        pad: u8,
    },
    /// Report whether the SRQ line is currently asserted.
    Srq,
    /// Enter/leave unaddressed-listen, or report it when the argument is None.
    ListenOnly(Option<bool>),
    /// Become a device at an address, stop being one, or report the state.
    DeviceMode(Option<Option<u8>>),
    /// Report all eight GPIB control lines. A ugpibd extension, not a real
    /// Prologix command: `++srq` answers one bit of the same register, and
    /// diagnosing a bus needs the other seven.
    BusLines,
    /// Forward data to GPIB instrument.
    Forward {
        pad: u8,
        data: Vec<u8>,
        send_eoi: bool,
        auto_read: bool,
    },
    /// Perform a GPIB read.
    Read {
        until_eoi: bool,
        until_char: Option<u8>,
    },
    /// Send Selected Device Clear to `pad`.
    DeviceClear {
        pad: u8,
    },
    /// Send an addressed Go To Local to `pad` (`++loc`).
    GoToLocal {
        pad: u8,
    },
    /// Send Local Lockout to the whole bus (`++llo`).
    LocalLockout,
    /// Pulse IFC.
    Ifc,
    /// Reset daemon GPIB state (not the instrument).
    Reset,
}

/// Parse an optional GPIB primary address argument, falling back to `default`
/// (the currently addressed instrument) when the argument is empty.
fn parse_optional_pad(args: &str, default: u8) -> Result<u8, String> {
    if args.is_empty() {
        return Ok(default);
    }
    match args.parse::<u8>() {
        Ok(n) if n <= 30 => Ok(n),
        _ => Err(format!("invalid address: {args}")),
    }
}

#[derive(Debug)]
pub struct PrologixState {
    pub addr: u8,
    pub auto_read: bool,
    pub eoi: bool,
    /// 0=CR+LF, 1=CR, 2=LF, 3=nothing
    pub eos_mode: u8,
    pub eot_enable: bool,
    pub eot_char: u8,
    pub read_tmo_ms: u32,
}

impl Default for PrologixState {
    fn default() -> Self {
        Self {
            addr: 0,
            auto_read: false,
            eoi: true,
            eos_mode: 0, // CR+LF — real Prologix default
            eot_enable: false,
            eot_char: 0,
            read_tmo_ms: 3000,
        }
    }
}

impl PrologixState {
    /// Like `default()`, but with the initial addressed PAD set to `addr`.
    /// Used to seed the front-end from the daemon's `--default-address`.
    pub fn with_addr(addr: u8) -> Self {
        Self {
            addr,
            ..Self::default()
        }
    }

    /// Process one line from the TCP client.
    pub fn handle_line(&mut self, line: &str) -> LineResult {
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(rest) = line.strip_prefix("++") {
            self.handle_command(rest)
        } else {
            self.handle_data(line)
        }
    }

    fn handle_command(&mut self, cmd: &str) -> LineResult {
        let (name, args) = cmd
            .split_once(char::is_whitespace)
            .map(|(n, a)| (n.trim(), a.trim()))
            .unwrap_or((cmd.trim(), ""));

        match name {
            "addr" => {
                if args.is_empty() {
                    LineResult::Response(self.addr.to_string())
                } else {
                    match args.parse::<u8>() {
                        Ok(n) if n <= 30 => {
                            self.addr = n;
                            LineResult::Ok
                        }
                        _ => LineResult::Error(format!("invalid address: {args}")),
                    }
                }
            }
            "auto" => match args {
                "0" => {
                    self.auto_read = false;
                    LineResult::Ok
                }
                "1" => {
                    self.auto_read = true;
                    LineResult::Ok
                }
                "" => LineResult::Response(if self.auto_read { "1" } else { "0" }.into()),
                _ => LineResult::Error("++auto requires 0 or 1".into()),
            },
            "eoi" => match args {
                "0" => {
                    self.eoi = false;
                    LineResult::Ok
                }
                "1" => {
                    self.eoi = true;
                    LineResult::Ok
                }
                "" => LineResult::Response(if self.eoi { "1" } else { "0" }.into()),
                _ => LineResult::Error("++eoi requires 0 or 1".into()),
            },
            "eos" => match args {
                "0" | "1" | "2" | "3" => {
                    self.eos_mode = args.parse().unwrap();
                    LineResult::Ok
                }
                "" => LineResult::Response(self.eos_mode.to_string()),
                _ => LineResult::Error("++eos requires 0-3".into()),
            },
            "eot_enable" => match args {
                "0" => {
                    self.eot_enable = false;
                    LineResult::Ok
                }
                "1" => {
                    self.eot_enable = true;
                    LineResult::Ok
                }
                "" => LineResult::Response(if self.eot_enable { "1" } else { "0" }.into()),
                _ => LineResult::Error("++eot_enable requires 0 or 1".into()),
            },
            "eot_char" => {
                if args.is_empty() {
                    LineResult::Response(self.eot_char.to_string())
                } else {
                    match args.parse::<u8>() {
                        Ok(n) => {
                            self.eot_char = n;
                            LineResult::Ok
                        }
                        Err(_) => LineResult::Error("++eot_char requires 0-255".into()),
                    }
                }
            }
            "read_tmo_ms" => {
                if args.is_empty() {
                    LineResult::Response(self.read_tmo_ms.to_string())
                } else {
                    match args.parse::<u32>() {
                        Ok(n) => {
                            self.read_tmo_ms = n;
                            LineResult::Ok
                        }
                        Err(_) => LineResult::Error("++read_tmo_ms requires integer".into()),
                    }
                }
            }
            "read" => {
                if args == "eoi" || args.is_empty() {
                    LineResult::Read {
                        until_eoi: true,
                        until_char: None,
                    }
                } else if let Ok(n) = args.parse::<u8>() {
                    LineResult::Read {
                        until_eoi: true,
                        until_char: Some(n),
                    }
                } else {
                    LineResult::Error(format!("++read invalid arg: {args}"))
                }
            }
            "clr" => LineResult::DeviceClear { pad: self.addr },
            "ifc" => LineResult::Ifc,
            "rst" => LineResult::Reset,
            "ver" => LineResult::Response("Prologix GPIB-USB Controller version 6.107".to_string()),
            "mode" => match args {
                "1" => LineResult::Ok,
                "0" => LineResult::Error(
                    "device mode not supported (hardware is controller-only)".into(),
                ),
                _ => LineResult::Error("++mode requires 0 or 1".into()),
            },
            "srq" => LineResult::Srq,
            // ++lines — dump the bus status register. Not Prologix; see the
            // LineResult variant.
            "lines" => LineResult::BusLines,
            // ++lon [0|1] — unaddressed listen. A ugpibd extension. Runtime
            // switchable on purpose: the daemon is usually socket/systemd
            // activated, so a mode reachable only by restarting with a
            // different flag would not be reachable in practice.
            // ++dev [addr|off] — act as a GPIB device rather than a controller.
            "dev" => match args {
                "" => LineResult::DeviceMode(None),
                "off" => LineResult::DeviceMode(Some(None)),
                a => match a.parse::<u8>() {
                    Ok(n) if n <= 30 => LineResult::DeviceMode(Some(Some(n))),
                    _ => LineResult::Error("++dev requires an address 0-30, or off".into()),
                },
            },
            "lon" => match args {
                "" => LineResult::ListenOnly(None),
                "0" => LineResult::ListenOnly(Some(false)),
                "1" => LineResult::ListenOnly(Some(true)),
                _ => LineResult::Error("++lon requires 0 or 1".into()),
            },
            // ++spoll [pad] — serial-poll the given address, or the currently
            // addressed instrument when no argument is given.
            "spoll" => match parse_optional_pad(args, self.addr) {
                Ok(pad) => LineResult::SerialPoll { pad },
                Err(e) => LineResult::Error(e),
            },
            // ++trg [pad] — group execute trigger.
            "trg" => match parse_optional_pad(args, self.addr) {
                Ok(pad) => LineResult::Trigger { pad },
                Err(e) => LineResult::Error(e),
            },
            // ++llo — local lockout. Universal: the standard has no
            // per-device form, so this disables the local key on every
            // instrument on the bus, and dropping REN is what clears it.
            "llo" => LineResult::LocalLockout,
            // ++loc [pad] — return one instrument to front-panel control.
            // Addressed, so unlike dropping REN it leaves the rest of the bus
            // in remote. The next write to the instrument puts it back into
            // remote, which is what the standard says happens.
            "loc" => match parse_optional_pad(args, self.addr) {
                Ok(pad) => LineResult::GoToLocal { pad },
                Err(e) => LineResult::Error(e),
            },
            // Accepted and ignored: neither has a reply in the real Prologix
            // firmware, so silence is not misleading. See docs/ROADMAP.md.
            "status" | "savecfg" => LineResult::Ok,
            _ => LineResult::Error(format!("unknown command: {name}")),
        }
    }

    fn handle_data(&self, line: &str) -> LineResult {
        let mut data: Vec<u8> = line.as_bytes().to_vec();
        match self.eos_mode {
            0 => {
                data.push(b'\r');
                data.push(b'\n');
            }
            1 => data.push(b'\r'),
            2 => data.push(b'\n'),
            _ => {}
        }
        LineResult::Forward {
            pad: self.addr,
            data,
            send_eoi: self.eoi,
            auto_read: self.auto_read,
        }
    }

    /// Append `eot_char` to a read response if `eot_enable` is set.
    pub fn apply_eot(&self, mut data: Vec<u8>) -> Vec<u8> {
        if self.eot_enable {
            data.push(self.eot_char);
        }
        data
    }
}
