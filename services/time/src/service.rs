/// Implementation of the time service.
use crate::generated::common::*;
use crate::generated::service::*;
use crate::time_manager::*;
use android_utils::{AndroidProperties, PropertyGetter};
use common::core::BaseMessage;
use common::traits::{
    CommonResponder, DispatcherId, OriginAttributes, Service, SessionSupport, Shared,
    SharedSessionContext, TrackerId,
};
use common::{JsonValue, SystemTime};
use log::{debug, error, info};
use settings_service::db::{DbObserver, ObserverType};
use settings_service::service::SettingsService;
use std::collections::HashMap;
use std::time::SystemTime as StdTime;
use threadpool::ThreadPool;

pub struct SharedObj {
    event_broadcaster: TimeEventBroadcaster,

    // Current id that we hand out when an observer is registered.
    id: DispatcherId,
    observers: HashMap<CallbackReason, Vec<(TimeObserverProxy, DispatcherId)>>,
}

impl SharedObj {
    pub fn add_observer(
        &mut self,
        reason: CallbackReason,
        observer: &TimeObserverProxy,
    ) -> DispatcherId {
        self.id += 1;

        match self.observers.get_mut(&reason) {
            Some(observers) => {
                observers.push((observer.clone(), self.id));
            }
            None => {
                let init = vec![(observer.clone(), self.id)];
                self.observers.insert(reason, init);
            }
        }

        self.id
    }

    pub fn remove_observer(&mut self, reason: CallbackReason, id: DispatcherId) {
        for (key, entry) in self.observers.iter_mut() {
            if reason != *key {
                continue;
            }
            // Remove the vector items that have the matching id.
            // Note: Once it's in stable Rustc, we could simply use:
            // entry.drain_filter(|item| item.1 == id);
            let mut i = 0;
            while i != entry.len() {
                if entry[i].1 == id {
                    entry.remove(i);
                } else {
                    i += 1;
                }
            }
        }
    }

    pub fn broadcast(&mut self, rn: CallbackReason, tz: String, time_delta: i64) {
        let mut info = TimeInfo {
            reason: rn,
            timezone: tz,
            delta: time_delta,
        };

        if info.timezone.is_empty() {
            // caller doesn't specify timezone, get local timezone setting
            if let Ok(tz) = AndroidProperties::get("persist.sys.timezone", "") {
                info.timezone = tz;
            }
        }

        if let Some(observers) = self.observers.get_mut(&info.reason) {
            for observer in observers {
                observer.0.callback(info.clone());
            }
        }

        match info.reason {
            CallbackReason::TimeChanged => self.event_broadcaster.broadcast_time_changed(),
            CallbackReason::TimezoneChanged => self.event_broadcaster.broadcast_timezone_changed(),
            CallbackReason::None => error!("unexpected callback reason {:?}", info.reason),
        }
    }
}

lazy_static! {
    pub(crate) static ref TIME_SHARED_DATA: Shared<SharedObj> = Shared::adopt(SharedObj {
        event_broadcaster: TimeEventBroadcaster::default(),
        id: 0,
        observers: HashMap::new(),
    });
}

#[derive(Clone, Copy)]
struct SettingObserver {}

impl DbObserver for SettingObserver {
    fn callback(&self, name: &str, value: &JsonValue) {
        if name != "time.timezone" {
            error!(
                "unexpected key {} / value {}",
                name,
                value.as_str().unwrap().to_string()
            );
            return;
        }

        let timezone = value.as_str().unwrap().to_string();
        match TimeManager::set_timezone(timezone.clone()) {
            Ok(_) => {
                let shared = Time::shared_state();
                let mut shared_lock = shared.lock();
                shared_lock.broadcast(CallbackReason::TimezoneChanged, timezone, 0);
            }
            Err(e) => error!("set timezone failed: {:?}", e),
        }
    }
}

pub struct Time {
    id: TrackerId,
    pool: ThreadPool,
    shared_obj: Shared<SharedObj>,
    dispatcher_id: DispatcherId,
    proxy_tracker: TimeServiceProxyTracker,
    observers: HashMap<ObjectRef, Vec<(CallbackReason, DispatcherId)>>,
    setting_observer: (SettingObserver, DispatcherId),
    origin_attributes: OriginAttributes,
}

impl TimeService for Time {
    fn get_proxy_tracker(&mut self) -> &mut TimeServiceProxyTracker {
        &mut self.proxy_tracker
    }
}

impl TimeMethods for Time {
    fn set(&mut self, responder: &TimeSetResponder, time: SystemTime) {
        if responder.maybe_send_permission_error(
            &self.origin_attributes,
            "system-time:write",
            "set system time",
        ) {
            return;
        }

        let responder = responder.clone();
        self.pool.execute(move || {
            let since_epoch = (*time)
                .duration_since(StdTime::UNIX_EPOCH)
                .unwrap_or_else(|_| std::time::Duration::from_millis(0))
                .as_millis();

            // get time difference
            let mut time_delta = 0;
            match TimeManager::get_system_clock() {
                Ok(cur) => {
                    time_delta = (since_epoch as i64) - cur;
                }
                Err(e) => {
                    error!("get time failed {:?}", e);
                }
            }

            match TimeManager::set_system_clock(since_epoch as i64) {
                Ok(success) => {
                    if success {
                        let shared_obj = Time::shared_state();
                        let mut shared_lock = shared_obj.lock();
                        info!("broadcast time changed event ");
                        shared_lock.broadcast(
                            CallbackReason::TimeChanged,
                            "".to_string(),
                            time_delta,
                        );
                        responder.resolve();
                    } else {
                        responder.reject();
                    }
                }
                Err(e) => {
                    responder.reject();
                    error!("set time failed:{:?}", e);
                }
            }
        });
    }

    fn get(&mut self, responder: &TimeGetResponder) {
        match TimeManager::get_system_clock() {
            Ok(since_epoch) => {
                let time = StdTime::UNIX_EPOCH
                    .checked_add(std::time::Duration::from_millis(since_epoch as u64))
                    .unwrap_or(StdTime::UNIX_EPOCH);
                responder.resolve(SystemTime::from(time));
            }
            Err(e) => {
                error!("get time failed: {:?}", e);
                responder.reject()
            }
        }
    }

    fn set_timezone(&mut self, responder: &TimeSetTimezoneResponder, timezone: String) {
        info!("set time zone {:?}", timezone);
        if responder.maybe_send_permission_error(
            &self.origin_attributes,
            "system-time:write",
            "set system timezone",
        ) {
            return;
        }
        match TimeManager::set_timezone(timezone.clone()) {
            Ok(_) => {
                let mut shared_lock = self.shared_obj.lock();
                info!("broadcast timezone changed event");
                shared_lock.broadcast(CallbackReason::TimezoneChanged, timezone, 0);
                responder.resolve();
            }
            Err(e) => {
                error!("set timezone failed:{:?}", e);
                responder.reject();
            }
        }
    }

    fn get_elapsed_real_time(&mut self, responder: &TimeGetElapsedRealTimeResponder) {
        info!("get_elapse_real_time");
        match TimeManager::get_elapsed_real_time() {
            Ok(success) => responder.resolve(success),
            Err(_) => responder.reject(),
        }
    }

    fn add_observer(
        &mut self,
        responder: &TimeAddObserverResponder,
        reason: CallbackReason,
        observer: ObjectRef,
    ) {
        info!("Adding observer {:?}", observer);

        match self.proxy_tracker.get(&observer) {
            Some(TimeServiceProxy::TimeObserver(proxy)) => {
                let id = self.shared_obj.lock().add_observer(reason, proxy);
                match self.observers.get_mut(&observer) {
                    Some(obs) => {
                        obs.push((reason, id));
                    }
                    None => {
                        let init = vec![(reason, id)];
                        self.observers.insert(observer, init);
                    }
                }
                responder.resolve();
            }
            _ => {
                error!("Failed to get tracked observer");
                responder.reject();
            }
        }
    }

    fn remove_observer(
        &mut self,
        responder: &TimeRemoveObserverResponder,
        reason: CallbackReason,
        observer: ObjectRef,
    ) {
        info!("Remove observer {:?}", observer);

        if self.proxy_tracker.contains_key(&observer) {
            if let Some(target) = self.observers.get_mut(&observer) {
                let mut i = 0;
                while i != target.len() {
                    if target[i].0 == reason {
                        self.shared_obj.lock().remove_observer(reason, target[i].1);
                        target.remove(i);
                    } else {
                        i += 1;
                    }
                }
                responder.resolve();
                return;
            }
        }

        error!("Failed to find proxy for this observer");
        responder.reject();
    }
}

impl Service<Time> for Time {
    type State = SharedObj;

    fn shared_state() -> Shared<Self::State> {
        let shared = &*TIME_SHARED_DATA;
        shared.clone()
    }

    fn create(
        origin_attributes: &OriginAttributes,
        _context: SharedSessionContext,
        _shared_obj: Shared<Self::State>,
        helper: SessionSupport,
    ) -> Result<Time, String> {
        info!("TimeoService::create");
        let service_id = helper.session_tracker_id().service();
        let event_dispatcher = TimeEventDispatcher::from(helper, 0);
        let dispatcher_id = _shared_obj.lock().event_broadcaster.add(&event_dispatcher);

        let mut service = Time {
            id: service_id,
            pool: ThreadPool::new(1),
            shared_obj: _shared_obj,
            dispatcher_id,
            proxy_tracker: HashMap::new(),
            observers: HashMap::new(),
            setting_observer: (SettingObserver {}, 0),
            origin_attributes: origin_attributes.clone(),
        };

        let setting_service = SettingsService::shared_state();
        service.setting_observer.1 = setting_service.lock().db.add_observer(
            "time.timezone",
            ObserverType::FuncPtr(Box::new(service.setting_observer.0)),
        );

        Ok(service)
    }

    fn format_request(&mut self, _transport: &SessionSupport, message: &BaseMessage) -> String {
        info!("TimeManager::format_request");
        let req: Result<TimeServiceFromClient, common::BincodeError> =
            common::deserialize_bincode(&message.content);
        match req {
            Ok(req) => format!("TimeManager service request: {:?}", req),
            Err(err) => format!("Unable to TimeManager service request: {:?}", err),
        }
    }

    // Processes a request coming from the Session.
    fn on_request(&mut self, transport: &SessionSupport, message: &BaseMessage) {
        info!("incoming request");
        self.dispatch_request(transport, message);
    }

    fn release_object(&mut self, object_id: u32) -> bool {
        info!("releasing object {}", object_id);
        self.proxy_tracker.remove(&object_id.into()).is_some()
    }
}

impl Drop for Time {
    fn drop(&mut self) {
        debug!(
            "Dropping Time Service#{}, dispatcher_id {}",
            self.id, self.dispatcher_id
        );
        let shared_lock = &mut self.shared_obj.lock();
        shared_lock.event_broadcaster.remove(self.dispatcher_id);

        let setting_service = SettingsService::shared_state();
        setting_service
            .lock()
            .db
            .remove_observer("time.timezone", self.setting_observer.1);
    }
}
