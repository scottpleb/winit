#![cfg(any(target_os = "linux", target_os = "dragonfly", target_os = "freebsd", target_os = "openbsd"))]

pub use self::monitor::{MonitorId, get_available_monitors, get_primary_monitor};
pub use self::window::{Window2, XWindow};
pub use self::xdisplay::{XConnection, XNotSupported, XError};

pub mod ffi;

use platform::PlatformSpecificWindowBuilderAttributes;
use {CreationError, Event, EventsLoopClosed, WindowEvent, DeviceEvent,
     KeyboardInput, ControlFlow};
use events::ModifiersState;

use std::{mem, ptr, slice};
use std::sync::{Arc, Weak};
use std::sync::atomic::{self, AtomicBool};
use std::sync::mpsc;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::*;

use libc::{self, setlocale, LC_CTYPE};
use parking_lot::Mutex;

mod events;
mod monitor;
mod window;
mod xdisplay;
mod dnd;
mod ime;
mod util;
mod xkb;

use self::dnd::{Dnd, DndState};
use self::ime::{ImeReceiver, ImeSender, ImeCreationError, Ime};
use self::xkb::Xkb;

pub struct EventsLoop {
    display: Arc<XConnection>,
    wm_delete_window: ffi::Atom,
    dnd: Dnd,
    ime_receiver: ImeReceiver,
    ime_sender: ImeSender,
    ime: RefCell<Ime>,
    xkb: RefCell<Option<Xkb>>,
    windows: Arc<Mutex<HashMap<WindowId, WindowData>>>,
    // Please don't laugh at this type signature
    shared_state: RefCell<HashMap<WindowId, Weak<Mutex<window::SharedState>>>>,
    devices: RefCell<HashMap<DeviceId, Device>>,
    xi2ext: XExtension,
    pending_wakeup: Arc<AtomicBool>,
    root: ffi::Window,
    // A dummy, `InputOnly` window that we can use to receive wakeup events and interrupt blocking
    // `XNextEvent` calls.
    wakeup_dummy_window: ffi::Window,
}

#[derive(Clone)]
pub struct EventsLoopProxy {
    pending_wakeup: Weak<AtomicBool>,
    display: Weak<XConnection>,
    wakeup_dummy_window: ffi::Window,
}

impl EventsLoop {
    pub fn new(display: Arc<XConnection>) -> EventsLoop {
        let wm_delete_window = unsafe { util::get_atom(&display, b"WM_DELETE_WINDOW\0") }
            .expect("Failed to call XInternAtom (WM_DELETE_WINDOW)");

        let dnd = Dnd::new(Arc::clone(&display))
            .expect("Failed to call XInternAtoms when initializing drag and drop");

        let (ime_sender, ime_receiver) = mpsc::channel();
        // Input methods will open successfully without setting the locale, but it won't be
        // possible to actually commit pre-edit sequences.
        unsafe { setlocale(LC_CTYPE, b"\0".as_ptr() as *const _); }
        let ime = RefCell::new({
            let result = Ime::new(Arc::clone(&display));
            if let Err(ImeCreationError::OpenFailure(ref state)) = result {
                panic!(format!("Failed to open input method: {:#?}", state));
            }
            result.expect("Failed to set input method destruction callback")
        });

        let xi2ext = unsafe {
            let mut result = XExtension {
                opcode: mem::uninitialized(),
                first_event_id: mem::uninitialized(),
                first_error_id: mem::uninitialized(),
            };
            let res = (display.xlib.XQueryExtension)(
                display.display,
                b"XInputExtension\0".as_ptr() as *const c_char,
                &mut result.opcode as *mut c_int,
                &mut result.first_event_id as *mut c_int,
                &mut result.first_error_id as *mut c_int);
            if res == ffi::False {
                panic!("X server missing XInput extension");
            }
            result
        };

        unsafe {
            let mut xinput_major_ver = ffi::XI_2_Major;
            let mut xinput_minor_ver = ffi::XI_2_Minor;
            if (display.xinput2.XIQueryVersion)(
                display.display,
                &mut xinput_major_ver,
                &mut xinput_minor_ver,
            ) != ffi::Success as libc::c_int {
                panic!(
                    "X server has XInput extension {}.{} but does not support XInput2",
                    xinput_major_ver,
                    xinput_minor_ver,
                );
            }
        }

        let xkb = unsafe { Xkb::new(&display) }.ok();

        let root = unsafe { (display.xlib.XDefaultRootWindow)(display.display) };
        util::update_cached_wm_info(&display, root);

        let wakeup_dummy_window = unsafe {
            let (x, y, w, h) = (10, 10, 10, 10);
            let (border_w, border_px, background_px) = (0, 0, 0);
            (display.xlib.XCreateSimpleWindow)(
                display.display,
                root,
                x,
                y,
                w,
                h,
                border_w,
                border_px,
                background_px,
            )
        };

        let result = EventsLoop {
            pending_wakeup: Arc::new(AtomicBool::new(false)),
            display,
            wm_delete_window,
            dnd,
            ime_receiver,
            ime_sender,
            ime,
            xkb: RefCell::new(xkb),
            windows: Arc::new(Mutex::new(HashMap::new())),
            shared_state: RefCell::new(HashMap::new()),
            devices: RefCell::new(HashMap::new()),
            xi2ext,
            root,
            wakeup_dummy_window,
        };

        // Register for device hotplug events
        unsafe {
            util::select_xinput_events(
                &result.display,
                root,
                ffi::XIAllDevices,
                ffi::XI_HierarchyChangedMask,
            )
        }.queue(); // The request buffer is flushed during init_device

        result.init_device(ffi::XIAllDevices);

        result
    }

    /// Returns the `XConnection` of this events loop.
    #[inline]
    pub fn x_connection(&self) -> &Arc<XConnection> {
        &self.display
    }

    pub fn create_proxy(&self) -> EventsLoopProxy {
        EventsLoopProxy {
            pending_wakeup: Arc::downgrade(&self.pending_wakeup),
            display: Arc::downgrade(&self.display),
            wakeup_dummy_window: self.wakeup_dummy_window,
        }
    }

    pub fn poll_events<F>(&mut self, mut callback: F)
        where F: FnMut(Event)
    {
        let mut xev = unsafe { mem::uninitialized() };
        loop {
            // Get next event
            unsafe {
                // Ensure XNextEvent won't block
                let count = (self.display.xlib.XPending)(self.display.display);
                if count == 0 {
                    break;
                }

                (self.display.xlib.XNextEvent)(self.display.display, &mut xev);
            }
            self.process_event(&mut xev, &mut callback);
        }
    }

    pub fn run_forever<F>(&mut self, mut callback: F)
        where F: FnMut(Event) -> ControlFlow
    {
        let mut xev = unsafe { mem::uninitialized() };

        loop {
            unsafe { (self.display.xlib.XNextEvent)(self.display.display, &mut xev) }; // Blocks as necessary

            let mut control_flow = ControlFlow::Continue;

            // Track whether or not `Break` was returned when processing the event.
            {
                let mut cb = |event| {
                    if let ControlFlow::Break = callback(event) {
                        control_flow = ControlFlow::Break;
                    }
                };

                self.process_event(&mut xev, &mut cb);
            }

            if let ControlFlow::Break = control_flow {
                break;
            }
        }
    }

    fn process_event<F>(&mut self, xev: &mut ffi::XEvent, mut callback: F)
        where F: FnMut(Event)
    {
        let xlib = &self.display.xlib;

        // XFilterEvent tells us when an event has been discarded by the input method.
        // Specifically, this involves all of the KeyPress events in compose/pre-edit sequences,
        // along with an extra copy of the KeyRelease events. This also prevents backspace and
        // arrow keys from being detected twice.
        if ffi::True == unsafe { (self.display.xlib.XFilterEvent)(
            xev,
            { let xev: &ffi::XAnyEvent = xev.as_ref(); xev.window }
        ) } {
            return;
        }

        let event_type = xev.get_type();
        match event_type {
            ffi::MappingNotify => {
                unsafe { (xlib.XRefreshKeyboardMapping)(xev.as_mut()); }
                self.display.check_errors().expect("Failed to call XRefreshKeyboardMapping");
            }

            ffi::ClientMessage => {
                let client_msg: &ffi::XClientMessageEvent = xev.as_ref();

                let window = client_msg.window;
                let window_id = mkwid(window);

                if client_msg.data.get_long(0) as ffi::Atom == self.wm_delete_window {
                    callback(Event::WindowEvent { window_id, event: WindowEvent::CloseRequested });
                } else if client_msg.message_type == self.dnd.atoms.enter {
                    let source_window = client_msg.data.get_long(0) as c_ulong;
                    let flags = client_msg.data.get_long(1);
                    let version = flags >> 24;
                    self.dnd.version = Some(version);
                    let has_more_types = flags - (flags & (c_long::max_value() - 1)) == 1;
                    if !has_more_types {
                        let type_list = vec![
                            client_msg.data.get_long(2) as c_ulong,
                            client_msg.data.get_long(3) as c_ulong,
                            client_msg.data.get_long(4) as c_ulong
                        ];
                        self.dnd.type_list = Some(type_list);
                    } else if let Ok(more_types) = unsafe { self.dnd.get_type_list(source_window) } {
                        self.dnd.type_list = Some(more_types);
                    }
                } else if client_msg.message_type == self.dnd.atoms.position {
                    // This event occurs every time the mouse moves while a file's being dragged
                    // over our window. We emit HoveredFile in response; while the Mac OS X backend
                    // does that upon a drag entering, XDnD doesn't have access to the actual drop
                    // data until this event. For parity with other platforms, we only emit
                    // HoveredFile the first time, though if winit's API is later extended to
                    // supply position updates with HoveredFile or another event, implementing
                    // that here would be trivial.

                    let source_window = client_msg.data.get_long(0) as c_ulong;

                    // Equivalent to (x << shift) | y
                    // where shift = mem::size_of::<c_short>() * 8
                    // Note that coordinates are in "desktop space", not "window space"
                    // (in x11 parlance, they're root window coordinates)
                    //let packed_coordinates = client_msg.data.get_long(2);
                    //let shift = mem::size_of::<libc::c_short>() * 8;
                    //let x = packed_coordinates >> shift;
                    //let y = packed_coordinates & !(x << shift);

                    // By our own state flow, version should never be None at this point.
                    let version = self.dnd.version.unwrap_or(5);

                    // Action is specified in versions 2 and up, though we don't need it anyway.
                    //let action = client_msg.data.get_long(4);

                    let accepted = if let Some(ref type_list) = self.dnd.type_list {
                        type_list.contains(&self.dnd.atoms.uri_list)
                    } else {
                        false
                    };

                    if accepted {
                        self.dnd.source_window = Some(source_window);
                        unsafe {
                            if self.dnd.result.is_none() {
                                let time = if version >= 1 {
                                    client_msg.data.get_long(3) as c_ulong
                                } else {
                                    // In version 0, time isn't specified
                                    ffi::CurrentTime
                                };
                                // This results in the SelectionNotify event below
                                self.dnd.convert_selection(window, time);
                            }
                            self.dnd.send_status(window, source_window, DndState::Accepted)
                                .expect("Failed to send XDnD status message.");
                        }
                    } else {
                        unsafe {
                            self.dnd.send_status(window, source_window, DndState::Rejected)
                                .expect("Failed to send XDnD status message.");
                            self.dnd.send_finished(window, source_window, DndState::Rejected)
                                .expect("Failed to send XDnD finished message.");
                        }
                        self.dnd.reset();
                    }
                } else if client_msg.message_type == self.dnd.atoms.drop {
                    if let Some(source_window) = self.dnd.source_window {
                        if let Some(Ok(ref path_list)) = self.dnd.result {
                            for path in path_list {
                                callback(Event::WindowEvent {
                                    window_id,
                                    event: WindowEvent::DroppedFile(path.clone()),
                                });
                            }
                        }
                        unsafe {
                            self.dnd.send_finished(window, source_window, DndState::Accepted)
                                .expect("Failed to send XDnD finished message.");
                        }
                    }
                    self.dnd.reset();
                } else if client_msg.message_type == self.dnd.atoms.leave {
                    self.dnd.reset();
                    callback(Event::WindowEvent {
                        window_id,
                        event: WindowEvent::HoveredFileCancelled,
                    });
                } else if self.pending_wakeup.load(atomic::Ordering::Relaxed) {
                    self.pending_wakeup.store(false, atomic::Ordering::Relaxed);
                    callback(Event::Awakened);
                }
            }

            ffi::SelectionNotify => {
                let xsel: &ffi::XSelectionEvent = xev.as_ref();

                let window = xsel.requestor;
                let window_id = mkwid(window);

                if xsel.property == self.dnd.atoms.selection {
                    let mut result = None;

                    // This is where we receive data from drag and drop
                    if let Ok(mut data) = unsafe { self.dnd.read_data(window) } {
                        let parse_result = self.dnd.parse_data(&mut data);
                        if let Ok(ref path_list) = parse_result {
                            for path in path_list {
                                callback(Event::WindowEvent {
                                    window_id,
                                    event: WindowEvent::HoveredFile(path.clone()),
                                });
                            }
                        }
                        result = Some(parse_result);
                    }

                    self.dnd.result = result;
                }
            }

            ffi::ConfigureNotify => {
                let xev: &ffi::XConfigureEvent = xev.as_ref();

                // So apparently...
                // XSendEvent (synthetic ConfigureNotify) -> position relative to root
                // XConfigureNotify (real ConfigureNotify) -> position relative to parent
                // https://tronche.com/gui/x/icccm/sec-4.html#s-4.1.5
                // We don't want to send Moved when this is true, since then every Resized
                // (whether the window moved or not) is accompanied by an extraneous Moved event
                // that has a position relative to the parent window.
                let is_synthetic = xev.send_event == ffi::True;

                let window = xev.window;
                let window_id = mkwid(window);

                let new_size = (xev.width, xev.height);
                let new_position = (xev.x, xev.y);

                let (resized, moved) = {
                    let mut windows = self.windows.lock();
                    if let Some(window_data) = windows.get_mut(&WindowId(window)) {
                        let (mut resized, mut moved) = (false, false);

                        if window_data.config.size.is_none() {
                            window_data.config.size = Some(new_size);
                            resized = true;
                        }
                        if window_data.config.size.is_none() && is_synthetic {
                            window_data.config.position = Some(new_position);
                            moved = true;
                        }

                        if !resized {
                            if window_data.config.size != Some(new_size) {
                                window_data.config.size = Some(new_size);
                                resized = true;
                            }
                        }
                        if !moved && is_synthetic {
                            if window_data.config.position != Some(new_position) {
                                window_data.config.position = Some(new_position);
                                moved = true;
                            }
                        }

                        if !is_synthetic
                        && window_data.config.inner_position != Some(new_position) {
                            window_data.config.inner_position = Some(new_position);
                            // This way, we get sent Moved when the decorations are toggled.
                            window_data.config.position = None;
                            self.shared_state.borrow().get(&WindowId(window)).map(|window_state| {
                                if let Some(window_state) = window_state.upgrade() {
                                    // Extra insurance against stale frame extents
                                    (*window_state.lock()).frame_extents.take();
                                }
                            });
                        }

                        (resized, moved)
                    } else {
                        return;
                    }
                };

                if resized {
                    let (width, height) = (xev.width as u32, xev.height as u32);
                    callback(Event::WindowEvent {
                        window_id,
                        event: WindowEvent::Resized(width, height),
                    });
                }

                if moved {
                    // We need to convert client area position to window position.
                    self.shared_state.borrow().get(&WindowId(window)).map(|window_state| {
                        if let Some(window_state) = window_state.upgrade() {
                            let (x, y) = {
                                let (inner_x, inner_y) = (xev.x as i32, xev.y as i32);
                                let mut window_state_lock = window_state.lock();
                                if (*window_state_lock).frame_extents.is_some() {
                                    (*window_state_lock).frame_extents
                                        .as_ref()
                                        .unwrap()
                                        .inner_pos_to_outer(inner_x, inner_y)
                                } else {
                                    let extents = util::get_frame_extents_heuristic(
                                        &self.display,
                                        window,
                                        self.root,
                                    );
                                    let outer_pos = extents.inner_pos_to_outer(inner_x, inner_y);
                                    (*window_state_lock).frame_extents = Some(extents);
                                    outer_pos
                                }
                            };
                            callback(Event::WindowEvent {
                                window_id,
                                event: WindowEvent::Moved(x, y),
                            });
                        }
                    });
                }
            }

            ffi::ReparentNotify => {
                let xev: &ffi::XReparentEvent = xev.as_ref();

                let window = xev.window;

                // This is generally a reliable way to detect when the window manager's been
                // replaced, though this event is only fired by reparenting window managers
                // (which is almost all of them). Failing to correctly update WM info doesn't
                // really have much impact, since on the WMs affected (xmonad, dwm, etc.) the only
                // effect is that we waste some time trying to query unsupported properties.
                util::update_cached_wm_info(&self.display, self.root);

                self.shared_state
                    .borrow()
                    .get(&WindowId(window))
                    .map(|window_state| {
                        if let Some(window_state) = window_state.upgrade() {
                            (*window_state.lock()).frame_extents.take();
                        }
                    });
            }

            ffi::DestroyNotify => {
                let xev: &ffi::XDestroyWindowEvent = xev.as_ref();

                let window = xev.window;
                let window_id = mkwid(window);

                // In the event that the window's been destroyed without being dropped first, we
                // cleanup again here.
                self.windows.lock().remove(&WindowId(window));

                // Since all XIM stuff needs to happen from the same thread, we destroy the input
                // context here instead of when dropping the window.
                self.ime
                    .borrow_mut()
                    .remove_context(window)
                    .expect("Failed to destroy input context");

                callback(Event::WindowEvent { window_id, event: WindowEvent::Destroyed });
            }

            ffi::Expose => {
                let xev: &ffi::XExposeEvent = xev.as_ref();

                let window = xev.window;
                let window_id = mkwid(window);

                callback(Event::WindowEvent { window_id, event: WindowEvent::Refresh });
            }

            ffi::KeyPress | ffi::KeyRelease => {
                use events::ElementState::{Pressed, Released};

                // Note that in compose/pre-edit sequences, this will always be Released.
                let state = if xev.get_type() == ffi::KeyPress {
                    Pressed
                } else {
                    Released
                };

                let xkev: &mut ffi::XKeyEvent = xev.as_mut();

                let window = xkev.window;
                let window_id = mkwid(window);

                // Standard virtual core keyboard ID. XInput2 needs to be used to get a reliable
                // value, though this should only be an issue under multiseat configurations.
                let device = 3;
                let device_id = mkdid(device);

                // When a compose sequence or IME pre-edit is finished, it ends in a KeyPress with
                // a keycode of 0.
                if xkev.keycode != 0 {
                    let modifiers = ModifiersState {
                        alt: xkev.state & ffi::Mod1Mask != 0,
                        shift: xkev.state & ffi::ShiftMask != 0,
                        ctrl: xkev.state & ffi::ControlMask != 0,
                        logo: xkev.state & ffi::Mod4Mask != 0,
                    };

                    let keysym = self.xkb
                        .borrow()
                        .as_ref()
                        .and_then(|xkb| xkb.get_keysym(device, xkev.keycode as _))
                        .unwrap_or_else(|| {
                            unsafe {
                                let mut keysym = 0;
                                (self.display.xlib.XLookupString)(
                                    xkev,
                                    ptr::null_mut(),
                                    0,
                                    &mut keysym,
                                    ptr::null_mut(),
                                );
                                self.display.check_errors().expect("Failed to lookup keysym");
                                keysym as c_uint
                            }
                        });
                    let virtual_keycode = events::keysym_to_element(keysym);

                    callback(Event::WindowEvent {
                        window_id,
                        event: WindowEvent::KeyboardInput {
                            device_id,
                            input: KeyboardInput {
                                state,
                                scancode: xkev.keycode - 8,
                                virtual_keycode,
                                modifiers,
                            },
                        }
                    });
                }

                if state == Pressed {
                    let written = if let Some(ic) = self.ime.borrow().get_context(window) {
                        unsafe { util::lookup_utf8(&self.display, ic, xkev) }
                    } else {
                        return;
                    };

                    for chr in written.chars() {
                        let event = Event::WindowEvent {
                            window_id,
                            event: WindowEvent::ReceivedCharacter(chr),
                        };
                        callback(event);
                    }
                }
            }

            ffi::GenericEvent => {
                let guard = if let Some(e) = GenericEventCookie::from_event(&self.display, *xev) { e } else { return };
                let xev = &guard.cookie;
                if self.xi2ext.opcode != xev.extension {
                    return;
                }

                use events::WindowEvent::{Focused, CursorEntered, MouseInput, CursorLeft, CursorMoved, MouseWheel, AxisMotion};
                use events::ElementState::{Pressed, Released};
                use events::MouseButton::{Left, Right, Middle, Other};
                use events::MouseScrollDelta::LineDelta;
                use events::{Touch, TouchPhase};

                match xev.evtype {
                    ffi::XI_ButtonPress | ffi::XI_ButtonRelease => {
                        let xev: &ffi::XIDeviceEvent = unsafe { &*(xev.data as *const _) };
                        let window_id = mkwid(xev.event);
                        let device_id = mkdid(xev.deviceid);
                        if (xev.flags & ffi::XIPointerEmulated) != 0 {
                            let windows = self.windows.lock();
                            if let Some(window_data) = windows.get(&WindowId(xev.event)) {
                                if window_data.multitouch {
                                    // Deliver multi-touch events instead of emulated mouse events.
                                    return;
                                }
                            } else {
                                return;
                            }
                        }

                        let modifiers = ModifiersState::from(xev.mods);

                        let state = if xev.evtype == ffi::XI_ButtonPress {
                            Pressed
                        } else {
                            Released
                        };
                        match xev.detail as u32 {
                            ffi::Button1 => callback(Event::WindowEvent {
                                window_id,
                                event: MouseInput {
                                    device_id,
                                    state,
                                    button: Left,
                                    modifiers,
                                },
                            }),
                            ffi::Button2 => callback(Event::WindowEvent {
                                window_id,
                                event: MouseInput {
                                    device_id,
                                    state,
                                    button: Middle,
                                    modifiers,
                                },
                            }),
                            ffi::Button3 => callback(Event::WindowEvent {
                                window_id,
                                event: MouseInput {
                                    device_id,
                                    state,
                                    button: Right,
                                    modifiers,
                                },
                            }),

                            // Suppress emulated scroll wheel clicks, since we handle the real motion events for those.
                            // In practice, even clicky scroll wheels appear to be reported by evdev (and XInput2 in
                            // turn) as axis motion, so we don't otherwise special-case these button presses.
                            4 | 5 | 6 | 7 => if xev.flags & ffi::XIPointerEmulated == 0 {
                                callback(Event::WindowEvent {
                                    window_id,
                                    event: MouseWheel {
                                        device_id,
                                        delta: match xev.detail {
                                            4 => LineDelta(0.0, 1.0),
                                            5 => LineDelta(0.0, -1.0),
                                            6 => LineDelta(-1.0, 0.0),
                                            7 => LineDelta(1.0, 0.0),
                                            _ => unreachable!(),
                                        },
                                        phase: TouchPhase::Moved,
                                        modifiers,
                                    },
                                });
                            },

                            x => callback(Event::WindowEvent {
                                window_id,
                                event: MouseInput {
                                    device_id,
                                    state,
                                    button: Other(x as u8),
                                    modifiers,
                                },
                            }),
                        }
                    }
                    ffi::XI_Motion => {
                        let xev: &ffi::XIDeviceEvent = unsafe { &*(xev.data as *const _) };
                        let device_id = mkdid(xev.deviceid);
                        let window_id = mkwid(xev.event);
                        let new_cursor_pos = (xev.event_x, xev.event_y);

                        let modifiers = ModifiersState::from(xev.mods);

                        // Gymnastics to ensure self.windows isn't locked when we invoke callback
                        if {
                            let mut windows = self.windows.lock();
                            let window_data = {
                                if let Some(window_data) = windows.get_mut(&WindowId(xev.event)) {
                                    window_data
                                } else {
                                    return;
                                }
                            };
                            if Some(new_cursor_pos) != window_data.cursor_pos {
                                window_data.cursor_pos = Some(new_cursor_pos);
                                true
                            } else { false }
                        } {
                            callback(Event::WindowEvent {
                                window_id,
                                event: CursorMoved {
                                    device_id,
                                    position: new_cursor_pos,
                                    modifiers,
                                },
                            });
                        }

                        // More gymnastics, for self.devices
                        let mut events = Vec::new();
                        {
                            let mask = unsafe { slice::from_raw_parts(xev.valuators.mask, xev.valuators.mask_len as usize) };
                            let mut devices = self.devices.borrow_mut();
                            let physical_device = devices.get_mut(&DeviceId(xev.sourceid)).unwrap();

                            let mut value = xev.valuators.values;
                            for i in 0..xev.valuators.mask_len*8 {
                                if ffi::XIMaskIsSet(mask, i) {
                                    let x = unsafe { *value };
                                    if let Some(&mut (_, ref mut info)) = physical_device.scroll_axes.iter_mut().find(|&&mut (axis, _)| axis == i) {
                                        let delta = (x - info.position) / info.increment;
                                        info.position = x;
                                        events.push(Event::WindowEvent {
                                            window_id,
                                            event: MouseWheel {
                                                device_id,
                                                delta: match info.orientation {
                                                    ScrollOrientation::Horizontal => LineDelta(delta as f32, 0.0),
                                                    // X11 vertical scroll coordinates are opposite to winit's
                                                    ScrollOrientation::Vertical => LineDelta(0.0, -delta as f32),
                                                },
                                                phase: TouchPhase::Moved,
                                                modifiers,
                                            },
                                        });
                                    } else {
                                        events.push(Event::WindowEvent {
                                            window_id,
                                            event: AxisMotion {
                                                device_id,
                                                axis: i as u32,
                                                value: unsafe { *value },
                                            },
                                        });
                                    }
                                    value = unsafe { value.offset(1) };
                                }
                            }
                        }
                        for event in events {
                            callback(event);
                        }
                    }

                    ffi::XI_Enter => {
                        let xev: &ffi::XIEnterEvent = unsafe { &*(xev.data as *const _) };

                        let window_id = mkwid(xev.event);
                        let device_id = mkdid(xev.deviceid);

                        let mut devices = self.devices.borrow_mut();
                        let mut keyboard_id = 3;
                        let physical_device = devices.get_mut(&DeviceId(xev.sourceid)).unwrap();
                        for info in DeviceInfo::get(&self.display, ffi::XIAllDevices).iter() {
                            if info.deviceid == xev.deviceid {
                                keyboard_id = info.attachment;
                            }
                            if info.deviceid == xev.sourceid {
                                physical_device.reset_scroll_position(info);
                            }
                        }
                        callback(Event::WindowEvent {
                            window_id,
                            event: CursorEntered { device_id },
                        });

                        let new_cursor_pos = (xev.event_x, xev.event_y);

                        // The mods field on this event isn't actually populated, so we...
                        // A) Use Xkb state if it's available
                        // B) Query the pointer device if not (round-trip)
                        let modifiers = self.xkb
                            .borrow()
                            .as_ref()
                            .and_then(|xkb| xkb.get_modifiers(keyboard_id))
                            .unwrap_or_else(|| {
                                unsafe {
                                    util::query_pointer(
                                        &self.display,
                                        xev.event,
                                        xev.deviceid,
                                    )
                                }.expect("Failed to query pointer device").get_modifier_state()
                            });

                        callback(Event::WindowEvent { window_id, event: CursorMoved {
                            device_id,
                            position: new_cursor_pos,
                            modifiers,
                        }})
                    }
                    ffi::XI_Leave => {
                        let xev: &ffi::XILeaveEvent = unsafe { &*(xev.data as *const _) };

                        // Leave, FocusIn, and FocusOut can be received by a window that's already
                        // been destroyed, which the user presumably doesn't want to deal with.
                        let window_closed = self.windows
                            .lock()
                            .get(&WindowId(xev.event))
                            .is_none();

                        if !window_closed {
                            callback(Event::WindowEvent {
                                window_id: mkwid(xev.event),
                                event: CursorLeft { device_id: mkdid(xev.deviceid) },
                            });
                        }
                    }
                    ffi::XI_FocusIn => {
                        let xev: &ffi::XIFocusInEvent = unsafe { &*(xev.data as *const _) };

                        let window_id = mkwid(xev.event);

                        if let None = self.windows.lock().get(&WindowId(xev.event)) {
                            return;
                        }
                        self.ime
                            .borrow_mut()
                            .focus(xev.event)
                            .expect("Failed to focus input context");

                        callback(Event::WindowEvent { window_id, event: Focused(true) });

                        // The deviceid for this event is for a keyboard instead of a pointer,
                        // so we have to do a little extra work.
                        let pointer_id = self.devices
                            .borrow()
                            .get(&DeviceId(xev.deviceid))
                            .map(|device| device.attachment)
                            .unwrap_or(2);

                        callback(Event::WindowEvent {
                            window_id,
                            event: CursorMoved {
                                device_id: mkdid(pointer_id),
                                position: (xev.event_x, xev.event_y),
                                modifiers: ModifiersState::from(xev.mods),
                            }
                        });
                    }
                    ffi::XI_FocusOut => {
                        let xev: &ffi::XIFocusOutEvent = unsafe { &*(xev.data as *const _) };

                        if let None = self.windows.lock().get(&WindowId(xev.event)) {
                            return;
                        }
                        self.ime
                            .borrow_mut()
                            .unfocus(xev.event)
                            .expect("Failed to unfocus input context");

                        callback(Event::WindowEvent {
                            window_id: mkwid(xev.event),
                            event: Focused(false),
                        })
                    }

                    ffi::XI_TouchBegin | ffi::XI_TouchUpdate | ffi::XI_TouchEnd => {
                        let xev: &ffi::XIDeviceEvent = unsafe { &*(xev.data as *const _) };
                        let window_id = mkwid(xev.event);
                        let phase = match xev.evtype {
                            ffi::XI_TouchBegin => TouchPhase::Started,
                            ffi::XI_TouchUpdate => TouchPhase::Moved,
                            ffi::XI_TouchEnd => TouchPhase::Ended,
                            _ => unreachable!()
                        };
                        callback(Event::WindowEvent {
                            window_id,
                            event: WindowEvent::Touch(Touch {
                                device_id: mkdid(xev.deviceid),
                                phase,
                                location: (xev.event_x, xev.event_y),
                                id: xev.detail as u64,
                            },
                        )})
                    }

                    ffi::XI_RawButtonPress | ffi::XI_RawButtonRelease => {
                        let xev: &ffi::XIRawEvent = unsafe { &*(xev.data as *const _) };
                        if xev.flags & ffi::XIPointerEmulated == 0 {
                            callback(Event::DeviceEvent { device_id: mkdid(xev.deviceid), event: DeviceEvent::Button {
                                button: xev.detail as u32,
                                state: match xev.evtype {
                                    ffi::XI_RawButtonPress => Pressed,
                                    ffi::XI_RawButtonRelease => Released,
                                    _ => unreachable!(),
                                },
                            }});
                        }
                    }

                    ffi::XI_RawMotion => {
                        let xev: &ffi::XIRawEvent = unsafe { &*(xev.data as *const _) };
                        let did = mkdid(xev.deviceid);

                        let mask = unsafe { slice::from_raw_parts(xev.valuators.mask, xev.valuators.mask_len as usize) };
                        let mut value = xev.raw_values;
                        let mut mouse_delta = (0.0, 0.0);
                        let mut scroll_delta = (0.0, 0.0);
                        for i in 0..xev.valuators.mask_len*8 {
                            if ffi::XIMaskIsSet(mask, i) {
                                let x = unsafe { *value };
                                // We assume that every XInput2 device with analog axes is a pointing device emitting
                                // relative coordinates.
                                match i {
                                    0 => mouse_delta.0 = x,
                                    1 => mouse_delta.1 = x,
                                    2 => scroll_delta.0 = x as f32,
                                    3 => scroll_delta.1 = x as f32,
                                    _ => {},
                                }
                                callback(Event::DeviceEvent { device_id: did, event: DeviceEvent::Motion {
                                    axis: i as u32,
                                    value: x,
                                }});
                                value = unsafe { value.offset(1) };
                            }
                        }
                        if mouse_delta != (0.0, 0.0) {
                            callback(Event::DeviceEvent { device_id: did, event: DeviceEvent::MouseMotion {
                                delta: mouse_delta,
                            }});
                        }
                        if scroll_delta != (0.0, 0.0) {
                            callback(Event::DeviceEvent { device_id: did, event: DeviceEvent::MouseWheel {
                                delta: LineDelta(scroll_delta.0, scroll_delta.1),
                            }});
                        }
                    }

                    ffi::XI_RawKeyPress | ffi::XI_RawKeyRelease => {
                        let xev: &ffi::XIRawEvent = unsafe { &*(xev.data as *const _) };

                        let state = match xev.evtype {
                            ffi::XI_RawKeyPress => Pressed,
                            ffi::XI_RawKeyRelease => Released,
                            _ => unreachable!(),
                        };

                        let device_id = xev.sourceid;
                        let keycode = xev.detail;
                        if keycode < 8 { return; }
                        let scancode = (keycode - 8) as u32;

                        let (keysym, modifiers) = self.xkb
                            .borrow()
                            .as_ref()
                            .and_then(|xkb| {
                                let keysym = xkb.get_keysym(device_id, keycode);
                                let modifiers = xkb.get_modifiers(device_id);
                                keysym.and_then(|keysym| modifiers.map(|mods| (keysym, mods)))
                            })
                            .unwrap_or_else(|| {
                                let keysym = unsafe {
                                    (self.display.xlib.XKeycodeToKeysym)(
                                        self.display.display,
                                        xev.detail as ffi::KeyCode,
                                        0,
                                    )
                                };
                                self.display.check_errors().expect("Failed to lookup raw keysym");
                                (keysym as c_uint, ModifiersState::default())
                            });

                        let virtual_keycode = events::keysym_to_element(keysym);

                        callback(Event::DeviceEvent {
                            device_id: mkdid(device_id),
                            event: DeviceEvent::Key(KeyboardInput {
                                scancode,
                                virtual_keycode,
                                state,
                                modifiers,
                            }),
                        });
                    }

                    ffi::XI_HierarchyChanged => {
                        let xev: &ffi::XIHierarchyEvent = unsafe { &*(xev.data as *const _) };
                        for info in unsafe { slice::from_raw_parts(xev.info, xev.num_info as usize) } {
                            if 0 != info.flags & (ffi::XISlaveAdded | ffi::XIMasterAdded) {
                                self.init_device(info.deviceid);
                                callback(Event::DeviceEvent { device_id: mkdid(info.deviceid), event: DeviceEvent::Added });
                            } else if 0 != info.flags & (ffi::XISlaveRemoved | ffi::XIMasterRemoved) {
                                callback(Event::DeviceEvent { device_id: mkdid(info.deviceid), event: DeviceEvent::Removed });
                                let mut devices = self.devices.borrow_mut();
                                devices.remove(&DeviceId(info.deviceid));
                            }
                        }
                    }

                    _ => {}
                }
            }

            _ => {
                if self.xkb.borrow().as_ref().map(|xkb| xkb.event_code) == Some(event_type as _) {
                    let mut xkb_borrow = self.xkb.borrow_mut();
                    let xkb = xkb_borrow.as_mut().unwrap();
                    let xkb_event: &ffi::XkbAnyEvent = unsafe { &*(xev as *const _ as *const _) };
                    match xkb_event.xkb_type {
                        ffi::XkbNewKeyboardNotify => {
                            unsafe {
                                let xkb_event: &ffi::XkbNewKeyboardNotifyEvent =
                                    &*(xkb_event as *const _ as *const _);
                                xkb.add_keyboard(xkb_event.device)
                            }.expect("Failed to create XkbState for new keyboard");
                        },
                        ffi::XkbMapNotify => {
                            unsafe {
                                let xkb_event: &ffi::XkbMapNotifyEvent =
                                    &*(xkb_event as *const _ as *const _);
                                xkb.add_keyboard(xkb_event.device)
                            }.expect("Failed to replace XkbState for new mapping");
                        },
                        ffi::XkbStateNotify => {
                            unsafe {
                                let xkb_event: &ffi::XkbStateNotifyEvent =
                                    &*(xkb_event as *const _ as *const _);
                                xkb.update(
                                    xkb_event.device,
                                    xkb_event.base_mods,
                                    xkb_event.latched_mods,
                                    xkb_event.locked_mods,
                                    xkb_event.base_group,
                                    xkb_event.latched_group,
                                    xkb_event.locked_group,
                                );
                            }
                        },
                        _ => (),
                    }
                }
            }
        }

        match self.ime_receiver.try_recv() {
            Ok((window_id, x, y)) => {
                self.ime.borrow_mut().send_xim_spot(window_id, x, y);
            },
            Err(_) => (),
        }
    }

    fn init_device(&self, device: c_int) {
        let mut devices = self.devices.borrow_mut();
        for info in DeviceInfo::get(&self.display, device).iter() {
            devices.insert(DeviceId(info.deviceid), Device::new(&self, info));
        }
    }
}

impl EventsLoopProxy {
    pub fn wakeup(&self) -> Result<(), EventsLoopClosed> {
        // Update the `EventsLoop`'s `pending_wakeup` flag.
        let display = match (self.pending_wakeup.upgrade(), self.display.upgrade()) {
            (Some(wakeup), Some(display)) => {
                wakeup.store(true, atomic::Ordering::Relaxed);
                display
            },
            _ => return Err(EventsLoopClosed),
        };

        // Push an event on the X event queue so that methods run_forever will advance.
        //
        // NOTE: This design is taken from the old `WindowProxy::wakeup` implementation. It
        // assumes that X11 is thread safe. Is this true?
        // (WARNING: it's probably not true)
        unsafe {
            util::send_client_msg(
                &display,
                self.wakeup_dummy_window,
                self.wakeup_dummy_window,
                0,
                None,
                (0, 0, 0, 0, 0),
            )
        }.flush().expect("Failed to call XSendEvent after wakeup");

        Ok(())
    }
}

struct DeviceInfo<'a> {
    display: &'a XConnection,
    info: *const ffi::XIDeviceInfo,
    count: usize,
}

impl<'a> DeviceInfo<'a> {
    fn get(display: &'a XConnection, device: c_int) -> Self {
        unsafe {
            let mut count = mem::uninitialized();
            let info = (display.xinput2.XIQueryDevice)(display.display, device, &mut count);
            DeviceInfo {
                display: display,
                info: info,
                count: count as usize,
            }
        }
    }
}

impl<'a> Drop for DeviceInfo<'a> {
    fn drop(&mut self) {
        unsafe { (self.display.xinput2.XIFreeDeviceInfo)(self.info as *mut _) };
    }
}

impl<'a> ::std::ops::Deref for DeviceInfo<'a> {
    type Target = [ffi::XIDeviceInfo];
    fn deref(&self) -> &Self::Target {
        unsafe { slice::from_raw_parts(self.info, self.count) }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(ffi::Window);

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(c_int);

pub struct Window {
    pub window: Arc<Window2>,
    display: Weak<XConnection>,
    windows: Weak<Mutex<HashMap<WindowId, WindowData>>>,
    ime_sender: Mutex<ImeSender>,
}

impl ::std::ops::Deref for Window {
    type Target = Window2;
    #[inline]
    fn deref(&self) -> &Window2 {
        &*self.window
    }
}

impl Window {
    pub fn new(
        x_events_loop: &EventsLoop,
        window: &::WindowAttributes,
        pl_attribs: &PlatformSpecificWindowBuilderAttributes
    ) -> Result<Self, CreationError> {
        let win = Arc::new(Window2::new(&x_events_loop, window, pl_attribs)?);

        x_events_loop.shared_state
            .borrow_mut()
            .insert(win.id(), Arc::downgrade(&win.shared_state));

        x_events_loop.ime
            .borrow_mut()
            .create_context(win.id().0)
            .expect("Failed to create input context");

        x_events_loop.windows.lock().insert(win.id(), WindowData {
            config: Default::default(),
            multitouch: window.multitouch,
            cursor_pos: None,
        });

        Ok(Window {
            window: win,
            windows: Arc::downgrade(&x_events_loop.windows),
            display: Arc::downgrade(&x_events_loop.display),
            ime_sender: Mutex::new(x_events_loop.ime_sender.clone()),
        })
    }

    #[inline]
    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    #[inline]
    pub fn send_xim_spot(&self, x: i16, y: i16) {
        let _ = self.ime_sender
            .lock()
            .send((self.window.id().0, x, y));
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        if let (Some(windows), Some(display)) = (self.windows.upgrade(), self.display.upgrade()) {
            if let Some(_) = windows.lock().remove(&self.window.id()) {
                unsafe {
                    (display.xlib.XDestroyWindow)(display.display, self.window.id().0);
                }
            }
        }
    }
}

/// State maintained for translating window-related events
#[derive(Debug)]
struct WindowData {
    config: WindowConfig,
    multitouch: bool,
    cursor_pos: Option<(f64, f64)>,
}

// Required by ffi members
unsafe impl Send for WindowData {}

#[derive(Debug, Default)]
struct WindowConfig {
    pub size: Option<(c_int, c_int)>,
    pub position: Option<(c_int, c_int)>,
    pub inner_position: Option<(c_int, c_int)>,
}

/// XEvents of type GenericEvent store their actual data in an XGenericEventCookie data structure. This is a wrapper to
/// extract the cookie from a GenericEvent XEvent and release the cookie data once it has been processed
struct GenericEventCookie<'a> {
    display: &'a XConnection,
    cookie: ffi::XGenericEventCookie
}

impl<'a> GenericEventCookie<'a> {
    fn from_event<'b>(display: &'b XConnection, event: ffi::XEvent) -> Option<GenericEventCookie<'b>> {
        unsafe {
            let mut cookie: ffi::XGenericEventCookie = From::from(event);
            if (display.xlib.XGetEventData)(display.display, &mut cookie) == ffi::True {
                Some(GenericEventCookie{display: display, cookie: cookie})
            } else {
                None
            }
        }
    }
}

impl<'a> Drop for GenericEventCookie<'a> {
    fn drop(&mut self) {
        unsafe {
            let xlib = &self.display.xlib;
            (xlib.XFreeEventData)(self.display.display, &mut self.cookie);
        }
    }
}

#[derive(Debug, Copy, Clone)]
struct XExtension {
    opcode: c_int,
    first_event_id: c_int,
    first_error_id: c_int,
}

fn mkwid(w: ffi::Window) -> ::WindowId { ::WindowId(::platform::WindowId::X(WindowId(w))) }
fn mkdid(w: c_int) -> ::DeviceId { ::DeviceId(::platform::DeviceId::X(DeviceId(w))) }

#[derive(Debug)]
struct Device {
    name: String,
    scroll_axes: Vec<(i32, ScrollAxis)>,
    // For master devices, this is the paired device (pointer <-> keyboard).
    // For slave devices, this is the master.
    attachment: c_int,
}

#[derive(Debug, Copy, Clone)]
struct ScrollAxis {
    increment: f64,
    orientation: ScrollOrientation,
    position: f64,
}

#[derive(Debug, Copy, Clone)]
enum ScrollOrientation {
    Vertical,
    Horizontal,
}

impl Device {
    fn new(el: &EventsLoop, info: &ffi::XIDeviceInfo) -> Self {
        let name = unsafe { CStr::from_ptr(info.name).to_string_lossy() };
        let mut scroll_axes = Vec::new();

        let is_keyboard = info._use == ffi::XISlaveKeyboard || info._use == ffi::XIMasterKeyboard;
        if is_keyboard && el.xkb.borrow().is_some() {
            el.xkb
                .borrow_mut()
                .as_mut()
                .unwrap()
                .add_keyboard(info.deviceid)
                .expect("Failed to initialize XkbState for keyboard");
        }

        if Device::physical_device(info) {
            // Register for global raw events
            let mask = ffi::XI_RawMotionMask
                | ffi::XI_RawButtonPressMask
                | ffi::XI_RawButtonReleaseMask
                | ffi::XI_RawKeyPressMask
                | ffi::XI_RawKeyReleaseMask;
            unsafe {
                util::select_xinput_events(
                    &el.display,
                    el.root,
                    info.deviceid,
                    mask,
                )
            }.queue(); // The request buffer is flushed when we poll for events

            // Identify scroll axes
            for class_ptr in Device::classes(info) {
                let class = unsafe { &**class_ptr };
                match class._type {
                    ffi::XIScrollClass => {
                        let info = unsafe { mem::transmute::<&ffi::XIAnyClassInfo, &ffi::XIScrollClassInfo>(class) };
                        scroll_axes.push((info.number, ScrollAxis {
                            increment: info.increment,
                            orientation: match info.scroll_type {
                                ffi::XIScrollTypeHorizontal => ScrollOrientation::Horizontal,
                                ffi::XIScrollTypeVertical => ScrollOrientation::Vertical,
                                _ => { unreachable!() }
                            },
                            position: 0.0,
                        }));
                    }
                    _ => {}
                }
            }
        }

        let mut device = Device {
            name: name.into_owned(),
            scroll_axes: scroll_axes,
            attachment: info.attachment,
        };
        device.reset_scroll_position(info);
        device
    }

    fn reset_scroll_position(&mut self, info: &ffi::XIDeviceInfo) {
        if Device::physical_device(info) {
            for class_ptr in Device::classes(info) {
                let class = unsafe { &**class_ptr };
                match class._type {
                    ffi::XIValuatorClass => {
                        let info = unsafe { mem::transmute::<&ffi::XIAnyClassInfo, &ffi::XIValuatorClassInfo>(class) };
                        if let Some(&mut (_, ref mut axis)) = self.scroll_axes.iter_mut().find(|&&mut (axis, _)| axis == info.number) {
                            axis.position = info.value;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    #[inline]
    fn physical_device(info: &ffi::XIDeviceInfo) -> bool {
        info._use == ffi::XISlaveKeyboard || info._use == ffi::XISlavePointer || info._use == ffi::XIFloatingSlave
    }

    #[inline]
    fn classes(info: &ffi::XIDeviceInfo) -> &[*const ffi::XIAnyClassInfo] {
        unsafe { slice::from_raw_parts(info.classes as *const *const ffi::XIAnyClassInfo, info.num_classes as usize) }
    }
}
