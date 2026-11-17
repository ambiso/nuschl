// SPDX-License-Identifier: EUPL-1.2
//! A searchable single choice: a field that shows the chosen option and,
//! while it has focus, lists the options matching what is typed. Typed text
//! matches as in the quick switcher (`nuschl_client::finder::fuzzy`), best
//! first; otherwise the callers' order holds. Up to eight options show at
//! once. Arrow keys move the highlight, Enter or a click picks.
use crate::{
    state::{Desktop, accessible},
    theme::*,
};
use gpui_kit::{
    base::input::{Input, InputEvent, InputState, MoveDown, MoveUp},
    prelude::*,
    *,
};
use nuschl_client::finder::{fold, fuzzy};
use std::{cell::RefCell, ops::Range, rc::Rc};

const SHOWN: usize = 8;

pub struct Choice {
    pub key: String,
    pub label: String,
    /// Shown muted after the label, such as "Group" or "Online".
    pub kind: &'static str,
}

/// Finds the picker in the application state; its subscription ends with it.
pub type Locate = fn(&mut Desktop) -> Option<&mut Picker>;
type Pick = Rc<dyn Fn(&mut Desktop, &str, &mut Window, &mut Context<Desktop>)>;

pub struct Picker {
    label: &'static str,
    input: Entity<InputState>,
    locate: Locate,
    on_pick: Pick,
    open: bool,
    /// The field holds typed text, which filters the list.
    typed: bool,
    highlighted: usize,
    /// The matches last drawn, as (key, label), for the keyboard.
    shown: RefCell<Vec<(String, String)>>,
    _changes: Subscription,
}

impl Picker {
    pub fn new(
        label: &'static str,
        placeholder: &'static str,
        locate: Locate,
        on_pick: impl Fn(&mut Desktop, &str, &mut Window, &mut Context<Desktop>) + 'static,
        window: &mut Window,
        cx: &mut Context<Desktop>,
    ) -> Self {
        let input = cx.new(|cx| {
            accessible(
                InputState::new(window, cx).placeholder(placeholder),
                label,
                false,
                false,
            )
        });
        let changes = cx.subscribe_in(&input, window, move |this, input, event, window, cx| {
            let Some(picker) = locate(this) else {
                return;
            };
            match event {
                InputEvent::Focus => {
                    picker.open = true;
                    picker.typed = false;
                    picker.highlighted = 0;
                    input.update(cx, |input, cx| input.select_all(window, cx));
                }
                InputEvent::Blur => picker.open = false,
                InputEvent::Change => {
                    picker.open = true;
                    picker.typed = true;
                    picker.highlighted = 0;
                }
                InputEvent::PressEnter { .. } => {
                    let choice = picker.shown.borrow().get(picker.highlighted).cloned();
                    if let Some((key, label)) = choice {
                        choose(this, locate, &key, &label, window, cx);
                    }
                }
            }
            cx.notify();
        });
        Self {
            label,
            input,
            locate,
            on_pick: Rc::new(on_pick),
            open: false,
            typed: false,
            highlighted: 0,
            shown: RefCell::default(),
            _changes: changes,
        }
    }

    pub fn render(&self, id: &str, choices: &[Choice], cx: &mut Context<Desktop>) -> AnyElement {
        let query: Vec<char> = self
            .input
            .read(cx)
            .value()
            .trim()
            .chars()
            .map(fold)
            .collect();
        let mut matches: Vec<(i32, Vec<Range<usize>>, &Choice)> = choices
            .iter()
            .filter_map(|choice| {
                if !self.typed || query.is_empty() {
                    return Some((0, Vec::new(), choice));
                }
                let (score, highlights) = fuzzy(&query, &choice.label)?;
                Some((score, highlights, choice))
            })
            .collect();
        matches.sort_by_key(|(score, _, _)| std::cmp::Reverse(*score));
        *self.shown.borrow_mut() = matches
            .iter()
            .take(SHOWN)
            .map(|(_, _, choice)| (choice.key.clone(), choice.label.clone()))
            .collect();
        let locate = self.locate;
        let mut field = div()
            .flex()
            .flex_col()
            .gap_1()
            // Ahead of the field, which would move its cursor.
            .capture_action(cx.listener(move |this, _: &MoveUp, _, cx| {
                if let Some(picker) = locate(this).filter(|picker| picker.open) {
                    picker.highlighted = picker.highlighted.saturating_sub(1);
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .capture_action(cx.listener(move |this, _: &MoveDown, _, cx| {
                if let Some(picker) = locate(this) {
                    let last = picker.shown.borrow().len().saturating_sub(1);
                    picker.highlighted = if picker.open {
                        (picker.highlighted + 1).min(last)
                    } else {
                        0
                    };
                    picker.open = true;
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(
                div()
                    .min_w_0()
                    .px_3()
                    .py_2()
                    .rounded(px(6.))
                    .bg(color(APP))
                    .border_1()
                    .border_color(color(BORDER))
                    .child(Input::new(&self.input)),
            );
        if !self.open {
            return field.into_any_element();
        }
        let mut list = div()
            .id(SharedString::from(format!("{id}-list")))
            .role(Role::ListBox)
            .aria_label(self.label)
            .flex()
            .flex_col()
            .p_1()
            .rounded(px(6.))
            .border_1()
            .border_color(color(BORDER))
            .bg(color(ELEVATED));
        let count = matches.len().min(SHOWN);
        for (index, (_, highlights, choice)) in matches.iter().take(SHOWN).enumerate() {
            let (key, label) = (choice.key.clone(), choice.label.clone());
            let highlighted = index == self.highlighted;
            list = list.child(
                div()
                    .id(SharedString::from(format!("{id}-option-{index}")))
                    .role(Role::ListBoxOption)
                    .aria_label(choice.label.clone())
                    .aria_selected(highlighted)
                    .aria_position_in_set(index + 1)
                    .aria_size_of_set(count)
                    .px_2()
                    .py_1()
                    .rounded(px(4.))
                    .flex()
                    .gap_2()
                    .cursor_pointer()
                    .when(highlighted, |row| row.bg(color(SELECTED)))
                    .hover(|style| style.bg(color(HOVER)))
                    // On press: the field loses focus, and the list, before a click ends.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            choose(this, locate, &key, &label, window, cx);
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .child(crate::finder::matched(choice.label.clone(), highlights))
                    .child(
                        div()
                            .text_color(color(TEXT_MUTED))
                            .child(Text::new_inaccessible(choice.kind.into())),
                    ),
            );
        }
        let hint = match matches.len() {
            0 => Some("Nothing matches.".to_owned()),
            total if total > SHOWN => Some(format!("{} more; type to narrow.", total - SHOWN)),
            _ => None,
        };
        if let Some(hint) = hint {
            list = list.child(
                div()
                    .px_2()
                    .py_1()
                    .text_sm()
                    .text_color(color(TEXT_MUTED))
                    .child(text!(hint).with_id(SharedString::from(format!("{id}-hint")))),
            );
        }
        field = field.child(list);
        field.into_any_element()
    }
}

/// Shows `label` in the field, closes the list and reports `key`.
fn choose(
    this: &mut Desktop,
    locate: Locate,
    key: &str,
    label: &str,
    window: &mut Window,
    cx: &mut Context<Desktop>,
) {
    let Some(picker) = locate(this) else {
        return;
    };
    picker.open = false;
    picker.typed = false;
    let (input, on_pick) = (picker.input.clone(), picker.on_pick.clone());
    input.update(cx, |input, cx| {
        input.set_value(label.to_owned(), window, cx)
    });
    on_pick(this, key, window, cx);
}
