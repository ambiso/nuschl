// SPDX-License-Identifier: EUPL-1.2
// Horizontal swipes for the mobile navigation drawer: a right swipe on the chat
// opens it, a left swipe on the drawer closes it. Pure decisions over pointer
// samples ({x, y, t}: CSS pixels and milliseconds), and which targets keep their drags.
export const SWIPE = Object.freeze({
  slop: 10, // travel before the axis is decided
  dominance: 1.5, // horizontal travel must exceed vertical travel this many times
  hold: 450, // a press held this long before moving is a long press (selection)
  distance: 64, // travel that commits on release
});

// direction: 1 swipes right, -1 swipes left.
export function startSwipe(point, direction) {
  return {direction, start:point, last:point, axis:"pending"};
}

export function moveSwipe(swipe, point) {
  if (swipe.axis === "rejected") return swipe;
  const dx = point.x - swipe.start.x, dy = point.y - swipe.start.y;
  let axis = swipe.axis;
  if (axis === "pending") {
    if (Math.hypot(dx, dy) >= SWIPE.slop) {
      axis = dx * swipe.direction > 0 && Math.abs(dx) >= SWIPE.dominance * Math.abs(dy) ? "horizontal" : "rejected";
    } else if (point.t - swipe.start.t > SWIPE.hold) axis = "rejected";
  }
  return {...swipe, last:point, axis};
}

// Whether releasing here completes the swipe.
export function endSwipe(swipe) {
  const travel = (swipe.last.x - swipe.start.x) * swipe.direction;
  return swipe.axis === "horizontal" && travel >= SWIPE.distance && travel >= Math.abs(swipe.last.y - swipe.start.y);
}

// Where a horizontal drag already means something, it keeps that meaning: text
// fields, code, formulas and images, and drag handles. Elsewhere the browser
// cancels the pointer when it pans something sideways.
const EXEMPT = "input,textarea,select,[contenteditable],pre,canvas,video,.code-block,.image-button,.math-scroll,.server-order-handle";
export const swipeExempt = target => target.closest(EXEMPT) !== null;
