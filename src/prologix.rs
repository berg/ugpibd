// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors

#[derive(Debug, PartialEq, Eq)]
pub enum LineResult {
    Ok,
    Response(String),
    Error(String),
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
    /// Pulse IFC.
    Ifc,
    /// Reset daemon GPIB state (not the instrument).
    Reset,
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
            // Stubbed commands
            "srq" => LineResult::Response("0".to_string()),
            "spoll" | "llo" | "loc" | "trg" | "status" | "savecfg" => LineResult::Ok,
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
