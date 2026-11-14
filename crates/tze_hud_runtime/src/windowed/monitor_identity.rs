//! Per-monitor EDID identity, keyed by GDI display name (`\\.\DISPLAYn`), and
//! the `[displays."<key>"]` config key derived from it.
//!
//! Windows uses DisplayConfig for the GDI-name join, EDID ids, friendly name and
//! connector, then SetupAPI for the monitor's device instance id and, from its
//! device registry key, the raw EDID bytes (the serial is not exposed by
//! DisplayConfig). Other platforms return an empty list. Call on topology sync
//! only, never per frame. [`display_keys`] is pure.

// Non-Windows builds only construct identities in tests.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

/// Stable-across-reboot connector facts. The adapter LUID is deliberately
/// absent: it is regenerated every boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Connector {
    /// `DISPLAYCONFIG_PATH_TARGET_INFO::id` (adapter-local target id).
    pub target_id: u32,
    /// `DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY` raw value (HDMI, DP, ...).
    pub output_technology: i32,
    /// Instance segment of the monitor device path (e.g. `UID4352`).
    pub path_instance: String,
    /// Full PnP device instance id of the monitor
    /// (`DISPLAY\DEL41A2\5&1A2B3C&0&UID4352`): unique on the system and
    /// persistent across restarts. `None` when SetupAPI cannot report it.
    pub device_instance_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MonitorIdentity {
    /// `\\.\DISPLAYn`, joins winit's `MonitorHandle::name()`.
    pub gdi_name: String,
    /// 3-letter PNP manufacturer id (from EDID when available).
    pub manufacturer: String,
    pub product: u16,
    /// May be blank.
    pub serial: String,
    pub friendly_name: String,
    pub connector: Connector,
}

/// Parsed fields of a 128-byte EDID base block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Edid {
    pub manufacturer: String,
    pub product: u16,
    /// Descriptor string if present, else the numeric serial, else blank.
    pub serial: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EdidError {
    TooShort,
    BadHeader,
    BadChecksum,
}

const EDID_HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

/// Parse the EDID base block: validates header and checksum, then extracts the
/// manufacturer, product code and serial.
pub(crate) fn parse_edid(bytes: &[u8]) -> Result<Edid, EdidError> {
    let block = bytes.get(..128).ok_or(EdidError::TooShort)?;
    if block[..8] != EDID_HEADER {
        return Err(EdidError::BadHeader);
    }
    if block.iter().fold(0u8, |a, b| a.wrapping_add(*b)) != 0 {
        return Err(EdidError::BadChecksum);
    }
    let id = u16::from_be_bytes([block[8], block[9]]);
    let manufacturer: String = [10u32, 5, 0]
        .iter()
        .map(|shift| char::from(b'A' - 1 + ((id >> shift) & 0x1F) as u8))
        .collect();
    let product = u16::from_le_bytes([block[10], block[11]]);

    // Four 18-byte descriptors at 54..126; tag 0xFF is the serial string.
    let descriptor_serial = (0..4)
        .map(|i| &block[54 + 18 * i..72 + 18 * i])
        .find_map(|d| {
            if d[..3] != [0, 0, 0] || d[3] != 0xFF {
                return None;
            }
            let text = d[5..18].split(|b| *b == 0x0A).next().unwrap_or_default();
            let text = String::from_utf8_lossy(text).trim().to_string();
            (!text.is_empty()).then_some(text)
        });
    let serial = descriptor_serial.unwrap_or_else(|| {
        match u32::from_le_bytes([block[12], block[13], block[14], block[15]]) {
            0 => String::new(),
            n => n.to_string(),
        }
    });
    Ok(Edid {
        manufacturer,
        product,
        serial,
    })
}

/// Instance segment of a monitor device path:
/// `\\?\DISPLAY#DEL41A2#5&1a2b3c&0&UID4352#{guid}` -> `UID4352`.
fn path_instance(device_path: &str) -> String {
    device_path
        .split('#')
        .nth(2)
        .and_then(|s| s.rsplit('&').next())
        .unwrap_or_default()
        .to_string()
}

/// One connected monitor's `[displays."<key>"]` key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DisplayKey {
    /// What the config names this monitor by, uppercase.
    pub key: String,
    /// Model+serial key without a connector suffix, when the monitor has a
    /// usable EDID identity (even if `key` had to fall back to the OS name).
    pub base: Option<String>,
    /// False when `key` is the OS display name because no stable identity is
    /// available; the runtime warns.
    pub stable: bool,
}

/// `<MFG><PRODUCT as 4 hex>[-<SERIAL>]`, or `None` without a valid PNP id.
fn base_key(id: &MonitorIdentity) -> Option<String> {
    let mfg = &id.manufacturer;
    if mfg.len() != 3 || !mfg.bytes().all(|b| b.is_ascii_uppercase()) {
        return None;
    }
    let serial: String = id
        .serial
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    let mut key = format!("{mfg}{:04X}", id.product);
    if !serial.is_empty() {
        key.push('-');
        key.push_str(&serial);
    }
    Some(key)
}

/// Connector suffix from the PnP device instance id, injective and readable
/// in a quoted TOML key: `\` becomes `/`, `A-Z 0-9 . _ & -` stay, every
/// other byte is `%XX`. Instance ids are case-insensitive, so it is uppercase.
fn connector_suffix(device_instance_id: &str) -> String {
    let mut out = String::new();
    for b in device_instance_id.trim().to_ascii_uppercase().bytes() {
        match b {
            b'\\' => out.push('/'),
            b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'&' | b'-' => out.push(char::from(b)),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Config key of each monitor, in input order. Each entry is the monitor's
/// normalized OS name and its EDID identity, if one joined uniquely.
///
/// A unique model+serial is the key. Monitors sharing one (identical
/// model+serial, or same model with blank serials) all get
/// `@<connector>` from their device instance id. A monitor without usable
/// identity, or in a shared set without an instance id, keys by its OS name.
pub(crate) fn display_keys(monitors: &[(&str, Option<&MonitorIdentity>)]) -> Vec<DisplayKey> {
    let bases: Vec<Option<String>> = monitors
        .iter()
        .map(|(_, id)| id.and_then(base_key))
        .collect();
    let mut keys: Vec<DisplayKey> = monitors
        .iter()
        .zip(&bases)
        .map(|((name, id), base)| {
            let fallback = || DisplayKey {
                key: name.to_ascii_uppercase(),
                base: base.clone(),
                stable: false,
            };
            let Some(b) = base else {
                return fallback();
            };
            if bases.iter().filter(|o| o.as_ref() == Some(b)).count() == 1 {
                return DisplayKey {
                    key: b.clone(),
                    base: Some(b.clone()),
                    stable: true,
                };
            }
            match id.and_then(|i| i.connector.device_instance_id.as_deref()) {
                Some(instance) if !instance.trim().is_empty() => DisplayKey {
                    key: format!("{b}@{}", connector_suffix(instance)),
                    base: Some(b.clone()),
                    stable: true,
                },
                _ => fallback(),
            }
        })
        .collect();
    // Two monitors must never share a key (e.g. one instance id reported
    // twice): those fall back to their distinct OS names.
    let snapshot: Vec<String> = keys.iter().map(|k| k.key.clone()).collect();
    for (i, k) in keys.iter_mut().enumerate() {
        if k.stable && snapshot.iter().filter(|o| **o == k.key).count() > 1 {
            k.key = monitors[i].0.to_ascii_uppercase();
            k.stable = false;
        }
    }
    keys
}

/// Identities of all active monitors. Empty off Windows or on API failure.
pub(crate) fn monitor_identities() -> Vec<MonitorIdentity> {
    #[cfg(target_os = "windows")]
    {
        win::monitor_identities()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Vec::new()
    }
}

/// Outcome of one registry read attempt into a fixed buffer.
pub(crate) enum RegRead {
    /// Read succeeded; this many bytes are valid.
    Done(usize),
    /// Buffer too small (`ERROR_MORE_DATA`); the value needs this many bytes.
    NeedBytes(usize),
    Failed,
}

/// Run `read` with a buffer, growing it to the size the registry asks for
/// (EDIDs with extension blocks exceed 256 bytes).
pub(crate) fn read_growing(mut read: impl FnMut(&mut [u8]) -> RegRead) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 256];
    for _ in 0..3 {
        match read(&mut buf) {
            RegRead::Done(n) => {
                buf.truncate(n);
                return Some(buf);
            }
            RegRead::NeedBytes(n) if n > buf.len() => buf.resize(n, 0),
            RegRead::NeedBytes(_) | RegRead::Failed => return None,
        }
    }
    None
}

#[cfg(target_os = "windows")]
mod win {
    use super::{Connector, MonitorIdentity, RegRead, parse_edid, path_instance, read_growing};
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        DICS_FLAG_GLOBAL, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, DIREG_DEV, HDEVINFO,
        MAX_DEVICE_ID_LEN, SP_DEVICE_INTERFACE_DATA, SP_DEVINFO_DATA, SetupDiDestroyDeviceInfoList,
        SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceInterfaceDetailW,
        SetupDiOpenDevRegKey, SetupDiOpenDeviceInterfaceW,
    };
    use windows::Win32::Devices::Display::{
        DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
        DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
        DISPLAYCONFIG_TARGET_DEVICE_NAME, DisplayConfigGetDeviceInfo, GUID_DEVINTERFACE_MONITOR,
        GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
    };
    use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{HKEY, KEY_READ, RegCloseKey, RegQueryValueExW};
    use windows::core::{PCWSTR, w};

    /// `DISPLAYCONFIG_TARGET_DEVICE_NAME_FLAGS` bit 2.
    const EDID_IDS_VALID: u32 = 0x4;

    fn wide_to_string(w: &[u16]) -> String {
        let end = w.iter().position(|c| *c == 0).unwrap_or(w.len());
        String::from_utf16_lossy(&w[..end])
    }

    /// Closes the SetupAPI device info set on drop.
    struct DevInfoSet(HDEVINFO);
    impl Drop for DevInfoSet {
        fn drop(&mut self) {
            // SAFETY: handle came from SetupDiGetClassDevsW and is closed once.
            unsafe {
                let _ = SetupDiDestroyDeviceInfoList(self.0);
            }
        }
    }

    pub(super) fn monitor_identities() -> Vec<MonitorIdentity> {
        let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> = Vec::new();
        let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> = Vec::new();
        // The topology can change between sizing and querying; retry briefly.
        for _ in 0..3 {
            let (mut np, mut nm) = (0u32, 0u32);
            // SAFETY: out-pointers reference live locals.
            if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) }
                != ERROR_SUCCESS
            {
                return Vec::new();
            }
            paths.resize(np as usize, DISPLAYCONFIG_PATH_INFO::default());
            modes.resize(nm as usize, DISPLAYCONFIG_MODE_INFO::default());
            // SAFETY: buffers hold np/nm elements, counts passed by pointer.
            let rc = unsafe {
                QueryDisplayConfig(
                    QDC_ONLY_ACTIVE_PATHS,
                    &mut np,
                    paths.as_mut_ptr(),
                    &mut nm,
                    modes.as_mut_ptr(),
                    None,
                )
            };
            if rc == ERROR_SUCCESS {
                paths.truncate(np as usize);
                break;
            }
            paths.clear();
        }

        // SAFETY: valid class GUID; no enumerator or parent window.
        let set = unsafe {
            SetupDiGetClassDevsW(
                Some(&GUID_DEVINTERFACE_MONITOR),
                PCWSTR::null(),
                None,
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
        }
        .ok()
        .map(DevInfoSet);

        let mut out = Vec::new();
        for path in &paths {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            src.header.adapterId = path.sourceInfo.adapterId;
            src.header.id = path.sourceInfo.id;
            // SAFETY: header.size matches the packet type.
            if unsafe { DisplayConfigGetDeviceInfo(&mut src.header) } != 0 {
                continue;
            }

            let mut tgt = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            tgt.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            tgt.header.size = size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            tgt.header.adapterId = path.targetInfo.adapterId;
            tgt.header.id = path.targetInfo.id;
            // SAFETY: header.size matches the packet type.
            if unsafe { DisplayConfigGetDeviceInfo(&mut tgt.header) } != 0 {
                continue;
            }

            let device_path_w = tgt.monitorDevicePath;
            let device_path = wide_to_string(&device_path_w);
            // SAFETY: reading the plain-u32 view of the flags union.
            let ids_valid = unsafe { tgt.flags.Anonymous.value } & EDID_IDS_VALID != 0;
            let mut manufacturer = String::new();
            let mut product = 0u16;
            if ids_valid {
                let m = tgt.edidManufactureId.swap_bytes();
                manufacturer = [10u32, 5, 0]
                    .iter()
                    .map(|s| char::from(b'A' - 1 + ((m >> s) & 0x1F) as u8))
                    .collect();
                product = tgt.edidProductCodeId;
            }

            let mut serial = String::new();
            let device = set
                .as_ref()
                .map(|s| read_device(s.0, &device_path))
                .unwrap_or_default();
            if let Some(edid) = device.edid.and_then(|b| parse_edid(&b).ok()) {
                manufacturer = edid.manufacturer;
                product = edid.product;
                serial = edid.serial;
            }

            out.push(MonitorIdentity {
                gdi_name: wide_to_string(&src.viewGdiDeviceName),
                manufacturer,
                product,
                serial,
                friendly_name: wide_to_string(&tgt.monitorFriendlyDeviceName),
                connector: Connector {
                    target_id: path.targetInfo.id,
                    output_technology: tgt.outputTechnology.0,
                    path_instance: path_instance(&device_path),
                    device_instance_id: device.instance_id,
                },
            });
        }
        out
    }

    /// What SetupAPI reports for one monitor device; each part fails alone.
    #[derive(Default)]
    struct DeviceFacts {
        instance_id: Option<String>,
        edid: Option<Vec<u8>>,
    }

    /// The monitor's device instance id and raw EDID bytes (from its device
    /// registry key), both found through its interface path.
    fn read_device(set: HDEVINFO, device_path: &str) -> DeviceFacts {
        let Some(info) = device_info(set, device_path) else {
            return DeviceFacts::default();
        };
        DeviceFacts {
            instance_id: instance_id(set, &info),
            edid: read_edid(set, &info),
        }
    }

    fn device_info(set: HDEVINFO, device_path: &str) -> Option<SP_DEVINFO_DATA> {
        let wpath: Vec<u16> = device_path.encode_utf16().chain([0]).collect();
        let mut iface = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        // SAFETY: wpath is NUL-terminated; iface is a sized out-struct.
        unsafe {
            SetupDiOpenDeviceInterfaceW(set, PCWSTR(wpath.as_ptr()), 0, Some(&mut iface)).ok()?;
        }
        let mut info = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        // Sizing-style call: fails with ERROR_INSUFFICIENT_BUFFER but still
        // fills `info`, which is all we need here.
        // SAFETY: iface/info are sized structs; no detail buffer is passed.
        unsafe {
            let _ = SetupDiGetDeviceInterfaceDetailW(set, &iface, None, 0, None, Some(&mut info));
        }
        (info.DevInst != 0).then_some(info)
    }

    /// Full PnP device instance id; `None` on failure or a malformed reply.
    fn instance_id(set: HDEVINFO, info: &SP_DEVINFO_DATA) -> Option<String> {
        let mut buf = [0u16; MAX_DEVICE_ID_LEN as usize];
        let mut required = 0u32;
        // SAFETY: info was filled by device_info; buf/required are live locals.
        unsafe { SetupDiGetDeviceInstanceIdW(set, info, Some(&mut buf), Some(&mut required)) }
            .ok()?;
        let end = buf.iter().position(|c| *c == 0)?;
        if end == 0 || required as usize > buf.len() {
            return None;
        }
        String::from_utf16(&buf[..end]).ok()
    }

    /// Raw EDID bytes from the monitor's device registry key.
    fn read_edid(set: HDEVINFO, info: &SP_DEVINFO_DATA) -> Option<Vec<u8>> {
        // SAFETY: info was filled by device_info.
        let key: HKEY = unsafe {
            SetupDiOpenDevRegKey(set, info, DICS_FLAG_GLOBAL.0, 0, DIREG_DEV, KEY_READ.0).ok()?
        };
        let value = read_growing(|buf| {
            let mut len = buf.len() as u32;
            // SAFETY: buf/len describe a writable buffer; key is open.
            let rc = unsafe {
                RegQueryValueExW(
                    key,
                    w!("EDID"),
                    None,
                    None,
                    Some(buf.as_mut_ptr()),
                    Some(&mut len),
                )
            };
            if rc == ERROR_SUCCESS {
                RegRead::Done(len as usize)
            } else if rc == ERROR_MORE_DATA {
                RegRead::NeedBytes(len as usize)
            } else {
                RegRead::Failed
            }
        });
        // SAFETY: key was opened above and is closed once.
        unsafe {
            let _ = RegCloseKey(key);
        }
        value
    }
}

/// A monitor identity for tests: `gdi` is the GDI name, `instance` the
/// device instance id.
#[cfg(test)]
pub(crate) fn test_identity(
    gdi: &str,
    manufacturer: &str,
    product: u16,
    serial: &str,
    instance: Option<&str>,
) -> MonitorIdentity {
    MonitorIdentity {
        gdi_name: gdi.into(),
        manufacturer: manufacturer.into(),
        product,
        serial: serial.into(),
        friendly_name: String::new(),
        connector: Connector {
            target_id: 0,
            output_technology: 0,
            path_instance: String::new(),
            device_instance_id: instance.map(Into::into),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One table for every key case: unique monitors, blank serial,
    /// identical model+serial pair, blank-serial pair, sanitization,
    /// unavailable identity, and a shared key without an instance id.
    #[test]
    fn display_keys_cover_every_identity_case() {
        let id = test_identity;
        let pair_a = id(
            "A",
            "DEL",
            0x41A2,
            "CN0ABC",
            Some(r"DISPLAY\DEL41A2\5&1a2b&0&UID4352"),
        );
        let pair_b = id(
            "B",
            "DEL",
            0x41A2,
            "CN0ABC",
            Some(r"DISPLAY\DEL41A2\5&1a2b&0&UID4353"),
        );
        let blank_a = id("A", "GSM", 0x5B7F, "", Some(r"DISPLAY\GSM5B7F\1&0&UID1"));
        let blank_b = id("B", "GSM", 0x5B7F, " ", Some(r"DISPLAY\GSM5B7F\1&0 UID2"));
        let no_instance = id("B", "DEL", 0x41A2, "CN0ABC", None);
        let unique = id("A", "DEL", 0x41A2, "CN0ABC", None);
        let other = id("B", "ACI", 0x27A1, "M1LMQS0", None);
        let blank = id("A", "SAM", 0x0F00, "", None);
        let messy = id("A", "DEL", 0x41A2, "ab/c d\"é", None);
        let bad_mfg = id("A", "", 0, "", None);
        let stable = |key: &str, base: &str| DisplayKey {
            key: key.into(),
            base: Some(base.into()),
            stable: true,
        };
        let os = |key: &str, base: Option<&str>| DisplayKey {
            key: key.into(),
            base: base.map(Into::into),
            stable: false,
        };
        type Monitors<'a> = Vec<(&'a str, Option<&'a MonitorIdentity>)>;
        let cases: Vec<(&str, Monitors, Vec<DisplayKey>)> = vec![
            (
                "unique monitors",
                vec![("DISPLAY1", Some(&unique)), ("DISPLAY2", Some(&other))],
                vec![
                    stable("DEL41A2-CN0ABC", "DEL41A2-CN0ABC"),
                    stable("ACI27A1-M1LMQS0", "ACI27A1-M1LMQS0"),
                ],
            ),
            (
                "blank serial",
                vec![("DISPLAY1", Some(&blank))],
                vec![stable("SAM0F00", "SAM0F00")],
            ),
            (
                "identical model+serial pair",
                vec![("DISPLAY1", Some(&pair_a)), ("DISPLAY2", Some(&pair_b))],
                vec![
                    stable(
                        "DEL41A2-CN0ABC@DISPLAY/DEL41A2/5&1A2B&0&UID4352",
                        "DEL41A2-CN0ABC",
                    ),
                    stable(
                        "DEL41A2-CN0ABC@DISPLAY/DEL41A2/5&1A2B&0&UID4353",
                        "DEL41A2-CN0ABC",
                    ),
                ],
            ),
            (
                "blank-serial pair; other bytes are %XX",
                vec![("DISPLAY1", Some(&blank_a)), ("DISPLAY2", Some(&blank_b))],
                vec![
                    stable("GSM5B7F@DISPLAY/GSM5B7F/1&0&UID1", "GSM5B7F"),
                    stable("GSM5B7F@DISPLAY/GSM5B7F/1&0%20UID2", "GSM5B7F"),
                ],
            ),
            (
                "serial sanitized to a bare key",
                vec![("DISPLAY1", Some(&messy))],
                vec![stable("DEL41A2-AB_C_D__", "DEL41A2-AB_C_D__")],
            ),
            (
                "unavailable identity keys by OS name",
                vec![("DISPLAY1", None), ("display2", Some(&bad_mfg))],
                vec![os("DISPLAY1", None), os("DISPLAY2", None)],
            ),
            (
                "a shared key without an instance id falls back; its base stays ambiguous",
                vec![
                    ("DISPLAY1", Some(&pair_a)),
                    ("DISPLAY2", Some(&no_instance)),
                ],
                vec![
                    stable(
                        "DEL41A2-CN0ABC@DISPLAY/DEL41A2/5&1A2B&0&UID4352",
                        "DEL41A2-CN0ABC",
                    ),
                    os("DISPLAY2", Some("DEL41A2-CN0ABC")),
                ],
            ),
            (
                "one instance id reported twice: both fall back",
                vec![("DISPLAY1", Some(&pair_a)), ("DISPLAY2", Some(&pair_a))],
                vec![
                    os("DISPLAY1", Some("DEL41A2-CN0ABC")),
                    os("DISPLAY2", Some("DEL41A2-CN0ABC")),
                ],
            ),
        ];
        for (case, monitors, want) in cases {
            assert_eq!(display_keys(&monitors), want, "{case}");
        }
    }

    /// Build a valid 128-byte EDID block with a fixed-up checksum.
    fn edid(serial_u32: u32, descriptor_serial: Option<&[u8]>) -> Vec<u8> {
        let mut b = vec![0u8; 128];
        b[..8].copy_from_slice(&EDID_HEADER);
        // "DEL" = 4,5,12 -> 0b0_00100_00101_01100
        b[8..10].copy_from_slice(&0x10ACu16.to_be_bytes());
        b[10..12].copy_from_slice(&0x41A2u16.to_le_bytes());
        b[12..16].copy_from_slice(&serial_u32.to_le_bytes());
        if let Some(s) = descriptor_serial {
            let d = &mut b[72..90];
            d[3] = 0xFF;
            d[5..5 + s.len()].copy_from_slice(s);
            d[5 + s.len()] = 0x0A;
            d[5 + s.len() + 1..18].fill(b' ');
        }
        let sum = b[..127].iter().fold(0u8, |a, x| a.wrapping_add(*x));
        b[127] = 0u8.wrapping_sub(sum);
        b
    }

    #[test]
    fn serial_precedence_descriptor_numeric_blank() {
        let d = parse_edid(&edid(77, Some(b"CN0ABC123"))).unwrap();
        assert_eq!(
            (d.manufacturer.as_str(), d.product, d.serial.as_str()),
            ("DEL", 0x41A2, "CN0ABC123")
        );
        assert_eq!(
            parse_edid(&edid(16843009, None)).unwrap().serial,
            "16843009"
        );
        assert_eq!(parse_edid(&edid(0, None)).unwrap().serial, "");
    }

    #[test]
    fn rejects_malformed_blocks() {
        let mut bad_sum = edid(1, None);
        bad_sum[127] ^= 1;
        assert_eq!(parse_edid(&bad_sum), Err(EdidError::BadChecksum));
        let mut bad_header = edid(1, None);
        bad_header[1] = 0;
        assert_eq!(parse_edid(&bad_header), Err(EdidError::BadHeader));
        assert_eq!(parse_edid(&[0; 64]), Err(EdidError::TooShort));
    }

    #[test]
    fn read_growing_retries_with_the_size_the_registry_asks_for() {
        let value: Vec<u8> = (0..384u32).map(|i| i as u8).collect();
        let mut calls = 0;
        let got = read_growing(|buf| {
            calls += 1;
            if buf.len() < value.len() {
                return RegRead::NeedBytes(value.len());
            }
            buf[..value.len()].copy_from_slice(&value);
            RegRead::Done(value.len())
        });
        assert_eq!((got, calls), (Some(value), 2));
        assert_eq!(read_growing(|_| RegRead::Failed), None);
        // A registry that never settles does not loop forever.
        assert_eq!(read_growing(|b| RegRead::NeedBytes(b.len() + 1)), None);
    }

    #[test]
    fn path_instance_extracts_uid_segment() {
        let p = r"\\?\DISPLAY#DEL41A2#5&1a2b3c&0&UID4352#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
        assert_eq!(path_instance(p), "UID4352");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn non_windows_is_empty() {
        assert!(monitor_identities().is_empty());
    }
}
