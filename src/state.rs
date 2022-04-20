use {
    crate::{
        async_engine::{AsyncEngine, SpawnedFuture},
        backend::{
            Backend, BackendEvent, Connector, ConnectorId, ConnectorIds, InputDevice,
            InputDeviceId, InputDeviceIds, MonitorInfo,
        },
        cli::RunArgs,
        client::{Client, Clients, SerialRange, NUM_CACHED_SERIAL_RANGES},
        config::ConfigProxy,
        cursor::ServerCursors,
        dbus::Dbus,
        event_loop::EventLoop,
        forker::ForkerProxy,
        globals::{Globals, GlobalsError, WaylandGlobal},
        ifs::{
            wl_seat::{SeatIds, WlSeatGlobal},
            wl_surface::{
                zwp_idle_inhibitor_v1::{IdleInhibitorId, IdleInhibitorIds, ZwpIdleInhibitorV1},
                NoneSurfaceExt,
            },
        },
        logger::Logger,
        rect::Rect,
        render::RenderContext,
        theme::Theme,
        tree::{
            ContainerNode, ContainerSplit, DisplayNode, FloatNode, Node, NodeIds, NodeVisitorBase,
            OutputNode, SizedNode, WorkspaceNode,
        },
        utils::{
            asyncevent::AsyncEvent, clonecell::CloneCell, copyhashmap::CopyHashMap,
            errorfmt::ErrorFmt, fdcloser::FdCloser, linkedlist::LinkedList, numcell::NumCell,
            queue::AsyncQueue, run_toplevel::RunToplevel,
        },
        wheel::Wheel,
        xkbcommon::{XkbContext, XkbKeymap},
        xwayland::{self, XWaylandEvent},
    },
    ahash::AHashMap,
    jay_config::Direction,
    std::{
        cell::{Cell, RefCell},
        num::Wrapping,
        ops::Deref,
        rc::Rc,
        sync::Arc,
        time::Duration,
    },
};

pub struct State {
    pub xkb_ctx: XkbContext,
    pub backend: CloneCell<Rc<dyn Backend>>,
    pub forker: CloneCell<Option<Rc<ForkerProxy>>>,
    pub default_keymap: Rc<XkbKeymap>,
    pub eng: Rc<AsyncEngine>,
    pub el: Rc<EventLoop>,
    pub render_ctx: CloneCell<Option<Rc<RenderContext>>>,
    pub cursors: CloneCell<Option<Rc<ServerCursors>>>,
    pub wheel: Rc<Wheel>,
    pub clients: Clients,
    pub globals: Globals,
    pub connector_ids: ConnectorIds,
    pub seat_ids: SeatIds,
    pub idle_inhibitor_ids: IdleInhibitorIds,
    pub input_device_ids: InputDeviceIds,
    pub node_ids: NodeIds,
    pub root: Rc<DisplayNode>,
    pub workspaces: CopyHashMap<String, Rc<WorkspaceNode>>,
    pub dummy_output: CloneCell<Option<Rc<OutputNode>>>,
    pub backend_events: AsyncQueue<BackendEvent>,
    pub input_device_handlers: RefCell<AHashMap<InputDeviceId, InputDeviceData>>,
    pub seat_queue: LinkedList<Rc<WlSeatGlobal>>,
    pub slow_clients: AsyncQueue<Rc<Client>>,
    pub none_surface_ext: Rc<NoneSurfaceExt>,
    pub tree_changed_sent: Cell<bool>,
    pub config: CloneCell<Option<Rc<ConfigProxy>>>,
    pub theme: Theme,
    pub pending_container_layout: AsyncQueue<Rc<ContainerNode>>,
    pub pending_container_render_data: AsyncQueue<Rc<ContainerNode>>,
    pub pending_float_layout: AsyncQueue<Rc<FloatNode>>,
    pub pending_float_titles: AsyncQueue<Rc<FloatNode>>,
    pub dbus: Dbus,
    pub fdcloser: Arc<FdCloser>,
    pub logger: Arc<Logger>,
    pub connectors: CopyHashMap<ConnectorId, Rc<ConnectorData>>,
    pub outputs: CopyHashMap<ConnectorId, Rc<OutputData>>,
    pub status: CloneCell<Rc<String>>,
    pub idle: IdleState,
    pub run_args: RunArgs,
    pub xwayland: XWaylandState,
    pub socket_path: CloneCell<Rc<String>>,
    pub serial: NumCell<Wrapping<u32>>,
    pub run_toplevel: Rc<RunToplevel>,
}

pub struct XWaylandState {
    pub enabled: Cell<bool>,
    pub handler: RefCell<Option<SpawnedFuture<()>>>,
    pub queue: Rc<AsyncQueue<XWaylandEvent>>,
}

pub struct IdleState {
    pub input: Cell<bool>,
    pub change: AsyncEvent,
    pub timeout: Cell<Duration>,
    pub timeout_changed: Cell<bool>,
    pub inhibitors: CopyHashMap<IdleInhibitorId, Rc<ZwpIdleInhibitorV1>>,
    pub inhibitors_changed: Cell<bool>,
}

impl IdleState {
    pub fn set_timeout(&self, timeout: Duration) {
        self.timeout.set(timeout);
        self.timeout_changed.set(true);
        self.change.trigger();
    }

    pub fn add_inhibitor(&self, inhibitor: &Rc<ZwpIdleInhibitorV1>) {
        self.inhibitors.set(inhibitor.inhibit_id, inhibitor.clone());
        self.inhibitors_changed.set(true);
        self.change.trigger();
    }

    pub fn remove_inhibitor(&self, inhibitor: &ZwpIdleInhibitorV1) {
        self.inhibitors.remove(&inhibitor.inhibit_id);
        self.inhibitors_changed.set(true);
        self.change.trigger();
    }
}

pub struct InputDeviceData {
    pub handler: SpawnedFuture<()>,
    pub id: InputDeviceId,
    pub data: Rc<DeviceHandlerData>,
}

pub struct DeviceHandlerData {
    pub seat: CloneCell<Option<Rc<WlSeatGlobal>>>,
    pub device: Rc<dyn InputDevice>,
}

pub struct ConnectorData {
    pub connector: Rc<dyn Connector>,
    pub handler: Cell<Option<SpawnedFuture<()>>>,
    pub connected: Cell<bool>,
    pub name: String,
}

pub struct OutputData {
    pub connector: Rc<ConnectorData>,
    pub monitor_info: MonitorInfo,
    pub node: Rc<OutputNode>,
}

impl State {
    pub fn set_render_ctx(&self, ctx: &Rc<RenderContext>) {
        let cursors = match ServerCursors::load(ctx) {
            Ok(c) => Some(Rc::new(c)),
            Err(e) => {
                log::error!("Could not load the cursors: {}", ErrorFmt(e));
                None
            }
        };
        self.cursors.set(cursors);
        self.render_ctx.set(Some(ctx.clone()));

        struct Walker;
        impl NodeVisitorBase for Walker {
            fn visit_container(&mut self, node: &Rc<ContainerNode>) {
                node.schedule_compute_render_data();
                node.node_visit_children(self);
            }

            fn visit_output(&mut self, node: &Rc<OutputNode>) {
                node.update_render_data();
                node.node_visit_children(self);
            }

            fn visit_float(&mut self, node: &Rc<FloatNode>) {
                node.schedule_render_titles();
                node.node_visit_children(self);
            }
        }
        self.root.visit(&mut Walker);

        let seats = self.globals.seats.lock();
        for seat in seats.values() {
            seat.render_ctx_changed();
        }
    }

    pub fn add_global<T: WaylandGlobal>(&self, global: &Rc<T>) {
        self.globals.add_global(self, global)
    }

    pub fn remove_global<T: WaylandGlobal>(&self, global: &T) -> Result<(), GlobalsError> {
        self.globals.remove(self, global)
    }

    pub fn tree_changed(&self) {
        if self.tree_changed_sent.replace(true) {
            return;
        }
        let seats = self.globals.seats.lock();
        for seat in seats.values() {
            seat.trigger_tree_changed();
        }
    }

    pub fn map_tiled(self: &Rc<Self>, node: Rc<dyn Node>) {
        let seat = self.seat_queue.last();
        self.do_map_tiled(seat.as_deref(), node.clone());
        if let Some(seat) = seat {
            node.node_do_focus(&seat, Direction::Unspecified);
        }
    }

    fn do_map_tiled(self: &Rc<Self>, seat: Option<&Rc<WlSeatGlobal>>, node: Rc<dyn Node>) {
        let output = seat
            .map(|s| s.get_output())
            .or_else(|| self.root.outputs.lock().values().next().cloned())
            .or_else(|| self.dummy_output.get())
            .unwrap();
        let last_active = output.last_active_child();
        let last_active_parent = last_active.node_parent();
        if let Some(lap) = last_active_parent {
            if lap.node_is_container() {
                let container = lap.node_into_container().unwrap();
                container.add_child_after(last_active.deref(), node);
                return;
            }
            if lap.node_is_workspace() {
                let workspace = lap.node_into_workspace().unwrap();
                let container = ContainerNode::new(
                    self,
                    &workspace,
                    workspace.clone(),
                    last_active.clone(),
                    ContainerSplit::Horizontal,
                );
                workspace.set_container(&container);
                container.add_child_after(last_active.deref(), node);
                return;
            }
        }
        let workspace = output.ensure_workspace();
        let container = ContainerNode::new(
            self,
            &workspace,
            workspace.clone(),
            node,
            ContainerSplit::Horizontal,
        );
        workspace.set_container(&container);
    }

    pub fn map_floating(
        self: &Rc<Self>,
        node: Rc<dyn Node>,
        mut width: i32,
        mut height: i32,
        workspace: &Rc<WorkspaceNode>,
    ) {
        node.clone().node_set_workspace(workspace);
        width += 2 * self.theme.border_width.get();
        height += 2 * self.theme.border_width.get() + self.theme.title_height.get();
        let output = workspace.output.get();
        let output_rect = output.global.pos.get();
        let position = {
            let mut x1 = output_rect.x1();
            let mut y1 = output_rect.y1();
            if width < output_rect.width() {
                x1 += (output_rect.width() - width) as i32 / 2;
            } else {
                width = output_rect.width();
            }
            if height < output_rect.height() {
                y1 += (output_rect.height() - height) as i32 / 2;
            } else {
                height = output_rect.height();
            }
            Rect::new_sized(x1, y1, width, height).unwrap()
        };
        FloatNode::new(self, workspace, position, node);
    }

    pub fn show_workspace(&self, seat: &Rc<WlSeatGlobal>, name: &str) {
        let output = match self.workspaces.get(name) {
            Some(ws) => {
                let output = ws.output.get();
                let did_change = output.show_workspace(&ws);
                ws.last_active_child()
                    .node_do_focus(seat, Direction::Unspecified);
                if !did_change {
                    return;
                }
                output
            }
            _ => {
                let output = seat.get_output();
                if output.is_dummy {
                    log::warn!("Not showing workspace because seat is on dummy output");
                    return;
                }
                let workspace = Rc::new(WorkspaceNode {
                    id: self.node_ids.next(),
                    output: CloneCell::new(output.clone()),
                    position: Cell::new(Default::default()),
                    container: Default::default(),
                    stacked: Default::default(),
                    seat_state: Default::default(),
                    name: name.to_string(),
                    output_link: Cell::new(None),
                    visible: Cell::new(false),
                    fullscreen: Default::default(),
                });
                workspace
                    .output_link
                    .set(Some(output.workspaces.add_last(workspace.clone())));
                output.show_workspace(&workspace);
                self.workspaces.set(name.to_string(), workspace);
                output
            }
        };
        output.update_render_data();
        self.tree_changed();
        // let seats = self.globals.seats.lock();
        // for seat in seats.values() {
        //     seat.workspace_changed(&output);
        // }
    }

    pub fn float_map_ws(&self) -> Rc<WorkspaceNode> {
        if let Some(seat) = self.seat_queue.last() {
            let output = seat.get_output();
            if !output.is_dummy {
                return output.ensure_workspace();
            }
        }
        if let Some(output) = self.root.outputs.lock().values().cloned().next() {
            return output.ensure_workspace();
        }
        self.dummy_output.get().unwrap().ensure_workspace()
    }

    pub fn set_status(&self, status: &str) {
        let status = Rc::new(status.to_owned());
        self.status.set(status.clone());
        let outputs = self.root.outputs.lock();
        for output in outputs.values() {
            output.set_status(&status);
        }
    }

    pub fn input_occurred(&self) {
        if !self.idle.input.replace(true) {
            self.idle.change.trigger();
        }
    }

    pub fn start_xwayland(self: &Rc<Self>) {
        if !self.xwayland.enabled.get() {
            return;
        }
        let mut handler = self.xwayland.handler.borrow_mut();
        if handler.is_none() {
            *handler = Some(self.eng.spawn(xwayland::manage(self.clone())));
        }
    }

    pub fn next_serial(&self, client: Option<&Client>) -> u32 {
        let serial = self.serial.fetch_add(Wrapping(1)).0;
        if let Some(client) = client {
            'update_range: {
                let mut serials = client.serials.borrow_mut();
                if let Some(last) = serials.back_mut() {
                    if last.hi.wrapping_add(1) == serial {
                        last.hi = serial;
                        break 'update_range;
                    }
                }
                if serials.len() >= NUM_CACHED_SERIAL_RANGES {
                    serials.pop_front();
                }
                serials.push_back(SerialRange {
                    lo: serial,
                    hi: serial,
                });
            }
        }
        serial as _
    }
}
