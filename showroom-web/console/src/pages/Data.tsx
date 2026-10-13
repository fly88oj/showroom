import { useEffect, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Combobox } from "@/components/Combobox";
import { Confirm } from "@/components/Confirm";
import { Field, FieldDescription, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  Pagination, PaginationContent, PaginationItem, PaginationLink, PaginationNext, PaginationPrevious,
} from "@/components/ui/pagination";
import { Progress } from "@/components/ui/progress";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  Select, SelectContent, SelectGroup, SelectItem, SelectTrigger, SelectValue,
} from "@/components/ui/select";
import { Separator } from "@/components/ui/separator";
import {
  Table, TableBody, TableCell, TableHead, TableHeader, TableRow,
} from "@/components/ui/table";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "cn";
import { toast } from "sonner";
import { api, apiDo, respErr } from "@/lib/api";
import { useApp, apiFailToast } from "@/lib/app";
import { checkProj, DUMP_CHUNK, pemReady, platformPem } from "@/lib/state";
import { openSession, oneTimeRsaPemPair, sealToEnvelope, uploadPayload } from "@/lib/envelope";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const PREVIEW_MAX = 200; // 服务端 db-preview 上限（本地分页基于全量 ≤200 行）
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

interface SqlPanel {
  info: string[];
  tables: { title: string; cols: string[]; rows: string[][]; total: number }[];
}

/** 数据页：共享目标项目 → PostgreSQL / Redis 二级导航（真链路） */
export function Data({ onOpenSettings }: { onOpenSettings: () => void }) {
  const { projects } = useApp();
  const [dbProject, setDbProject] = useState("");
  const [panel, setPanel] = useState("pg");
  const [projErr, setProjErr] = useState("");

  // —— 表清单与预览 ——
  const [tables, setTables] = useState<string[] | null>(null);
  const [table, setTable] = useState("");
  const [limit, setLimit] = useState("100");
  const [preview, setPreview] = useState<{ cols: string[]; rows: string[][] } | null>(null);
  const [previewFb, setPreviewFb] = useState("");
  const [previewBusy, setPreviewBusy] = useState(false);
  const [page, setPage] = useState(1);

  // —— SQL（逐条执行；SELECT 响应带 csv 字段则表格化）——
  const [sql, setSql] = useState("");
  const [sqlBusy, setSqlBusy] = useState(false);
  const [sqlResult, setSqlResult] = useState<SqlPanel | null>(null);
  const [dangerSql, setDangerSql] = useState<string[] | null>(null);

  // —— 备份/恢复/重置 ——
  const [restoreConfirm, setRestoreConfirm] = useState(false);
  const [resetConfirm, setResetConfirm] = useState(false);
  const [restoreFile, setRestoreFile] = useState<File | null>(null);
  const [dangerBusy, setDangerBusy] = useState(false);
  const [prog, setProg] = useState<{ cur: number; total: number } | null>(null);
  const [progTxt, setProgTxt] = useState("");
  const [out, setOut] = useState("（尚无输出）");

  // —— Redis ——
  const [rdDbs, setRdDbs] = useState<{ db: number; project: string }[] | null>(null);
  const [rdDb, setRdDb] = useState<string>("");
  const [rdQ, setRdQ] = useState("");
  const [rdKeys, setRdKeys] = useState<string[] | null>(null);
  const [rdSel, setRdSel] = useState<{ key: string; type?: string; ttl?: string; value: string } | null>(null);
  const [rdCount, setRdCount] = useState("");

  function setProject(v: string) {
    setDbProject(v); setProjErr("");
    setTables(null); setTable(""); setPreview(null); setPreviewFb(""); setPage(1);
    setSqlResult(null);
    if (v) void loadTables(v);
  }

  async function loadTables(project: string) {
    try {
      const j = (await api("GET", `/_api/db-tables?project=${encodeURIComponent(project)}`)) as { tables?: string[]; out?: string };
      setTables(j.tables || []);
    } catch (e) {
      setTables([]);
      toast.error(`表清单获取失败：${(e as { msg?: string }).msg ?? ""}`);
    }
  }

  async function doPreview() {
    const bad = checkProj(dbProject);
    if (bad) { setProjErr(bad); return; }
    if (!table) { setPreviewFb("请先选择表名"); return; }
    setPreviewBusy(true); setPreviewFb(""); setPage(1);
    try {
      // 一次取满服务端上限，翻页在前端切片（服务端无 offset）
      const j = (await api("POST", "/_api/db-preview", { project: dbProject, table, limit: PREVIEW_MAX })) as { columns?: string[]; rows?: string[][] };
      const cols = j.columns || [], rows = j.rows || [];
      setPreview({ cols, rows });
      setPreviewFb(`表 ${table}：${rows.length} 行${rows.length >= PREVIEW_MAX ? "（预览上限）" : ""}`);
    } catch (e) {
      setPreview(null);
      toast.error(`表预览失败：${(e as { msg?: string }).msg ?? ""}`);
    } finally {
      setPreviewBusy(false);
    }
  }

  const pageSize = Number(limit);
  const totalPages = preview ? Math.max(1, Math.ceil(preview.rows.length / pageSize)) : 1;
  const pageRows = preview ? preview.rows.slice((page - 1) * pageSize, page * pageSize) : [];

  function pageList(cur: number, total: number): (number | "…")[] {
    if (total <= 7) return Array.from({ length: total }, (_, i) => i + 1);
    const keep = [...new Set([1, 2, cur - 1, cur, cur + 1, total - 1, total].filter((x) => x >= 1 && x <= total))].sort((a, b) => a - b);
    const o: (number | "…")[] = [];
    let prev = 0;
    for (const n of keep) {
      if (n - prev > 1) o.push("…");
      o.push(n);
      prev = n;
    }
    return o;
  }

  // —— SQL：按分号拆分逐条执行（psql 每条独立提交，语义一致）；危险语句先确认 ——
  async function runSql() {
    const bad = checkProj(dbProject);
    if (bad) { setProjErr(bad); return; }
    const stmts = splitSql(sql);
    if (stmts.length === 0) { setSqlResult({ info: ["请输入要执行的 SQL 语句"], tables: [] }); return; }
    await execSql(stmts);
  }

  async function execSql(stmts: string[], confirmed = false) {
    setSqlBusy(true); setSqlResult(null);
    const info: string[] = [];
    const tbs: SqlPanel["tables"] = [];
    for (let i = 0; i < stmts.length; i++) {
      const st = stmts[i];
      try {
        const j = (await api("POST", "/_api/db-sql", { project: dbProject, sql: st, confirm: confirmed })) as { out?: string; csv?: string };
        if (j.csv) {
          const t = parseCsv(j.csv);
          if (t.cols.length) tbs.push({ title: `结果 ${tbs.length + 1}`, cols: t.cols, rows: t.rows, total: t.rows.length });
          else info.push(`语句 ${i + 1}（SELECT）：${t.rows.length} 行`);
        } else {
          info.push(`语句 ${i + 1}：${(j.out ?? "OK").trim().split("\n").slice(0, 3).join(" ⏎ ")}`);
        }
      } catch (e) {
        const err = e as { error?: string; msg?: string };
        // 谓词单源：服务端 sql_needs_confirm 判定危险（E_STATE）→ 弹层确认后 confirm:true 重发
        if (err.error === "E_STATE" && (err.msg || "").includes("confirm") && !confirmed) {
          setSqlBusy(false);
          setDangerSql(stmts);
          return;
        }
        info.push(`语句 ${i + 1} 失败：${err.msg ?? err.error ?? "网络异常"}`);
      }
    }
    setSqlResult({ info, tables: tbs });
    setSqlBusy(false);
  }

  async function dump() {
    const bad = checkProj(dbProject);
    if (bad) { setProjErr(bad); return; }
    setProg({ cur: 0, total: 1 }); setProgTxt("正在准备下载…");
    try {
      const pair = await oneTimeRsaPemPair();
      setProgTxt("已发起备份（服务端后台 pg_dump 加密导出）…");
      const j = (await api("POST", "/_api/db-dump-begin", { project: dbProject, client_pub_pem: pair.pub })) as { dump_id: string };
      const dumpId = j.dump_id;
      const t0 = Date.now();
      let total = 0, srvOut = "";
      for (;;) {
        const st = (await api("GET", `/_api/db-dump-status?dump=${encodeURIComponent(dumpId)}`)) as { error?: string; ready?: boolean; total?: number; out?: string };
        if (st.error) throw new Error(`服务端备份失败：${st.error}`);
        if (st.ready) { total = Number(st.total) || 0; srvOut = st.out || ""; break; }
        if (Date.now() - t0 > 5 * 60 * 1000) throw new Error("备份超时（5 分钟未就绪）");
        setProgTxt(`服务端正在导出（已等待 ${Math.round((Date.now() - t0) / 1000)} 秒）…`);
        await sleep(1000);
      }
      const opener = openSession(pair.priv);
      const parts: Uint8Array[] = [];
      let offset = 0;
      try {
        while (offset < total) {
          const len = Math.min(DUMP_CHUNK, total - offset);
          const resp = await apiDo("GET", `/_api/db-dump-chunk?dump=${encodeURIComponent(dumpId)}&offset=${offset}&len=${len}`, null);
          if (!resp.ok) throw await respErr(resp);
          const buf = new Uint8Array(await resp.arrayBuffer());
          if (!buf.length || offset + buf.length > total) throw new Error(`下载数据长度异常（offset=${offset}，收到 ${buf.length} / 总 ${total}）`);
          const o = opener.push(buf);
          if (o) parts.push(o);
          offset += buf.length;
          setProg({ cur: offset, total });
          setProgTxt(`下载中：${offset} / ${total} 字节`);
        }
        const tail = opener.push(new Uint8Array(0));
        if (tail) parts.push(tail);
      } finally { opener.end(); }
      const plain = parts.reduce((n, p) => n + p.length, 0);
      const blob = new Blob(parts as BlobPart[], { type: "application/octet-stream" });
      const a = document.createElement("a");
      a.href = URL.createObjectURL(blob);
      a.download = `${dbProject}.dump`;
      a.click();
      setTimeout(() => URL.revokeObjectURL(a.href), 60000);
      setProg({ cur: total, total });
      setProgTxt(`完成：已下载 ${dbProject}.dump`);
      setOut(srvOut || `pg_dump -Fc 三段式下载完成：信封 ${total} 字节 → 本地解密 ${plain} 字节。`);
    } catch (e) {
      setProg(null);
      setProgTxt("备份失败或中断（可重新发起）");
      apiFailToast(e, "备份下载");
    }
  }

  async function restore() {
    const f = restoreFile;
    if (!f) return;
    if (!pemReady()) {
      toast.error("尚未完成安全配置：到「设置」完成");
      setRestoreConfirm(false);
      onOpenSettings();
      return;
    }
    setDangerBusy(true);
    setProg({ cur: 0, total: 1 }); setProgTxt("正在压缩并加密文件…");
    try {
      const { blob: payload } = await uploadPayload(f, (done, raw) => {
        setProg({ cur: done, total: raw });
        setProgTxt(`压缩中 ${done} / ${raw} 字节…`);
      });
      const envelope = await sealToEnvelope(platformPem(), payload);
      setProgTxt("正在上传恢复（服务端解密并 pg_restore）…");
      const j = (await api("POST", `/_api/db-restore?project=${encodeURIComponent(dbProject)}&confirm=true`, envelope)) as { out?: string };
      setProg(null); setProgTxt("");
      setOut(j.out || `pg_restore --clean --if-exists 完成：信封 ${envelope.length} 字节。`);
    } catch (e) {
      setProg(null); setProgTxt("");
      apiFailToast(e, "恢复上传");
    } finally {
      setDangerBusy(false);
      setRestoreConfirm(false);
    }
  }

  async function reset() {
    setDangerBusy(true);
    setOut("正在重置…");
    try {
      const j = (await api("POST", "/_api/db-reset", { project: dbProject, confirm: true })) as { out?: string };
      setOut(j.out || "数据库已删除并重建，凭据保持不变。");
    } catch (e) {
      toast.error(`重置数据库失败：${(e as { msg?: string }).msg ?? ""}`);
    } finally {
      setDangerBusy(false);
      setResetConfirm(false);
    }
  }

  // —— Redis ——
  async function loadRdDbs(): Promise<{ db: number; project: string }[]> {
    if (rdDbs) return rdDbs;
    const j = (await api("GET", "/_api/redis-dbs")) as { entries?: { db?: number; project?: string }[] };
    const named = (j.entries ?? []).filter((e) => Number.isInteger(e.db) && e.project);
    const list = named.map((e) => ({ db: e.db as number, project: e.project as string }));
    setRdDbs(list);
    if (list.length && !rdDb) setRdDb(String(list[0].db));
    return list;
  }

  async function queryRdFor(dbVal?: string) {
    const bad = checkProj(dbProject);
    if (bad) { setProjErr(bad); return; }
    let db = dbVal ?? rdDb;
    try {
      const dbs = await loadRdDbs();
      if (!dbs.length) { setRdCount("您名下暂无可浏览的项目（发布项目后自动出现）"); return; }
      if (!db) { db = String(dbs[0].db); setRdDb(db); } // 原版行为：select 载入即默认首项
      const j = (await api("GET", `/_api/redis-keys?db=${encodeURIComponent(db)}&q=${encodeURIComponent(rdQ.trim())}&limit=200`)) as { keys?: string[] };
      setRdKeys(j.keys || []);
      setRdCount(`共 ${(j.keys || []).length} 个键`);
    } catch (e) {
      setRdKeys([]);
      toast.error(`Redis 键列表获取失败：${(e as { msg?: string }).msg ?? ""}`);
    }
  }
  const queryRd = () => queryRdFor();

  async function pickKey(key: string) {
    try {
      const j = (await api("GET", `/_api/redis-get?db=${encodeURIComponent(rdDb)}&key=${encodeURIComponent(key)}`)) as { type?: string; ttl_ms?: number; ttl?: number; value?: unknown };
      const ttl = Number(j.ttl_ms ?? j.ttl ?? NaN);
      const ttlCn = Number.isFinite(ttl) ? (ttl === -1 ? "永久（-1）" : `${ttl} ms`) : "未知";
      let v = j.value;
      if (Array.isArray(v)) v = v.join("\n");
      else if (v != null && typeof v !== "string") v = JSON.stringify(v, null, 2);
      else v = v ?? "";
      const cut = (v as string).length >= 4096;
      setRdSel({
        key,
        type: j.type ?? "未知",
        ttl: cut ? `${ttlCn} · 值已截断（≥4KB）` : ttlCn,
        value: (v as string) === "" ? "（空值）" : (v as string),
      });
    } catch (e) {
      toast.error(`Redis 值读取失败：${(e as { msg?: string }).msg ?? ""}`);
    }
  }

  useEffect(() => {
    setRdKeys(null); setRdSel(null); setRdCount(""); setRdDbs(null); setRdDb("");
  }, [dbProject]);

  return (
    <div className="flex flex-col gap-4">
      <h2 className="text-sm font-semibold text-info">目标项目</h2>
      <FieldGroup>
        <div className="grid gap-4 sm:grid-cols-2">
          <Field data-invalid={!!projErr}>
            <FieldLabel>项目</FieldLabel>
            <Combobox
              value={dbProject} onValueChange={setProject}
              ariaLabel="目标项目" className="w-44"
              placeholder="输入或选择项目…"
              searchPlaceholder="输入前缀过滤项目名…"
              options={projects.map((p) => ({ value: p.project, label: p.project }))}
            />
            {projErr ? null : <FieldDescription>只能操作本人名下项目对应的数据库。</FieldDescription>}
          </Field>
        </div>
      </FieldGroup>

      <Tabs value={panel} onValueChange={setPanel} className="gap-4">
        <TabsList variant="line" className="h-9 w-fit gap-0.5">
          <TabsTrigger value="pg">PostgreSQL</TabsTrigger>
          <TabsTrigger value="redis">Redis</TabsTrigger>
        </TabsList>

        {/* —— PostgreSQL —— */}
        <TabsContent value="pg" className="flex flex-col gap-4">
          <h3 className="text-sm font-semibold text-info">表清单与预览</h3>
          <div className="flex flex-wrap items-end gap-3">
            <Field>
              <FieldLabel>表名</FieldLabel>
              <Combobox
                value={table} onValueChange={(v) => { setTable(v); setPage(1); }} disabled={!tables}
                ariaLabel="表名" className="w-44"
                placeholder={tables === null ? "选定项目后自动载入…" : tables.length ? "输入或选择表名…" : "该项目暂无表"}
                searchPlaceholder="输入前缀过滤表名…"
                options={(tables ?? []).map((t) => ({ value: t, label: t }))}
              />
            </Field>
            <Field>
              <FieldLabel>每页行数</FieldLabel>
              <Select value={limit} onValueChange={(v) => { setLimit(v); setPage(1); }}>
                <SelectTrigger className="w-24"><SelectValue /></SelectTrigger>
                <SelectContent>
                  <SelectGroup>
                    <SelectItem value="50">50</SelectItem>
                    <SelectItem value="100">100</SelectItem>
                    <SelectItem value="200">200</SelectItem>
                  </SelectGroup>
                </SelectContent>
              </Select>
            </Field>
            <Button disabled={!table || previewBusy} onClick={doPreview}>
              {previewBusy ? "预览中…" : "预览"}
            </Button>
            <span className="text-xs text-muted-foreground" aria-live="polite">{previewFb}</span>
          </div>
          {preview && (
            <div className="flex flex-col gap-3">
              <DataTable cols={preview.cols} rows={pageRows} />
              {totalPages > 1 && (
                <div className="flex flex-wrap items-center justify-between gap-2">
                  <span className="text-xs text-muted-foreground">
                    第 {page}/{totalPages} 页 · 每页 {pageSize} 行
                  </span>
                  <Pagination className="mx-0 w-auto justify-end">
                    <PaginationContent>
                      <PaginationItem>
                        <PaginationPrevious
                          text="上一页" href="#"
                          aria-disabled={page === 1}
                          className={page === 1 ? "pointer-events-none opacity-50" : ""}
                          onClick={(e) => { e.preventDefault(); setPage((p) => Math.max(1, p - 1)); }}
                        />
                      </PaginationItem>
                      {pageList(page, totalPages).map((n, i) =>
                        n === "…" ? (
                          <PaginationItem key={`e${i}`}>…</PaginationItem>
                        ) : (
                          <PaginationItem key={n}>
                            <PaginationLink href="#" isActive={n === page} onClick={(e) => { e.preventDefault(); setPage(n); }}>
                              {n}
                            </PaginationLink>
                          </PaginationItem>
                        ),
                      )}
                      <PaginationItem>
                        <PaginationNext
                          text="下一页" href="#"
                          aria-disabled={page === totalPages}
                          className={page === totalPages ? "pointer-events-none opacity-50" : ""}
                          onClick={(e) => { e.preventDefault(); setPage((p) => Math.min(totalPages, p + 1)); }}
                        />
                      </PaginationItem>
                    </PaginationContent>
                  </Pagination>
                </div>
              )}
            </div>
          )}

          <Separator />
          <h3 className="text-sm font-semibold text-info">SQL 编辑器</h3>
          <Field>
            <FieldLabel htmlFor="db-sql">SQL 语句（多条以分号分隔，逐条执行）</FieldLabel>
            <Textarea
              id="db-sql" className="min-h-24 font-mono text-xs" spellCheck={false}
              placeholder="输入要执行的语句…" value={sql} onChange={(e) => setSql(e.target.value)}
            />
          </Field>
          <div><Button onClick={runSql} disabled={sqlBusy}>{sqlBusy ? "执行中…" : "执行 SQL"}</Button></div>

          {sqlResult && (
            <div className="flex flex-col gap-3">
              {sqlResult.info.length > 0 && (
                <pre className="rounded-md border bg-background p-3 font-mono text-xs leading-5 whitespace-pre-wrap">
                  {sqlResult.info.join("\n")}
                </pre>
              )}
              {sqlResult.tables.length === 1 ? (
                <DataTable cols={sqlResult.tables[0].cols} rows={sqlResult.tables[0].rows} />
              ) : sqlResult.tables.length > 1 ? (
                <Tabs defaultValue="t0">
                  <TabsList>
                    {sqlResult.tables.map((t, i) => (
                      <TabsTrigger key={t.title} value={`t${i}`}>
                        {t.title}（{t.total} 行）
                      </TabsTrigger>
                    ))}
                  </TabsList>
                  {sqlResult.tables.map((t, i) => (
                    <TabsContent key={t.title} value={`t${i}`} className="mt-3">
                      <DataTable cols={t.cols} rows={t.rows} />
                    </TabsContent>
                  ))}
                </Tabs>
              ) : null}
            </div>
          )}

          <Separator />
          <h3 className="text-sm font-semibold text-info">备份与恢复</h3>
          <div className="flex flex-wrap items-center gap-3">
            <Button variant="secondary" onClick={dump}>备份下载</Button>
            <label htmlFor="db-restore-file">
              <Button variant="outline" asChild>
                <span>选择恢复文件</span>
              </Button>
            </label>
            <input
              id="db-restore-file" type="file" accept=".dump" className="hidden"
              onChange={(e) => setRestoreFile(e.target.files?.[0] ?? null)}
            />
            <span className="text-xs text-muted-foreground">
              {restoreFile ? `${restoreFile.name}（${restoreFile.size} 字节）` : "未选择文件"}
            </span>
            <Button variant="destructive" disabled={!restoreFile || !dbProject} onClick={() => setRestoreConfirm(true)}>
              恢复上传
            </Button>
            <Button variant="destructive" disabled={!dbProject} onClick={() => setResetConfirm(true)}>
              重置数据库
            </Button>
          </div>
          {prog && (
            <div className="flex flex-col gap-1">
              <Progress value={prog.total ? (prog.cur / prog.total) * 100 : 0} />
              <div className="text-xs text-muted-foreground" aria-live="polite">{progTxt}</div>
            </div>
          )}
          <h3 className="mt-2 text-sm font-semibold text-info">操作输出</h3>
          <ScrollArea className="h-40 rounded-md border bg-background">
            <pre className="p-2 font-mono text-xs leading-5 whitespace-pre-wrap" aria-live="polite">{out}</pre>
          </ScrollArea>
        </TabsContent>

        {/* —— Redis —— */}
        <TabsContent value="redis" className="flex flex-col gap-3">
          <div className="flex flex-wrap items-end gap-3">
            <Field>
              <FieldLabel>项目</FieldLabel>
              <Select
                value={rdDb}
                onValueChange={(v) => {
                  setRdDb(v); setRdKeys(null); setRdSel(null); setRdCount("");
                  if (v) void queryRdFor(v); // C2：选库即自动加载键列表
                }}
              >
                <SelectTrigger className="w-44 font-mono">
                  <SelectValue placeholder={rdDbs?.length ? "选择项目…" : "查询后载入…"} />
                </SelectTrigger>
                <SelectContent>
                  <SelectGroup>
                    {(rdDbs ?? []).map((e) => (
                      <SelectItem key={e.db} value={String(e.db)} className="font-mono">{e.project}</SelectItem>
                    ))}
                  </SelectGroup>
                </SelectContent>
              </Select>
            </Field>
            <Field>
              <FieldLabel htmlFor="rd-q">键前缀过滤（回车查询）</FieldLabel>
              <Input
                id="rd-q" className="w-56 font-mono" placeholder="如 sr_demo_ 或留空…"
                value={rdQ} onChange={(e) => setRdQ(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && queryRd()}
              />
            </Field>
            <Button variant="outline" disabled={!dbProject} onClick={queryRd}>查询</Button>
            <span className="text-xs text-muted-foreground" aria-live="polite">{rdCount}</span>
          </div>
          <div className="grid gap-3 md:grid-cols-2">
            <Field>
              <FieldLabel>键列表</FieldLabel>
              <ScrollArea className="h-56 rounded-md border bg-background">
                {rdKeys === null ? (
                  <p className="p-3 text-xs text-muted-foreground">（点击「查询」载入键列表）</p>
                ) : rdKeys.length === 0 ? (
                  <p className="p-3 text-xs text-muted-foreground">（无匹配键）</p>
                ) : (
                  <div className="flex flex-col p-1">
                    {rdKeys.map((k) => (
                      <button
                        key={k} type="button"
                        className={cn(
                          "truncate rounded-sm px-2 py-1 text-left font-mono text-xs hover:bg-accent",
                          rdSel?.key === k && "bg-accent text-accent-foreground",
                        )}
                        onClick={() => pickKey(k)}
                      >
                        {k}
                      </button>
                    ))}
                  </div>
                )}
              </ScrollArea>
            </Field>
            <Field>
              <FieldLabel>
                值 {rdSel && <Badge variant="outline" className="ml-1 font-mono">{rdSel.type} · ttl {rdSel.ttl}</Badge>}
              </FieldLabel>
              <ScrollArea className="h-56 rounded-md border bg-background">
                <pre className="p-2 font-mono text-xs leading-5 whitespace-pre-wrap">
                  {rdSel ? rdSel.value : "（未选择键）"}
                </pre>
              </ScrollArea>
            </Field>
          </div>
          <p className="text-xs text-muted-foreground">
            每个项目一个独立的键空间；管理员可见全部项目的键空间。键列表单次最多 200 条（可能截断），值超过 4KB 会被服务端截断。
          </p>
        </TabsContent>
      </Tabs>

      <Confirm
        open={!!dangerSql} onOpenChange={(v) => !v && setDangerSql(null)}
        title="确认执行危险 SQL？" confirmText="执行" busy={sqlBusy}
        onConfirm={() => { if (dangerSql) void execSql(dangerSql, true); }}
      >
        本次执行包含 DROP / DELETE / TRUNCATE / ALTER 语句，数据变更不可自动回滚（E_STATE：危险 SQL 需确认）；如需保留现况请先「备份下载」。
      </Confirm>
      <Confirm
        open={restoreConfirm} onOpenChange={(v) => !v && setRestoreConfirm(false)}
        title={`恢复 ${dbProject} 的数据库？`} confirmText="恢复上传" busy={dangerBusy} onConfirm={restore}
      >
        将用 {restoreFile?.name ?? "backup.dump"} 覆盖当前数据库的全部数据（zstd 解压 + 端到端解密），此操作不可撤销；如需保留现况请先「备份下载」。
      </Confirm>
      <Confirm
        open={resetConfirm} onOpenChange={(v) => !v && setResetConfirm(false)}
        title={`重置 ${dbProject} 的数据库？`} confirmText="重置" busy={dangerBusy} onConfirm={reset}
      >
        全部表与数据将被删除并重建（连接凭据不变），不可恢复。此操作不可撤销，如需保留请先「备份下载」。
      </Confirm>
    </div>
  );
}

/** 通用数据表 */
function DataTable({ cols, rows }: { cols: string[]; rows: string[][] }) {
  return (
    <div className="rounded-lg border">
      <Table>
        <TableHeader>
          <TableRow className="bg-muted/30">
            {cols.map((c) => <TableHead key={c} className="font-mono">{c}</TableHead>)}
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.length ? rows.map((r, i) => (
            <TableRow key={i}>
              {r.map((cell, j) => (
                <TableCell key={j} className="font-mono text-xs">{cell ?? "NULL"}</TableCell>
              ))}
            </TableRow>
          )) : (
            <TableRow><TableCell colSpan={cols.length || 1} className="py-4 text-center text-xs text-muted-foreground">（无数据）</TableCell></TableRow>
          )}
        </TableBody>
      </Table>
    </div>
  );
}

/** CSV（psql --csv）→ 表格：带引号/双引号转义/换行的完整解析。 */
function parseCsv(text: string): { cols: string[]; rows: string[][] } {
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

