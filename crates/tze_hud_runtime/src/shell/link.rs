//! Owner-pressed notification links. Call only after all scene/state guards drop.
#[cfg(test)]
type TestLinkWorker = std::sync::Arc<dyn Fn(String) -> Result<(), String> + Send + Sync>;

#[derive(Default)]
pub(crate) struct LinkOpener {
    #[cfg(test)]
    pub(crate) worker: Option<TestLinkWorker>,
    #[cfg(test)]
    pub(crate) workers: Vec<std::thread::JoinHandle<()>>,
    #[cfg(test)]
    pub(crate) fail_spawn: bool,
}

impl LinkOpener {
    pub(crate) fn launch(&mut self, url: String) {
        #[cfg(test)]
        if self.fail_spawn {
            tracing::warn!("notification link worker spawn failed (injected)");
            return;
        }
        #[cfg(test)]
        let worker = self.worker.clone();
        let spawned = std::thread::Builder::new()
            .name("notification-link".into())
            .spawn(move || {
                #[cfg(test)]
                let result = worker.map_or_else(|| platform_open(&url), |open| open(url.clone()));
                #[cfg(not(test))]
                let result = platform_open(&url);
                if let Err(error) = result {
                    // Never include the target: URLs may contain query credentials.
                    tracing::warn!(%error, "notification browser handoff failed");
                }
            });
        match spawned {
            #[cfg(test)]
            Ok(worker) => self.workers.push(worker),
            #[cfg(not(test))]
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "notification link worker spawn failed"),
        }
    }
}

#[cfg(not(windows))]
fn platform_open(_url: &str) -> Result<(), String> {
    tracing::info!("notification browser handoff is supported on Windows only");
    Ok(())
}

#[cfg(windows)]
fn platform_open(url: &str) -> Result<(), String> {
    use windows::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
    };
    use windows::Win32::UI::Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{PCWSTR, w};
    struct Com;
    impl Drop for Com {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }
    // SAFETY: this dedicated worker has no pre-existing apartment. Its successful
    // initialization is balanced even if ShellExecuteExW fails.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).ok() }
        .map_err(|e| format!("COM initialization: {e}"))?;
    let _com = Com;
    let encoded: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
    let mut request = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC,
        lpVerb: w!("open"),
        lpFile: PCWSTR(encoded.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: the owned encoded URL lives through this call. No arguments or
    // shell command are constructed; any blocking shell work stays on the worker.
    unsafe { ShellExecuteExW(&mut request) }.map_err(|e| format!("ShellExecuteExW open: {e}"))
}
