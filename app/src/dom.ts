// A tiny element builder: h("div", { class: "x", onclick: fn }, child, "text").

type Attrs = Record<string, unknown>;
export type Child = Node | string | number | null | undefined | false | Child[];

export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: Attrs | null = null,
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs ?? {})) {
    if (value === undefined || value === null || value === false) continue;
    if (key.startsWith("on") && typeof value === "function") {
      el.addEventListener(key.slice(2), value as EventListener);
    } else if (key === "style" && typeof value === "object") {
      Object.assign(el.style, value);
    } else if (key in el && typeof value !== "string") {
      (el as unknown as Record<string, unknown>)[key] = value;
    } else {
      el.setAttribute(key, value === true ? "" : String(value));
    }
  }
  append(el, children);
  return el;
}

function append(el: Element, children: Child[]) {
  for (const child of children) {
    if (child === null || child === undefined || child === false) continue;
    if (Array.isArray(child)) append(el, child);
    else el.append(child instanceof Node ? child : String(child));
  }
}

export function replace(target: Element, ...children: Child[]) {
  target.replaceChildren();
  append(target, children);
}
