// --- Kage measurement only, never shipped: dump every preference ---------
// Appended by scripts/audit-camoufox-prefs.mjs to a copy of a browser's
// autoconfig file. Twelve seconds after startup (after the profile's own
// preferences and the driver's) it writes every preference with its
// default and its effective value to $HOME/kage-prefs.json.
(function () {
  const C = typeof Cc !== "undefined" ? Cc : Components.classes;
  const I = typeof Ci !== "undefined" ? Ci : Components.interfaces;
  const svc =
    typeof Services !== "undefined"
      ? Services.prefs
      : C["@mozilla.org/preferences-service;1"].getService(I.nsIPrefService).QueryInterface(I.nsIPrefBranch);
  const home = getenv("HOME");
  if (!home) {
    return;
  }
  const dump = () => {
    const defaults = svc.getDefaultBranch("");
    const read = (branch, name, type) => {
      try {
        if (type === svc.PREF_BOOL) return branch.getBoolPref(name);
        if (type === svc.PREF_INT) return branch.getIntPref(name);
        if (type === svc.PREF_STRING) return branch.getStringPref(name);
      } catch (e) {
        return null;
      }
      return null;
    };
    const out = [];
    for (const name of svc.getChildList("")) {
      const type = svc.getPrefType(name);
      out.push({
        name,
        type: type === svc.PREF_BOOL ? "bool" : type === svc.PREF_INT ? "int" : "string",
        value: read(svc, name, type),
        default: read(defaults, name, type),
        user: svc.prefHasUserValue(name),
        locked: svc.prefIsLocked(name),
      });
    }
    const file = C["@mozilla.org/file/local;1"].createInstance(I.nsIFile);
    file.initWithPath(home + "/kage-prefs.json");
    const stream = C["@mozilla.org/network/file-output-stream;1"].createInstance(I.nsIFileOutputStream);
    stream.init(file, 0x02 | 0x08 | 0x20, 0o600, 0);
    const writer = C["@mozilla.org/intl/converter-output-stream;1"].createInstance(I.nsIConverterOutputStream);
    writer.init(stream, "UTF-8");
    writer.writeString(JSON.stringify(out));
    writer.close();
  };
  const timer = C["@mozilla.org/timer;1"].createInstance(I.nsITimer);
  timer.initWithCallback({ notify: dump }, 12000, I.nsITimer.TYPE_ONE_SHOT);
  globalThis.__kagePrefDumpTimer = timer;
})();
