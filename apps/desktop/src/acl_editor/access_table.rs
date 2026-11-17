// SPDX-License-Identifier: EUPL-1.2
//! Channel permissions (#341): the channel's own rules as a table of rows
//! (Everyone, groups, people) and five columns. A cell cycles Not set, Allow
//! and Deny and rewrites the rules in the table's order.
use super::{Editor, MEMBER, change, group_target, name, note, section, with_help};
use crate::{
    dialogs::{Tone, Tooltip, named_button, styled_button},
    state::{Desktop, ServerView},
    theme::*,
};
use gpui_kit::{prelude::*, *};
use nuschl_core::{
    access::{self, Bits, COLUMNS, Cell, Who},
    acl::{self, Reason, Subject},
    admin,
    model::{Acl, AclEntry, permission},
};
use std::collections::HashMap;

fn label(view: &ServerView, who: &Who) -> String {
    match who {
        Who::Everyone => "Everyone".into(),
        Who::Group(group) => group_target(group),
        Who::Person(id) => name(view, *id),
    }
}

fn cycle(this: &mut Desktop, token: u64, who: Who, column: usize) {
    change(this, token, |editor| {
        let Some(draft) = editor.state.draft_mut() else {
            return;
        };
        let Some(mut table) = draft.table() else {
            return;
        };
        let bits = table.row(who);
        let next = match bits.cell(column) {
            Cell::NotSet | Cell::Custom => Cell::Allow,
            Cell::Allow => Cell::Deny,
            Cell::Deny => Cell::NotSet,
        };
        bits.set(column, next);
        if let Err(error) = draft.set_table(&table) {
            editor.state.error = true;
            editor.state.message = error.into();
        }
        editor.selected_rule = None;
    });
}

/// Clears everything `who`'s row sets here; inherited values stay.
fn remove(this: &mut Desktop, token: u64, who: Who) {
    change(this, token, |editor| {
        editor.table_rows.retain(|row| *row != who);
        let Some(draft) = editor.state.draft_mut() else {
            return;
        };
        let Some(mut table) = draft.table() else {
            return;
        };
        table.rows.retain(|(row, _)| *row != who);
        if let Err(error) = draft.set_table(&table) {
            editor.state.error = true;
            editor.state.message = error.into();
        }
        editor.selected_rule = None;
    });
}

/// What a member of `who`'s row may do in the channel, and why.
fn evaluate(snapshot: &Acl, base: &acl::Node, who: &Who) -> (u32, [Reason; 32]) {
    let mut node = base.clone();
    if let Who::Group(group) = who {
        node.groups
            .entry(group.clone())
            .or_default()
            .add
            .insert(MEMBER);
    }
    let channel = snapshot.channel;
    let tree = HashMap::from([(channel, node)]);
    let subject = Subject {
        user_id: match who {
            Who::Everyone => None,
            Who::Group(_) => Some(MEMBER),
            Who::Person(id) => Some(*id),
        },
        channel,
        hash: None,
        tokens: &[],
        strong: false,
    };
    acl::explain(&tree, &subject, channel)
}

/// What decided a permission, in words.
fn source(reason: Reason, entries: &[AclEntry], who: &Who, view: &ServerView) -> String {
    match reason {
        Reason::Default => "Mumble's default".into(),
        Reason::Rule { index, .. } => {
            let Some(entry) = entries.get(index) else {
                return "a rule".into();
            };
            let target = match entry.user_id {
                Some(id) => name(view, id),
                None => group_target(entry.group.as_deref().unwrap_or("")),
            };
            if entry.inherited {
                format!("a {target} rule in a parent channel")
            } else if access::who(entry).as_ref() == Some(who) {
                "this row".into()
            } else {
                format!("the {target} row")
            }
        }
        Reason::Write => "Write ACL".into(),
        Reason::Superuser => "SuperUser".into(),
        Reason::Unreachable { .. } => "Traverse denied here".into(),
    }
}

/// For a partly allowed column, which permissions are allowed.
fn partly(bits: u32, allowed: u32) -> String {
    if allowed == 0 || allowed == bits {
        return String::new();
    }
    let names = |wanted: bool| {
        permission::NAMED
            .iter()
            .filter(|(bit, _)| bits & bit != 0 && (allowed & bit != 0) == wanted)
            .map(|(_, label)| *label)
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(" Allows {}; not {}.", names(true), names(false))
}

fn row_help(who: &Who) -> String {
    match who {
        Who::Everyone => "Everyone, unless a group or person row sets the same column.".into(),
        Who::Group(group) if group == "auth" => {
            "Everyone with a registered account, unless a person row sets the same column.".into()
        }
        Who::Group(group) => {
            format!("Members of @{group}, unless a person row sets the same column.")
        }
        Who::Person(_) => "This registered account.".into(),
    }
}

/// Permissions outside the columns that a row sets, in words.
fn others(grant: u32, deny: u32) -> Option<String> {
    let shown = COLUMNS.iter().fold(0, |bits, (_, column, _)| bits | column);
    let names = |bits: u32| {
        permission::NAMED
            .iter()
            .filter(|(bit, _)| bits & bit & !shown != 0)
            .map(|(_, label)| *label)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (allowed, denied) = (names(grant), names(deny));
    match (allowed.is_empty(), denied.is_empty()) {
        (true, true) => None,
        (false, true) => Some(format!("Also allows {allowed}")),
        (true, false) => Some(format!("Also denies {denied}")),
        (false, false) => Some(format!("Also allows {allowed}; denies {denied}")),
    }
}

pub(super) fn render(view: &ServerView, editor: &Editor, cx: &mut Context<Desktop>) -> AnyElement {
    let token = editor.token;
    let section = section(
        "acl-table-title",
        "Channel permissions",
        "What each person or group may do in this channel and its subchannels. A person's setting beats their groups'; a group's beats Everyone's. Dimmed cells come from defaults, other rows or parent channels.",
    );
    let Some(draft) = editor.state.draft() else {
        return section.into_any_element();
    };
    let Some(mut table) = draft.table() else {
        return section
            .child(note(
                "acl-table-advanced",
                "This channel's rules depend on their order in ways the table cannot show. Edit them under Advanced rules.",
            ))
            .into_any_element();
    };
    for who in &editor.table_rows {
        table.row(who.clone());
    }
    let snapshot = draft.snapshot();
    let base = admin::flat_node(&snapshot);
    let inherited = if draft.inherit_acls() {
        access::inherited(&snapshot.entries)
    } else {
        Vec::new()
    };
    let bits_of = |rows: &[(Who, Bits)], who: &Who| {
        rows.iter()
            .find(|(row, _)| row == who)
            .map_or_else(Bits::default, |(_, bits)| *bits)
    };
    let mut targets: Vec<Who> = table
        .rows
        .iter()
        .chain(&inherited)
        .map(|(who, _)| who.clone())
        .collect();
    targets.sort();
    targets.dedup();
    let line = || {
        div()
            .flex()
            .items_center()
            .gap_1()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(color(BORDER))
    };
    let mut header = div()
        .flex()
        .items_center()
        .gap_1()
        .px_3()
        .py_2()
        .bg(color(ELEVATED))
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .child(div().w(px(130.)).flex_shrink_0().child("Who"));
    for (column, (heading, _, help)) in COLUMNS.iter().enumerate() {
        header = header.child(
            div()
                .id(SharedString::from(format!("acl-column-{token}-{column}")))
                .flex_1()
                .min_w_0()
                .pl_3()
                .whitespace_nowrap()
                .tooltip(move |_, cx| cx.new(|_| Tooltip((*help).into())).into())
                .child(SharedString::from(*heading)),
        );
    }
    let mut grid = div()
        .flex()
        .flex_col()
        .border_1()
        .border_color(color(BORDER))
        .rounded_md()
        .overflow_hidden()
        .child(header.child(div().w(px(84.)).flex_shrink_0()));
    if targets.is_empty() {
        grid = grid.child(line().py_2().child(note(
            "acl-table-empty",
            "Nothing set here or above. Add someone below.",
        )));
    }
    for (number, who) in targets.iter().enumerate() {
        let own = bits_of(&table.rows, who);
        let row_label = label(view, who);
        let mut row = line().child(
            div()
                .id(SharedString::from(format!("acl-row-{token}-{number}")))
                .w(px(130.))
                .flex_shrink_0()
                .tooltip({
                    let help = row_help(who);
                    move |_, cx| cx.new(|_| Tooltip(help.clone().into())).into()
                })
                .child(SharedString::from(row_label.clone())),
        );
        let (granted, reasons) = evaluate(&snapshot, &base, who);
        for (column, (heading, bits, _)) in COLUMNS.iter().enumerate() {
            let target = who.clone();
            let click = move |this: &mut Desktop, _: &mut Window, cx: &mut Context<Desktop>| {
                cycle(this, token, target.clone(), column);
                cx.notify();
            };
            let allowed = granted & bits;
            let (label, state) = match allowed {
                0 => ("No", "Not allowed"),
                _ if allowed == *bits => ("Yes", "Allowed"),
                _ => ("Partly", "Partly allowed"),
            };
            let detail = partly(*bits, allowed);
            let cell = own.cell(column);
            let (tone, set_here, help) = if cell == Cell::NotSet {
                let mut sources: Vec<String> = Vec::new();
                for bit in (0..32).filter(|bit| bits & (1 << bit) != 0) {
                    let source = source(reasons[bit], &snapshot.entries, who, view);
                    if !sources.contains(&source) {
                        sources.push(source);
                    }
                }
                (
                    Tone::Ghost,
                    false,
                    format!(
                        "{state} by {}.{detail} Click to set it here.",
                        sources.join(" and ")
                    ),
                )
            } else {
                let tone = match cell {
                    Cell::Allow => Tone::Primary,
                    Cell::Deny => Tone::Danger,
                    Cell::Custom | Cell::NotSet => Tone::Secondary,
                };
                (
                    tone,
                    true,
                    format!("{state}, set here.{detail} Click to change it."),
                )
            };
            let button = named_button(
                SharedString::from(format!("acl-cell-{token}-{number}-{column}")),
                label,
                format!(
                    "{row_label} · {heading}: {label}, {}",
                    if set_here { "set here" } else { "not set here" }
                ),
                tone,
                click,
                cx,
            );
            row = row.child(
                div()
                    .id(SharedString::from(format!(
                        "acl-cell-help-{token}-{number}-{column}"
                    )))
                    .flex_1()
                    .min_w_0()
                    .when(!set_here, |cell| cell.opacity(0.6))
                    .tooltip(move |_, cx| cx.new(|_| Tooltip(help.clone().into())).into())
                    .child(button),
            );
        }
        let mut end = div().w(px(84.)).flex_shrink_0();
        if own != Bits::default() || editor.table_rows.contains(who) {
            let target = who.clone();
            end = end.child(named_button(
                SharedString::from(format!("acl-remove-row-{token}-{number}")),
                "Remove",
                format!("Remove {row_label}"),
                Tone::Ghost,
                move |this, _, cx| {
                    remove(this, token, target.clone());
                    cx.notify();
                },
                cx,
            ));
        }
        grid = grid.child(row.child(end));
        if let Some(others) = others(own.grant, own.deny) {
            grid = grid.child(div().px_3().pb_1().child(note(
                SharedString::from(format!("acl-table-others-{token}-{number}")),
                others,
            )));
        }
    }
    let section = section.child(grid);
    let mut people: Vec<_> = view
        .users
        .values()
        .filter_map(|user| Some((user.user_id?, user.name.clone())))
        .collect();
    people.sort_by(|a, b| a.1.cmp(&b.1));
    let candidates = [Who::Everyone, Who::Group("auth".into())]
        .into_iter()
        .chain(
            draft
                .groups()
                .iter()
                .map(|row| Who::Group(row.value.name.clone())),
        )
        .chain(people.into_iter().map(|(id, _)| Who::Person(id)))
        .filter(|who| !targets.contains(who));
    let mut picker = div().flex().flex_wrap().gap_2();
    for (number, who) in candidates.enumerate() {
        let row_label = label(view, &who);
        picker = picker.child(with_help(
            format!("acl-table-add-help-{token}-{number}"),
            "Add a row; it is saved once one of its cells is set.",
            styled_button(
                SharedString::from(format!("acl-table-add-{token}-{number}")),
                format!("Add {row_label}"),
                Tone::Ghost,
                move |this, _, cx| {
                    change(this, token, |editor| {
                        if !editor.table_rows.contains(&who) {
                            editor.table_rows.push(who.clone());
                        }
                    });
                    cx.notify();
                },
                cx,
            ),
        ));
    }
    section.child(picker).into_any_element()
}
