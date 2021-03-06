// Manages the state shared by GeckoBridge instances and exposes
// an api usable by other services.

use crate::generated::common::{
    AppsServiceDelegateProxy, CardInfoType, MobileManagerDelegateProxy, NetworkInfo,
    NetworkManagerDelegateProxy, NetworkOperator, ObjectRef, PowerManagerDelegateProxy,
    PreferenceDelegateProxy, WakelockProxy,
};
use crate::generated::service::{GeckoBridgeProxy, GeckoBridgeProxyTracker};
use crate::service::PROXY_TRACKER;
use common::tokens::SharedTokensManager;
use common::traits::{OriginAttributes, Shared};
use common::JsonValue;
use log::{debug, error};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fmt;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug)]
pub struct BridgeError;

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GeckoBridge Error")
    }
}

impl std::error::Error for BridgeError {}

#[derive(Error, Debug)]
pub enum DelegateError {
    #[error("Report errors from web runtime")]
    InvalidWebRuntimeService,
    #[error("Receive receiver error")]
    InvalidChannel,
    #[error("Failed to get delegate manager")]
    InvalidDelegator,
    #[error("Invalid wakelock")]
    InvalidWakelock,
}

pub enum PrefValue {
    Str(String),
    Int(i64),
    Bool(bool),
}

lazy_static! {
    pub(crate) static ref GECKO_BRIDGE_SHARED_STATE: Shared<GeckoBridgeState> =
        Shared::adopt(GeckoBridgeState::default());
}

#[derive(Default)]
pub struct GeckoBridgeState {
    prefs: HashMap<String, PrefValue>,
    appsservice: Option<AppsServiceDelegateProxy>,
    powermanager: Option<PowerManagerDelegateProxy>,
    preference: Option<PreferenceDelegateProxy>,
    mobilemanager: Option<MobileManagerDelegateProxy>,
    networkmanager: Option<NetworkManagerDelegateProxy>,
    observers: Vec<Sender<()>>,
    tokens: SharedTokensManager,
}

impl GeckoBridgeState {
    fn proxy_tracker(&mut self) -> Arc<Mutex<GeckoBridgeProxyTracker>> {
        let a = &*PROXY_TRACKER;
        a.clone()
    }

    /// Reset the state, making it possible to set new delegates.
    pub fn reset(&mut self) {
        self.prefs = HashMap::new();
        self.powermanager = None;
        self.preference = None;
        self.appsservice = None;
        self.mobilemanager = None;
        self.networkmanager = None;
        // Reset the proxy tracker content, which only holds proxy objects for the
        // delegates.
        let tracker = self.proxy_tracker();
        tracker.lock().clear();

        // On session dropped, do no reset the observers.
    }

    /// Delegates that are common to device and desktop builds.
    pub fn common_delegates_ready(&self) -> bool {
        self.appsservice.is_some() && self.powermanager.is_some() && self.preference.is_some()
    }

    /// Delegates that are only available on device builds.
    pub fn device_delegates_ready(&self) -> bool {
        self.mobilemanager.is_some() && self.networkmanager.is_some()
    }

    /// true if all the expected delegates have been set.
    #[cfg(target_os = "android")]
    pub fn is_ready(&self) -> bool {
        self.common_delegates_ready() && self.device_delegates_ready()
    }

    /// true if all the expected delegates have been set.
    #[cfg(not(target_os = "android"))]
    pub fn is_ready(&self) -> bool {
        self.common_delegates_ready()
    }

    fn notify_readyness_observers(&mut self) {
        if !self.is_ready() {
            return;
        }
        for sender in &self.observers {
            let _ = sender.send(());
        }
    }

    // Return a 'Receiver' to receivce the update when all delegates are ready;
    pub fn observe_bridge(&mut self) -> Receiver<()> {
        let (sender, receiver) = channel();
        {
            self.observers.push(sender);
        }
        receiver
    }

    // Preferences related methods.
    pub fn set_bool_pref(&mut self, name: String, value: bool) {
        let _ = self.prefs.insert(name, PrefValue::Bool(value));
    }

    pub fn get_bool_pref(&self, name: &str) -> Option<bool> {
        match self.prefs.get(name) {
            Some(PrefValue::Bool(value)) => Some(*value),
            _ => None,
        }
    }

    pub fn set_int_pref(&mut self, name: String, value: i64) {
        let _ = self.prefs.insert(name, PrefValue::Int(value));
    }

    pub fn get_int_pref(&self, name: &str) -> Option<i64> {
        match self.prefs.get(name) {
            Some(PrefValue::Int(value)) => Some(*value),
            _ => None,
        }
    }

    pub fn set_char_pref(&mut self, name: String, value: String) {
        let _ = self.prefs.insert(name, PrefValue::Str(value));
    }

    pub fn get_char_pref(&self, name: &str) -> Option<String> {
        match self.prefs.get(name) {
            Some(PrefValue::Str(value)) => Some(value.clone()),
            _ => None,
        }
    }

    pub fn get_pref(&self, name: &str) -> Option<PrefValue> {
        match self.prefs.get(name) {
            Some(PrefValue::Bool(value)) => Some(PrefValue::Bool(*value)),
            Some(PrefValue::Int(value)) => Some(PrefValue::Int(*value)),
            Some(PrefValue::Str(value)) => Some(PrefValue::Str(value.clone())),
            _ => None,
        }
    }

    // Power manager delegate management.
    pub fn set_powermanager_delegate(&mut self, delegate: PowerManagerDelegateProxy) {
        self.powermanager = Some(delegate);
        self.notify_readyness_observers();
    }

    pub fn powermanager_set_screen_enabled(
        &mut self,
        value: bool,
        is_external_screen: bool,
    ) -> Result<(), BridgeError> {
        if let Some(powermanager) = &mut self.powermanager {
            let rx = powermanager.set_screen_enabled(value, is_external_screen);
            if let Ok(result) = rx.recv() {
                result.map_err(|_| BridgeError)
            } else {
                error!("Failed to set screen : invalid delegate channel.");
                Err(BridgeError)
            }
        } else {
            error!("The powermanager delegate is not set!");
            Err(BridgeError)
        }
    }

    pub fn powermanager_request_wakelock(
        &mut self,
        topic: String,
    ) -> Result<ObjectRef, DelegateError> {
        if let Some(powermanager) = &mut self.powermanager {
            let rx = powermanager.request_wakelock(topic);
            if let Ok(result) = rx.recv() {
                match result {
                    Ok(obj_ref) => {
                        if let Some(GeckoBridgeProxy::Wakelock(_proxy)) =
                            self.proxy_tracker().lock().get(&obj_ref)
                        {
                            debug!("Request the wakelock successfully.");
                            Ok(obj_ref)
                        } else {
                            error!("Failed to get wakelock: no proxy object.");
                            Err(DelegateError::InvalidWakelock)
                        }
                    }
                    Err(_) => {
                        error!("Failed to request wake lock, invalid object reference.");
                        Err(DelegateError::InvalidWakelock)
                    }
                }
            } else {
                error!("Failed to get the wakelock: invalid delegate channel.");
                Err(DelegateError::InvalidChannel)
            }
        } else {
            error!("Failed to get the wakelock: powermanager delegate is not set!");
            Err(DelegateError::InvalidDelegator)
        }
    }

    fn get_wakelock_proxy(&mut self, wakelock: ObjectRef) -> Result<WakelockProxy, DelegateError> {
        match self.proxy_tracker().lock().get(&wakelock) {
            Some(GeckoBridgeProxy::Wakelock(proxy)) => Ok(proxy.clone()),
            _ => Err(DelegateError::InvalidWakelock),
        }
    }

    pub fn powermanager_wakelock_get_topic(
        &mut self,
        wakelock: ObjectRef,
    ) -> Result<String, DelegateError> {
        if let Ok(mut proxy) = self.get_wakelock_proxy(wakelock) {
            let rx = proxy.get_topic();
            if let Ok(result) = rx.recv() {
                match result {
                    Ok(topic) => {
                        debug!("powermanager_wakelock_get_topic: {}.", topic);
                        Ok(topic)
                    }
                    Err(_) => {
                        error!("powermanager_wakelock_get_topic: invalid wakelock.");
                        Err(DelegateError::InvalidWakelock)
                    }
                }
            } else {
                error!("powermanager_wakelock_get_topic: invalid channel.");
                Err(DelegateError::InvalidChannel)
            }
        } else {
            error!("powermanager_wakelock_get_topic: invalid wakelock proxy.");
            Err(DelegateError::InvalidWakelock)
        }
    }

    pub fn powermanager_wakelock_unlock(
        &mut self,
        wakelock: ObjectRef,
    ) -> Result<(), DelegateError> {
        let mut proxy = self.get_wakelock_proxy(wakelock)?;
        let rx = proxy.unlock();
        if let Ok(result) = rx.recv() {
            match result {
                Ok(()) => {
                    debug!("powermanager_wakelock_unlock: successful.");
                    Ok(())
                }
                Err(_) => {
                    error!("powermanager_wakelock_unlock: invalid channel.");
                    Err(DelegateError::InvalidChannel)
                }
            }
        } else {
            error!("powermanager_wakelock_unlock: invalid channel.");
            Err(DelegateError::InvalidChannel)
        }
    }

    // Apps service delegate management.
    pub fn is_apps_service_ready(&self) -> bool {
        self.appsservice.is_some()
    }

    pub fn set_apps_service_delegate(&mut self, delegate: AppsServiceDelegateProxy) {
        self.appsservice = Some(delegate);
        self.notify_readyness_observers();
    }

    pub fn apps_service_on_clear(
        &mut self,
        manifest_url: String,
        data_type: String,
    ) -> Result<(), DelegateError> {
        debug!("apps_service_on_clear: {}", &manifest_url);
        if let Some(service) = &mut self.appsservice {
            let rx = service.on_clear(manifest_url, data_type);
            if let Ok(result) = rx.recv() {
                match result {
                    Ok(_) => Ok(()),
                    Err(_) => Err(DelegateError::InvalidWebRuntimeService),
                }
            } else {
                error!("The apps service delegate rx channel error!");
                Err(DelegateError::InvalidChannel)
            }
        } else {
            error!("The apps service delegate is not set!");
            Err(DelegateError::InvalidDelegator)
        }
    }

    pub fn apps_service_on_boot(&mut self, manifest_url: String, value: JsonValue) {
        debug!("apps_service_on_boot: {} - {:?}", &manifest_url, value);
        if let Some(service) = &mut self.appsservice {
            let _ = service.on_boot(manifest_url, value);
        } else {
            error!("The apps service delegate is not set!");
        }
    }

    pub fn apps_service_on_boot_done(&mut self) {
        debug!("apps_service_on_boot_done");
        if let Some(service) = &mut self.appsservice {
            let _ = service.on_boot_done();
        } else {
            error!("The apps service delegate is not set!");
        }
    }

    pub fn apps_service_on_install(&mut self, manifest_url: String, value: JsonValue) {
        debug!("apps_service_on_install: {} - {:?}", &manifest_url, value);
        if let Some(service) = &mut self.appsservice {
            let _ = service.on_install(manifest_url, value);
        } else {
            error!("The apps service delegate is not set!");
        }
    }

    pub fn apps_service_on_update(&mut self, manifest_url: String, value: JsonValue) {
        debug!("apps_service_on_update: {} - {:?}", &manifest_url, value);
        if let Some(service) = &mut self.appsservice {
            let _ = service.on_update(manifest_url, value);
        } else {
            error!("The apps service delegate is not set!");
        }
    }

    pub fn apps_service_on_uninstall(&mut self, manifest_url: String) {
        debug!("apps_service_on_uninstall: {}", &manifest_url);
        if let Some(service) = &mut self.appsservice {
            let _ = service.on_uninstall(manifest_url);
        } else {
            error!("The apps service delegate is not set!");
        }
    }

    // CardInfo manager delegate management.
    pub fn set_mobilemanager_delegate(&mut self, delegate: MobileManagerDelegateProxy) {
        self.mobilemanager = Some(delegate);
        self.notify_readyness_observers();
    }

    pub fn mobilemanager_get_cardinfo(
        &mut self,
        service_id: i64,
        info_type: CardInfoType,
    ) -> Result<String, DelegateError> {
        if let Some(mobilemanager) = &mut self.mobilemanager {
            let rx = mobilemanager.get_card_info(service_id, info_type);
            if let Ok(result) = rx.recv() {
                match result {
                    Ok(info) => Ok(info),
                    Err(_) => Err(DelegateError::InvalidWebRuntimeService),
                }
            } else {
                Err(DelegateError::InvalidChannel)
            }
        } else {
            Err(DelegateError::InvalidDelegator)
        }
    }

    pub fn mobilemanager_get_mnc_mcc(
        &mut self,
        service_id: i64,
        is_sim: bool,
    ) -> Result<NetworkOperator, DelegateError> {
        if let Some(mobilemanager) = &mut self.mobilemanager {
            let rx = mobilemanager.get_mnc_mcc(service_id, is_sim);
            if let Ok(result) = rx.recv() {
                match result {
                    Ok(operator) => Ok(operator),
                    Err(_) => Err(DelegateError::InvalidWebRuntimeService),
                }
            } else {
                Err(DelegateError::InvalidChannel)
            }
        } else {
            Err(DelegateError::InvalidDelegator)
        }
    }

    // Network manager delegate management.
    pub fn set_networkmanager_delegate(&mut self, delegate: NetworkManagerDelegateProxy) {
        self.networkmanager = Some(delegate);
        self.notify_readyness_observers();
    }

    pub fn networkmanager_get_network_info(&mut self) -> Result<NetworkInfo, DelegateError> {
        if let Some(networkmanager) = &mut self.networkmanager {
            let rx = networkmanager.get_network_info();
            if let Ok(result) = rx.recv() {
                match result {
                    Ok(info) => Ok(info),
                    Err(_) => Err(DelegateError::InvalidWebRuntimeService),
                }
            } else {
                Err(DelegateError::InvalidChannel)
            }
        } else {
            Err(DelegateError::InvalidDelegator)
        }
    }

    // Preference delegate management.
    pub fn set_preference_delegate(&mut self, delegate: PreferenceDelegateProxy) {
        self.preference = Some(delegate);
        self.notify_readyness_observers();
    }

    pub fn preference_get_int(&mut self, pref_name: String) -> Result<i64, DelegateError> {
        self.preference
            .as_mut()
            .map_or(Err(DelegateError::InvalidDelegator), |p| {
                p.get_int(pref_name)
                    .recv()
                    .map_err(|_| DelegateError::InvalidChannel)
                    .and_then(|result| result.map_err(|_| DelegateError::InvalidWebRuntimeService))
            })
    }

    pub fn preference_get_char(&mut self, pref_name: String) -> Result<String, DelegateError> {
        self.preference
            .as_mut()
            .map_or(Err(DelegateError::InvalidDelegator), |p| {
                p.get_char(pref_name)
                    .recv()
                    .map_err(|_| DelegateError::InvalidChannel)
                    .and_then(|result| result.map_err(|_| DelegateError::InvalidWebRuntimeService))
            })
    }

    pub fn preference_get_bool(&mut self, pref_name: String) -> Result<bool, DelegateError> {
        self.preference
            .as_mut()
            .map_or(Err(DelegateError::InvalidDelegator), |p| {
                p.get_bool(pref_name)
                    .recv()
                    .map_err(|_| DelegateError::InvalidChannel)
                    .and_then(|result| result.map_err(|_| DelegateError::InvalidWebRuntimeService))
            })
    }

    pub fn preference_set_int(
        &mut self,
        pref_name: String,
        value: i64,
    ) -> Result<(), DelegateError> {
        self.preference
            .as_mut()
            .map_or(Err(DelegateError::InvalidDelegator), |p| {
                p.set_int(pref_name, value)
                    .recv()
                    .map_err(|_| DelegateError::InvalidChannel)
                    .map(|_| ())
            })
    }

    pub fn preference_set_char(
        &mut self,
        pref_name: String,
        value: String,
    ) -> Result<(), DelegateError> {
        self.preference
            .as_mut()
            .map_or(Err(DelegateError::InvalidDelegator), |p| {
                p.set_char(pref_name, value)
                    .recv()
                    .map_err(|_| DelegateError::InvalidChannel)
                    .map(|_| ())
            })
    }

    pub fn preference_set_bool(
        &mut self,
        pref_name: String,
        value: bool,
    ) -> Result<(), DelegateError> {
        self.preference
            .as_mut()
            .map_or(Err(DelegateError::InvalidDelegator), |p| {
                p.set_bool(pref_name, value)
                    .recv()
                    .map_err(|_| DelegateError::InvalidChannel)
                    .map(|_| ())
            })
    }

    pub fn register_token(&mut self, token: &str, origin_attribute: OriginAttributes) -> bool {
        self.tokens.lock().register(token, origin_attribute)
    }

    pub fn get_tokens_manager(&self) -> SharedTokensManager {
        self.tokens.clone()
    }
}
