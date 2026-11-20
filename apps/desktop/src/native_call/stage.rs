// SPDX-License-Identifier: EUPL-1.2
//! Fullscreen controls stay in the focus order while their paint fades.

use crate::{motion::Ease, state::Desktop};
use gpui_kit::*;
use std::time::{Duration, Instant};

const IDLE: Duration = Duration::from_secs(3);

pub(super) struct Controls {
    pub(super) focus: FocusHandle,
    last_activity: Instant,
    hover: bool,
    fade: Ease,
    timer: Option<Task<()>>,
    _focus_in: Subscription,
    _focus_out: Subscription,
}

impl Controls {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Desktop>) -> Self {
        let focus = cx.focus_handle();
        let focus_in = cx.on_focus_in(&focus, window, |this, window, cx| {
            activity(this, cx);
            window.on_next_frame(|window, _| window.refresh());
        });
        let focus_out = cx.on_focus_out(&focus, window, |this, _, window, cx| {
            activity(this, cx);
            window.on_next_frame(|window, _| window.refresh());
        });
        Self {
            focus,
            last_activity: Instant::now(),
            hover: false,
            fade: Ease::new(true),
            timer: None,
            _focus_in: focus_in,
            _focus_out: focus_out,
        }
    }

    pub(super) fn opacity(&mut self, menu_open: bool, window: &mut Window, cx: &App) -> f32 {
        let hold = self.hover || menu_open || self.focus.contains_focused(window, cx);
        let visible = hold || self.last_activity.elapsed() < IDLE;
        if cx.reduce_motion() {
            self.fade = Ease::new(visible);
            return self.fade.value();
        }
        self.fade.set(visible, false);
        if self.fade.moving() {
            window.request_animation_frame();
        }
        self.fade.value()
    }
}

pub(super) fn activity(this: &mut Desktop, cx: &mut Context<Desktop>) {
    let Some(controls) = this
        .native_call
        .as_mut()
        .and_then(|call| call.stage.as_mut())
    else {
        return;
    };
    controls.last_activity = Instant::now();
    if !controls.fade.target() {
        cx.notify();
    }
    if controls.timer.is_none() {
        let generation = this.native_call_generation;
        controls.timer = Some(cx.spawn(async move |this, cx| {
            let mut delay = IDLE;
            loop {
                cx.background_executor().timer(delay).await;
                let Ok(Some(remaining)) = this.update(cx, |this, cx| {
                    let controls = this
                        .native_call
                        .as_mut()
                        .filter(|_| this.native_call_generation == generation)?
                        .stage
                        .as_mut()?;
                    let remaining = IDLE.saturating_sub(controls.last_activity.elapsed());
                    if remaining.is_zero() {
                        controls.timer = None;
                        cx.notify();
                    }
                    Some(remaining)
                }) else {
                    break;
                };
                if remaining.is_zero() {
                    break;
                }
                delay = remaining;
            }
        }));
    }
}

pub(super) fn hover(this: &mut Desktop, hovering: bool, cx: &mut Context<Desktop>) {
    if let Some(controls) = this
        .native_call
        .as_mut()
        .and_then(|call| call.stage.as_mut())
    {
        controls.hover = hovering;
        activity(this, cx);
    }
}
