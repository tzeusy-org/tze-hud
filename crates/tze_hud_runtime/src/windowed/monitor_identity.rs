//! Per-monitor EDID identity, keyed by GDI display name (`\\.\DISPLAYn`).
//!
//! Retrieval only (hud-avr7s.1): key derivation and matching live elsewhere.
//! Windows uses DisplayConfig for the GDI-name join, EDID ids, friendly name and
//! connector, then SetupAPI + the device registry key for raw EDID bytes (the
//! serial is not exposed by DisplayConfig). Other platforms return an empty
//! list. Call on topology sync only, never per frame.

// Fields are consumed by the key-derivation sibling bead; non-Windows builds
// never construct them.
#![allow(dead_code)]

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

/// Log one debug line per monitor identity. Safe to call anywhere off the
/// frame path.
pub(crate) fn log_monitor_identities() {
    // No FFI work unless someone will read the line.
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    for id in monitor_identities() {
        tracing::debug!(
            gdi = %id.gdi_name,
            manufacturer = %id.manufacturer,
            product = format_args!("{:#06x}", id.product),
            serial = %id.serial,
            friendly = %id.friendly_name,
            target_id = id.connector.target_id,
            output_technology = id.connector.output_technology,
            path_instance = %id.connector.path_instance,
            "monitor EDID identity"
        );
    }
}

#[cfg(target_os = "windows")]
mod win {
    use super::{Connector, MonitorIdentity, RegRead, parse_edid, path_instance, read_growing};
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        DICS_FLAG_GLOBAL, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, DIREG_DEV, HDEVINFO,
        SP_DEVICE_INTERFACE_DATA, SP_DEVINFO_DATA, SetupDiDestroyDeviceInfoList,
        SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW, SetupDiOpenDevRegKey,
        SetupDiOpenDeviceInterfaceW,
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
            if let Some(edid) = set
                .as_ref()
                .and_then(|s| read_edid(s.0, &device_path))
                .and_then(|b| parse_edid(&b).ok())
            {
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
                },
            });
        }
        out
    }

    /// Raw EDID bytes from the monitor's device registry key.
    fn read_edid(set: HDEVINFO, device_path: &str) -> Option<Vec<u8>> {
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
        if info.DevInst == 0 {
            return None;
        }
        // SAFETY: info was filled above.
        let key: HKEY = unsafe {
            SetupDiOpenDevRegKey(set, &info, DICS_FLAG_GLOBAL.0, 0, DIREG_DEV, KEY_READ.0).ok()?
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

#[cfg(test)]
mod tests {
    use super::*;

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
