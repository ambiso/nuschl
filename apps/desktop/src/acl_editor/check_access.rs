// SPDX-License-Identifier: EUPL-1.2
//! Check access (#341): what someone may do in the edited channel and what
//! decided each permission. The parents' own rules are read one query at a
//! time; the channel itself comes from the draft, so unsaved changes count.
use super::{Editor, active_editor, group_target, name, note, with_help};
use crate::{
    dialogs::{Tone, styled_button},
    menus::{ActionOrigin, MenuTarget},
    panels::{Panel, channel_name},
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

/// A registration number servers leave unused, standing for "a member of
/// this group".
const MEMBER: u32 = u32::MAX;

#[derive(Clone, PartialEq, Eq)]
pub enum Who {
    Everyone,
    Group(String),
    /// Someone online, by session.
    Person(u32),
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

fn pick(this: &mut Desktop, token: u64, who: Who) {
    if let Some(editor) = active_editor(this, token) {
        editor.check = Some(Check {
            who,
            parents: Vec::new(),
            reading: None,
            error: None,
        });
        next(this, token);
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
    let mut people: Vec<_> = view.users.values().collect();
    people.sort_by(|a, b| a.name.cmp(&b.name));
    let choices = [(
        Who::Everyone,
        "Everyone".to_owned(),
        "Someone without an account, in this channel.",
    )]
    .into_iter()
    .chain(draft.groups().iter().map(|row| {
        (
            Who::Group(row.value.name.clone()),
            format!("@{}", row.value.name),
            "A registered member of this group, in this channel.",
        )
    }))
    .chain(people.into_iter().map(|user| {
        (
            Who::Person(user.session),
            user.name.clone(),
            "As they are now: their account, certificate and channel.",
        )
    }));
    let chosen = editor.check.as_ref().map(|check| &check.who);
    let mut picker = div().flex().flex_wrap().gap_2();
    for (number, (who, label, help)) in choices.enumerate() {
        let selected = chosen == Some(&who);
        picker = picker.child(with_help(
            format!("acl-check-help-{token}-{number}"),
            help,
            styled_button(
                SharedString::from(format!("acl-check-{token}-{number}")),
                if selected {
                    format!("✓ {label}")
                } else {
                    label
                },
                Tone::Ghost,
                move |this, _, cx| {
                    pick(this, token, who.clone());
                    cx.notify();
                },
                cx,
            ),
        ));
    }
    let mut section = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(note("acl-check-title", "Check access"))
        .child(picker);
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
    let person = match check.who {
        Who::Person(session) => match view.users.get(&session) {
            Some(user) => Some(user),
            None => {
                return section
                    .child(note("acl-check-left", "They left the server."))
                    .into_any_element();
            }
        },
        _ => None,
    };
    let tree = tree(check, draft);
    let subject = Subject {
        user_id: match check.who {
            Who::Everyone => None,
            Who::Group(_) => Some(MEMBER),
            Who::Person(_) => person.and_then(|user| user.user_id),
        },
        channel: person.map_or(channel, |user| user.channel),
        hash: person.and_then(|user| user.certificate_hash.as_deref()),
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
