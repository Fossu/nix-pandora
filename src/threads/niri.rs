use crate::daemon::Daemon;
use crate::pithos::commands::{
    CommandType, DaemonCommand, RenderCommand, RenderMode, RenderThreadCommand, ScrollCommand,
};
use crate::pithos::config::DaemonConfig;

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, Weak};
use std::thread;

use miette::Error;
use niri_ipc::socket::Socket;
use niri_ipc::{Event, Output, Request, Response, Window, Workspace};

pub struct NiriAgent {
    config: DaemonConfig,
    cmd_queue: Arc<Mutex<Receiver<DaemonCommand>>>,
    pub queue: Sender<DaemonCommand>,
}

impl NiriAgent {
    pub fn new(config: DaemonConfig) -> Result<Arc<NiriAgent>, Error> {
        match Socket::connect() {
            Ok(_) => {
                let (send, recv) = channel::<DaemonCommand>();
                Ok(Arc::new(NiriAgent {
                    config,
                    cmd_queue: Arc::new(Mutex::new(recv)),
                    queue: send,
                }))
            }
            Err(e) => Err(miette::miette!(e)), // todo
        }
    }

    pub fn start(&self, weak: Weak<dyn Daemon + Send + Sync>) {
        let pandora = weak.upgrade().unwrap();
        let config = self.config.clone();
        let cmd_queue = self.cmd_queue.clone();
        match thread::Builder::new()
            .name("niri agent".to_string())
            .spawn(move || {
                run(config, pandora.clone(), cmd_queue);
                pandora.log(
                    "niri-agent",
                    "thread exiting (is session exiting?)".to_string(),
                );
            }) {
            Ok(_) => (), // [tf2 medic voice] i will live forever!
            Err(e) => panic!("could not spawn niri ipc handler thread: {e:?}"),
        };
    }
}

fn get_niri_state(socket: &mut Socket) -> (HashMap<String, Output>, Vec<Workspace>, Vec<Window>) {
    let outputs_response = match socket.send(Request::Outputs).unwrap() {
        Ok(Response::Outputs(response)) => response,
        Ok(_) => unreachable!(), // must not receive a differente type of response
        Err(e) => panic!("error getting outputs from niri: {e:?}"),
    };
    let workspaces_response = match socket.send(Request::Workspaces).unwrap() {
        Ok(Response::Workspaces(response)) => response,
        Ok(_) => unreachable!(), // must not receive a differente type of response
        Err(e) => panic!("error getting workspaces from niri: {e:?}"),
    };

    let windows_response = match socket.send(Request::Windows).unwrap() {
        Ok(Response::Windows(response)) => response,
        Ok(_) => unreachable!(),
        Err(e) => panic!("error getting windows from niri: {e:?}"),
    };

    (outputs_response, workspaces_response, windows_response)
}

fn index_scroll_percent(curr_idx: usize, max_idx: usize) -> f64 {
    if max_idx <= 1 {
        return 50.0;
    }
    let mut scroll_ercent = 100.0 * (curr_idx.saturating_sub(1)) as f64 / (max_idx - 1) as f64;
    if scroll_percent.is_nan() {
        scroll_ercent = 50.0;
    }
    scroll_percent
}

fn workspace_scroll_percent(curr_idx: u8, max_workspace_idx: u8) -> f64 {
    index_scroll_percent(curr_idx as usize, max_workspace_idx as usize)
}
fn run(
    config: DaemonConfig,
    pandora: Arc<dyn Daemon + Send + Sync>,
    cmd_queue: Arc<Mutex<Receiver<DaemonCommand>>>,
) {
    let mut socket = Socket::connect().unwrap();
    let mut processor = NiriProcessor {
        config,
        ..Default::default()
    };

    processor.init_state(&mut socket);
    processor.reseat_scroll_positions(pandora.clone());

    let reply = socket.send(Request::EventStream).unwrap();
    if matches!(reply, Ok(Response::Handled)) {
        let mut read_event = socket.read_events();
        loop {
            match read_event() {
                Ok(event) => {
                    processor.process(pandora.clone(), event);
                    match cmd_queue.lock() {
                        Ok(channel) => {
                            if let Ok(cmd) = channel.try_recv() {
                                match cmd {
                                    DaemonCommand::ReloadConfig(config) => {
                                        processor.update_config(config, pandora.clone())
                                    }
                                    DaemonCommand::Lock => (), // i think ?
                                    DaemonCommand::Stop => (),
                                }
                            }
                        }
                        Err(e) => {
                            pandora
                                .log("niri-agent", format!("error acquiring channel lock: {e:?}"));
                        }
                    }
                }
                Err(e) => {
                    pandora.debug("niri-agent", format!("event read failed {e:?}"));
                }
            }
        }
    }
}

#[derive(Debug)]
struct OutputState {
    current_image: String,
    mode: Option<RenderMode>,
    max_workspace_idx: u8,
    active_workspace_id: Option<u64>,
    active_workspace_idx: Option<u8>,
}

#[derive(Default)]
struct NiriProcessor {
    config: DaemonConfig,
    outputs: Vec<(String, OutputState)>,
    workspaces: Vec<Workspace>,
    windows: HashMap<u64, Window>,
}

impl NiriProcessor {
    fn update_config(&mut self, new_config: DaemonConfig, pandora: Arc<dyn Daemon + Send + Sync>) {
        for new_output_conf in &new_config.outputs {
            let p = pandora.clone();
            let new_mode = new_output_conf.mode.unwrap_or(RenderMode::Static);

            // First, find the output and check if we need to update
            let needs_update = if let Some((_, state)) =
                self.outputs.iter().find(|o| o.0 == new_output_conf.name)
            {
                state.current_image != new_output_conf.image
                    || state.mode.unwrap_or(RenderMode::Static) != new_mode
            } else {
                continue; // Output not found
            };

            if needs_update {
                let position =
                    self.get_current_position_for_output(&new_output_conf.name, new_mode);
                let cmd = RenderCommand {
                    output: new_output_conf.name.clone(),
                    image: new_output_conf.image.clone(),
                    mode: new_mode,
                    position,
                };
                p.handle_cmd(&CommandType::Tc(RenderThreadCommand::Render(cmd)));

                // Update state to reflect the change
                if let Some((_, state)) = self
                    .outputs
                    .iter_mut()
                    .find(|o| o.0 == new_output_conf.name)
                {
                    state.current_image = new_output_conf.image.clone();
                    state.mode = Some(new_mode);
                }
            }
        }
        self.config = new_config;
    }

    fn get_current_position_for_output(&self, output_name: &str, mode: RenderMode) -> (f64, f64) {
        match mode {
            RenderMode::Static => (0.0, 0.0),
            RenderMode::ScrollVertical | RenderMode::ScrollHorizontal | RenderMode::ScrollBoth => {
                let output = match self.outputs.iter().find(|o| o.0 == output_name) {
                    Some((_, state)) => state,
                    None => return (0.0, 0.0),
                };

                let active_workspace_idx = output.active_workspace_idx.unwrap_or(1);
                let active_workspace_id = output.active_workspace_id;
                let column_scroll_percent = active_workspace_id
                    .map(|workspace_id| self.get_column_scroll_percent_for_workspace(workspace_id))
                    .unwrap_or(50.0);
                match mode {
                    RenderMode::ScrollVertical => (
                        50.0,
                        workspace_scroll_percent(active_workspace_idx, output.max_workspace_idx)
                    ),
                    RenderMode::ScrollHorizontal => (column_scroll_percent, 50.0),
                    RenderMode::ScrollBoth => (
                        column_scroll_percent,
                        workspace_scroll_percent(active_workspace_idx, output.max_workspace_idx)
                    ),
                    RenderMode::Static => unreachable!(),
                }
            }
        }
    }

    fn update_workspaces(&mut self, workspaces: &[Workspace]) {
        for (_, output_state) in &mut self.outputs {
            output_state.max_workspace_idx = 0;
            output_state.active_workspace_id = None;
            output_state.active_workspace_idx = None;
        }
        for workspace in workspaces {
            if workspace.output.is_some() {
                let output_name = workspace.output.clone().unwrap();
                let output_state = match self.outputs.iter_mut().find(|os| os.0 == output_name) {
                    Some(v) => v,
                    None => continue,
                };
                let cur_max_idx = output_state.1.max_workspace_idx;
                output_state.1.max_workspace_idx = u8::max(workspace.idx, cur_max_idx);

                // Update active workspace index
                if workspace.is_active {
                    output_state.1.active_workspace_id = Some(workspace.id);
                    output_state.1.active_workspace_idx = Some(workspace.idx);
                }
            }
        }

        self.workspaces = workspaces.to_vec();
    }

    fn update_windows(&mut self, windows: Vec<Window>) {
        self.windows = windows
            .into_iter()
            .map(|window| (window.id, window))
            .collect();
    }

    fn get_workspace(&self, workspace_id: u64) -> Option<&Workspace> {
        self.workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
    }

    fn get_active_column_scroll_state(&self, workspace_id: u64) -> Option<(usize, usize)> {
        let workspace = self.get_workspace(workspace_id)?;
        let mut columns = self
            .windows
            .values()
            .filter(|window| window.workspace_id == Some(workspace_id) && !window.is_floating)
            .filter_map(|window| {
                window
                    .layout
                    .pos_in_scrolling_layout
                    .map(|(column_idx, _)| column_idx)
            })
            .collect::<Vec<_>>();
        if columns.is_empty() {
            return None;
        }

        columns.sort_unstable();
        columns.dedup();

        let active_column_idx = workspace
            .active_window_id
            .and_then(|window_id| self.windows.get(&window_id))
            .filter(|window| window.workspace_id == Some(workspace_id) && !window.is_floating)
            .and_then(|window| {
                window
                    .layout
                    .pos_in_scrolling_layout
                    .map(|(column_idx, _)| column_idx)
            })
            .or_else(|| {
                self.windows
                    .values()
                    .find(|window| {
                        window.workspace_id == Some(workspace_id)
                        && window.is_focused
                        && !window.is_floating
                    })
                    .and_then(|window| {
                        window
                            .layout
                            .pos_in_scrolling_layout
                            .map(|(column_idx, _)| column_idx)
                    })
            })?;
        let active_rank = columns
            .iter()
            .position(|column_idx| *column_idx == active_column_idx)
            .map(|idx| idx + 1)?;
        Some((active_rank, columns.len()))
    }

    fn get_column_scroll_percent_for_workspace(&self, workspace_id: u64) -> f64 {
        match self.get_active_column_scroll_state(workspace_id) {
            Some((active_rank, column_count)) => index_scroll_percent(active_rank, column_count),
            None => 50.0,
        }
    }

    fn is_workspace_active(&self, workspace_id: u64) -> bool {
        self.get_workspace(workspace_id)
            .is_some_and(|workspace| workspace.is_active)
    }

    fn emit_scroll_for_workspace_if_active(
        &self,
        pandora: Arc<dyn Daemon + Send + Sync>,
        workspace_id: u64
    ) {
        if self.is_workspace_active(workspace_id) {
            self.gen_scroll_cmd_for_workspace_id(pandora, workspace_id);
        }
    }

    fn handle_workspace_activated(&mut self, id: u64, focused: bool) {
        let output_name = self
            .workspaces
            .iter()
            .find(|workspace| workspace.id == id)
            .and_then(|workspace| workspace.output.clone());
        let active_workspace_state = self
            .workspaces
            .iter()
            .find(|workspace| workspace.id == id)
            .map(|workspace| (workspace.id, workspace.idx));

        for workspace in &mut self.workspaces {
            let got_activated = workspace.id == id;
            if workspace.output == output_name {
                workspace.is_active = got_activated;
            }
            if focused {
                workspace.is_focused = got_activated;
            }
        }

        if let (Some(output_name), Some((workspace_id, workspace_idx))) =
            (output_name, active_workspace_state)
            && let Some((_, output_state)) = self
                .outputs
                .iter_mut()
                .find(|output| output.0 == output_name)
        {
            output_state.active_workspace_id = Some(workspace_id);
            output_state.active_workspace_idx = Some(workspace_idx);
        }
    }

    fn handle_workspace_active_window_changed(
        &mut self,
        workspace_id: u64,
        active_window_id: Option<u64>,
    ) {
        if let Some(workspace) = self
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
        {
            workspace.active_window_id = active_window_id;
        }
    }

    fn handle_window_opened_or_changed(&mut self, window: Window) -> Vec<u64> {
        let window_id = window.id;
        let prior_workspace_id = self
            .window
            .get(&window_id)
            .and_then(|existing_window| existing_window.workspace_id);
        let is_focused = window.is_focused;
        let workspace_id = window.workspace_id;
        self.windows.insert(window_id, window);

        let mut workspaces_to_refresh = Vec::new();
        if let Some(workspace_id) = prior_workspace_id {
            workspaces_to_refresh.push(workspace_id);
        }
        if let Some(workspace_id) = workspace_id
            && !workspaces_to_refresh.contains(&workspace_id)
        {
            workspaces_to_refresh.push(workspace_id);
        }

        if is_focused {
            for other_window in self.windows.values_mut() {
                if other_window.id != window_id {
                    other_window.is_focused = false;
                }
            }
            if let Some(workspace_id) = workspace_id {
                self.handle_workspace_active_window_changed(workspace_id, Some(window_id));
            }
        }
        workspaces_to_refresh
    }

    fn handle_window_closed(&mut self, id: u64) -> Option<u64> {
        let window = self.windows.remove(&id)?;
        let workspace_id = window.workspace_id?;
        if self
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .and_then(|workspace| workspace.active_window_id)
            == Some(id)
        {
            let replacement = self
                .windows
                .values()
                .find(|candidate| {
                    candidate.workspace_id == Some(workspace_id)
                        && !candidate.is_floating
                        && candidate.layout.pos_in_scrolling_layout.is_some()
                })
                .map(|candidate| candidate.id);
            self.handle_workspace_active_window_changed(workspace_id, replacement);
        }
        Some(workspace_id)
    }

    fn handle_window_focus_changed(&mut self, id: Option<u64>) -> Option<u64> {
        let mut focused_workspace_id = None;
        for window in self.windows.values_mut() {
            window.is_focused = Some(window.id) == id;
            if window.is_focused {
                focused_workspace_id = window.workspace_id;
            }
        }
        if let Some(workspace_id) = focused_workspace_id {
            self.handle_workspace_active_window_changed(workspace_id, id);
        }
        focused_workspace_id
    }

    fn handle_window_layouts_changed(
        &mut self,
        changes: Vec<(u64, niri_ipc::WindowLayout)>,
    ) -> Vec<u64> {
        let mut workspaces_to_refresh = Vec::new();
        for (window_id, layout) in changes {
            if let Some(window) = self.windows.get_mut(&window_id) {
                window.layout = layout;
                if let Some(workspace_id) = window.workspace_id
                && !workspaces_to_refresh.contains(&workspace_id)
                {
                    workspaces_to_refresh.push(workspace_id)
                }
            }
        }
        workspaces_to_refresh
    }

    fn init_state(&mut self, niri_socket: &mut Socket) {
        let (outputs, workspaces, windows) = get_niri_state(niri_socket);
        for (output_name, output) in outputs {
            let output_config = match self
                .config
                .outputs
                .iter()
                .find(|oc| oc.name == *output_name)
            {
                Some(c) => c,
                None => continue,
            };

            if output.current_mode.is_some() {
                let img_path = output_config.image.clone();
                let output_state = OutputState {
                    current_image: img_path,
                    mode: output_config.mode,
                    max_workspace_idx: 0,
                    active_workspace_id: None,
                    active_workspace_idx: None,
                };
                self.outputs.push((output_name.clone(), output_state));
            }
        }
        self.update_workspaces(&workspaces);
        self.update_windows(windows);
    }

    fn reseat_scroll_positions(&mut self, pandora: Arc<dyn Daemon + Send + Sync>) {
        for workspace in &self.workspaces.clone() {
            if workspace.is_active {
                self.gen_scroll_cmd_for_workspace_id(pandora.clone(), workspace.id);
            }
        }
    }

    fn process(&mut self, pandora: Arc<dyn Daemon + Send + Sync>, e: niri_ipc::Event) {
        match e {
            Event::WorkspacesChanged { workspaces } => {
                self.poke(pandora.clone());
                self.update_workspaces(&workspaces);

                self.reseat_scroll_positions(pandora.clone());
            }
            Event::WorkspaceActivated { id, focused } => {
                self.handle_workspace_activated(id, focused);
                self.gen_scroll_cmd_for_workspace_id(pandora.clone(), id)
            }
            //Event::WindowFocusChanged { id: _id } => {
            //    self.poke(pandora.clone());
            //}
            Event::WorkspaceActiveWindowChanged {
                workspace_id,
                active_window_id,
            } => {
                self.handle_workspace_active_window_changed(workspace_id, active_window_id);
                self.emit_scroll_for_workspace_if_active(pandora.clone(), workspace_id);
            }
            Event::WindowsChanged { windows } => {
                self.update_windows(windows);
                self.reseat_scroll_positions(pandora.clone());
            }
            Event::WindowOpenedOrChanged { window } => {
                for workspace_id in self.handle_window_opened_or_changed(window) {
                    self.emit_scroll_for_workspace_if_active(pandora.clone(), workspace_id);
                }
            }
            Event::WindowClose { id } => {
                if let Some(workspace_id) = self.handle_window_closed(id) {
                    self.emit_scroll_for_workspace_if_active(pandora.clone(), workspace_id);
                }
            }
            Event::WindowFocusChanged { id } => {
                if let Some(workspace_id) = self.handle_window_focus_changed(id) {
                    self.emit_scroll_for_workspace_if_active(pandora.clone(), workspace_id);
                }
            }
            Event::WindowLayoutsChanged { changed } => {
                for workspace_id in self.handle_window_layouts_changed(changes) {
                    self.emit_scroll_for_workspace_if_active(pandora.clone(), workspace_id);
                }
                self.poke(pandora.clone());
            }
            _ => (), // idc about other events rn
        }
    }

    fn poke(&self, pandora: Arc<dyn Daemon + Send + Sync>) {
        pandora.handle_cmd(&CommandType::Tc(RenderThreadCommand::Poke));
    }

    fn gen_scroll_cmd_for_workspace_id(&self, pandora: Arc<dyn Daemon + Send + Sync>, id: u64) {
        let workspace = self.workspaces.iter().find(|w| w.id == id).unwrap();
        let curr_idx = workspace.idx;

        let output_name = match workspace.output.clone() {
            Some(o) => o,
            None => return, // focused a workspace while no outputs connected / all outputs unplugged. whatever lol
        };
        let output = match &self.outputs.iter().find(|o| o.0 == output_name) {
            Some(tuple) => &tuple.1,
            None => {
                pandora.log(
                    "niri-agent",
                    format!("{output_name} not found in config; ignoring"),
                );
                return; // display not configured
            }
        };
        if let Some(cmd) = match &output.mode {
            None => None,
            Some(RenderMode::ScrollVertical)
            | Some(RenderMode::ScrollHorizontal)
            | Some(RenderMode::ScrollBoth) => {
                let column_scroll_percent =
                    self.get_column_scroll_percent_for_workspace(workspace.id);
                let (position_x, position_y) = match output.mode.unwrap() {
                    RenderMode::ScrollVertical => (
                        50.0,
                        workspace_scroll_percent(curr_idx, output.max_workspace_idx),
                    ),
                    RenderMode::ScrollHorizontal => (column_scroll_percent, 50.0),
                    RenderMode::ScrollBoth => (
                        column_scroll_percent,
                        workspace_scroll_percent(curr_idx, output.max_workspace_idx),
                    ),
                    RenderMode::Static => unreachable!(),
                };
                let cmd = RenderThreadCommand::Scroll(ScrollCommand {
                    output: output_name,
                    position_x,
                    position_y,
                });
                Some(CommandType::Tc(cmd))
            }
            Some(RenderMode::Static) => None,
        } {
            pandora.verbose("niri-agent", format!("emitting command {cmd:?}"));
            pandora.handle_cmd(&cmd);
        }
    }
}
