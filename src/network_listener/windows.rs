#![cfg(windows)]

// huge thx to
// https://github.com/suryatmodulus/firezone/blob/7c296494bd96c34ef1c0be75285ff92566f4c12c/rust/gui-client/src-tauri/src/client/network_changes.rs

use crate::updaters::Updater;
use crate::{abort_unreachable, dbg_println};
use std::future::Future;
use std::marker::{PhantomData, PhantomPinned};
use std::pin::Pin;
use tokio::runtime::Handle as TokioHandle;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use windows::core::Result as WinResult;
use windows::core::{implement, Interface, GUID};
use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::Networking::NetworkListManager::{
    INetworkEvents, INetworkEvents_Impl, INetworkListManager, NetworkListManager, NLM_CONNECTIVITY,
    NLM_CONNECTIVITY_IPV4_INTERNET, NLM_CONNECTIVITY_IPV6_INTERNET, NLM_NETWORK_PROPERTY_CHANGE,
};
use windows::Win32::System::Com;

#[derive(thiserror::Error, Debug)]
pub enum UpdaterError {
    #[error("Couldn't initialize COM: {0}")]
    ComInitialize(windows::core::Error),
    #[error("Couldn't create NetworkListManager")]
    CreateNetworkListManager(windows::core::Error),
    #[error("Couldn't start listening to network events: {0}")]
    Listening(windows::core::Error),
    #[error("Couldn't stop listening to network events: {0}")]
    Unadvise(windows::core::Error),
}

struct Permit<'a>(PhantomData<Pin<&'a ComGuard>>);

#[clippy::has_significant_drop]
struct ComGuard {
    _pinned: PhantomPinned,
    _unsend_unsync: PhantomData<*const ()>,
}

impl ComGuard {
    pub fn new() -> Result<Self, UpdaterError> {
        unsafe { Com::CoInitializeEx(None, Com::COINIT_MULTITHREADED) }
            .ok()
            .map_err(UpdaterError::ComInitialize)?;
        Ok(Self {
            _pinned: PhantomPinned,
            _unsend_unsync: PhantomData,
        })
    }

    #[inline(always)]
    pub const fn get_permit(self: Pin<&Self>) -> Permit<'_> {
        Permit(PhantomData)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { Com::CoUninitialize() };
    }
}

struct UpdateManager<'a> {
    advise_cookie_net: Option<u32>,
    cxn_point_net: Com::IConnectionPoint,
    _permit: Permit<'a>,
}

impl<'a> Drop for UpdateManager<'a> {
    fn drop(&mut self) {
        if let Some(cookie) = self.advise_cookie_net.take() {
            unsafe { self.cxn_point_net.Unadvise(cookie) }
                .map_err(UpdaterError::Unadvise)
                .unwrap();
        }
    }
}

macro_rules! get {
    ($x: expr) => {
        (($x)
            .as_ref()
            .unwrap_or_else(|err| abort_unreachable!("Fatal win32 api error {err}")))
    };
}

thread_local! {
    static COM_GUARD: Result<Pin<Box<ComGuard>>, UpdaterError> = ComGuard::new().map(Box::pin);

    static NETWORK_MANGER: Result<INetworkListManager, UpdaterError> = {
        COM_GUARD.with(|x| {
            let _permit = get!(x).as_ref().get_permit();
            unsafe { Com::CoCreateInstance(&NetworkListManager, None, Com::CLSCTX_ALL) }
                .map_err(UpdaterError::CreateNetworkListManager)
        })
    }
}

pub async fn has_internet() -> bool {
    fn inner() -> bool {
        NETWORK_MANGER
            .with(|x| unsafe { get!(x).IsConnectedToInternet() })
            .map_or(false, VARIANT_BOOL::as_bool)
    }

    tokio::task::spawn_blocking(inner)
        .await
        .expect("internet check blocking task panicked")
}

fn listen<F: Fn(), S: Future>(notify_callback: F, shutdown: S) -> Result<S::Output, UpdaterError> {
    COM_GUARD.with(move |com_guard| {
        let com_guard = get!(com_guard).as_ref();

        let cxn_point_net = NETWORK_MANGER.with(|network_list_manager| {
            let cpc: Com::IConnectionPointContainer = get!(network_list_manager)
                .cast()
                .map_err(UpdaterError::Listening)?;

            unsafe { cpc.FindConnectionPoint(&INetworkEvents::IID) }
                .map_err(UpdaterError::Listening)
        })?;

        let mut this = UpdateManager {
            advise_cookie_net: None,
            cxn_point_net,
            _permit: com_guard.get_permit(),
        };

        let callbacks: INetworkEvents = UpdaterInner {
            notify_callback: &notify_callback,
            _permit: com_guard.get_permit(),
        }
        .into();

        this.advise_cookie_net = Some(
            unsafe { this.cxn_point_net.Advise(&callbacks) }.map_err(UpdaterError::Listening)?,
        );

        Ok(TokioHandle::current().block_on(shutdown))
    })
}

pub fn subscribe(updater: Updater) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let local_notify = Notify::new();

        let notify_callback = || {
            dbg_println!("Network Listener: got network update!");
            if updater.update().is_err() {
                local_notify.notify_waiters()
            }
        };

        let shutdown = async {
            tokio::select! {
                _ = local_notify.notified() => (),
                _ = updater.wait_shutdown() => ()
            }
        };

        let res = listen(notify_callback, shutdown);
        updater.exit(res)
    })
}

#[implement(INetworkEvents)]
struct UpdaterInner<'a> {
    notify_callback: &'a dyn Fn(),
    _permit: Permit<'a>,
}

#[allow(non_snake_case)]
impl<'a> INetworkEvents_Impl for UpdaterInner<'a> {
    fn NetworkAdded(&self, _: &GUID) -> WinResult<()> {
        Ok(())
    }

    fn NetworkDeleted(&self, _: &GUID) -> WinResult<()> {
        Ok(())
    }

    fn NetworkConnectivityChanged(
        &self,
        _: &GUID,
        new_connectivity: NLM_CONNECTIVITY,
    ) -> WinResult<()> {
        const HAS_INTERNET_MASK: i32 =
            NLM_CONNECTIVITY_IPV4_INTERNET.0 | NLM_CONNECTIVITY_IPV6_INTERNET.0;

        let has_internet = (new_connectivity.0 & HAS_INTERNET_MASK) != 0;
        if has_internet {
            (self.notify_callback)()
        }

        Ok(())
    }

    fn NetworkPropertyChanged(&self, _: &GUID, _: NLM_NETWORK_PROPERTY_CHANGE) -> WinResult<()> {
        Ok(())
    }
}
