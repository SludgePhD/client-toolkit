use std::{mem, sync::Mutex};
use wayland_backend::smallvec::SmallVec;

use wayland_client::{
    protocol::{wl_pointer, wl_surface},
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};

use super::SeatState;

/* From linux/input-event-codes.h - the buttons usually used by mice */
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
/// The fourth non-scroll button, which is often used as "back" in web browsers.
pub const BTN_SIDE: u32 = 0x113;
/// The fifth non-scroll button, which is often used as "forward" in web browsers.
pub const BTN_EXTRA: u32 = 0x114;

/// See also [`BTN_EXTRA`].
pub const BTN_FORWARD: u32 = 0x115;
/// See also [`BTN_SIDE`].
pub const BTN_BACK: u32 = 0x116;
pub const BTN_TASK: u32 = 0x117;

/// Describes a scroll along one axis
#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub struct AxisScroll {
    /// The scroll measured in pixels.
    pub absolute: f64,

    /// The scroll measured in steps.
    ///
    /// Note: this might always be zero if the scrolling is due to a touchpad or other continuous
    /// source.
    pub discrete: i32,

    /// The scroll was stopped.
    ///
    /// Generally this is encountered when hardware indicates the end of some continuous scrolling.
    pub stop: bool,
}

impl AxisScroll {
    /// Returns true if there was no movement along this axis.
    pub fn is_none(&self) -> bool {
        *self == Self::default()
    }

    fn merge(&mut self, other: &Self) {
        self.absolute += other.absolute;
        self.discrete += other.discrete;
        self.stop |= other.stop;
    }
}

/// A single pointer event.
#[derive(Debug, Clone)]
pub struct PointerEvent {
    pub surface: wl_surface::WlSurface,
    pub position: (f64, f64),
    pub kind: PointerEventKind,
}

#[derive(Debug, Clone)]
pub enum PointerEventKind {
    Enter {
        serial: u32,
    },
    Leave {
        serial: u32,
    },
    Motion {
        time: u32,
    },
    Press {
        time: u32,
        button: u32,
        serial: u32,
    },
    Release {
        time: u32,
        button: u32,
        serial: u32,
    },
    Axis {
        time: u32,
        horizontal: AxisScroll,
        vertical: AxisScroll,
        source: Option<wl_pointer::AxisSource>,
    },
}

pub trait PointerHandler: Sized {
    /// One or more pointer events are available.
    ///
    /// Multiple related events may be grouped together in a single frame.  Some examples:
    ///
    /// - A drag that terminates outside the surface may send the Release and Leave events as one frame
    /// - Movement from one surface to another may send the Enter and Leave events in one frame
    fn pointer_frame(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    );
}

#[derive(Debug, Default)]
pub struct PointerData {
    inner: Mutex<PointerDataInner>,
}

pub trait PointerDataExt: Send + Sync {
    fn pointer_data(&self) -> &PointerData;
}

impl PointerDataExt for PointerData {
    fn pointer_data(&self) -> &PointerData {
        self
    }
}

#[macro_export]
macro_rules! delegate_pointer {
    ($ty: ty) => {
        $crate::reexports::client::delegate_dispatch!($ty:
            [
                $crate::reexports::client::protocol::wl_pointer::WlPointer: $crate::seat::pointer::PointerData
            ] => $crate::seat::SeatState
        );
    };
    ($ty: ty, pointer: [$($pointer_data:ty),* $(,)?]) => {
        $crate::reexports::client::delegate_dispatch!($ty:
            [
                $(
                    $crate::reexports::client::protocol::wl_pointer::WlPointer: $pointer_data,
                )*
            ] => $crate::seat::SeatState
        );
    };
}

#[derive(Debug, Default)]
pub(crate) struct PointerDataInner {
    /// Surface the pointer most recently entered
    surface: Option<wl_surface::WlSurface>,
    /// Position relative to the surface
    position: (f64, f64),

    /// List of pending events.  Only used for version >= 5.
    pending: SmallVec<[PointerEvent; 3]>,
}

impl<D, U> Dispatch<wl_pointer::WlPointer, U, D> for SeatState
where
    D: Dispatch<wl_pointer::WlPointer, U> + PointerHandler,
    U: PointerDataExt,
{
    fn event(
        data: &mut D,
        pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        udata: &U,
        conn: &Connection,
        qh: &QueueHandle<D>,
    ) {
        let udata = udata.pointer_data();
        let mut guard = udata.inner.lock().unwrap();
        let mut leave_surface = None;
        let kind = match event {
            wl_pointer::Event::Enter { surface, surface_x, surface_y, serial } => {
                guard.surface = Some(surface);
                guard.position = (surface_x, surface_y);

                PointerEventKind::Enter { serial }
            }

            wl_pointer::Event::Leave { surface, serial } => {
                if guard.surface.as_ref() == Some(&surface) {
                    guard.surface = None;
                }
                leave_surface = Some(surface);

                PointerEventKind::Leave { serial }
            }

            wl_pointer::Event::Motion { time, surface_x, surface_y } => {
                guard.position = (surface_x, surface_y);

                PointerEventKind::Motion { time }
            }

            wl_pointer::Event::Button { time, button, state, serial } => match state {
                WEnum::Value(wl_pointer::ButtonState::Pressed) => {
                    PointerEventKind::Press { time, button, serial }
                }
                WEnum::Value(wl_pointer::ButtonState::Released) => {
                    PointerEventKind::Release { time, button, serial }
                }
                WEnum::Unknown(unknown) => {
                    log::warn!(target: "sctk", "{}: invalid pointer button state: {:x}", pointer.id(), unknown);
                    return;
                }
                _ => unreachable!(),
            },

            // Axis logical events.
            wl_pointer::Event::Axis { time, axis, value } => match axis {
                WEnum::Value(axis) => {
                    let (mut horizontal, mut vertical) = <(AxisScroll, AxisScroll)>::default();
                    match axis {
                        wl_pointer::Axis::VerticalScroll => {
                            vertical.absolute = value;
                        }
                        wl_pointer::Axis::HorizontalScroll => {
                            horizontal.absolute = value;
                        }
                        _ => unreachable!(),
                    };

                    PointerEventKind::Axis { time, horizontal, vertical, source: None }
                }
                WEnum::Unknown(unknown) => {
                    log::warn!(target: "sctk", "{}: invalid pointer axis: {:x}", pointer.id(), unknown);
                    return;
                }
            },

            wl_pointer::Event::AxisSource { axis_source } => match axis_source {
                WEnum::Value(source) => PointerEventKind::Axis {
                    horizontal: AxisScroll::default(),
                    vertical: AxisScroll::default(),
                    source: Some(source),
                    time: 0,
                },
                WEnum::Unknown(unknown) => {
                    log::warn!(target: "sctk", "unknown pointer axis source: {:x}", unknown);
                    return;
                }
            },

            wl_pointer::Event::AxisStop { time, axis } => match axis {
                WEnum::Value(axis) => {
                    let (mut horizontal, mut vertical) = <(AxisScroll, AxisScroll)>::default();
                    match axis {
                        wl_pointer::Axis::VerticalScroll => vertical.stop = true,
                        wl_pointer::Axis::HorizontalScroll => horizontal.stop = true,

                        _ => unreachable!(),
                    }

                    PointerEventKind::Axis { time, horizontal, vertical, source: None }
                }

                WEnum::Unknown(unknown) => {
                    log::warn!(target: "sctk", "{}: invalid pointer axis: {:x}", pointer.id(), unknown);
                    return;
                }
            },

            wl_pointer::Event::AxisDiscrete { axis, discrete } => match axis {
                WEnum::Value(axis) => {
                    let (mut horizontal, mut vertical) = <(AxisScroll, AxisScroll)>::default();
                    match axis {
                        wl_pointer::Axis::VerticalScroll => {
                            vertical.discrete = discrete;
                        }

                        wl_pointer::Axis::HorizontalScroll => {
                            horizontal.discrete = discrete;
                        }

                        _ => unreachable!(),
                    };

                    PointerEventKind::Axis { time: 0, horizontal, vertical, source: None }
                }

                WEnum::Unknown(unknown) => {
                    log::warn!(target: "sctk", "{}: invalid pointer axis: {:x}", pointer.id(), unknown);
                    return;
                }
            },

            wl_pointer::Event::Frame => {
                let pending = mem::take(&mut guard.pending);
                drop(guard);
                if !pending.is_empty() {
                    data.pointer_frame(conn, qh, pointer, &pending);
                }
                return;
            }

            _ => unreachable!(),
        };

        let surface = match (leave_surface, &guard.surface) {
            (Some(surface), _) => surface,
            (None, Some(surface)) => surface.clone(),
            (None, None) => {
                log::warn!(target: "sctk", "{}: got pointer event {:?} without an entered surface", pointer.id(), kind);
                return;
            }
        };

        let event = PointerEvent { surface, position: guard.position, kind };

        if pointer.version() < 5 {
            drop(guard);
            // No Frame events, send right away
            data.pointer_frame(conn, qh, pointer, &[event]);
        } else {
            // Merge a new Axis event with the previous event to create an event with more
            // information and potentially diagonal scrolling.
            if let (
                Some(PointerEvent {
                    kind:
                        PointerEventKind::Axis { time: ot, horizontal: oh, vertical: ov, source: os },
                    ..
                }),
                PointerEvent {
                    kind:
                        PointerEventKind::Axis { time: nt, horizontal: nh, vertical: nv, source: ns },
                    ..
                },
            ) = (guard.pending.last_mut(), &event)
            {
                // A time of 0 is "don't know", so avoid using it if possible.
                if *ot == 0 {
                    *ot = *nt;
                }
                oh.merge(nh);
                ov.merge(nv);
                *os = os.or(*ns);
                return;
            }

            guard.pending.push(event);
        }
    }
}
