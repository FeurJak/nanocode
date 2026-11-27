import argparse, json, math, statistics
from xml.sax.saxutils import escape

COLORS = ["#2F6DB5", "#E07B00", "#2E9E5B", "#B23A8E"]
WIDTH, HEIGHT = 1600, 900
FONT = "font-family=\"Helvetica, Arial, sans-serif\""
METRICS = [("usd", "Cost per run (US$)", lambda v: f"${v:.2f}"), ("wall_s", "Wall time per run (s)", lambda v: f"{v:.0f} s")]


def ticks(top):
    raw = top / 4
    power = 10 ** math.floor(math.log10(raw))
    step = next(power * m for m in (1, 2, 2.5, 5, 10) if power * m >= raw)
    return [step * i for i in range(int(top / step) + 2) if step * i <= top * 1.0001 + step], step


def text(x, y, body, size, anchor="middle", weight="normal", fill="#1A1A1A"):
    return (f'<text x="{x:.1f}" y="{y:.1f}" font-size="{size}" text-anchor="{anchor}" font-weight="{weight}" '
            f'fill="{fill}" {FONT}>{escape(body)}</text>')


def panel(rows, clusters, groups, metric, label, fmt, left, top, width, height):
    values = [row[metric] for row in rows]
    peak = max(values) * 1.18
    marks, step = ticks(peak)
    peak = marks[-1] if marks[-1] >= max(values) * 1.12 else marks[-1] + step
    scale = lambda v: top + height - v / peak * height
    out = [text(left + width / 2, top - 40, label, 34, weight="bold")]
    for mark in marks:
        if mark > peak:
            continue
        y = scale(mark)
        out.append(f'<line x1="{left}" y1="{y:.1f}" x2="{left + width}" y2="{y:.1f}" stroke="#D0D0D0" stroke-width="1.5"/>')
        out.append(text(left - 14, y + 9, f"${mark:g}" if metric == "usd" else fmt(mark), 24, anchor="end", fill="#444"))
    span = width / len(clusters)
    bar = min(150, span * 0.8 / len(groups))
    for index, cluster in enumerate(clusters):
        middle = left + span * (index + 0.5)
        for position, group in enumerate(groups):
            mine = [row[metric] for row in rows if row["cluster"] == cluster and row["group"] == group]
            if not mine:
                continue
            x = middle + (position - (len(groups) - 1) / 2) * bar * 1.08
            mean = statistics.mean(mine)
            out.append(f'<rect x="{x - bar / 2:.1f}" y="{scale(mean):.1f}" width="{bar:.1f}" '
                       f'height="{top + height - scale(mean):.1f}" fill="{COLORS[position]}" opacity="0.85" rx="4"/>')
            out.append(text(x, scale(max(mean, max(mine))) - 16, fmt(mean), 30, weight="bold"))
            for number, value in enumerate(sorted(mine)):
                offset = (number - (len(mine) - 1) / 2) * min(22, bar * 0.6 / max(1, len(mine)))
                out.append(f'<circle cx="{x + offset:.1f}" cy="{scale(value):.1f}" r="9" fill="#1A1A1A" '
                           f'stroke="#FFFFFF" stroke-width="3"/>')
        if len(clusters) > 1 or clusters[0]:
            out.append(text(middle, top + height + 44, cluster, 26, fill="#333"))
    out.append(f'<line x1="{left}" y1="{top + height}" x2="{left + width}" y2="{top + height}" stroke="#333" stroke-width="2"/>')
    return out


def render(rows, title, subtitle, note, out):
    clusters = list(dict.fromkeys(row["cluster"] for row in rows))
    groups = list(dict.fromkeys(row["group"] for row in rows))
    body = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}">',
            f'<rect width="{WIDTH}" height="{HEIGHT}" fill="#FFFFFF"/>',
            text(70, 70, title, 46, anchor="start", weight="bold")]
    if subtitle:
        body.append(text(70, 118, subtitle, 28, anchor="start", fill="#444"))
    for index, (metric, label, fmt) in enumerate(METRICS):
        body += panel(rows, clusters, groups, metric, label, fmt, 150 + index * 760, 230, 600, 470)
    x = 150
    for position, group in enumerate(groups):
        body.append(f'<rect x="{x}" y="808" width="30" height="30" fill="{COLORS[position]}" opacity="0.85" rx="4"/>')
        body.append(text(x + 42, 832, group, 28, anchor="start"))
        x += 90 + len(group) * 17
    body.append(f'<circle cx="{x + 20}" cy="823" r="9" fill="#1A1A1A" stroke="#FFFFFF" stroke-width="3"/>')
    body.append(text(x + 40, 832, "one run, bar and label: mean" + (f" · {note}" if note else ""), 28, anchor="start", fill="#444"))
    body.append("</svg>")
    open(out, "w").write("\n".join(body) + "\n")


def main():
    parser = argparse.ArgumentParser(prog="chart.py")
    parser.add_argument("results", nargs="+")
    parser.add_argument("--group", required=True)
    parser.add_argument("--cluster")
    parser.add_argument("--name", action="append", default=[])
    parser.add_argument("--where", action="append", default=[])
    parser.add_argument("--title", required=True)
    parser.add_argument("--subtitle", default="")
    parser.add_argument("--note", default="")
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    names = dict(name.split("=", 1) for name in args.name)
    where = [condition.split("=", 1) for condition in args.where]
    rows = []
    for path in args.results:
        for line in open(path):
            row = json.loads(line)
            if all(str(row.get(key)) == value for key, value in where):
                row["group"] = names.get(str(row.get(args.group)), str(row.get(args.group)))
                row["cluster"] = names.get(str(row.get(args.cluster)), str(row.get(args.cluster))) if args.cluster else ""
                rows.append(row)
    if not rows:
        raise SystemExit("no runs match")
    render(rows, args.title, args.subtitle, args.note, args.out)


if __name__ == "__main__":
    main()
