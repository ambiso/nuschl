// SPDX-License-Identifier: EUPL-1.2
//! Check access (#341): what someone may do in the edited channel and what
//! decided each permission. The parents' own rules are read one query at a
//! time; the channel itself comes from the draft, so unsaved changes count.
use super::{Editor, MEMBER, active_editor, group_target, name, note, section, with_help};
use crate::{
    menus::{ActionOrigin, MenuTarget},
    panels::{Panel, channel_name},
    picker::{Choice, Picker},
    state::{Desktop, ServerView},
};
use gpui_kit::{prelude::*, *};
use nuschl_client::platform::request_id;
use nuschl_core::{
    acl::{self, Reason, Subject, perm::ROOT_ONLY},
    admin::{self, AclDraft},
    model::{Acl, TextTarget, permission},
};
use nuschl_session::connection::Command;
use std::collections::HashMap;

#[derive(Clone, PartialEq, Eq)]
pub enum Who {
    Everyone,
    /// A group name, including Mumble's `auth` (registered users).
    Group(String),
    /// A registered account.
    Person(u32),
}

impl Who {
    fn key(&self) -> String {
        match self {
            Who::Everyone => "all".into(),
            Who::Group(group) => format!("group:{group}"),
            Who::Person(id) => format!("person:{id}"),
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        if key == "all" {
            return Some(Who::Everyone);
        }
        if let Some(group) = key.strip_prefix("group:") {
            return Some(Who::Group(group.to_owned()));
        }
        key.strip_prefix("person:")?.parse().ok().map(Who::Person)
    }
}

/// The search field that picks whom to check, for `Editor::new`.
pub(super) fn picker(token: u64, window: &mut Window, cx: &mut Context<Desktop>) -> Picker {
    Picker::new(
        "Check access for",
        "Everyone, a group or a person",
        |this| match this.panel.as_mut() {
            Some(Panel::Acl { editor, .. }) => Some(&mut editor.check_picker),
            _ => None,
        },
        move |this, key, _, cx| {
            if let Some(who) = Who::from_key(key) {
                pick(this, token, who);
            }
            cx.notify();
        },
        window,
        cx,
    )
}

pub struct Check {
    who: Who,
    /// The parent channels' own rules, root first.
    parents: Vec<(u32, Acl)>,
    /// The parent whose rules are being read, and the request.
    reading: Option<(u32, u64)>,
    error: Option<String>,
}

/// The channels above `channel`, root first.
fn parents(view: &ServerView, channel: u32) -> Option<Vec<u32>> {
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

/// Shows `who`'s access; the parents' rules are read once per editor, so
/// later picks switch at once.
fn pick(this: &mut Desktop, token: u64, who: Who) {
    if let Some(editor) = active_editor(this, token) {
        match editor.check.as_mut() {
            Some(check) if check.error.is_none() => check.who = who,
            _ => {
                editor.check = Some(Check {
                    who,
                    parents: Vec::new(),
                    reading: None,
                    error: None,
                });
                next(this, token);
            }
        }
    }
}

/// Reads the next parent's rules, if any are missing.
fn next(this: &mut Desktop, token: u64) {
    let Some(Panel::Acl {
        origin,
        channel,
        editor,
        ..
    }) = this.panel.as_ref()
    else {
        return;
    };
    let (Some(index), Some(check)) = (origin.live_index(this), editor.check.as_ref()) else {
        return;
    };
    let view = &this.client.servers[index];
    let wanted = parents(view, *channel).map(|chain| chain.get(check.parents.len()).copied());
    let sent = match wanted {
        None => Err("This channel's place in the tree is unknown.".to_owned()),
        Some(None) => return,
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
                .map(|()| (parent, request))
        }
    };
    if let Some(check) = active_editor(this, token).and_then(|editor| editor.check.as_mut()) {
        match sent {
            Ok(reading) => check.reading = Some(reading),
            Err(error) => check.error = Some(error),
        }
    }
}

/// Takes the reply to the parent being read.
pub(super) fn sync(this: &mut Desktop, token: u64, index: usize) {
    let Some((parent, request)) =
        active_editor(this, token).and_then(|editor| editor.check.as_ref()?.reading)
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
    let Some(check) = active_editor(this, token).and_then(|editor| editor.check.as_mut()) else {
        return;
    };
    check.reading = None;
    match reply.result {
        Ok(acl) => {
            check.parents.push((parent, acl));
            next(this, token);
        }
        Err(error) => check.error = Some(error),
    }
}

/// The parents' rules and the draft as the permission model reads them; a
/// group member is added wherever the group is defined.
fn tree(check: &Check, draft: &AclDraft) -> HashMap<u32, acl::Node> {
    let mut tree = HashMap::new();
    let mut parent = None;
    for (id, acl) in &check.parents {
        tree.insert(*id, admin::node(acl, parent));
        parent = Some(*id);
    }
    let channel = draft.channel();
    tree.insert(channel, admin::node(&draft.snapshot(), parent));
    if let Who::Group(group) = &check.who {
        for node in tree.values_mut() {
            if let Some(defined) = node.groups.get_mut(group) {
                defined.add.insert(MEMBER);
            }
        }
        if let Some(node) = tree.get_mut(&channel) {
            node.groups
                .entry(group.clone())
                .or_insert_with(acl::Group::new)
                .add
                .insert(MEMBER);
        }
    }
    tree
}

fn why(reason: Reason, allowed: bool, tree: &HashMap<u32, acl::Node>, view: &ServerView) -> String {
    match reason {
        Reason::Default if allowed => "Mumble's default".into(),
        Reason::Default => "Not allowed by default".into(),
        Reason::Rule { channel, index } => {
            let target = tree
                .get(&channel)
                .and_then(|node| node.acls.get(index))
                .map_or_else(String::new, |entry| match entry.user_id {
                    Some(id) => name(view, id),
                    None => group_target(&entry.group),
                });
            format!("{target} rule in {}", channel_name(view, channel))
        }
        Reason::Write => "Comes with Write ACL".into(),
        Reason::Superuser => "SuperUser may do everything except talk".into(),
        Reason::Unreachable { channel } => {
            format!("Cannot pass {}", channel_name(view, channel))
        }
    }
}

pub(super) fn render(view: &ServerView, editor: &Editor, cx: &mut Context<Desktop>) -> AnyElement {
    let token = editor.token;
    let Some(draft) = editor.state.draft() else {
        return div().into_any_element();
    };
    let channel = draft.channel();
    // Everyone and the groups first, then every account the client knows.
    let mut accounts: Vec<(u32, String)> = view
        .registered_users
        .iter()
        .flatten()
        .filter_map(|account| Some((account.user_id, account.name.clone()?)))
        .chain(
            view.users
                .values()
                .filter_map(|user| Some((user.user_id?, user.name.clone()))),
        )
        .collect();
    accounts.sort_by(|a, b| {
        a.1.to_lowercase()
            .cmp(&b.1.to_lowercase())
            .then(a.0.cmp(&b.0))
    });
    accounts.dedup_by_key(|account| account.0);
    let online = |id: u32| view.users.values().any(|user| user.user_id == Some(id));
    let choices: Vec<Choice> = [
        (Who::Everyone, "Everyone".to_owned(), "Without an account"),
        (
            Who::Group("auth".into()),
            group_target("auth"),
            "Built-in group",
        ),
    ]
    .into_iter()
    .chain(draft.groups().iter().map(|row| {
        (
            Who::Group(row.value.name.clone()),
            format!("@{}", row.value.name),
            "Group",
        )
    }))
    .chain(accounts.into_iter().map(|(id, account)| {
        (
            Who::Person(id),
            account,
            if online(id) { "Online" } else { "Account" },
        )
    }))
    .map(|(who, label, kind)| Choice {
        key: who.key(),
        label,
        kind,
    })
    .collect();
    let mut section = section(
        "acl-check-title",
        "Check access",
        "Pick someone to see what they may do here and why: their account, groups and certificate, in this channel.",
    )
    .child(editor.check_picker.render(&format!("acl-check-{token}"), &choices, cx));
    let Some(check) = &editor.check else {
        return section.into_any_element();
    };
    if let Some(error) = &check.error {
        return section
            .child(note("acl-check-error", error.clone()))
            .into_any_element();
    }
    if let Some((parent, _)) = check.reading {
        return section
            .child(note(
                "acl-check-reading",
                format!("Reading the rules of {}…", channel_name(view, parent)),
            ))
            .into_any_element();
    }
    let online = match check.who {
        Who::Person(id) => view.users.values().find(|user| user.user_id == Some(id)),
        _ => None,
    };
    let tree = tree(check, draft);
    let subject = Subject {
        user_id: match check.who {
            Who::Everyone => None,
            Who::Group(_) => Some(MEMBER),
            Who::Person(id) => Some(id),
        },
        channel,
        hash: online.and_then(|user| user.certificate_hash.as_deref()),
        tokens: &[],
        strong: false,
    };
    let (granted, reasons) = acl::explain(&tree, &subject, channel);
    if editor.dirty {
        section = section.child(note("acl-check-draft", "Includes unsaved changes"));
    }
    for &(bit, label) in permission::NAMED {
        if channel != 0 && bit & ROOT_ONLY != 0 {
            continue;
        }
        let allowed = granted & bit != 0;
        let reason = reasons[bit.trailing_zeros() as usize];
        section = section.child(with_help(
            format!("acl-check-row-help-{token}-{bit}"),
            permission::description(bit),
            note(
                SharedString::from(format!("acl-check-row-{token}-{bit}")),
                format!(
                    "{label}: {} · {}",
                    if allowed { "Allowed" } else { "Not allowed" },
                    why(reason, allowed, &tree, view)
                ),
            ),
        ));
    }
    section.into_any_element()
}
