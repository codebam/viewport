// SPDX-License-Identifier: GPL-3.0-or-later
//
// ext-workspace-v1: telling a client outside the shell what the workspaces are.
//
// Every other compositor keeps its workspaces itself, so publishing them is a
// matter of exposing what is already there. Here they are the shell's —
// `data/shell/*.js` decides how many there are, what they are called and which
// is showing — and the compositor has never had a reason to know. So this is a
// relay rather than a source: the shell sends its list with `workspace.list`,
// this republishes it, and a request from a client goes back the other way as
// `workspace.request` for the shell to act on. The shell stays the one place
// that decides, which is the property worth keeping — a compositor that
// switched a workspace behind the shell's back would leave two answers to the
// question of what is showing.
//
// A shell that never sends `workspace.list` publishes nothing, and a client
// sees a manager with no groups in it. That is the honest description of a
// desktop whose workspaces nobody has described.
//
// Smithay implements none of this. The wire bindings come from
// `wayland-protocols`' staging set, so unlike the wlr protocols here there is
// no XML in `protocols/`.
//
// The protocol batches: activate, assign, remove and the rest do nothing until
// `commit`. That is honoured — requests pile up against the manager that
// received them and are forwarded in order on commit — because a bar that
// moves a workspace and activates it in one commit means both, and acting on
// the first as it arrives shows a frame with the workspace in the wrong place.

use std::collections::{BTreeSet, HashMap};

use smithay::output::Output;
use smithay::reexports::wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1, GroupCapabilities},
    ext_workspace_handle_v1::{
        self, ExtWorkspaceHandleV1, State as HandleState, WorkspaceCapabilities,
    },
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::wayland::{Dispatch2, GlobalDispatch2};

pub use viewport_ipc::request::Workspace;

const VERSION: u32 = 1;

/// What a client asked for, once its commit says it meant it.
#[derive(Debug, Clone, PartialEq)]
pub enum Ask {
    Activate(String),
    Deactivate(String),
    /// Move a workspace to the group that stands for this output.
    Assign {
        id: String,
        output: String,
    },
    Remove(String),
    Create {
        output: String,
        name: String,
    },
}

/// What the compositor has to be able to do for a request to mean anything.
pub trait WorkspaceHandler {
    fn workspace_state(&mut self) -> &mut WorkspaceState;

    /// Hand a client's request to whoever owns the workspaces.
    fn workspace_asked(&mut self, ask: Ask);
}

/// The global's data, and the manager's. A type of this crate's own because
/// the fork dispatches on the user data rather than on the compositor state,
/// and `()` belongs to nobody.
#[derive(Debug)]
pub struct ManagerData;

/// Which workspace a handle stands for.
#[derive(Debug, Clone)]
pub struct HandleData {
    pub id: String,
}

/// Which output a group stands for.
#[derive(Debug, Clone)]
pub struct GroupData {
    pub output: String,
}

/// One bound client's view of the world.
///
/// Objects are per client: a handle means nothing to anybody else, and the
/// protocol has no way to share one.
#[derive(Debug)]
struct Bound {
    manager: ExtWorkspaceManagerV1,
    groups: HashMap<String, ExtWorkspaceGroupHandleV1>,
    handles: HashMap<String, Membership>,
    /// What has been asked since this client's last commit.
    pending: Vec<Ask>,
}

/// One client's handle for a workspace, and the group it is currently in.
///
/// The group is tracked rather than derived because entering one is an event,
/// not a property: a workspace that moves to another screen has to be told to
/// leave the group it was in, and a handle cannot be asked which that was.
#[derive(Debug)]
struct Membership {
    handle: ExtWorkspaceHandleV1,
    group: Option<String>,
}

/// The workspaces as the shell last described them, and who is watching.
#[derive(Debug, Default)]
pub struct WorkspaceState {
    workspaces: Vec<Workspace>,
    bound: Vec<Bound>,
}

impl WorkspaceState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<ExtWorkspaceManagerV1, ManagerData> + 'static,
    {
        display.create_global::<D, ExtWorkspaceManagerV1, _>(VERSION, ManagerData);
        Self::default()
    }

    /// Replace the list with the shell's, and tell everyone watching.
    ///
    /// One `done` per client at the end: that is the protocol's transaction
    /// boundary, and a bar redrawing per event would flicker through
    /// half-states on every switch.
    pub fn set<D>(&mut self, dh: &DisplayHandle, outputs: &[Output], workspaces: Vec<Workspace>)
    where
        D: Dispatch<ExtWorkspaceGroupHandleV1, GroupData>
            + Dispatch<ExtWorkspaceHandleV1, HandleData>
            + 'static,
    {
        if self.workspaces == workspaces {
            return;
        }
        self.workspaces = workspaces;
        let workspaces = self.workspaces.clone();
        for bound in &mut self.bound {
            Self::reconcile::<D>(dh, outputs, bound, &workspaces);
            bound.manager.done();
        }
    }

    /// Give a newly bound client the world as it stands.
    fn greet<D>(&mut self, dh: &DisplayHandle, outputs: &[Output], manager: ExtWorkspaceManagerV1)
    where
        D: Dispatch<ExtWorkspaceGroupHandleV1, GroupData>
            + Dispatch<ExtWorkspaceHandleV1, HandleData>
            + 'static,
    {
        let mut bound = Bound {
            manager,
            groups: HashMap::new(),
            handles: HashMap::new(),
            pending: Vec::new(),
        };
        let workspaces = self.workspaces.clone();
        Self::reconcile::<D>(dh, outputs, &mut bound, &workspaces);
        bound.manager.done();
        self.bound.push(bound);
    }

    /// Bring one client's objects in line with the list.
    fn reconcile<D>(
        dh: &DisplayHandle,
        outputs: &[Output],
        bound: &mut Bound,
        workspaces: &[Workspace],
    ) where
        D: Dispatch<ExtWorkspaceGroupHandleV1, GroupData>
            + Dispatch<ExtWorkspaceHandleV1, HandleData>
            + 'static,
    {
        let Ok(client) = bound.manager.client().ok_or(()) else {
            return;
        };

        // Groups first: a workspace can only enter a group that exists.
        let wanted: BTreeSet<String> = workspaces.iter().filter_map(|w| w.output.clone()).collect();

        bound.groups.retain(|output, group| {
            let keep = wanted.contains(output);
            if !keep {
                group.removed();
            }
            keep
        });

        // A group that went takes its members' membership with it: the object
        // is gone, so nothing can be told to leave it, and a handle still
        // claiming to be in it would never be entered into its next one.
        bound
            .handles
            .values_mut()
            .filter(|m| m.group.as_ref().is_some_and(|g| !wanted.contains(g)))
            .for_each(|m| m.group = None);

        for output in &wanted {
            if bound.groups.contains_key(output) {
                continue;
            }
            let Ok(group) = client.create_resource::<ExtWorkspaceGroupHandleV1, GroupData, D>(
                dh,
                VERSION,
                GroupData {
                    output: output.clone(),
                },
            ) else {
                continue;
            };
            bound.manager.workspace_group(&group);
            tracing::debug!("workspace group for {output}");
            group.capabilities(GroupCapabilities::CreateWorkspace);
            // Which screen the group is, in the client's own terms. A bar
            // draws its workspaces on the monitor it is on, and without this
            // it cannot tell which group that is.
            if let Some(wl_output) = outputs
                .iter()
                .find(|o| &o.name() == output)
                .and_then(|o| o.client_outputs(&client).next())
            {
                group.output_enter(&wl_output);
            }
            bound.groups.insert(output.clone(), group);
        }

        // Then the workspaces.
        let ids: BTreeSet<&str> = workspaces.iter().map(|w| w.id.as_str()).collect();
        bound.handles.retain(|id, membership| {
            let keep = ids.contains(id.as_str());
            if !keep {
                membership.handle.removed();
            }
            keep
        });

        for workspace in workspaces {
            let handle = match bound.handles.get(&workspace.id) {
                Some(membership) => membership.handle.clone(),
                None => {
                    let Ok(handle) = client.create_resource::<ExtWorkspaceHandleV1, HandleData, D>(
                        dh,
                        VERSION,
                        HandleData {
                            id: workspace.id.clone(),
                        },
                    ) else {
                        continue;
                    };
                    bound.manager.workspace(&handle);
                    tracing::debug!("workspace handle {}", workspace.id);
                    handle.id(workspace.id.clone());
                    handle.capabilities(
                        WorkspaceCapabilities::Activate
                            | WorkspaceCapabilities::Deactivate
                            | WorkspaceCapabilities::Remove
                            | WorkspaceCapabilities::Assign,
                    );
                    bound.handles.insert(
                        workspace.id.clone(),
                        Membership {
                            handle: handle.clone(),
                            group: None,
                        },
                    );
                    handle
                }
            };

            // The group it belongs in now, which is not necessarily the one it
            // was in: a workspace moves between screens, and a bar told only
            // that it was created on the first draws it on the wrong monitor
            // for the rest of the session. Leave first, then enter — a handle
            // in two groups at once is a workspace that appears twice.
            let membership = bound
                .handles
                .get_mut(&workspace.id)
                .expect("just inserted or already present");
            if membership.group != workspace.output {
                if let Some(previous) = membership
                    .group
                    .as_ref()
                    .and_then(|output| bound.groups.get(output))
                {
                    previous.workspace_leave(&handle);
                }
                membership.group = None;
                if let Some(output) = workspace.output.as_ref() {
                    if let Some(group) = bound.groups.get(output) {
                        group.workspace_enter(&handle);
                        membership.group = Some(output.clone());
                    }
                }
            }

            // Name and state go out every time: they are the two things that
            // change without the workspace itself coming or going.
            handle.name(workspace.name.clone());
            let mut state = HandleState::empty();
            if workspace.active {
                state |= HandleState::Active;
            }
            if workspace.urgent {
                state |= HandleState::Urgent;
            }
            if workspace.hidden {
                state |= HandleState::Hidden;
            }
            handle.state(state);
        }
    }

    /// Forget a client that has gone.
    fn unbind(&mut self, manager: &ExtWorkspaceManagerV1) {
        self.bound.retain(|bound| &bound.manager != manager);
    }

    /// Remember a request until its commit.
    fn push(&mut self, manager: &ExtWorkspaceManagerV1, ask: Ask) {
        if let Some(bound) = self.bound.iter_mut().find(|b| &b.manager == manager) {
            bound.pending.push(ask);
        }
    }

    /// Everything asked since the last commit, in order.
    fn take(&mut self, manager: &ExtWorkspaceManagerV1) -> Vec<Ask> {
        self.bound
            .iter_mut()
            .find(|b| &b.manager == manager)
            .map(|bound| std::mem::take(&mut bound.pending))
            .unwrap_or_default()
    }

    /// The manager a handle or group belongs to.
    ///
    /// A request arrives on the handle, and the batch it belongs to is the
    /// manager's — so the handle has to be traced back to it.
    fn manager_of_handle(&self, handle: &ExtWorkspaceHandleV1) -> Option<ExtWorkspaceManagerV1> {
        self.bound
            .iter()
            .find(|bound| bound.handles.values().any(|m| &m.handle == handle))
            .map(|bound| bound.manager.clone())
    }

    fn manager_of_group(&self, group: &ExtWorkspaceGroupHandleV1) -> Option<ExtWorkspaceManagerV1> {
        self.bound
            .iter()
            .find(|bound| bound.groups.values().any(|g| g == group))
            .map(|bound| bound.manager.clone())
    }
}

/// Everything a compositor has to hand this module to make the global work.
pub trait WorkspaceOutputs {
    /// The outputs, so a group can say which screen it is.
    fn workspace_outputs(&self) -> Vec<Output>;
}

impl<D> GlobalDispatch2<ExtWorkspaceManagerV1, D> for ManagerData
where
    D: Dispatch<ExtWorkspaceManagerV1, ManagerData>
        + Dispatch<ExtWorkspaceGroupHandleV1, GroupData>
        + Dispatch<ExtWorkspaceHandleV1, HandleData>
        + WorkspaceHandler
        + WorkspaceOutputs
        + 'static,
{
    fn bind(
        &self,
        state: &mut D,
        dh: &DisplayHandle,
        _client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ManagerData);
        let outputs = state.workspace_outputs();
        state.workspace_state().greet::<D>(dh, &outputs, manager);
    }
}

impl<D> Dispatch2<ExtWorkspaceManagerV1, D> for ManagerData
where
    D: WorkspaceHandler + 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        manager: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_workspace_manager_v1::Request::Commit => {
                for ask in state.workspace_state().take(manager) {
                    state.workspace_asked(ask);
                }
            }
            ext_workspace_manager_v1::Request::Stop => {
                manager.finished();
                state.workspace_state().unbind(manager);
            }
            _ => {}
        }
    }

    fn destroyed(
        &self,
        state: &mut D,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        manager: &ExtWorkspaceManagerV1,
    ) {
        state.workspace_state().unbind(manager);
    }
}

impl<D> Dispatch2<ExtWorkspaceGroupHandleV1, D> for GroupData
where
    D: WorkspaceHandler + 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        group: &ExtWorkspaceGroupHandleV1,
        request: ext_workspace_group_handle_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_workspace_group_handle_v1::Request::CreateWorkspace { workspace } => {
                let Some(manager) = state.workspace_state().manager_of_group(group) else {
                    return;
                };
                state.workspace_state().push(
                    &manager,
                    Ask::Create {
                        output: self.output.clone(),
                        name: workspace,
                    },
                );
            }
            ext_workspace_group_handle_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl<D> Dispatch2<ExtWorkspaceHandleV1, D> for HandleData
where
    D: WorkspaceHandler + 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        handle: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let Some(manager) = state.workspace_state().manager_of_handle(handle) else {
            return;
        };
        let id = self.id.clone();
        let ask = match request {
            ext_workspace_handle_v1::Request::Activate => Ask::Activate(id),
            ext_workspace_handle_v1::Request::Deactivate => Ask::Deactivate(id),
            ext_workspace_handle_v1::Request::Remove => Ask::Remove(id),
            ext_workspace_handle_v1::Request::Assign { workspace_group } => {
                // The group is the client's own object, so the output it
                // stands for is in its user data rather than on the wire.
                let Some(output) = workspace_group
                    .data::<GroupData>()
                    .map(|data| data.output.clone())
                else {
                    return;
                };
                Ask::Assign { id, output }
            }
            ext_workspace_handle_v1::Request::Destroy => return,
            _ => return,
        };
        state.workspace_state().push(&manager, ask);
    }
}
