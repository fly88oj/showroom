// —— sql.ts：SQL 工具纯函数（分号拆分状态机 + psql --csv 解析）——
// 从 Data 页迁出形成可测 seam（评审 design#8）。

/** 按分号拆分多语句：单引号（含 '' 转义）/双引号/$$ 美元引用内的分号不拆（psql 语义）。 */
export function splitSql(text: string): string[] {
  const out: string[] = [];
  let cur = "", q: "'" | '"' | null = null, dollar = false;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (dollar) {
      if (c === "$" && text.slice(i, i + 2) === "$$") { dollar = false; cur += "$$"; i++; continue; }
      cur += c;
      continue;
    }
    if (q) {
      if (c === q) {
        if (q === "'" && text[i + 1] === "'") { cur += "''"; i++; continue; } // SQL '' 转义
        q = null;
      }
      cur += c;
      continue;
    }
    if (c === "'") { q = "'"; cur += c; continue; }
    if (c === '"') { q = '"'; cur += c; continue; }
    if (c === "$" && text.slice(i, i + 2) === "$$") { dollar = true; cur += "$$"; i++; continue; }
    if (c === ";") { if (cur.trim()) out.push(cur.trim()); cur = ""; continue; }
    cur += c;
  }
  if (cur.trim()) out.push(cur.trim());
  return out;
}

/** CSV（psql --csv）→ 表格：带引号/双引号转义/换行的完整解析；尾行允许无换行。 */
export function parseCsv(text: string): { cols: string[]; rows: string[][] } {
  const rows: string[][] = [];
  let row: string[] = [], cell = "", q = false;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (q) {
      if (c === '"') {
        if (text[i + 1] === '"') { cell += '"'; i++; } else q = false;
      } else cell += c;
    } else if (c === '"') q = true;
    else if (c === ",") { row.push(cell); cell = ""; }
    else if (c === "\n") { row.push(cell); rows.push(row); row = []; cell = ""; }
    else if (c !== "\r") cell += c;
  }
  if (cell.length || row.length) { row.push(cell); rows.push(row); }
  const out = rows.filter((r) => r.some((x) => x !== ""));
  return { cols: out[0] ?? [], rows: out.slice(1) };
}
