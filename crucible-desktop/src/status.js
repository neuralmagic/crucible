(() => {
  if (location.origin !== "__CONTROLLER_ORIGIN__") return;
  const invoke = (cmd, args) => window.__TAURI_INTERNALS__.invoke(cmd, args);
  const ACTIVE = new Set(["running", "building"]);
  const UNIT = { s: 1, m: 60, h: 3600, d: 86400 };
  let timer = null;

  const seconds = (text) => {
    const m = /^(\d+)([smhd])$/.exec(String(text).trim());
    return m ? Number(m[1]) * UNIT[m[2]] : null;
  };
  const duration = (s) => (s < 3600 ? `${Math.round(s / 60)}m` : `${(s / 3600).toFixed(1)}h`);

  const summarize = (run) => {
    const started = Date.parse(run.created_at);
    const elapsed = Number.isNaN(started) ? 0 : Math.max(0, (Date.now() - started) / 1000);
    const limit = seconds(run.max_time);
    const cost = run.cost_usd ?? 0;
    const byTime = limit ? elapsed / limit : 0;
    const byCost = run.max_cost > 0 ? cost / run.max_cost : 0;
    return {
      key: run.key,
      playbook: run.playbook,
      cost_usd: cost,
      elapsed: duration(elapsed),
      progress: Math.min(1, Math.max(byTime, byCost)),
    };
  };

  const json = async (path) => {
    const res = await fetch(path, { credentials: "same-origin" });
    return res.ok ? res.json() : null;
  };

  const refresh = async () => {
    timer = null;
    try {
      const [approvals, decisions, launches] = await Promise.all([
        json("/api/approvals"),
        json("/api/decisions"),
        json("/api/playbook-runs"),
      ]);
      if (approvals === null && launches === null) return;
      await invoke("report_status", {
        approvals:
          (approvals?.awaiting_approval?.length ?? 0) + (approvals?.pending_imports?.length ?? 0),
        decisions: decisions?.open?.length ?? 0,
        runs: (launches ?? []).filter((r) => ACTIVE.has(r.status)).map(summarize),
      });
    } catch (err) {
      console.warn("crucible-desktop: status refresh failed", err);
    }
  };

  const schedule = () => {
    if (timer === null) timer = setTimeout(refresh, 2000);
  };

  const start = () => {
    void refresh();
    new EventSource("/api/events/stream").onmessage = schedule;
    setInterval(schedule, 30000);
  };

  if (document.readyState === "loading") addEventListener("DOMContentLoaded", start);
  else start();
})();
