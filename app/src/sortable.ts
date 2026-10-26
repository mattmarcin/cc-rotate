// Reordering a list by dragging a row's handle. Pointer events rather than HTML drag and drop,
// which the desktop webviews handle differently (or hand to the window as a file drop).

export interface Drag {
  /** The rows that move, top to bottom, the dragged one among them. */
  rows: HTMLElement[];
  row: HTMLElement;
  /** The drag really started: the pointer moved past a few pixels. */
  started?: () => void;
  /** The row was dropped at index `to`, other than where it started. */
  dropped: (to: number) => void;
  /** The drag ended without a move: cancelled, or dropped where it started. */
  ended?: () => void;
}

/** Follow a pointer that went down on `grip` until it is released or Escape is pressed. */
export function dragRow(e: PointerEvent, grip: HTMLElement, d: Drag) {
  if (e.button !== 0 || d.rows.length < 2) return;
  e.preventDefault();
  const { rows, row } = d;
  const list = row.parentElement;
  const from = rows.indexOf(row);
  const boxes = rows.map((r) => r.getBoundingClientRect());
  const height = boxes[from].height;
  // Rows can have gaps between them: a neighbour moves by the dragged row's height plus its gap.
  const step = rows.length > 1 ? (boxes[boxes.length - 1].top - boxes[0].top) / (rows.length - 1) : height;
  const startY = e.clientY;
  let to = from;
  let moved = false;

  const update = (clientY: number) => {
    const min = boxes[0].top - boxes[from].top;
    const max = boxes[boxes.length - 1].bottom - boxes[from].bottom;
    const dy = Math.max(min, Math.min(max, clientY - startY));
    if (!moved && Math.abs(dy) < 3) return;
    if (!moved) {
      moved = true;
      d.started?.();
      row.classList.add("dragging");
      list?.classList.add("sorting");
      document.body.classList.add("grabbing");
    }
    row.style.transform = `translateY(${dy}px)`;
    const center = boxes[from].top + height / 2 + dy;
    to = from;
    boxes.forEach((b, i) => {
      const mid = b.top + b.height / 2;
      if (i < from && center < mid) to = Math.min(to, i);
      if (i > from && center > mid) to = Math.max(to, i);
    });
    rows.forEach((r, i) => {
      if (i === from) return;
      const shift = from < to && i > from && i <= to ? -step : to < from && i >= to && i < from ? step : 0;
      r.style.transform = shift ? `translateY(${shift}px)` : "";
    });
  };
  const finish = (commit: boolean) => {
    grip.removeEventListener("pointermove", onMove);
    grip.removeEventListener("pointerup", onUp);
    grip.removeEventListener("pointercancel", onCancel);
    window.removeEventListener("keydown", onKey, true);
    rows.forEach((r) => (r.style.transform = ""));
    row.classList.remove("dragging");
    list?.classList.remove("sorting");
    document.body.classList.remove("grabbing");
    if (commit && moved && to !== from) d.dropped(to);
    else d.ended?.();
  };
  const onMove = (ev: PointerEvent) => update(ev.clientY);
  const onUp = () => finish(true);
  const onCancel = () => finish(false);
  const onKey = (ev: KeyboardEvent) => {
    if (ev.key === "Escape") {
      ev.preventDefault();
      ev.stopPropagation();
      finish(false);
    }
  };
  grip.setPointerCapture(e.pointerId);
  grip.addEventListener("pointermove", onMove);
  grip.addEventListener("pointerup", onUp);
  grip.addEventListener("pointercancel", onCancel);
  window.addEventListener("keydown", onKey, true);
}

/** How far the arrow, Home and End keys move a row at `at` of `count`; 0 for other keys. */
export function keyMove(e: KeyboardEvent, at: number, count: number): number {
  switch (e.key) {
    case "ArrowUp":
      return at > 0 ? -1 : 0;
    case "ArrowDown":
      return at < count - 1 ? 1 : 0;
    case "Home":
      return -at;
    case "End":
      return count - 1 - at;
    default:
      return 0;
  }
}
