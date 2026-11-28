/* SPDX-License-Identifier: MIT
 *
 * Example user shell extension: Workspace Pill.
 *
 * Copy this folder to ~/.config/viewport/extensions/workspace-pill/ to try it.
 * Edit index.js or style.css and save: Viewport watches the directory and
 * live hot-reloads the extension in place without restarting the compositor.
 */
window.viewportExtensions.register('workspace-pill', {
  mount(container, ctx) {
    const pill = document.createElement('span');
    pill.className = 'vp-ext-workspace-pill';
    container.append(pill);

    const prefix = typeof ctx.options.prefix === 'string' && ctx.options.prefix
      ? ctx.options.prefix
      : 'WS';

    const unsubscribe = ctx.subscribe((state) => {
      const ws = state.workspaces.find((entry) => entry.active);
      const label = ws ? ws.name : (state.activeWorkspace ?? '?');
      const count = state.windows.length;
      const focused = state.windows.find((w) => w.focused);
      const focusPart = focused ? ` · ${focused.app_id || focused.title}` : '';
      const text = `${prefix} ${label} (${count})${focusPart}`;
      if (pill.textContent !== text) {
        pill.textContent = text;
      }
    });

    return () => {
      unsubscribe();
    };
  },

  unmount(container) {
    container.replaceChildren();
  },
});
