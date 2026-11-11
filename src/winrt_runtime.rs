//! Own the WinRT apartment of each caller and keep the process MTA alive between tasks.
//!
//! windows-rs caches agile activation factories in process-wide statics. Their lifetime must
//! not outlast the runtime apartment that made them valid: tearing down the last apartment
//! can unload the factory DLL while a later task still finds the cached interface pointer.
use std::{marker::PhantomData, rc::Rc, sync::OnceLock};
use windows::Win32::System::{
    Com::{
        CoGetApartmentType, CoIncrementMTAUsage, APTTYPE, APTTYPEQUALIFIER, APTTYPE_MAINSTA,
        APTTYPE_STA,
    },
    WinRT::{
        RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED, RO_INIT_SINGLETHREADED, RO_INIT_TYPE,
    },
};

/// A successful MTA usage cookie is intentionally retained until the OS tears down the process.
/// No Drop is used: windows-rs's static factory caches have no corresponding shutdown/clear API,
/// and detached workers may still be unwinding when the main function returns. Decrementing this
/// cookie on UI suspension, a task ending, or main's return would recreate the dangling cache.
/// This owns only runtime support, not an OCR engine, screenshot, GUI, or translation task.
struct ProcessMta {
    _cookie: usize,
}

static PROCESS_MTA: OnceLock<Result<ProcessMta, String>> = OnceLock::new();

fn keep_process_mta_alive() -> Result<(), String> {
    PROCESS_MTA
        .get_or_init(|| {
            // The documented cookie may be used from any thread. We never dereference it.
            let cookie = unsafe { CoIncrementMTAUsage() }
                .map_err(|error| format!("无法保持 Windows 运行时：{error}"))?;
            if cookie.is_invalid() {
                return Err("Windows 运行时返回了无效的保活句柄。".into());
            }
            Ok(ProcessMta {
                _cookie: cookie.0 as usize,
            })
        })
        .as_ref()
        .map(|_| ())
        .map_err(Clone::clone)
}

/// Balanced initialization on the current thread; must outlive all its WinRT objects/operations.
/// The Rc marker prevents moving a guard to another thread and uninitializing the wrong apartment.
pub struct Apartment {
    _same_thread: PhantomData<Rc<()>>,
}

impl Apartment {
    pub fn mta() -> Result<Self, String> {
        Self::new(RO_INIT_MULTITHREADED)
    }

    pub fn sta() -> Result<Self, String> {
        Self::new(RO_INIT_SINGLETHREADED)
    }

    /// Join the current UI apartment, or initialize a fresh worker as MTA.
    pub fn current() -> Result<Self, String> {
        let mut apartment = APTTYPE::default();
        let mut qualifier = APTTYPEQUALIFIER::default();
        let is_sta = unsafe { CoGetApartmentType(&mut apartment, &mut qualifier) }.is_ok()
            && matches!(apartment, APTTYPE_STA | APTTYPE_MAINSTA);
        if is_sta {
            Self::sta()
        } else {
            Self::mta()
        }
    }

    fn new(model: RO_INIT_TYPE) -> Result<Self, String> {
        // Both S_OK and S_FALSE are success, and require a matching RoUninitialize.
        unsafe { RoInitialize(model) }
            .map_err(|error| format!("无法初始化 Windows 运行时：{error}"))?;
        let apartment = Self {
            _same_thread: PhantomData,
        };
        // Establish keep-alive BEFORE any cached activation factory can be acquired or used.
        // If this fails, the local guard still balances the successful initialization.
        keep_process_mta_alive()?;
        Ok(apartment)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}
