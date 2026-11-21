// What the widget knows about the host's `ui/update-model-context`: whether
// the host advertised it (`hostCapabilities.updateModelContext`) and how the
// latest update ended. The widget sends this report with its next in-card
// call, so Triton's log shows whether a host applies card state to its model
// (the widget's sandbox is otherwise invisible from outside).
//
// `last`: none (nothing sent yet), pending (sent, no answer), accepted,
// rejected, or widget_state (ChatGPT's `setWidgetState`, no MCP Apps bridge).

export function hostContext() {
  let advertised = false;
  let last = 'none';
  let latest = 0;
  return {
    /** Record the host's capabilities from `ui/initialize`. */
    advertised(caps) {
      advertised = !!(caps && caps.updateModelContext);
    },
    /** Follow one update request; only the latest one decides `last`. */
    track(sent) {
      const n = ++latest;
      last = 'pending';
      return Promise.resolve(sent).then(
        () => { if (n === latest) last = 'accepted'; },
        () => { if (n === latest) last = 'rejected'; },
      );
    },
    widgetState() {
      latest++;
      last = 'widget_state';
    },
    report() {
      return { advertised, last };
    },
  };
}
