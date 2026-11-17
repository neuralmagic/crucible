export const inputs = ["verdicts.json"];
const ACTIONS = [
    { id: "fix", label: "Fix", tone: "bad", means: "affected" },
    { id: "decide", label: "Decide", tone: "warn", means: "triage could not tell" },
    { id: "spot", label: "Spot-check", tone: "info", means: "cleared on low or medium confidence" },
    { id: "close", label: "Close", tone: "good", means: "cleared on high confidence" },
];
const toneOf = (action) => ACTIONS.find((a) => a.id === action)?.tone ?? "neutral";
const labelOf = (action) => ACTIONS.find((a) => a.id === action)?.label ?? action;
function noted(text, label) {
    const line = text.split("\n").find((l) => l.startsWith(label));
    return line ? line.slice(label.length).trim() : "";
}
function read(files) {
    const byKey = new Map();
    for (const file of files) {
        const records = JSON.parse(file.text);
        if (!Array.isArray(records))
            throw new Error(`${file.path} from ${file.task} is not a list`);
        for (const record of records) {
            const v = record?.verdict_property ?? {};
            const verdict = String(v.verdict ?? "");
            const confidence = String(v.confidence ?? "");
            const rationale = String(v.rationale ?? "");
            const action = verdict === "affected" ? "fix"
                : verdict !== "not_affected" ? "decide"
                    : confidence === "high" ? "close"
                        : "spot";
            const key = String(record?.ticket_key ?? "");
            byKey.set(key, {
                key,
                short: key.replace(/^.*-/, ""),
                lane: file.task.replace(/^triage-/, ""),
                status: String(record?.jira_status ?? ""),
                age: Number(record?.age_days ?? 0) || 0,
                verdict,
                confidence,
                cvss: Number(v.proposed_actions?.cvss?.score ?? 0) || 0,
                cve: (Array.isArray(v.cve_ids) ? v.cve_ids : []).join(", "),
                pkg: [v.package?.name, v.package?.version].filter(Boolean).join(" "),
                release: String(v.release?.name ?? ""),
                basis: String(v.proposed_actions?.lane ?? ""),
                why: noted(rationale, "Why review:") || rationale.split("\n")[0] || "",
                rationale,
                task: file.task,
                file: `run:file/${file.task}/${file.path}`,
                action,
            });
        }
    }
    const order = ACTIONS.map((a) => a.id);
    return [...byKey.values()].sort((a, b) => order.indexOf(a.action) - order.indexOf(b.action) ||
        b.cvss - a.cvss ||
        b.age - a.age ||
        a.key.localeCompare(b.key));
}
/** Every ticket as one cell, in a block per action, most severe first. */
function ticketMap(tickets) {
    const rows = 8;
    const cell = { w: 58, h: 20, gapX: 4, pitch: 28 };
    const top = 30;
    const oldest = Math.max(1, ...tickets.map((t) => t.age));
    const canvas = new Canvas(600, top + rows * cell.pitch + 26);
    const ctx = canvas.getContext("2d");
    let left = 0;
    for (const action of ACTIONS) {
        const group = tickets.filter((t) => t.action === action.id);
        const columns = Math.max(1, Math.ceil(group.length / rows));
        ctx.globalAlpha = 1;
        ctx.textAlign = "left";
        ctx.fillStyle = action.tone;
        ctx.font = "bold 13px sans-serif";
        ctx.fillText(`${action.label} ${group.length}`, left, 13);
        ctx.fillStyle = "muted";
        ctx.font = "10px sans-serif";
        if (columns > 1)
            ctx.fillText(action.means, left, 25);
        group.forEach((ticket, i) => {
            const x = left + Math.floor(i / rows) * (cell.w + cell.gapX);
            const y = top + (i % rows) * cell.pitch;
            ctx.fillStyle = action.tone;
            ctx.globalAlpha = 0.16 + 0.74 * Math.min(1, ticket.cvss / 10);
            ctx.fillRect(x, y, cell.w, cell.h);
            ctx.globalAlpha = 0.9;
            ctx.fillRect(x, y + cell.h + 1, (cell.w * ticket.age) / oldest, 2);
            ctx.globalAlpha = 1;
            ctx.fillStyle = "neutral";
            ctx.font = "bold 10px monospace";
            ctx.textAlign = "left";
            ctx.fillText(ticket.short + (ticket.confidence === "low" ? "?" : ""), x + 5, y + 14);
            ctx.font = "10px monospace";
            ctx.textAlign = "right";
            ctx.fillText(ticket.cvss.toFixed(1), x + cell.w - 5, y + 14);
        });
        left += columns * (cell.w + cell.gapX) - cell.gapX + 14;
    }
    ctx.globalAlpha = 1;
    ctx.textAlign = "left";
    ctx.fillStyle = "muted";
    ctx.font = "10px sans-serif";
    ctx.fillText(`One cell per ticket: number and CVSS. Deeper fill is higher CVSS, the underline is days open (longest ${oldest}), ? is low confidence.`, 0, top + rows * cell.pitch + 16);
    return canvas.node();
}
/** The tickets to act on along a CVSS axis, labels stacked so none overlap. */
function severityRuler(tickets) {
    const marks = tickets
        .filter((t) => t.action === "fix" || t.action === "decide")
        .sort((a, b) => a.cvss - b.cvss);
    const span = { left: 24, right: 576 };
    const at = (cvss) => span.left + ((span.right - span.left) * Math.min(10, Math.max(0, cvss))) / 10;
    const labelWidth = 30;
    const ends = [];
    const placed = marks.map((ticket) => {
        const x = at(ticket.cvss);
        let row = ends.findIndex((end) => end + 3 <= x - labelWidth / 2);
        if (row < 0)
            row = ends.push(0) - 1;
        ends[row] = x + labelWidth / 2;
        return { ticket, x, row };
    });
    const rowHeight = 13;
    const axis = 30 + ends.length * rowHeight + 14;
    const canvas = new Canvas(600, axis + 30);
    const ctx = canvas.getContext("2d");
    for (const band of [{ from: 7, tone: "warn", name: "high" }, { from: 9, tone: "bad", name: "critical" }]) {
        ctx.globalAlpha = 0.09;
        ctx.fillStyle = band.tone;
        ctx.fillRect(at(band.from), 18, span.right - at(band.from), axis - 18);
        ctx.globalAlpha = 1;
        ctx.font = "10px sans-serif";
        ctx.textAlign = "left";
        ctx.fillText(band.name, at(band.from) + 4, 13);
    }
    ctx.globalAlpha = 1;
    ctx.fillStyle = "muted";
    ctx.font = "bold 11px sans-serif";
    ctx.textAlign = "left";
    ctx.fillText("Fix and decide, by CVSS", 0, 13);
    ctx.strokeStyle = "muted";
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(span.left, axis);
    ctx.lineTo(span.right, axis);
    for (let score = 0; score <= 10; score++) {
        ctx.moveTo(at(score), axis);
        ctx.lineTo(at(score), axis + 4);
    }
    ctx.stroke();
    ctx.font = "10px monospace";
    ctx.textAlign = "center";
    for (let score = 0; score <= 10; score += 2)
        ctx.fillText(String(score), at(score), axis + 16);
    for (const { ticket, x, row } of placed) {
        const labelY = axis - 16 - row * rowHeight;
        const tone = toneOf(ticket.action);
        ctx.globalAlpha = 0.45;
        ctx.strokeStyle = tone;
        ctx.beginPath();
        ctx.moveTo(x, axis - 5);
        ctx.lineTo(x, labelY + 3);
        ctx.stroke();
        ctx.globalAlpha = 1;
        ctx.fillStyle = tone;
        ctx.beginPath();
        ctx.arc(x, axis - 5, ticket.action === "fix" ? 4.5 : 3.5, 0, Math.PI * 2);
        ctx.fill();
        ctx.font = ticket.action === "fix" ? "bold 10px monospace" : "10px monospace";
        ctx.textAlign = "center";
        ctx.fillText(ticket.short, x, labelY);
    }
    return canvas.node();
}
/** What a selected row shows under the table. */
function detail(t) {
    return [
        `### ${t.key}: ${labelOf(t.action)}`,
        "",
        t.rationale.replaceAll("\n", "\n\n"),
        "",
        "| Field | Value |",
        "|---|---|",
        `| Package | \`${t.pkg}\` |`,
        `| CVE | ${t.cve} |`,
        `| CVSS | ${t.cvss.toFixed(1)} |`,
        `| Decided by | ${t.basis} |`,
        "",
        `Triaged by [${t.task}](run:task/${t.task}), from [its verdicts](${t.file}).`,
    ].join("\n");
}
function table(id, tickets) {
    return {
        table: {
            id,
            columns: [
                { id: "ticket", title: "Ticket", kind: "mono", width: "narrow" },
                { id: "action", title: "Action", kind: "badge", width: "narrow" },
                { id: "cvss", title: "CVSS", kind: "number", width: "narrow" },
                { id: "age", title: "Open", kind: "number", width: "narrow" },
                { id: "confidence", title: "Confidence", kind: "badge", width: "narrow" },
                { id: "why", title: "Why", width: "fill" },
                { id: "package", title: "Package", width: "wide" },
                { id: "release", title: "Release", kind: "mono", width: "narrow" },
                { id: "lane", title: "Lane", width: "narrow" },
                { id: "cve", title: "CVE", kind: "mono", width: "medium" },
                { id: "status", title: "Jira", width: "narrow" },
            ],
            rows: tickets.map((t) => ({
                id: t.key,
                cells: [
                    { text: t.key },
                    { text: labelOf(t.action), tone: toneOf(t.action) },
                    { text: t.cvss.toFixed(1), tone: t.cvss >= 9 ? "bad" : t.cvss >= 7 ? "warn" : "neutral" },
                    { text: `${t.age} d`, value: t.age },
                    { text: t.confidence, tone: t.confidence === "high" ? "good" : t.confidence === "low" ? "bad" : "warn" },
                    { text: t.why },
                    { text: t.pkg },
                    { text: t.release },
                    { text: t.lane, href: `run:task/${t.task}` },
                    { text: t.cve },
                    { text: t.status },
                ],
                detail: detail(t),
            })),
        },
    };
}
/** Tickets flowing from their lane, through what the decision rested on, to what to do. */
function howDecided(tickets) {
    const lanes = [...new Set(tickets.map((t) => t.lane))].sort();
    const bases = [...new Set(tickets.map((t) => t.basis))].sort();
    const count = (keep) => tickets.filter(keep).length;
    const links = [];
    for (const basis of bases) {
        for (const lane of lanes) {
            links.push({ from: `lane:${lane}`, to: `basis:${basis}`, value: count((t) => t.lane === lane && t.basis === basis) });
        }
        for (const action of ACTIONS) {
            links.push({ from: `basis:${basis}`, to: `action:${action.id}`, value: count((t) => t.basis === basis && t.action === action.id) });
        }
    }
    return {
        flow: {
            nodes: [
                ...lanes.map((lane) => ({ id: `lane:${lane}`, label: lane, tone: "muted" })),
                ...bases.map((basis) => ({ id: `basis:${basis}`, label: basis })),
                ...ACTIONS.filter((a) => tickets.some((t) => t.action === a.id)).map((a) => ({
                    id: `action:${a.id}`,
                    label: a.label,
                    tone: a.tone,
                })),
            ],
            links: links.filter((link) => link.value > 0),
        },
    };
}
/** The verdicts are read as of this moment, in Unix seconds: 1 Nov 2026, UTC. */
const AS_OF = Date.UTC(2026, 10, 1) / 1000;
/** Tickets opened so far, per lane, over the days they were opened. */
function openedOverTime(tickets) {
    const lanes = [...new Set(tickets.map((t) => t.lane))].sort();
    return {
        plot: {
            kind: "line",
            x: "opened",
            y: "tickets",
            x_scale: "time",
            series: lanes.map((lane) => ({
                label: lane,
                points: tickets
                    .filter((t) => t.lane === lane)
                    .map((t) => AS_OF - t.age * 86400)
                    .sort((a, b) => a - b)
                    .map((opened, i) => [opened, i + 1]),
            })),
        },
    };
}
/** Days open against CVSS, a series per package, on a log scale of days. */
function ageByPackage(tickets) {
    const packages = [...new Set(tickets.map((t) => t.pkg.split(" ")[0]))].sort();
    return {
        plot: {
            kind: "scatter",
            x: "CVSS",
            y: "days open",
            y_scale: "log",
            series: packages.map((pkg) => ({
                label: pkg,
                points: tickets
                    .filter((t) => t.pkg.split(" ")[0] === pkg)
                    .map((t) => [t.cvss, Math.max(1, t.age)]),
            })),
        },
    };
}
export function view({ files, params }) {
    const every = read(files);
    const allLanes = [...new Set(every.map((t) => t.lane))].sort();
    const lane = typeof params.lane === "string" && allLanes.includes(params.lane) ? params.lane : "all";
    const minCvss = typeof params.cvss === "number" ? params.cvss : 0;
    const controls = [
        {
            select: {
                id: "lane",
                label: "Lane",
                options: [{ value: "all", label: "All" }, ...allLanes.map((l) => ({ value: l, label: l }))],
                value: lane,
            },
        },
        { range: { id: "cvss", label: "Min CVSS", min: 0, max: 10, step: 0.5, value: minCvss } },
    ];
    const tickets = every.filter((t) => (lane === "all" || t.lane === lane) && t.cvss >= minCvss);
    if (tickets.length === 0) {
        const why = every.length === 0 ? "No verdicts were captured." : "No ticket matches.";
        return { title: "Triage", body: { text: { text: why, tone: "muted" } }, controls };
    }
    const of = (action) => tickets.filter((t) => t.action === action);
    const [fix, decide, spot, close] = [of("fix"), of("decide"), of("spot"), of("close")];
    const high = (list) => list.filter((t) => t.cvss >= 7).length;
    const lanes = [...new Set(tickets.map((t) => t.lane))].sort();
    const act = [...fix, ...decide];
    const first = fix[0];
    const summary = `**${tickets.length} tickets** from the ${lanes.join(" and ")} ${lanes.length === 1 ? "lane" : "lanes"}.\n\n` +
        `- **${fix.length} are affected** and need a fix, ${high(fix)} of them CVSS 7 or higher` +
        (first ? `; start with \`${first.key}\` (${first.cvss.toFixed(1)}, open ${first.age} days).\n` : ".\n") +
        `- **${decide.length} could not be decided** and need a person.\n` +
        `- **${spot.length} were cleared on low or medium confidence**, ${high(spot)} of those CVSS 7 or higher, so check them before closing.\n` +
        `- **${close.length} can be closed.**`;
    return {
        title: `Triage: ${act.length} to act on, ${spot.length} to spot-check, ${close.length} to close`,
        body: {
            stack: {
                children: [
                    {
                        stats: {
                            items: [
                                { label: "Triaged", value: String(tickets.length) },
                                ...ACTIONS.map((a) => ({ label: a.label, value: String(of(a.id).length), tone: a.tone })),
                            ],
                        },
                    },
                    {
                        tabs: {
                            id: "queue",
                            tabs: [
                                {
                                    label: "Overview",
                                    body: {
                                        stack: {
                                            children: [
                                                { columns: { children: [{ markdown: { text: summary } }, howDecided(tickets)] } },
                                                { section: { id: "map", title: "Ticket map", body: ticketMap(tickets) } },
                                            ],
                                        },
                                    },
                                },
                                {
                                    label: "Act first",
                                    count: act.length,
                                    body: {
                                        stack: {
                                            children: [
                                                { section: { id: "ruler", title: "By CVSS", body: severityRuler(tickets) } },
                                                table("act", act),
                                            ],
                                        },
                                    },
                                },
                                {
                                    label: "Spot-check",
                                    count: spot.length,
                                    body: {
                                        stack: {
                                            children: [
                                                {
                                                    text: {
                                                        text: "Cleared, but not on high confidence. Highest CVSS first: a wrong dismissal costs most at the top.",
                                                        tone: "muted",
                                                    },
                                                },
                                                table("spot", spot),
                                            ],
                                        },
                                    },
                                },
                                { label: "All", count: tickets.length, body: table("all", tickets) },
                                {
                                    label: "Charts",
                                    body: {
                                        stack: {
                                            children: [
                                                {
                                                    columns: {
                                                        children: [
                                                            openedOverTime(tickets),
                                                            { histogram: { x: "CVSS", values: tickets.map((t) => t.cvss), bins: 10, tone: "warn" } },
                                                        ],
                                                    },
                                                },
                                                ageByPackage(tickets),
                                            ],
                                        },
                                    },
                                },
                            ],
                        },
                    },
                ],
            },
        },
        controls,
    };
}
