// SPDX-License-Identifier: EUPL-1.2
//! A searchable single choice: a field that shows the chosen option and,
//! while it has focus, lists the options matching what is typed. Typed text
//! matches as in the quick switcher (`nuschl_client::finder::fuzzy`), best
//! first; otherwise the callers' order holds. The list scrolls, eight rows
//! tall; arrow keys move the highlight, Enter or a click picks.
use crate::{
    state::{Desktop, accessible},
    theme::*,
};
use gpui_kit::{
    base::input::{Input, InputEvent, InputState, MoveDown, MoveUp},
    prelude::*,
    *,
};
use nuschl_client::{
    finder::{fold, fuzzy},
    platform::request_id,
};
use std::{cell::RefCell, ops::Range, rc::Rc};

const ROW: f32 = 30.;
const VISIBLE_ROWS: usize = 8;

pub struct Choice {
    pub key: String,
    pub label: String,
    /// Shown muted after the label, such as "Group" or "Online".
    pub kind: &'static str,
}

/// A match as last drawn: the choice and the letters the query matched.
#[derive(Clone)]
struct Shown {
    key: String,
    label: String,
    kind: &'static str,
    highlights: Vec<Range<usize>>,
}

/// Finds the picker in the application state; its subscription ends with it.
pub type Locate = fn(&mut Desktop) -> Option<&mut Picker>;
type Pick = Rc<dyn Fn(&mut Desktop, &str, &mut Window, &mut Context<Desktop>)>;

pub struct Picker {
    label: &'static str,
    /// Tells this picker's list apart from earlier ones in callbacks.
    version: u64,
    input: Entity<InputState>,
    locate: Locate,
    on_pick: Pick,
    open: bool,
    /// The field holds typed text, which filters the list.
    typed: bool,
    highlighted: usize,
    shown: RefCell<Vec<Shown>>,
    scroll: UniformListScrollHandle,
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
                    picker.highlight(0);
                    input.update(cx, |input, cx| input.select_all(window, cx));
                }
                InputEvent::Blur => picker.open = false,
                InputEvent::Change => {
                    picker.open = true;
                    picker.typed = true;
                    picker.highlight(0);
                }
                InputEvent::PressEnter { .. } => {
                    let choice = picker.shown.borrow().get(picker.highlighted).cloned();
                    if let Some(choice) = choice {
                        choose(this, locate, &choice.key, &choice.label, window, cx);
                    }
                }
            }
            cx.notify();
        });
        Self {
            label,
            version: request_id(),
            input,
            locate,
            on_pick: Rc::new(on_pick),
            open: false,
            typed: false,
            highlighted: 0,
            shown: RefCell::default(),
            scroll: UniformListScrollHandle::new(),
            _changes: changes,
        }
    }

    /// Empties the field, for pickers that add rather than choose.
    pub fn clear(&self, window: &mut Window, cx: &mut Context<Desktop>) {
        self.input
            .update(cx, |input, cx| input.set_value(String::new(), window, cx));
    }

    fn highlight(&mut self, index: usize) {
        self.highlighted = index;
        self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
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
        let mut matches: Vec<(i32, Shown)> = choices
            .iter()
            .filter_map(|choice| {
                let (score, highlights) = if !self.typed || query.is_empty() {
                    (0, Vec::new())
                } else {
                    fuzzy(&query, &choice.label)?
                };
                Some((
                    score,
                    Shown {
                        key: choice.key.clone(),
                        label: choice.label.clone(),
                        kind: choice.kind,
                        highlights,
                    },
                ))
            })
            .collect();
        matches.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        let count = matches.len();
        *self.shown.borrow_mut() = matches.into_iter().map(|(_, shown)| shown).collect();
        let locate = self.locate;
        let field = div()
            .flex()
            .flex_col()
            .gap_1()
            // Ahead of the field, which would move its cursor.
            .capture_action(cx.listener(move |this, _: &MoveUp, _, cx| {
                if let Some(picker) = locate(this).filter(|picker| picker.open) {
                    picker.highlight(picker.highlighted.saturating_sub(1));
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .capture_action(cx.listener(move |this, _: &MoveDown, _, cx| {
                if let Some(picker) = locate(this) {
                    let last = picker.shown.borrow().len().saturating_sub(1);
                    let next = if picker.open {
                        (picker.highlighted + 1).min(last)
                    } else {
                        0
                    };
                    picker.open = true;
                    picker.highlight(next);
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
        let frame = div()
            .p_1()
            .rounded(px(6.))
            .border_1()
            .border_color(color(BORDER))
            .bg(color(ELEVATED));
        if count == 0 {
            return field
                .child(frame.text_sm().text_color(color(TEXT_MUTED)).child(
                    text!("Nothing matches.").with_id(SharedString::from(format!("{id}-empty"))),
                ))
                .into_any_element();
        }
        let list = uniform_list(
            SharedString::from(format!("{id}-options")),
            count,
            cx.processor(move |this, range: Range<usize>, _, cx| {
                let Some(picker) = locate(this) else {
                    return Vec::new();
                };
                let highlighted = picker.highlighted;
                let shown = picker.shown.borrow().clone();
                range
                    .filter_map(|index| {
                        let choice = shown.get(index)?;
                        Some(option(
                            index,
                            count,
                            index == highlighted,
                            choice,
                            locate,
                            cx,
                        ))
                    })
                    .collect()
            }),
        )
        .track_scroll(&self.scroll)
        .h(px(ROW * count.min(VISIBLE_ROWS) as f32))
        .w_full();
        let version = self.version;
        let list = crate::dialogs::nested_virtual_list(
            "picker-options",
            version,
            &self.scroll,
            list,
            // The list's handlers exist only while it is drawn.
            |_: &Desktop| true,
            cx,
        )
        .role(Role::ListBox)
        .aria_label(self.label);
        field.child(frame.child(list)).into_any_element()
    }
}

fn option(
    index: usize,
    count: usize,
    highlighted: bool,
    choice: &Shown,
    locate: Locate,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let (key, label) = (choice.key.clone(), choice.label.clone());
    div()
        .id(("picker-option", index))
        .role(Role::ListBoxOption)
        .aria_label(choice.label.clone())
        .aria_selected(highlighted)
        .aria_position_in_set(index + 1)
        .aria_size_of_set(count)
        .h(px(ROW))
        .px_2()
        .rounded(px(4.))
        .flex()
        .items_center()
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
        .child(div().min_w_0().truncate().child(crate::finder::matched(
            choice.label.clone(),
            &choice.highlights,
        )))
        .child(
            div()
                .flex_shrink_0()
                .text_sm()
                .text_color(color(TEXT_MUTED))
                .child(Text::new_inaccessible(choice.kind.into())),
        )
        .into_any_element()
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
