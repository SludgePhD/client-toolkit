use std::{
    any::Any,
    fmt::{self, Display, Formatter},
    sync::{Arc, Mutex, Weak},
};

use wayland_client::{
    protocol::wl_output::{self, Subpixel, Transform},
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::{self, ZxdgOutputManagerV1},
    zxdg_output_v1,
};

use crate::{
    globals::GlobalData,
    registry::{GlobalProxy, ProvidesRegistryState, RegistryHandler},
};

pub trait OutputHandler: Sized {
    fn output_state(&mut self) -> &mut OutputState;

    /// A new output has been advertised.
    fn new_output(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    );

    /// An existing output has changed.
    fn update_output(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    );

    /// An output is no longer advertised.
    ///
    /// The info passed to this function was the state of the output before destruction.
    fn output_destroyed(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    );
}

type ScaleWatcherFn =
    dyn Fn(&mut dyn Any, &Connection, &dyn Any, &wl_output::WlOutput) + Send + Sync;

pub struct OutputState {
    xdg: GlobalProxy<ZxdgOutputManagerV1>,
    outputs: Vec<OutputInner>,
    callbacks: Vec<Weak<ScaleWatcherFn>>,
}

impl fmt::Debug for OutputState {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("OutputState")
            .field("xdg", &self.xdg)
            .field("outputs", &self.outputs)
            .field("callbacks", &self.callbacks.len())
            .finish()
    }
}

pub struct ScaleWatcherHandle(Arc<ScaleWatcherFn>);

impl fmt::Debug for ScaleWatcherHandle {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("ScaleWatcherHandle").finish_non_exhaustive()
    }
}

impl OutputState {
    pub fn new() -> OutputState {
        OutputState { xdg: GlobalProxy::new(), outputs: vec![], callbacks: vec![] }
    }

    /// Returns an iterator over all outputs.
    pub fn outputs(&self) -> impl Iterator<Item = wl_output::WlOutput> {
        self.outputs.iter().map(|output| &output.wl_output).cloned().collect::<Vec<_>>().into_iter()
    }

    /// Returns information about an output.
    ///
    /// This may be none if the output has been destroyed or the compositor has not sent information about the
    /// output yet.
    pub fn info(&self, output: &wl_output::WlOutput) -> Option<OutputInfo> {
        self.outputs
            .iter()
            .find(|inner| &inner.wl_output == output)
            .and_then(|inner| inner.current_info.clone())
    }

    pub fn add_scale_watcher<F, D>(data: &mut D, f: F) -> ScaleWatcherHandle
    where
        D: OutputHandler + 'static,
        F: Fn(&mut D, &Connection, &QueueHandle<D>, &wl_output::WlOutput) + Send + Sync + 'static,
    {
        let state = data.output_state();
        let rv = ScaleWatcherHandle(Arc::new(move |data, conn, qh, output| {
            if let (Some(data), Some(qh)) = (data.downcast_mut(), qh.downcast_ref()) {
                f(data, conn, qh, output);
            }
        }));
        state.callbacks.retain(|f| f.upgrade().is_some());
        state.callbacks.push(Arc::downgrade(&rv.0));
        rv
    }

    fn setup<D>(&mut self, wl_output: wl_output::WlOutput, qh: &QueueHandle<D>)
    where
        D: Dispatch<zxdg_output_v1::ZxdgOutputV1, OutputData> + 'static,
    {
        let data = wl_output.data::<OutputData>().unwrap().clone();

        let pending_info = data.0.lock().unwrap().clone();
        let name = pending_info.id;

        let version = wl_output.version();
        let pending_xdg = version < 4 && self.xdg.get().is_ok();

        let xdg_output = if pending_xdg {
            let xdg = self.xdg.get().unwrap();

            Some(xdg.get_xdg_output(&wl_output, qh, data).unwrap())
        } else {
            None
        };

        let inner = OutputInner {
            name,
            wl_output,
            xdg_output,
            just_created: true,
            // wl_output::done was added in version 2.
            // If we have an output at version 1, assume the data was already sent.
            current_info: if version > 1 { None } else { Some(OutputInfo::new(name)) },

            pending_info,
            pending_wl: true,
            pending_xdg,
        };

        self.outputs.push(inner);
    }
}

#[derive(Debug, Clone)]
pub struct OutputData(Arc<Mutex<OutputInfo>>);

impl OutputData {
    pub fn new(name: u32) -> OutputData {
        OutputData(Arc::new(Mutex::new(OutputInfo::new(name))))
    }

    pub fn scale_factor(&self) -> i32 {
        let guard = self.0.lock().unwrap();

        guard.scale_factor
    }
}

#[derive(Debug, Clone)]
pub struct Mode {
    /// Number of pixels of this mode in format `(width, height)`
    ///
    /// for example `(1920, 1080)`
    pub dimensions: (i32, i32),

    /// Refresh rate for this mode.
    ///
    /// The refresh rate is specified in terms of millihertz (mHz). To convert approximately to Hertz,
    /// divide the value by 1000.
    ///
    /// This value could be zero if an output has no correct refresh rate, such as a virtual output.
    pub refresh_rate: i32,

    /// Whether this is the current mode for this output.
    ///
    /// Per the Wayland protocol, non-current modes are deprecated and clients should not rely on deprecated
    /// modes.
    pub current: bool,

    /// Whether this is the preferred mode for this output.
    pub preferred: bool,
}

impl Display for Mode {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if self.current {
            write!(f, "(current) ")?;
        }

        if self.preferred {
            write!(f, "(preferred) ")?;
        }

        write!(
            f,
            "{}×{}px @ {}.{:03} Hz",
            self.dimensions.0,
            self.dimensions.1,
            // Print the refresh rate in hertz since it is more familiar unit.
            self.refresh_rate / 1000,
            self.refresh_rate % 1000
        )
    }
}

/// Information about an output.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct OutputInfo {
    /// The id of the output.
    ///
    /// This corresponds to the global `name` of the wl_output.
    pub id: u32,

    /// The model name of this output as advertised by the server.
    pub model: String,

    /// The make name of this output as advertised by the server.
    pub make: String,

    /// Location of the top-left corner of this output in compositor space.
    ///
    /// Note that the compositor may decide to always report (0,0) if it decides clients are not allowed to
    /// know this information.
    pub location: (i32, i32),

    /// Physical dimensions of this output, in millimeters.
    ///
    /// This value may be set to (0, 0) if a physical size does not make sense for the output (e.g. projectors
    /// and virtual outputs).
    pub physical_size: (i32, i32),

    /// The subpixel layout for this output.
    pub subpixel: Subpixel,

    /// The current transformation applied to this output
    ///
    /// You can pre-render your buffers taking this information into account and advertising it via
    /// `wl_buffer.set_transform` for better performance.
    pub transform: Transform,

    /// The scaling factor of this output
    ///
    /// Any buffer whose scaling factor does not match the one of the output it is displayed on will be
    /// rescaled accordingly.
    ///
    /// For example, a buffer of scaling factor 1 will be doubled in size if the output scaling factor is 2.
    ///
    /// You can pre-render your buffers taking this information into account and advertising it via
    /// `wl_surface.set_buffer_scale` so you may advertise a higher detail image.
    pub scale_factor: i32,

    /// Possible modes for an output.
    pub modes: Vec<Mode>,

    /// The name of the this output as advertised by the surface.
    ///
    /// Examples of names include 'HDMI-A-1', 'WL-1', 'X11-1', etc. However, do not assume that the name is a
    /// reflection of an underlying DRM connector, X11 connection, etc.
    ///
    /// Compositors are not required to provide a name for the output and the value may be [`None`].
    ///
    /// The name will be [`None`] if the compositor does not support version 4 of the wl-output protocol or
    /// version 2 of the zxdg-output-v1 protocol.
    pub name: Option<String>,

    /// The description of this output as advertised by the server
    ///
    /// The description is a UTF-8 string with no convention defined for its contents. The description is not
    /// guaranteed to be unique among all wl_output globals. Examples might include 'Foocorp 11" Display' or
    /// 'Virtual X11 output via :1'.
    ///
    /// Compositors are not required to provide a description of the output and the value may be [`None`].
    ///
    /// The value will be [`None`] if the compositor does not support version 4 of the wl-output
    /// protocol, version 2 of the zxdg-output-v1 protocol.
    pub description: Option<String>,
}

#[macro_export]
macro_rules! delegate_output {
    ($ty: ty) => {
        $crate::reexports::client::delegate_dispatch!($ty: [
            $crate::reexports::client::protocol::wl_output::WlOutput: $crate::output::OutputData,
            $crate::reexports::protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1: $crate::globals::GlobalData,
            $crate::reexports::protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::ZxdgOutputV1: $crate::output::OutputData,
        ] => $crate::output::OutputState);
    };
}

impl<D> Dispatch<wl_output::WlOutput, OutputData, D> for OutputState
where
    D: Dispatch<wl_output::WlOutput, OutputData> + OutputHandler + 'static,
{
    fn event(
        state: &mut D,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        data: &OutputData,
        conn: &Connection,
        qh: &QueueHandle<D>,
    ) {
        match event {
            wl_output::Event::Geometry {
                x,
                y,
                physical_width,
                physical_height,
                subpixel,
                make,
                model,
                transform,
            } => {
                let inner = state
                    .output_state()
                    .outputs
                    .iter_mut()
                    .find(|inner| &inner.wl_output == output)
                    .expect("Received event for dead output");

                inner.pending_info.location = (x, y);
                inner.pending_info.physical_size = (physical_width, physical_height);
                inner.pending_info.subpixel = match subpixel {
                    WEnum::Value(subpixel) => subpixel,
                    WEnum::Unknown(_) => todo!("Warn about invalid subpixel value"),
                };
                inner.pending_info.make = make;
                inner.pending_info.model = model;
                inner.pending_info.transform = match transform {
                    WEnum::Value(subpixel) => subpixel,
                    WEnum::Unknown(_) => todo!("Warn about invalid transform value"),
                };
                inner.pending_wl = true;
            }

            wl_output::Event::Mode { flags, width, height, refresh } => {
                let inner = state
                    .output_state()
                    .outputs
                    .iter_mut()
                    .find(|inner| &inner.wl_output == output)
                    .expect("Received event for dead output");

                if let Some((index, _)) =
                    inner.pending_info.modes.iter().enumerate().find(|(_, mode)| {
                        mode.dimensions == (width, height) && mode.refresh_rate == refresh
                    })
                {
                    // We found a match, remove the old mode.
                    inner.pending_info.modes.remove(index);
                }

                let flags = match flags {
                    WEnum::Value(flags) => flags,
                    WEnum::Unknown(_) => panic!("Invalid flags"),
                };

                let current = flags.contains(wl_output::Mode::Current);
                let preferred = flags.contains(wl_output::Mode::Preferred);

                // Now create the new mode.
                inner.pending_info.modes.push(Mode {
                    dimensions: (width, height),
                    refresh_rate: refresh,
                    current,
                    preferred,
                });

                let index = inner.pending_info.modes.len() - 1;

                // Any mode that isn't current is deprecated, let's deprecate any existing modes that may be
                // marked as current.
                //
                // If a new mode is advertised as preferred, then mark the existing preferred mode as not.
                inner.pending_info.modes.iter_mut().enumerate().for_each(|(mode_index, mode)| {
                    if index != mode_index {
                        // This mode is no longer preferred.
                        if mode.preferred && preferred {
                            mode.preferred = false;
                        }

                        // This mode is no longer current.
                        if mode.current && current {
                            mode.current = false;
                        }
                    }
                });

                inner.pending_wl = true;
            }

            wl_output::Event::Scale { factor } => {
                let inner = state
                    .output_state()
                    .outputs
                    .iter_mut()
                    .find(|inner| &inner.wl_output == output)
                    .expect("Received event for dead output");

                inner.pending_info.scale_factor = factor;
                inner.pending_wl = true;
            }

            wl_output::Event::Name { name } => {
                let inner = state
                    .output_state()
                    .outputs
                    .iter_mut()
                    .find(|inner| &inner.wl_output == output)
                    .expect("Received event for dead output");

                inner.pending_info.name = Some(name);
                inner.pending_wl = true;
            }

            wl_output::Event::Description { description } => {
                let inner = state
                    .output_state()
                    .outputs
                    .iter_mut()
                    .find(|inner| &inner.wl_output == output)
                    .expect("Received event for dead output");

                inner.pending_info.description = Some(description);
                inner.pending_wl = true;
            }

            wl_output::Event::Done => {
                let inner = state
                    .output_state()
                    .outputs
                    .iter_mut()
                    .find(|inner| &inner.wl_output == output)
                    .expect("Received event for dead output");

                let info = inner.pending_info.clone();
                inner.current_info = Some(info.clone());
                inner.pending_wl = false;

                if inner
                    .xdg_output
                    .as_ref()
                    .map(Proxy::version)
                    .map(|v| v > 3) // version 3 of xdg_output deprecates xdg_output::done
                    .unwrap_or(false)
                {
                    inner.pending_xdg = false;
                }

                // Set the user data, see if we need to run scale callbacks
                let run_callbacks = data.set(info);

                if inner.just_created {
                    inner.just_created = false;
                    state.new_output(conn, qh, output.clone());
                } else {
                    state.update_output(conn, qh, output.clone());
                }

                if run_callbacks {
                    let callbacks = state.output_state().callbacks.clone();
                    for cb in callbacks {
                        if let Some(cb) = cb.upgrade() {
                            cb(state, conn, qh, output);
                        }
                    }
                }
            }

            _ => unreachable!(),
        }
    }
}

impl<D> Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, GlobalData, D> for OutputState
where
    D: Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, GlobalData> + OutputHandler,
{
    fn event(
        _: &mut D,
        _: &zxdg_output_manager_v1::ZxdgOutputManagerV1,
        _: zxdg_output_manager_v1::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<D>,
    ) {
        unreachable!("zxdg_output_manager_v1 has no events")
    }
}

impl<D> Dispatch<zxdg_output_v1::ZxdgOutputV1, OutputData, D> for OutputState
where
    D: Dispatch<zxdg_output_v1::ZxdgOutputV1, OutputData> + OutputHandler,
{
    fn event(
        state: &mut D,
        output: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        data: &OutputData,
        conn: &Connection,
        qh: &QueueHandle<D>,
    ) {
        match event {
            // Already provided by wl_output
            zxdg_output_v1::Event::LogicalPosition { x: _, y: _ } => (),
            zxdg_output_v1::Event::LogicalSize { width: _, height: _ } => (),

            zxdg_output_v1::Event::Name { name } => {
                let inner = state
                    .output_state()
                    .outputs
                    .iter_mut()
                    .find(|inner| inner.xdg_output.as_ref() == Some(output))
                    .expect("Received event for dead output");

                inner.pending_info.name = Some(name);
                inner.pending_xdg = true;
            }

            zxdg_output_v1::Event::Description { description } => {
                let inner = state
                    .output_state()
                    .outputs
                    .iter_mut()
                    .find(|inner| inner.xdg_output.as_ref() == Some(output))
                    .expect("Received event for dead output");

                inner.pending_info.description = Some(description);
                inner.pending_xdg = true;
            }

            zxdg_output_v1::Event::Done => {
                // This event is deprecated starting in version 3, wl_output::done should be sent instead.
                if output.version() < 3 {
                    let inner = state
                        .output_state()
                        .outputs
                        .iter_mut()
                        .find(|inner| inner.xdg_output.as_ref() == Some(output))
                        .expect("Received event for dead output");

                    let info = inner.pending_info.clone();
                    inner.current_info = Some(info.clone());
                    inner.pending_xdg = false;

                    // Set the user data
                    data.set(info);

                    let pending_wl = inner.pending_wl;
                    let just_created = inner.just_created;
                    let output = inner.wl_output.clone();

                    if just_created {
                        inner.just_created = false;
                    }

                    if !pending_wl {
                        if just_created {
                            state.new_output(conn, qh, output);
                        } else {
                            state.update_output(conn, qh, output);
                        }
                    }
                }
            }

            _ => unreachable!(),
        }
    }
}

impl<D> RegistryHandler<D> for OutputState
where
    D: Dispatch<wl_output::WlOutput, OutputData>
        + Dispatch<zxdg_output_v1::ZxdgOutputV1, OutputData>
        + Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, GlobalData>
        + OutputHandler
        + ProvidesRegistryState
        + 'static,
{
    fn ready(data: &mut D, _: &Connection, qh: &QueueHandle<D>) {
        let outputs: Vec<wl_output::WlOutput> =
            data.registry().bind_all(qh, 1..=4, OutputData::new).expect("Failed to bind global");

        // Only bind xdg output manager if it's needed
        let xdg = if outputs.iter().any(|o| o.version() < 4) {
            data.registry().bind_one(qh, 1..=3, GlobalData).into()
        } else {
            GlobalProxy::NotReady
        };

        let output_state = data.output_state();
        output_state.xdg = xdg;

        for wl_output in outputs {
            output_state.setup(wl_output, qh);
        }
    }

    fn new_global(
        data: &mut D,
        _: &Connection,
        qh: &QueueHandle<D>,
        name: u32,
        interface: &str,
        version: u32,
    ) {
        if interface == "wl_output" {
            // Lazily bind xdg output manager if it's needed
            if version < 4 && matches!(data.output_state().xdg, GlobalProxy::NotReady) {
                data.output_state().xdg = data.registry().bind_one(qh, 1..=3, GlobalData).into();
            }

            let output = data
                .registry()
                .bind_specific(qh, name, 1..=4, OutputData::new(name))
                .expect("Failed to bind global");
            data.output_state().setup(output, qh);
        }
    }

    fn remove_global(
        data: &mut D,
        conn: &Connection,
        qh: &QueueHandle<D>,
        name: u32,
        interface: &str,
    ) {
        if interface == "wl_output" {
            let output = data
                .output_state()
                .outputs
                .iter()
                .position(|o| o.name == name)
                .expect("Removed non-existing output");

            let wl_output = data.output_state().outputs[output].wl_output.clone();
            data.output_destroyed(conn, qh, wl_output);

            let output = data.output_state().outputs.remove(output);
            if let Some(xdg_output) = &output.xdg_output {
                xdg_output.destroy();
            }
            output.wl_output.release();
        }
    }
}

impl OutputInfo {
    fn new(id: u32) -> OutputInfo {
        OutputInfo {
            id,
            model: String::new(),
            make: String::new(),
            location: (0, 0),
            physical_size: (0, 0),
            subpixel: Subpixel::Unknown,
            transform: Transform::Normal,
            scale_factor: 1,
            modes: vec![],
            name: None,
            description: None,
        }
    }
}

impl OutputData {
    pub(crate) fn set(&self, info: OutputInfo) -> bool {
        let mut guard = self.0.lock().unwrap();

        let rv = guard.scale_factor != info.scale_factor;

        *guard = info;

        rv
    }
}

#[derive(Debug)]
struct OutputInner {
    /// The name of the wl_output global.
    name: u32,
    wl_output: wl_output::WlOutput,
    xdg_output: Option<zxdg_output_v1::ZxdgOutputV1>,
    /// Whether this output was just created and has not an event yet.
    just_created: bool,

    current_info: Option<OutputInfo>,
    pending_info: OutputInfo,
    pending_wl: bool,
    pending_xdg: bool,
}
