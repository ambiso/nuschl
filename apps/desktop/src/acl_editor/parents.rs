// SPDX-License-Identifier: EUPL-1.2
//! The parent channels' own rules, read once per editor with one scoped query
//! at a time, so permissions are evaluated over the whole channel path.
use super::{MEMBER, active_editor};
use crate::{
    menus::{ActionOrigin, MenuTarget},
    panels::Panel,
    state::{Desktop, ServerView},
};
use nuschl_client::platform::request_id;
use nuschl_core::{
    access::Who,
    acl, admin,
    model::{Acl, TextTarget},
};
use nuschl_session::connection::Command;
use std::collections::HashMap;

#[derive(Default)]
pub struct Parents {
    /// Root first.
    pub acls: Vec<(u32, Acl)>,
    /// The parent being read, and the request.
    reading: Option<(u32, u64)>,
    pub error: Option<String>,
    pub done: bool,
}

impl Parents {
    pub fn reading(&self) -> Option<u32> {
        self.reading.map(|(channel, _)| channel)
    }
}

/// The channels above `channel`, root first.
fn chain(view: &ServerView, channel: u32) -> Option<Vec<u32>> {
    let mut chain = Vec::new();
    let mut current = channel;
    while current != 0 && chain.len() <= 64 {
        current = view
            .channels
            .iter()
            .find(|view| view.target == TextTarget::Channel(current))?
            .parent?;
        chain.push(current);
    }
    chain.reverse();
    Some(chain)
}

/// Reads the next parent's rules, or marks them all read.
pub(super) fn next(this: &mut Desktop, token: u64) {
    let Some(Panel::Acl {
        origin,
        channel,
        editor,
        ..
    }) = this.panel.as_ref()
    else {
        return;
    };
    let Some(index) = origin.live_index(this) else {
        return;
    };
    if editor.token != token
        || editor.parents.reading.is_some()
        || editor.parents.done
        || editor.parents.error.is_some()
    {
        return;
    }
    let view = &this.client.servers[index];
    let wanted = chain(view, *channel).map(|chain| chain.get(editor.parents.acls.len()).copied());
    let sent = match wanted {
        None => Err("This channel's place in the tree is unknown.".to_owned()),
        Some(None) => Ok(None),
        Some(Some(parent)) => {
            let request = request_id();
            let server = view.id.clone();
            ActionOrigin::capture(view, MenuTarget::Channel(parent))
                .with_scope(this, |this| {
                    this.driver.command(
                        &server,
                        Command::QueryAclDraft {
                            channel: parent,
                            request,
                        },
                    )
                })
                .map(|()| Some((parent, request)))
        }
    };
    if let Some(editor) = active_editor(this, token) {
        match sent {
            Ok(Some(reading)) => editor.parents.reading = Some(reading),
            Ok(None) => editor.parents.done = true,
            Err(error) => editor.parents.error = Some(error),
        }
    }
}

/// Takes the reply to the parent being read.
pub(super) fn sync(this: &mut Desktop, token: u64, index: usize) {
    let Some((parent, request)) =
        active_editor(this, token).and_then(|editor| editor.parents.reading)
    else {
        return;
    };
    let Some(reply) = this.client.servers[index]
        .acl_reply
        .as_ref()
        .filter(|reply| reply.request == request)
        .cloned()
    else {
        return;
    };
    let Some(editor) = active_editor(this, token) else {
        return;
    };
    editor.parents.reading = None;
    match reply.result {
        Ok(acl) => {
            editor.parents.acls.push((parent, acl));
            next(this, token);
        }
        Err(error) => editor.parents.error = Some(error),
    }
}

/// The parents' rules and `channel`'s (the draft) as the permission model
/// reads them; a member of `who`'s group is added wherever it is defined.
pub fn tree(parents: &[(u32, Acl)], channel: &Acl, who: &Who) -> HashMap<u32, acl::Node> {
    let mut tree = HashMap::new();
    let mut parent = None;
    for (id, acl) in parents {
        tree.insert(*id, admin::node(acl, parent));
        parent = Some(*id);
    }
    tree.insert(channel.channel, admin::node(channel, parent));
    if let Who::Group(group) = who {
        for node in tree.values_mut() {
            if let Some(defined) = node.groups.get_mut(group) {
                defined.add.insert(MEMBER);
            }
        }
        if let Some(node) = tree.get_mut(&channel.channel) {
            node.groups
                .entry(group.clone())
                .or_insert_with(acl::Group::new)
                .add
                .insert(MEMBER);
        }
    }
    tree
}

/// Who the permission model sees for a row: no account, a group member, or
/// the account, in `channel`.
pub fn subject<'a>(who: &Who, channel: u32, hash: Option<&'a str>) -> acl::Subject<'a> {
    acl::Subject {
        user_id: match who {
            Who::Everyone => None,
            Who::Group(_) => Some(MEMBER),
            Who::Person(id) => Some(*id),
        },
        channel,
        hash,
        tokens: &[],
        strong: false,
    }
}
