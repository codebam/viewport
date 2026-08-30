/* Example user layout. Loaded only when named by layout_extensions. */
registerLayout('monocle', {
  render({ root, views, focusedId, helpers }) {
    if (!root) return null;
    const ids = helpers.dynamicOrder(root).filter(
      (id) => views.has(id) && !helpers.isFloating(id) && !helpers.isMinimized(id));
    const id = ids.includes(focusedId) ? focusedId : ids[0];
    const view = views.get(id);
    if (!view) return null;
    view.el.classList.add('monocle');
    view.el.style.flexGrow = '1';
    helpers.markRendered(id);
    return view.el;
  },

  clear({ views }) {
    for (const view of views.values()) {
      view.el.classList.remove('monocle');
    }
  },
});
