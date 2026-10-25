// A small fake DOM for the session-list tests: elements with attributes, a parent, an ordered child list and the node
// operations the list renderer uses (insertBefore, removeChild, nextSibling). Counts the operations so a test can say
// "nothing was inserted" (local #275).
export class El {
  constructor(tag) {
    this.tagName = tag.toUpperCase();
    this.attrs = new Map();
    this.children = [];
    this.parentNode = null;
    this.dataset = {};
    this.style = { setProperty() {} };
    this.className = '';
    this.textContent = '';
    this.title = '';
    this.id = '';
    this.tabIndex = -1;
    this.ops = { insert: 0, remove: 0 };
  }
  get firstChild() { return this.children[0] ?? null; }
  get nextSibling() { return this.parentNode ? this.parentNode.children[this.parentNode.children.indexOf(this) + 1] ?? null : null; }
  setAttribute(k, v) { this.attrs.set(k, String(v)); }
  getAttribute(k) { return this.attrs.has(k) ? this.attrs.get(k) : null; }
  removeAttribute(k) { this.attrs.delete(k); }
  append(...n) { for (const c of n) { c.parentNode = this; this.children.push(c); } }
  prepend(...n) { for (const c of n) c.parentNode = this; this.children.unshift(...n); }
  replaceChildren(...n) { for (const c of this.children) c.parentNode = null; this.children = []; this.append(...n); }
  removeChild(c) { this.children = this.children.filter((x) => x !== c); c.parentNode = null; this.ops.remove += 1; return c; }
  insertBefore(node, ref) {
    if (node.parentNode) node.parentNode.children = node.parentNode.children.filter((x) => x !== node);
    const at = ref ? this.children.indexOf(ref) : this.children.length;
    this.children.splice(at, 0, node);
    node.parentNode = this;
    this.ops.insert += 1;
    return node;
  }
  addEventListener() {}
}
