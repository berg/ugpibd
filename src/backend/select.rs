// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// Selecting *which* attached USB-GPIB adapter to drive when more than one is
// present. Adapters are identified by their physical USB port — an OS-native
// "port id" — never by USB serial number (this hardware's serials are not
// trustworthy: clones reuse them and some units omit them). The serial is only
// surfaced for humans in `--list`.

use anyhow::{Context, Result};

use super::BackendKind;

/// How the operator asked us to pick among attached adapters.
#[derive(Debug, Clone)]
pub enum UsbSelector {
    /// No explicit port: require exactly one match.
    Auto,
    /// Bind to the adapter at this OS-native port id.
    Port(String),
}

impl UsbSelector {
    /// The requested port id, if any.
    pub fn port(&self) -> Option<&str> {
        match self {
            UsbSelector::Auto => None,
            UsbSelector::Port(p) => Some(p),
        }
    }
}

/// A supported adapter found on the bus, with the info `--list` shows.
#[derive(Debug)]
pub struct DiscoveredAdapter {
    pub kind: BackendKind,
    pub port_id: String,
    pub vid: u16,
    pub pid: u16,
    pub product: Option<String>,
    pub serial: Option<String>,
}

/// Short, OS-native identifier for the physical port a device is plugged into.
/// Stable across replug into the same socket and across the 82357's firmware
/// renumeration (unlike `device_address`, which is reassigned each enumeration).
///
/// - Linux/Android: sysfs node basename, e.g. `1-1.2` (root hubs: `usb1`).
/// - macOS: IOKit location id hex, e.g. `0x03440000`.
/// - Other: best-effort; not a supported/tested path.
pub fn port_id(dev: &nusb::DeviceInfo) -> String {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        return dev
            .sysfs_path()
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("bus{}-addr{}", dev.bus_number(), dev.device_address()));
    }
    #[cfg(target_os = "macos")]
    {
        return format!("{:#010x}", dev.location_id());
    }
    #[cfg(target_os = "windows")]
    {
        return dev.instance_id().to_string_lossy().into_owned();
    }
    #[allow(unreachable_code)]
    {
        format!("bus{}-addr{}", dev.bus_number(), dev.device_address())
    }
}

/// Enumerate every attached *supported* adapter, in `BackendKind::ALL` order.
pub fn enumerate() -> Result<Vec<DiscoveredAdapter>> {
    let mut out = Vec::new();
    for dev in nusb::list_devices().context("failed to list USB devices")? {
        let ids = (dev.vendor_id(), dev.product_id());
        if let Some(kind) = BackendKind::ALL
            .iter()
            .copied()
            .find(|k| k.usb_ids().contains(&ids))
        {
            out.push(DiscoveredAdapter {
                kind,
                port_id: port_id(&dev),
                vid: dev.vendor_id(),
                pid: dev.product_id(),
                product: dev.product_string().map(str::to_owned),
                serial: dev.serial_number().map(str::to_owned),
            });
        }
    }
    Ok(out)
}

/// Resolve `found` to exactly one adapter given an optional backend-id filter
/// and the port selector, or a descriptive error naming the candidates.
pub fn resolve<'a>(
    found: &'a [DiscoveredAdapter],
    backend: Option<&str>,
    selector: &UsbSelector,
) -> Result<&'a DiscoveredAdapter> {
    let by_backend: Vec<&DiscoveredAdapter> = found
        .iter()
        .filter(|a| backend.map_or(true, |id| a.kind.id() == id))
        .collect();

    let candidates: Vec<&DiscoveredAdapter> = match selector.port() {
        None => by_backend,
        Some(want) => by_backend
            .into_iter()
            .filter(|a| port_input_matches(&a.port_id, want))
            .collect(),
    };

    match candidates.as_slice() {
        [one] => Ok(one),
        [] => {
            if let Some(want) = selector.port() {
                anyhow::bail!("no supported adapter at USB port {want:?}");
            }
            match backend {
                Some(id) => anyhow::bail!("no {id} adapter found"),
                None => anyhow::bail!(
                    "no supported USB-GPIB adapter detected (known backends: {})",
                    super::known_ids()
                ),
            }
        }
        many => {
            let list = many
                .iter()
                .map(|a| format!("  {:<16} port {}", a.kind.id(), a.port_id))
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!(
                "multiple adapters present; select one with --usb-port <PORT> \
                 (see --list):\n{list}"
            )
        }
    }
}

/// Whether the operator's `--usb-port` `input` designates the port whose
/// canonical id is `canonical_id`. Dispatches to the per-OS rule.
fn port_input_matches(canonical_id: &str, input: &str) -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        return linux_port_input_matches(canonical_id, input);
    }
    #[cfg(target_os = "macos")]
    {
        return macos_port_input_matches(canonical_id, input);
    }
    #[allow(unreachable_code)]
    {
        input == canonical_id
    }
}

/// Linux: accept the bare id (`1-1.2`), a full sysfs path, or its trailing
/// segment (so a copy-pasted `sysfs_path()` also works).
#[allow(dead_code)]
fn linux_port_input_matches(canonical_id: &str, input: &str) -> bool {
    input == canonical_id || input.rsplit('/').next() == Some(canonical_id)
}

/// Parse a location id, tolerating an optional `0x`/`0X` prefix and leading
/// zeros. `None` if it isn't hex.
#[allow(dead_code)]
fn parse_hex_u32(s: &str) -> Option<u32> {
    let t = s.trim();
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    u32::from_str_radix(t, 16).ok()
}

/// macOS: compare location ids numerically so `0x03440000`, `03440000`, and
/// `0X3440000` all match; fall back to string compare for non-hex input.
#[allow(dead_code)]
fn macos_port_input_matches(canonical_id: &str, input: &str) -> bool {
    match (parse_hex_u32(canonical_id), parse_hex_u32(input)) {
        (Some(a), Some(b)) => a == b,
        _ => input == canonical_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(kind: BackendKind, port_id: &str) -> DiscoveredAdapter {
        DiscoveredAdapter {
            kind,
            port_id: port_id.to_string(),
            vid: 0,
            pid: 0,
            product: None,
            serial: None,
        }
    }

    #[test]
    fn linux_matcher_accepts_id_full_path_and_tail() {
        assert!(linux_port_input_matches("1-1.2", "1-1.2"));
        assert!(linux_port_input_matches(
            "1-1.2",
            "/sys/bus/usb/devices/1-1.2"
        ));
        assert!(!linux_port_input_matches("1-1.2", "1-1.3"));
        assert!(!linux_port_input_matches(
            "1-1.2",
            "/sys/bus/usb/devices/1-1"
        ));
    }

    #[test]
    fn macos_matcher_normalizes_hex() {
        assert!(macos_port_input_matches("0x03440000", "0x03440000"));
        assert!(macos_port_input_matches("0x03440000", "03440000"));
        assert!(macos_port_input_matches("0x03440000", "0X3440000"));
        assert!(!macos_port_input_matches("0x03440000", "0x03450000"));
    }

    #[test]
    fn resolve_single_auto() {
        let found = vec![adapter(BackendKind::Agilent82357b, "1-1.1")];
        let got = resolve(&found, None, &UsbSelector::Auto).unwrap();
        assert_eq!(got.port_id, "1-1.1");
    }

    #[test]
    fn resolve_multiple_auto_is_error() {
        let found = vec![
            adapter(BackendKind::Agilent82357b, "1-1.1"),
            adapter(BackendKind::Agilent82357b, "1-1.2"),
        ];
        let err = resolve(&found, None, &UsbSelector::Auto).unwrap_err();
        assert!(err.to_string().contains("--usb-port"));
    }

    #[test]
    fn resolve_port_selects_one() {
        let found = vec![
            adapter(BackendKind::Agilent82357b, "1-1.1"),
            adapter(BackendKind::Agilent82357b, "1-1.2"),
        ];
        let got = resolve(&found, None, &UsbSelector::Port("1-1.2".into())).unwrap();
        assert_eq!(got.port_id, "1-1.2");
    }

    #[test]
    fn resolve_backend_filter() {
        let found = vec![
            adapter(BackendKind::Agilent82357b, "1-1.1"),
            adapter(BackendKind::NiUsbHs, "1-1.2"),
        ];
        let got = resolve(&found, Some("ni-usb-hs"), &UsbSelector::Auto).unwrap();
        assert_eq!(got.port_id, "1-1.2");
    }

    #[test]
    fn resolve_no_match_at_port() {
        let found = vec![adapter(BackendKind::Agilent82357b, "1-1.1")];
        let err = resolve(&found, None, &UsbSelector::Port("9-9".into())).unwrap_err();
        assert!(err.to_string().contains("9-9"));
    }
}
