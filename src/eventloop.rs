use core::fmt::Debug;
use core::marker::PhantomData;
use core::mem;
use core::ptr;
use core::result::Result;
use core::time::Duration;

extern crate alloc;
use alloc::sync::Arc;

use ::log::*;
use embedded_svc::utils::nonblocking::event_bus::Channel;

use embedded_svc::utils::nonblocking::Asyncify;
use embedded_svc::{event_bus, service};

use esp_idf_hal::cpu::Core;
use esp_idf_hal::delay::TickType;
use esp_idf_hal::mutex::{self, Condvar};

use esp_idf_sys::*;

use crate::private::cstr::RawCstrs;

pub type EspSystemSubscription = EspSubscription<System>;
pub type EspBackgroundSubscription = EspSubscription<User<Background>>;
pub type EspExplicitSubscription = EspSubscription<User<Explicit>>;
pub type EspPinnedSubscription = EspSubscription<User<Pinned>>;

pub type EspSystemEventLoop = EspEventLoop<System>;
pub type EspBackgroundEventLoop = EspEventLoop<User<Background>>;
pub type EspExplicitEventLoop = EspEventLoop<User<Explicit>>;
pub type EspPinnedEventLoop = EspEventLoop<User<Pinned>>;

#[derive(Debug)]
pub struct BackgroundLoopConfiguration<'a> {
    pub queue_size: usize,
    pub task_name: &'a str,
    pub task_priority: u8,
    pub task_stack_size: usize,
    pub task_pin_to_core: Core,
}

impl<'a> Default for BackgroundLoopConfiguration<'a> {
    fn default() -> Self {
        Self {
            queue_size: 64,
            task_name: "(unknown)",
            task_priority: 0,
            task_stack_size: 3072,
            task_pin_to_core: Core::Core0,
        }
    }
}

impl<'a> From<&BackgroundLoopConfiguration<'a>> for (esp_event_loop_args_t, RawCstrs) {
    fn from(conf: &BackgroundLoopConfiguration<'a>) -> Self {
        let mut rcs = RawCstrs::new();

        let ela = esp_event_loop_args_t {
            queue_size: conf.queue_size as _,
            task_name: rcs.as_ptr(conf.task_name),
            task_priority: conf.task_priority as _,
            task_stack_size: conf.task_stack_size as _,
            task_core_id: conf.task_pin_to_core as _,
        };

        (ela, rcs)
    }
}

#[derive(Debug)]
pub struct ExplicitLoopConfiguration {
    pub queue_size: usize,
}

impl Default for ExplicitLoopConfiguration {
    fn default() -> Self {
        Self { queue_size: 8192 }
    }
}

impl From<&ExplicitLoopConfiguration> for esp_event_loop_args_t {
    fn from(conf: &ExplicitLoopConfiguration) -> Self {
        esp_event_loop_args_t {
            queue_size: conf.queue_size as _,
            ..Default::default()
        }
    }
}

static TAKEN: mutex::Mutex<bool> = mutex::Mutex::new(false);

#[derive(Clone, Debug)]
pub struct System;
#[derive(Clone, Debug)]
pub struct User<T>(esp_event_loop_handle_t, PhantomData<fn() -> T>);
#[derive(Clone, Debug)]
pub struct Background;
#[derive(Clone, Debug)]
pub struct Explicit;
#[derive(Clone, Debug)]
pub struct Pinned;

unsafe impl<T> Send for User<T> {}
unsafe impl<T> Sync for User<T> {}

pub trait EspEventLoopType {
    fn is_system() -> bool;
}

impl EspEventLoopType for System {
    fn is_system() -> bool {
        true
    }
}

impl<T> EspEventLoopType for User<T> {
    fn is_system() -> bool {
        false
    }
}

pub struct EspEventPostData {
    pub source: *const c_types::c_char,
    pub event_id: i32,
    pub payload: *const c_types::c_void,
    pub payload_len: usize,
}

impl<'a> EspEventPostData {
    pub unsafe fn new<P: Copy>(
        source: *const c_types::c_char,
        event_id: Option<i32>,
        payload: &P,
    ) -> EspEventPostData {
        Self {
            source,
            event_id: event_id.unwrap_or(0),
            payload: payload as *const _ as *const _,
            payload_len: mem::size_of::<P>(),
        }
    }
}

pub struct EspEventFetchData {
    pub source: *const c_types::c_char,
    pub event_id: i32,
    pub payload: *const c_types::c_void,
}

impl EspEventFetchData {
    pub unsafe fn as_payload<P: Copy>(&self) -> &P {
        let payload: &P = (self.payload as *const P).as_ref().unwrap();

        payload
    }
}

struct UnsafeCallback(*mut Box<dyn for<'a> FnMut(&'a EspEventFetchData) + 'static>);

impl UnsafeCallback {
    fn from(boxed: &mut Box<Box<dyn for<'a> FnMut(&'a EspEventFetchData) + 'static>>) -> Self {
        Self(boxed.as_mut())
    }

    unsafe fn from_ptr(ptr: *mut c_types::c_void) -> Self {
        Self(ptr as *mut _)
    }

    fn as_ptr(&self) -> *mut c_types::c_void {
        self.0 as *mut _
    }

    unsafe fn call(&self, data: &EspEventFetchData) {
        let reference = self.0.as_mut().unwrap();

        (reference)(data);
    }
}

pub struct EspSubscription<T>
where
    T: EspEventLoopType,
{
    event_loop_handle: Arc<EventLoopHandle<T>>,
    handler_instance: esp_event_handler_instance_t,
    source: *const c_types::c_char,
    event_id: i32,
    _callback: Box<Box<dyn for<'a> FnMut(&'a EspEventFetchData) + 'static>>,
}

impl<T> EspSubscription<T>
where
    T: EspEventLoopType,
{
    extern "C" fn handle(
        event_handler_arg: *mut c_types::c_void,
        event_base: esp_event_base_t,
        event_id: i32,
        event_data: *mut c_types::c_void,
    ) {
        let data = EspEventFetchData {
            source: event_base,
            event_id,
            payload: event_data,
        };

        unsafe {
            UnsafeCallback::from_ptr(event_handler_arg).call(&data);
        }
    }
}

unsafe impl<T> Send for EspSubscription<T> where T: EspEventLoopType + Send {}

impl<T> Drop for EspSubscription<T>
where
    T: EspEventLoopType,
{
    fn drop(&mut self) {
        if T::is_system() {
            unsafe {
                esp!(esp_event_handler_instance_unregister(
                    self.source,
                    self.event_id,
                    self.handler_instance
                ))
                .unwrap();
            }
        } else {
            unsafe {
                let handle: &T = &self.event_loop_handle.0;
                let user: &User<Background> = mem::transmute(handle);

                esp!(esp_event_handler_instance_unregister_with(
                    user.0,
                    self.source,
                    self.event_id,
                    self.handler_instance
                ))
                .unwrap();
            }
        }
    }
}

#[derive(Debug)]
struct EventLoopHandle<T>(T)
where
    T: EspEventLoopType;

impl EventLoopHandle<System> {
    fn new() -> Result<Self, EspError> {
        let mut taken = TAKEN.lock();

        if *taken {
            esp!(ESP_ERR_INVALID_STATE as i32)?;
        }

        esp!(unsafe { esp_event_loop_create_default() })?;

        *taken = true;

        Ok(Self(System))
    }
}

impl<T> EventLoopHandle<User<T>> {
    fn new_internal(conf: &esp_event_loop_args_t) -> Result<Self, EspError> {
        let mut handle: esp_event_loop_handle_t = ptr::null_mut();

        esp!(unsafe { esp_event_loop_create(conf as *const _, &mut handle as _) })?;

        Ok(Self(User(handle, PhantomData)))
    }
}

impl EventLoopHandle<User<Background>> {
    fn new(conf: &BackgroundLoopConfiguration) -> Result<Self, EspError> {
        let (nconf, _rcs) = conf.into();

        Self::new_internal(&nconf)
    }
}

impl EventLoopHandle<User<Explicit>> {
    fn new(conf: &ExplicitLoopConfiguration) -> Result<Self, EspError> {
        Self::new_internal(&conf.into())
    }
}

impl EventLoopHandle<User<Pinned>> {
    fn new(conf: &ExplicitLoopConfiguration) -> Result<Self, EspError> {
        Self::new_internal(&conf.into())
    }
}

impl<T> Drop for EventLoopHandle<T>
where
    T: EspEventLoopType,
{
    fn drop(&mut self) {
        if T::is_system() {
            let mut taken = TAKEN.lock();

            unsafe {
                esp!(esp_event_loop_delete_default()).unwrap();
            }

            *taken = false;
        } else {
            unsafe {
                let handle: &T = &self.0;
                let user: &User<Background> = mem::transmute(handle);

                esp!(esp_event_loop_delete(user.0)).unwrap();
            }
        }

        info!("Dropped");
    }
}

#[derive(Debug)]
pub struct EspEventLoop<T>(Arc<EventLoopHandle<T>>)
where
    T: EspEventLoopType;

impl<T> EspEventLoop<T>
where
    T: EspEventLoopType,
{
    pub fn subscribe_raw(
        &mut self,
        source: *const c_types::c_char,
        event_id: i32,
        mut callback: impl for<'a> FnMut(&EspEventFetchData) + 'static,
    ) -> Result<EspSubscription<T>, EspError> {
        let mut handler_instance: esp_event_handler_instance_t = ptr::null_mut();

        let callback: Box<dyn for<'a> FnMut(&EspEventFetchData) + 'static> =
            Box::new(move |data| callback(data));
        let mut callback = Box::new(callback);

        let unsafe_callback = UnsafeCallback::from(&mut callback);

        if T::is_system() {
            esp!(unsafe {
                esp_event_handler_instance_register(
                    source,
                    event_id,
                    Some(EspSubscription::<System>::handle),
                    unsafe_callback.as_ptr(),
                    &mut handler_instance as *mut _,
                )
            })?;
        } else {
            esp!(unsafe {
                let handle: &T = &self.0 .0;
                let user: &User<Background> = mem::transmute(handle);

                esp_event_handler_instance_register_with(
                    user.0,
                    source,
                    event_id,
                    Some(EspSubscription::<User<T>>::handle),
                    unsafe_callback.as_ptr(),
                    &mut handler_instance as *mut _,
                )
            })?;
        }

        Ok(EspSubscription {
            event_loop_handle: self.0.clone(),
            handler_instance,
            source,
            event_id,
            _callback: callback,
        })
    }

    pub fn post_raw(
        &mut self,
        data: &EspEventPostData,
        wait: Option<Duration>,
    ) -> Result<bool, EspError> {
        let result = if T::is_system() {
            unsafe {
                esp_event_post(
                    data.source,
                    data.event_id,
                    data.payload as *mut _,
                    data.payload_len as _,
                    TickType::from(wait).0,
                )
            }
        } else {
            unsafe {
                let handle: &T = &self.0 .0;
                let user: &User<Background> = mem::transmute(handle);

                esp_event_post_to(
                    user.0,
                    data.source,
                    data.event_id,
                    data.payload as *mut _,
                    data.payload_len as _,
                    TickType::from(wait).0,
                )
            }
        };

        if result == ESP_ERR_TIMEOUT {
            Ok(false)
        } else {
            esp!(result)?;

            Ok(true)
        }
    }

    #[cfg(esp_idf_esp_event_post_from_isr)]
    pub fn isr_post_raw(&mut self, data: &EspEventPostData) -> Result<bool, EspError> {
        let result = if T::is_system() {
            unsafe {
                esp_event_isr_post(
                    data.source,
                    data.event_id,
                    data.payload as *const _ as *mut _,
                    data.payload_len as _,
                    ptr::null_mut(),
                )
            }
        } else {
            unsafe {
                let handle: &T = &self.0 .0;
                let user: &User<Background> = mem::transmute(handle);

                esp_event_isr_post_to(
                    user.0,
                    data.source,
                    data.event_id,
                    data.payload as *const _ as *mut _,
                    data.payload_len as _,
                    ptr::null_mut(),
                )
            }
        };

        if result == ESP_FAIL {
            Ok(false)
        } else {
            esp!(result)?;

            Ok(true)
        }
    }

    pub fn into_typed<M, P>(self) -> EspTypedEventLoop<M, P, Self> {
        EspTypedEventLoop::new(self)
    }

    pub fn as_typed<M, P>(&mut self) -> EspTypedEventLoop<M, P, &mut Self> {
        EspTypedEventLoop::new(self)
    }
}

impl EspEventLoop<System> {
    pub fn new() -> Result<Self, EspError> {
        Ok(Self(Arc::new(EventLoopHandle::<System>::new()?)))
    }
}

impl EspEventLoop<User<Background>> {
    pub fn new(conf: &BackgroundLoopConfiguration) -> Result<Self, EspError> {
        Ok(Self(Arc::new(EventLoopHandle::<User<Background>>::new(
            conf,
        )?)))
    }
}

impl EspEventLoop<User<Explicit>> {
    pub fn new(conf: &ExplicitLoopConfiguration) -> Result<Self, EspError> {
        Ok(Self(Arc::new(EventLoopHandle::<User<Explicit>>::new(
            conf,
        )?)))
    }
}

impl EspEventLoop<User<Pinned>> {
    pub fn new(conf: &ExplicitLoopConfiguration) -> Result<Self, EspError> {
        Ok(Self(Arc::new(EventLoopHandle::<User<Pinned>>::new(conf)?)))
    }
}

impl<T> Clone for EspEventLoop<T>
where
    T: EspEventLoopType,
{
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

unsafe impl<T> Send for EspEventLoop<T> where T: EspEventLoopType + Send {}
unsafe impl<T> Sync for EspEventLoop<T> where T: EspEventLoopType + Sync {}

impl<T> service::Service for EspEventLoop<T>
where
    T: EspEventLoopType,
{
    type Error = EspError;
}

impl<T> event_bus::Spin for EspEventLoop<User<T>> {
    fn spin(&mut self, duration: Option<Duration>) -> Result<(), EspError> {
        esp!(unsafe { esp_event_loop_run(self.0 .0 .0, TickType::from(duration).0,) })
    }
}

pub trait EspTypedEventSource {
    fn source() -> *const c_types::c_char;

    fn event_id() -> Option<i32> {
        None
    }
}

pub trait EspTypedEventSerializer<P>: EspTypedEventSource {
    fn serialize<R>(payload: &P, f: impl for<'a> FnOnce(&'a EspEventPostData) -> R) -> R;
}

pub trait EspTypedEventDeserializer<P>: EspTypedEventSource {
    fn deserialize<R>(data: &EspEventFetchData, f: &mut impl for<'a> FnMut(&'a P) -> R) -> R;
}

impl<T> Asyncify for EspEventLoop<T>
where
    T: EspEventLoopType,
{
    type AsyncWrapper<S> = Channel<Condvar, S>;
}

impl<P, T> event_bus::Postbox<P> for EspEventLoop<T>
where
    P: EspTypedEventSerializer<P>,
    T: EspEventLoopType,
{
    fn post(&mut self, payload: &P, wait: Option<Duration>) -> Result<bool, Self::Error> {
        P::serialize(payload, |raw_event| self.post_raw(raw_event, wait))
    }
}

impl<P, T> event_bus::EventBus<P> for EspEventLoop<T>
where
    P: EspTypedEventDeserializer<P>,
    T: EspEventLoopType,
{
    type Subscription = EspSubscription<T>;

    fn subscribe(
        &mut self,
        mut callback: impl for<'b> FnMut(&'b P) + Send + 'static,
    ) -> Result<Self::Subscription, EspError> {
        self.subscribe_raw(
            P::source(),
            P::event_id().unwrap_or(ESP_EVENT_ANY_ID),
            move |raw_event| P::deserialize(raw_event, &mut callback),
        )
    }
}

impl<P, T> event_bus::PostboxProvider<P> for EspEventLoop<T>
where
    P: EspTypedEventSerializer<P>,
    T: EspEventLoopType,
{
    type Postbox = Self;

    fn postbox(&mut self) -> Result<Self::Postbox, Self::Error>
    where
        P: EspTypedEventSerializer<P>,
    {
        Ok(self.clone())
    }
}

impl<P> event_bus::PinnedEventBus<P> for EspEventLoop<User<Pinned>>
where
    P: EspTypedEventDeserializer<P>,
{
    type Subscription = EspSubscription<User<Pinned>>;

    fn subscribe(
        &mut self,
        mut callback: impl for<'b> FnMut(&'b P) + 'static,
    ) -> Result<Self::Subscription, EspError> {
        self.subscribe_raw(
            P::source(),
            P::event_id().unwrap_or(ESP_EVENT_ANY_ID),
            move |raw_event| P::deserialize(raw_event, &mut callback),
        )
    }
}

pub struct EspTypedEventLoop<M, P, L> {
    untyped_event_loop: L,
    _serde: PhantomData<fn() -> M>,
    _payload: PhantomData<fn() -> P>,
}

impl<M, P, L> EspTypedEventLoop<M, P, L> {
    pub fn new(untyped_event_loop: L) -> Self {
        Self {
            untyped_event_loop,
            _serde: PhantomData,
            _payload: PhantomData,
        }
    }
}

impl<M, P, L> Clone for EspTypedEventLoop<M, P, L>
where
    L: Clone,
{
    fn clone(&self) -> Self {
        Self {
            untyped_event_loop: self.untyped_event_loop.clone(),
            _serde: PhantomData,
            _payload: PhantomData,
        }
    }
}

impl<M, P, L> service::Service for EspTypedEventLoop<M, P, L>
where
    L: service::Service,
{
    type Error = L::Error;
}

impl<M, P, L> Asyncify for EspTypedEventLoop<M, P, L> {
    type AsyncWrapper<S> = Channel<Condvar, S>;
}

impl<M, P, T> event_bus::Postbox<P> for EspTypedEventLoop<M, P, EspEventLoop<T>>
where
    M: EspTypedEventSerializer<P>,
    T: EspEventLoopType,
{
    fn post(&mut self, payload: &P, wait: Option<Duration>) -> Result<bool, Self::Error> {
        M::serialize(payload, |raw_event| {
            self.untyped_event_loop.post_raw(raw_event, wait)
        })
    }
}

impl<'a, M, P, T> event_bus::Postbox<P> for EspTypedEventLoop<M, P, &'a mut EspEventLoop<T>>
where
    M: EspTypedEventSerializer<P>,
    T: EspEventLoopType,
{
    fn post(&mut self, payload: &P, wait: Option<Duration>) -> Result<bool, Self::Error> {
        M::serialize(payload, |raw_event| {
            self.untyped_event_loop.post_raw(raw_event, wait)
        })
    }
}

impl<M, P, T> event_bus::EventBus<P> for EspTypedEventLoop<M, P, EspEventLoop<T>>
where
    M: EspTypedEventDeserializer<P>,
    T: EspEventLoopType,
{
    type Subscription = EspSubscription<T>;

    fn subscribe(
        &mut self,
        mut callback: impl for<'b> FnMut(&'b P) + Send + 'static,
    ) -> Result<Self::Subscription, EspError> {
        self.untyped_event_loop.subscribe_raw(
            M::source(),
            M::event_id().unwrap_or(ESP_EVENT_ANY_ID),
            move |raw_event| M::deserialize(raw_event, &mut callback),
        )
    }
}

impl<'a, M, P, T> event_bus::EventBus<P> for EspTypedEventLoop<M, P, &'a mut EspEventLoop<T>>
where
    M: EspTypedEventDeserializer<P>,
    T: EspEventLoopType,
{
    type Subscription = EspSubscription<T>;

    fn subscribe(
        &mut self,
        mut callback: impl for<'b> FnMut(&'b P) + Send + 'static,
    ) -> Result<Self::Subscription, EspError> {
        self.untyped_event_loop.subscribe_raw(
            M::source(),
            M::event_id().unwrap_or(ESP_EVENT_ANY_ID),
            move |raw_event| M::deserialize(raw_event, &mut callback),
        )
    }
}

impl<M, P, T> event_bus::PostboxProvider<P> for EspTypedEventLoop<M, P, EspEventLoop<T>>
where
    M: EspTypedEventSerializer<P>,
    T: EspEventLoopType,
{
    type Postbox = Self;

    fn postbox(&mut self) -> Result<Self::Postbox, Self::Error> {
        Ok(Self::new(self.untyped_event_loop.clone()))
    }
}

impl<'a, M, P, T> event_bus::PostboxProvider<P> for EspTypedEventLoop<M, P, &'a mut EspEventLoop<T>>
where
    M: EspTypedEventSerializer<P>,
    T: EspEventLoopType,
{
    type Postbox = EspTypedEventLoop<M, P, EspEventLoop<T>>;

    fn postbox(&mut self) -> Result<Self::Postbox, Self::Error> {
        Ok(EspTypedEventLoop::new(self.untyped_event_loop.clone()))
    }
}

impl<M, P> event_bus::PinnedEventBus<P> for EspTypedEventLoop<M, P, EspEventLoop<User<Pinned>>>
where
    M: EspTypedEventDeserializer<P>,
{
    type Subscription = EspSubscription<User<Pinned>>;

    fn subscribe(
        &mut self,
        mut callback: impl for<'b> FnMut(&'b P) + 'static,
    ) -> Result<Self::Subscription, EspError> {
        self.untyped_event_loop.subscribe_raw(
            M::source(),
            M::event_id().unwrap_or(ESP_EVENT_ANY_ID),
            move |raw_event| M::deserialize(raw_event, &mut callback),
        )
    }
}

impl<'a, M, P> event_bus::PinnedEventBus<P>
    for EspTypedEventLoop<M, P, &'a mut EspEventLoop<User<Pinned>>>
where
    M: EspTypedEventDeserializer<P>,
{
    type Subscription = EspSubscription<User<Pinned>>;

    fn subscribe(
        &mut self,
        mut callback: impl for<'b> FnMut(&'b P) + 'static,
    ) -> Result<Self::Subscription, EspError> {
        self.untyped_event_loop.subscribe_raw(
            M::source(),
            M::event_id().unwrap_or(ESP_EVENT_ANY_ID),
            move |raw_event| M::deserialize(raw_event, &mut callback),
        )
    }
}
