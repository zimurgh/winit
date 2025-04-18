//! The [`EventLoop`] struct and assorted supporting types, including
//! [`ControlFlow`].
//!
//! If you want to send custom events to the event loop, use
//! [`EventLoop::create_proxy`] to acquire an [`EventLoopProxy`] and call its
//! [`wake_up`][EventLoopProxy::wake_up] method. Then during handling the wake up
//! you can poll your event sources.
//!
//! See the root-level documentation for information on how to create and use an event loop to
//! handle events.
use std::fmt;
use std::marker::PhantomData;
#[cfg(any(x11_platform, wayland_platform))]
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
#[cfg(not(web_platform))]
use std::time::{Duration, Instant};

use rwh_06::{DisplayHandle, HandleError, HasDisplayHandle};
#[cfg(web_platform)]
use web_time::{Duration, Instant};

use crate::application::ApplicationHandler;
use crate::error::{EventLoopError, RequestError};
use crate::monitor::MonitorHandle;
use crate::platform_impl;
use crate::utils::{impl_dyn_casting, AsAny};
use crate::window::{CustomCursor, CustomCursorSource, Theme, Window, WindowAttributes};

/// Provides a way to retrieve events from the system and from the windows that were registered to
/// the events loop.
///
/// An `EventLoop` can be seen more or less as a "context". Calling [`EventLoop::new`]
/// initializes everything that will be required to create windows. For example on Linux creating
/// an event loop opens a connection to the X or Wayland server.
///
/// To wake up an `EventLoop` from a another thread, see the [`EventLoopProxy`] docs.
///
/// Note that this cannot be shared across threads (due to platform-dependant logic
/// forbidding it), as such it is neither [`Send`] nor [`Sync`]. If you need cross-thread access,
/// the [`Window`] created from this _can_ be sent to an other thread, and the
/// [`EventLoopProxy`] allows you to wake up an `EventLoop` from another thread.
///
/// [`Window`]: crate::window::Window
#[derive(Debug)]
pub struct EventLoop {
    pub(crate) event_loop: platform_impl::EventLoop,
    pub(crate) _marker: PhantomData<*mut ()>, // Not Send nor Sync
}

/// Object that allows building the event loop.
///
/// This is used to make specifying options that affect the whole application
/// easier. But note that constructing multiple event loops is not supported.
///
/// This can be created using [`EventLoop::builder`].
#[derive(Default, Debug, PartialEq, Eq, Hash)]
pub struct EventLoopBuilder {
    pub(crate) platform_specific: platform_impl::PlatformSpecificEventLoopAttributes,
}

static EVENT_LOOP_CREATED: AtomicBool = AtomicBool::new(false);

impl EventLoopBuilder {
    /// Builds a new event loop.
    ///
    /// ***For cross-platform compatibility, the [`EventLoop`] must be created on the main thread,
    /// and only once per application.***
    ///
    /// Calling this function will result in display backend initialisation.
    ///
    /// ## Panics
    ///
    /// Attempting to create the event loop off the main thread will panic. This
    /// restriction isn't strictly necessary on all platforms, but is imposed to
    /// eliminate any nasty surprises when porting to platforms that require it.
    /// `EventLoopBuilderExt::with_any_thread` functions are exposed in the relevant
    /// [`platform`] module if the target platform supports creating an event
    /// loop on any thread.
    ///
    /// ## Platform-specific
    ///
    /// - **Wayland/X11:** to prevent running under `Wayland` or `X11` unset `WAYLAND_DISPLAY` or
    ///   `DISPLAY` respectively when building the event loop.
    /// - **Android:** must be configured with an `AndroidApp` from `android_main()` by calling
    ///   [`.with_android_app(app)`] before calling `.build()`, otherwise it'll panic.
    ///
    /// [`platform`]: crate::platform
    #[cfg_attr(
        android_platform,
        doc = "[`.with_android_app(app)`]: \
               crate::platform::android::EventLoopBuilderExtAndroid::with_android_app"
    )]
    #[cfg_attr(
        not(android_platform),
        doc = "[`.with_android_app(app)`]: #only-available-on-android"
    )]
    #[inline]
    pub fn build(&mut self) -> Result<EventLoop, EventLoopError> {
        let _span = tracing::debug_span!("winit::EventLoopBuilder::build").entered();

        if EVENT_LOOP_CREATED.swap(true, Ordering::Relaxed) {
            return Err(EventLoopError::RecreationAttempt);
        }

        // Certain platforms accept a mutable reference in their API.
        #[allow(clippy::unnecessary_mut_passed)]
        Ok(EventLoop {
            event_loop: platform_impl::EventLoop::new(&mut self.platform_specific)?,
            _marker: PhantomData,
        })
    }

    #[cfg(web_platform)]
    pub(crate) fn allow_event_loop_recreation() {
        EVENT_LOOP_CREATED.store(false, Ordering::Relaxed);
    }
}

/// Set through [`ActiveEventLoop::set_control_flow()`].
///
/// Indicates the desired behavior of the event loop after [`about_to_wait`] is called.
///
/// Defaults to [`Wait`].
///
/// [`Wait`]: Self::Wait
/// [`about_to_wait`]: crate::application::ApplicationHandler::about_to_wait
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum ControlFlow {
    /// When the current loop iteration finishes, immediately begin a new iteration regardless of
    /// whether or not new events are available to process.
    Poll,

    /// When the current loop iteration finishes, suspend the thread until another event arrives.
    #[default]
    Wait,

    /// When the current loop iteration finishes, suspend the thread until either another event
    /// arrives or the given time is reached.
    ///
    /// Useful for implementing efficient timers. Applications which want to render at the
    /// display's native refresh rate should instead use [`Poll`] and the VSync functionality
    /// of a graphics API to reduce odds of missed frames.
    ///
    /// [`Poll`]: Self::Poll
    WaitUntil(Instant),
}

impl ControlFlow {
    /// Creates a [`ControlFlow`] that waits until a timeout has expired.
    ///
    /// In most cases, this is set to [`WaitUntil`]. However, if the timeout overflows, it is
    /// instead set to [`Wait`].
    ///
    /// [`WaitUntil`]: Self::WaitUntil
    /// [`Wait`]: Self::Wait
    pub fn wait_duration(timeout: Duration) -> Self {
        match Instant::now().checked_add(timeout) {
            Some(instant) => Self::WaitUntil(instant),
            None => Self::Wait,
        }
    }
}

impl EventLoop {
    /// Create the event loop.
    ///
    /// This is an alias of `EventLoop::builder().build()`.
    #[inline]
    pub fn new() -> Result<EventLoop, EventLoopError> {
        Self::builder().build()
    }

    /// Start building a new event loop.
    ///
    /// This returns an [`EventLoopBuilder`], to allow configuring the event loop before creation.
    ///
    /// To get the actual event loop, call [`build`][EventLoopBuilder::build] on that.
    #[inline]
    pub fn builder() -> EventLoopBuilder {
        EventLoopBuilder { platform_specific: Default::default() }
    }
}

impl EventLoop {
    /// Run the application with the event loop on the calling thread.
    ///
    /// ## Event loop flow
    ///
    /// This function internally handles the different parts of a traditional event-handling loop.
    /// You can imagine this method as being implemented like this:
    ///
    /// ```rust,ignore
    /// let mut start_cause = StartCause::Init;
    ///
    /// // Run the event loop.
    /// while !event_loop.exiting() {
    ///     // Wake up.
    ///     app.new_events(event_loop, start_cause);
    ///
    ///     // Indicate that surfaces can now safely be created.
    ///     if start_cause == StartCause::Init {
    ///         app.can_create_surfaces(event_loop);
    ///     }
    ///
    ///     // Handle proxy wake-up event.
    ///     if event_loop.proxy_wake_up_set() {
    ///         event_loop.proxy_wake_up_clear();
    ///         app.proxy_wake_up(event_loop);
    ///     }
    ///
    ///     // Handle actions done by the user / system such as moving the cursor, resizing the
    ///     // window, changing the window theme, etc.
    ///     for event in event_loop.events() {
    ///         match event {
    ///             window event => app.window_event(event_loop, window_id, event),
    ///             device event => app.device_event(event_loop, device_id, event),
    ///         }
    ///     }
    ///
    ///     // Handle redraws.
    ///     for window_id in event_loop.pending_redraws() {
    ///         app.window_event(event_loop, window_id, WindowEvent::RedrawRequested);
    ///     }
    ///
    ///     // Done handling events, wait until we're woken up again.
    ///     app.about_to_wait(event_loop);
    ///     start_cause = event_loop.wait_if_necessary();
    /// }
    ///
    /// // Finished running, drop application state.
    /// drop(app);
    /// ```
    ///
    /// This is of course a very coarse-grained overview, and leaves out timing details like
    /// [`ControlFlow::WaitUntil`] and life-cycle methods like [`ApplicationHandler::resumed`], but
    /// it should give you an idea of how things fit together.
    ///
    /// ## Platform-specific
    ///
    /// - **iOS:** Will never return to the caller and so values not passed to this function will
    ///   *not* be dropped before the process exits.
    /// - **Web:** Will _act_ as if it never returns to the caller by throwing a Javascript
    ///   exception (that Rust doesn't see) that will also mean that the rest of the function is
    ///   never executed and any values not passed to this function will *not* be dropped.
    ///
    ///   Web applications are recommended to use
    #[cfg_attr(
        web_platform,
        doc = "  [`EventLoopExtWeb::spawn_app()`][crate::platform::web::EventLoopExtWeb::spawn_app()]"
    )]
    #[cfg_attr(not(web_platform), doc = "  `EventLoopExtWeb::spawn_app()`")]
    ///   [^1] instead of [`run_app()`] to avoid the need for the Javascript exception trick, and to
    ///   make   it clearer that the event loop runs asynchronously (via the browser's own,
    ///   internal, event   loop) and doesn't block the current thread of execution like it does
    ///   on other platforms.
    ///
    ///   This function won't be available with `target_feature = "exception-handling"`.
    ///
    /// [^1]: `spawn_app()` is only available on the Web platform.
    ///
    /// [`set_control_flow()`]: ActiveEventLoop::set_control_flow()
    /// [`run_app()`]: Self::run_app()
    #[inline]
    #[cfg(not(all(web_platform, target_feature = "exception-handling")))]
    pub fn run_app<A: ApplicationHandler>(self, app: A) -> Result<(), EventLoopError> {
        self.event_loop.run_app(app)
    }

    /// Creates an [`EventLoopProxy`] that can be used to dispatch user events
    /// to the main event loop, possibly from another thread.
    pub fn create_proxy(&self) -> EventLoopProxy {
        self.event_loop.window_target().create_proxy()
    }

    /// Gets a persistent reference to the underlying platform display.
    ///
    /// See the [`OwnedDisplayHandle`] type for more information.
    pub fn owned_display_handle(&self) -> OwnedDisplayHandle {
        self.event_loop.window_target().owned_display_handle()
    }

    /// Change if or when [`DeviceEvent`]s are captured.
    ///
    /// See [`ActiveEventLoop::listen_device_events`] for details.
    ///
    /// [`DeviceEvent`]: crate::event::DeviceEvent
    pub fn listen_device_events(&self, allowed: DeviceEvents) {
        let _span = tracing::debug_span!(
            "winit::EventLoop::listen_device_events",
            allowed = ?allowed
        )
        .entered();
        self.event_loop.window_target().listen_device_events(allowed)
    }

    /// Sets the [`ControlFlow`].
    pub fn set_control_flow(&self, control_flow: ControlFlow) {
        self.event_loop.window_target().set_control_flow(control_flow);
    }

    /// Create custom cursor.
    ///
    /// ## Platform-specific
    ///
    /// **iOS / Android / Orbital:** Unsupported.
    pub fn create_custom_cursor(
        &self,
        custom_cursor: CustomCursorSource,
    ) -> Result<CustomCursor, RequestError> {
        self.event_loop.window_target().create_custom_cursor(custom_cursor)
    }
}

impl HasDisplayHandle for EventLoop {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        HasDisplayHandle::display_handle(self.event_loop.window_target().rwh_06_handle())
    }
}

#[cfg(any(x11_platform, wayland_platform))]
impl AsFd for EventLoop {
    /// Get the underlying [EventLoop]'s `fd` which you can register
    /// into other event loop, like [`calloop`] or [`mio`]. When doing so, the
    /// loop must be polled with the [`pump_app_events`] API.
    ///
    /// [`calloop`]: https://crates.io/crates/calloop
    /// [`mio`]: https://crates.io/crates/mio
    /// [`pump_app_events`]: crate::platform::pump_events::EventLoopExtPumpEvents::pump_app_events
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.event_loop.as_fd()
    }
}

#[cfg(any(x11_platform, wayland_platform))]
impl AsRawFd for EventLoop {
    /// Get the underlying [EventLoop]'s raw `fd` which you can register
    /// into other event loop, like [`calloop`] or [`mio`]. When doing so, the
    /// loop must be polled with the [`pump_app_events`] API.
    ///
    /// [`calloop`]: https://crates.io/crates/calloop
    /// [`mio`]: https://crates.io/crates/mio
    /// [`pump_app_events`]: crate::platform::pump_events::EventLoopExtPumpEvents::pump_app_events
    fn as_raw_fd(&self) -> RawFd {
        self.event_loop.as_raw_fd()
    }
}

pub trait ActiveEventLoop: AsAny + fmt::Debug {
    /// Creates an [`EventLoopProxy`] that can be used to dispatch user events
    /// to the main event loop, possibly from another thread.
    fn create_proxy(&self) -> EventLoopProxy;

    /// Create the window.
    ///
    /// Possible causes of error include denied permission, incompatible system, and lack of memory.
    ///
    /// ## Platform-specific
    ///
    /// - **Web:** The window is created but not inserted into the Web page automatically. Please
    ///   see the Web platform module for more information.
    fn create_window(
        &self,
        window_attributes: WindowAttributes,
    ) -> Result<Box<dyn Window>, RequestError>;

    /// Create custom cursor.
    ///
    /// ## Platform-specific
    ///
    /// **iOS / Android / Orbital:** Unsupported.
    fn create_custom_cursor(
        &self,
        custom_cursor: CustomCursorSource,
    ) -> Result<CustomCursor, RequestError>;

    /// Returns the list of all the monitors available on the system.
    ///
    /// ## Platform-specific
    ///
    /// **Web:** Only returns the current monitor without
    #[cfg_attr(
        web_platform,
        doc = "[detailed monitor permissions][crate::platform::web::ActiveEventLoopExtWeb::request_detailed_monitor_permission]."
    )]
    #[cfg_attr(not(web_platform), doc = "detailed monitor permissions.")]
    fn available_monitors(&self) -> Box<dyn Iterator<Item = MonitorHandle>>;

    /// Returns the primary monitor of the system.
    ///
    /// Returns `None` if it can't identify any monitor as a primary one.
    ///
    /// ## Platform-specific
    ///
    /// - **Wayland:** Always returns `None`.
    /// - **Web:** Always returns `None` without
    #[cfg_attr(
        web_platform,
        doc = "  [detailed monitor permissions][crate::platform::web::ActiveEventLoopExtWeb::request_detailed_monitor_permission]."
    )]
    #[cfg_attr(not(web_platform), doc = "  detailed monitor permissions.")]
    fn primary_monitor(&self) -> Option<MonitorHandle>;

    /// Change if or when [`DeviceEvent`]s are captured.
    ///
    /// Since the [`DeviceEvent`] capture can lead to high CPU usage for unfocused windows, winit
    /// will ignore them by default for unfocused windows on Linux/BSD. This method allows changing
    /// this at runtime to explicitly capture them again.
    ///
    /// ## Platform-specific
    ///
    /// - **Wayland / macOS / iOS / Android / Orbital:** Unsupported.
    ///
    /// [`DeviceEvent`]: crate::event::DeviceEvent
    fn listen_device_events(&self, allowed: DeviceEvents);

    /// Returns the current system theme.
    ///
    /// Returns `None` if it cannot be determined on the current platform.
    ///
    /// ## Platform-specific
    ///
    /// - **iOS / Android / Wayland / x11 / Orbital:** Unsupported.
    fn system_theme(&self) -> Option<Theme>;

    /// Sets the [`ControlFlow`].
    fn set_control_flow(&self, control_flow: ControlFlow);

    /// Gets the current [`ControlFlow`].
    fn control_flow(&self) -> ControlFlow;

    /// Stop the event loop.
    ///
    /// ## Platform-specific
    ///
    /// ### iOS
    ///
    /// It is not possible to programmatically exit/quit an application on iOS, so this function is
    /// a no-op there. See also [this technical Q&A][qa1561].
    ///
    /// [qa1561]: https://developer.apple.com/library/archive/qa/qa1561/_index.html
    fn exit(&self);

    /// Returns whether the [`EventLoop`] is about to stop.
    ///
    /// Set by [`exit()`][Self::exit].
    fn exiting(&self) -> bool;

    /// Gets a persistent reference to the underlying platform display.
    ///
    /// See the [`OwnedDisplayHandle`] type for more information.
    fn owned_display_handle(&self) -> OwnedDisplayHandle;

    /// Get the raw-window-handle handle.
    fn rwh_06_handle(&self) -> &dyn HasDisplayHandle;
}

impl HasDisplayHandle for dyn ActiveEventLoop + '_ {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        self.rwh_06_handle().display_handle()
    }
}

impl_dyn_casting!(ActiveEventLoop);

/// A proxy for the underlying display handle.
///
/// The purpose of this type is to provide a cheaply cloneable handle to the underlying
/// display handle. This is often used by graphics APIs to connect to the underlying APIs.
/// It is difficult to keep a handle to the [`EventLoop`] type or the [`ActiveEventLoop`]
/// type. In contrast, this type involves no lifetimes and can be persisted for as long as
/// needed.
///
/// For all platforms, this is one of the following:
///
/// - A zero-sized type that is likely optimized out.
/// - A reference-counted pointer to the underlying type.
#[derive(Clone)]
pub struct OwnedDisplayHandle {
    pub(crate) handle: Arc<dyn HasDisplayHandle>,
}

impl OwnedDisplayHandle {
    pub(crate) fn new(handle: Arc<dyn HasDisplayHandle>) -> Self {
        Self { handle }
    }
}

impl HasDisplayHandle for OwnedDisplayHandle {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        self.handle.display_handle()
    }
}

impl fmt::Debug for OwnedDisplayHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedDisplayHandle").finish_non_exhaustive()
    }
}

impl PartialEq for OwnedDisplayHandle {
    fn eq(&self, other: &Self) -> bool {
        match (self.display_handle(), other.display_handle()) {
            (Ok(lhs), Ok(rhs)) => lhs == rhs,
            _ => false,
        }
    }
}

impl Eq for OwnedDisplayHandle {}

pub(crate) trait EventLoopProxyProvider: Send + Sync + fmt::Debug {
    /// See [`EventLoopProxy::wake_up`] for details.
    fn wake_up(&self);
}

/// Control the [`EventLoop`], possibly from a different thread, without referencing it directly.
#[derive(Clone, Debug)]
pub struct EventLoopProxy {
    pub(crate) proxy: Arc<dyn EventLoopProxyProvider>,
}

impl EventLoopProxy {
    /// Wake up the [`EventLoop`], resulting in [`ApplicationHandler::proxy_wake_up()`] being
    /// called.
    ///
    /// Calls to this method are coalesced into a single call to [`proxy_wake_up`], see the
    /// documentation on that for details.
    ///
    /// If the event loop is no longer running, this is a no-op.
    ///
    /// [`proxy_wake_up`]: ApplicationHandler::proxy_wake_up
    ///
    /// # Platform-specific
    ///
    /// - **Windows**: The wake-up may be ignored under high contention, see [#3687].
    ///
    /// [#3687]: https://github.com/rust-windowing/winit/pull/3687
    pub fn wake_up(&self) {
        self.proxy.wake_up();
    }

    pub(crate) fn new(proxy: Arc<dyn EventLoopProxyProvider>) -> Self {
        Self { proxy }
    }
}

/// Control when device events are captured.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum DeviceEvents {
    /// Report device events regardless of window focus.
    Always,
    /// Only capture device events while the window is focused.
    #[default]
    WhenFocused,
    /// Never capture device events.
    Never,
}

/// A unique identifier of the winit's async request.
///
/// This could be used to identify the async request once it's done
/// and a specific action must be taken.
///
/// One of the handling scenarios could be to maintain a working list
/// containing [`AsyncRequestSerial`] and some closure associated with it.
/// Then once event is arriving the working list is being traversed and a job
/// executed and removed from the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AsyncRequestSerial {
    serial: usize,
}

impl AsyncRequestSerial {
    // TODO(kchibisov): Remove `cfg` when the clipboard will be added.
    #[allow(dead_code)]
    pub(crate) fn get() -> Self {
        static CURRENT_SERIAL: AtomicUsize = AtomicUsize::new(0);
        // NOTE: We rely on wrap around here, while the user may just request
        // in the loop usize::MAX times that's issue is considered on them.
        let serial = CURRENT_SERIAL.fetch_add(1, Ordering::Relaxed);
        Self { serial }
    }
}
