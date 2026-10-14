import { Fragment, useCallback, useEffect, useState } from "react";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Confirm } from "@/components/Confirm";
import {
  Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle,
} from "@/components/ui/dialog";
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Separator } from "@/components/ui/separator";
import { Switch } from "@/components/ui/switch";
import {
  Table, TableBody, TableCell, TableHead, TableHeader, TableRow,
} from "@/components/ui/table";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { CopyIcon, ShieldAlertIcon, UserPlusIcon } from "lucide-react";
import { toast } from "sonner";
import { api } from "@/lib/api";
import { useApp, fmtTime } from "@/lib/app";
import { randAlnum } from "@/lib/bytes";
import { StatusBadge } from "@/components/StatusBadge";
import { NAME_RE } from "@/lib/state";

const MW_NAMES = ["postgres", "redis", "registry"] as const;
const KIND_CN: Record<string, string> = {
  CreateDir: "建目录", WriteFile: "写文件", BtrfsSubvol: "btrfs 子卷", NetworkCreate: "建网络",
  ContainerUp: "起容器", ContainerRm: "删容器", PackageInstall: "装软件包",
  UnitInstall: "装单元", UnitEnable: "启用单元", UnitDisableRm: "停用单元", Cmd: "执行命令",
};
const kindCn = (k: string) => KIND_CN[k] || k || "动作";
const SVC_CN: Record<string, string> = { active: "运行中", inactive: "未运行", failed: "失败", activating: "启动中", deactivating: "停止中" };
const svcCn = (s: string) => SVC_CN[s] || s || "未知";
const fmtGB = (n: unknown) => {
  const v = Number(n);
  return Number.isFinite(v) ? `${(v / 1024 ** 3).toFixed(1)} GB` : String(n ?? "—");
};

interface HostState {
  host_env: Record<string, unknown>;
  policy: Record<string, number | string>;
  drift: { severity?: string; item: string; expected: unknown; actual: unknown; advice?: string }[];
  redis_pool?: { total?: number; used?: number; headroom?: number; suggest_expand?: boolean };
}

/** 管理（仅 admin）：账号 / 全部项目 / 平台状态 / 宿主配置 / 实时事件（真链路） */
export function Admin() {
  return (
    <Tabs defaultValue="users" className="flex flex-col gap-4">
      <TabsList variant="line" className="h-9 w-fit gap-0.5">
        <TabsTrigger value="users">账号</TabsTrigger>
        <TabsTrigger value="projects">全部项目</TabsTrigger>
        <TabsTrigger value="status">平台状态</TabsTrigger>
        <TabsTrigger value="host">宿主配置</TabsTrigger>
        <TabsTrigger value="events">实时事件</TabsTrigger>
      </TabsList>
      <TabsContent value="users"><UsersPanel /></TabsContent>
      <TabsContent value="projects"><AllProjectsPanel /></TabsContent>
      <TabsContent value="status"><StatusPanel /></TabsContent>
      <TabsContent value="host"><HostPanel /></TabsContent>
      <TabsContent value="events"><EventsPanel /></TabsContent>
    </Tabs>
  );
}

// —— 账号：账号行（项目/设备数·密码徽标·设免管理员·吊销）+ 设备行（移除此设备）+ 发卡/补发 ——
interface UserRow {
  user: string; admin?: boolean; has_password?: boolean; projects?: number;
  keys?: { kid: string; since: number }[];
}

/** 确认动作三件套（账号/全部项目两面板共用）。 */
function useConfirmAct(load: () => Promise<void>) {
  const [confirmAct, setConfirmAct] = useState<{ title: string; body: string; run: () => Promise<void> } | null>(null);
  const [busy, setBusy] = useState(false);
  const runConfirm = useCallback(async () => {
    if (!confirmAct) return;
    setBusy(true);
    try {
      await confirmAct.run();
      setConfirmAct(null);
      await load();
    } catch (e) {
      toast.error(`操作失败：${(e as { msg?: string }).msg ?? ""}`);
    } finally {
      setBusy(false);
    }
  }, [confirmAct, load]);
  return { confirmAct, setConfirmAct, busy, runConfirm };
}

function UsersPanel() {
  const [users, setUsers] = useState<UserRow[] | null>(null);
  const [addOpen, setAddOpen] = useState(false);
  const [psTarget, setPsTarget] = useState<string | null>(null);
  const [resetConfirm, setResetConfirm] = useState<string | null>(null); // 重置确认目标账号
  const [psPw, setPsPw] = useState("");
  const load = useCallback(async () => {
    try {
      const j = (await api("GET", "/_api/admin/users")) as { users?: UserRow[] };
      setUsers(j.users || []);
    } catch {
      setUsers([]);
    }
  }, []);
  const { confirmAct, setConfirmAct, busy, runConfirm } = useConfirmAct(load);

  useEffect(() => { void load(); }, [load]);

  function adminAction(u: UserRow, act: "useradmin" | "userunadmin" | "revoke" | "keyrm", kid?: string) {
    const defs = {
      useradmin: { ep: "user-admin", cn: "设为管理员", body: { user: u.user, admin: true }, ask: `确认将账号 ${u.user} 设为管理员？该账号登录即具管理权限（可管理全部账号、项目与平台状态）。` },
      userunadmin: { ep: "user-admin", cn: "取消管理员", body: { user: u.user, admin: false }, ask: `确认取消账号 ${u.user} 的管理员权限？该账号所有设备同时失去管理权限。` },
      revoke: { ep: "user-revoke", cn: "删除账号", body: { user: u.user }, ask: `确认删除账号 ${u.user}？其名下全部设备与镜像库访问凭据将一并删除，该账号即刻无法登录。` },
      keyrm: { ep: "key-rm", cn: "移除此设备", body: { user: u.user, kid }, ask: `确认移除账号 ${u.user} 的此设备（${kid}）？该设备立即无法使用，重新登录即可重新登记。` },
    } as const;
    const d = defs[act];
    setConfirmAct({
      title: `${d.cn}？`,
      body: d.ask,
      run: async () => {
        await api("POST", `/_api/admin/${d.ep}`, d.body);
        toast.success(`已执行「${d.cn}」`);
      },
    });
  }

  /** 重置密码：先确认（modal 二次确认，用户裁决），确认后生成+passwd-set+一次性展示。 */
  async function passwdSet(user: string) {
    const pw = randAlnum(16);
    setResetConfirm(null);
    try {
      const j = (await api("POST", "/_api/admin/passwd-set", { user, password: pw })) as { password?: string };
      setPsPw(j.password || pw);
      setPsTarget(user);
      await load();
    } catch (e) {
      toast.error(`重置密码失败：${(e as { msg?: string }).msg ?? ""}`);
    }
  }

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold text-info">账号</h2>
        <Button onClick={() => setAddOpen(true)}>
          <UserPlusIcon data-icon="inline-start" />发放新账号
        </Button>
      </div>
      <div className="rounded-lg border">
        <Table>
          <TableHeader>
            <TableRow className="bg-muted/30">
              <TableHead className="w-40">设备</TableHead>
              <TableHead className="w-40">登录时间</TableHead>
              <TableHead className="text-right">操作</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {users === null ? (
              <TableRow><TableCell colSpan={3} className="py-6 text-center text-xs text-muted-foreground">加载中…</TableCell></TableRow>
            ) : users.map((u) => (
              <Fragment key={u.user}>
                <TableRow className="bg-muted/20">
                  <TableCell colSpan={3}>
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="font-mono text-sm">{u.user}</span>
                      {u.admin && <Badge variant="success">管理员</Badge>}
                      <span className="text-xs text-muted-foreground">· 项目数 {u.projects ?? "—"} · 设备 {u.keys?.length ?? 0} 台</span>
                      <span className="flex-1" />
                      <Button size="sm" variant="outline" onClick={() => setResetConfirm(u.user)}>
                        重置密码
                      </Button>
                      {u.admin
                        ? <Button size="sm" variant="outline" onClick={() => adminAction(u, "userunadmin")}>取消管理员</Button>
                        : <Button size="sm" variant="outline" onClick={() => adminAction(u, "useradmin")}>设为管理员</Button>}
                      <Button size="sm" variant="destructive" onClick={() => adminAction(u, "revoke")}>删除</Button>
                    </div>
                  </TableCell>
                </TableRow>
                {(u.keys ?? []).map((k) => (
                  <TableRow key={k.kid}>
                    <TableCell className="font-mono text-xs">{k.kid}</TableCell>
                    <TableCell className="font-mono text-xs text-muted-foreground">{fmtTime(k.since)}</TableCell>
                    <TableCell className="text-right">
                      <Button size="sm" variant="destructive" onClick={() => adminAction(u, "keyrm", k.kid)}>
                        移除此设备
                      </Button>
                    </TableCell>
                  </TableRow>
                ))}
                {(!u.keys || u.keys.length === 0) && (
                  <TableRow>
                    <TableCell colSpan={3} className="py-1 text-xs text-muted-foreground">（该账号暂无设备）</TableCell>
                  </TableRow>
                )}
              </Fragment>
            ))}
            {users?.length === 0 && (
              <TableRow><TableCell colSpan={3} className="py-6 text-center text-xs text-muted-foreground">暂无账号</TableCell></TableRow>
            )}
          </TableBody>
        </Table>
      </div>
      <p className="text-xs text-muted-foreground">管理员是账号属性：该账号登录即具管理权限。</p>

      <UserAddDialog open={addOpen} onOpenChange={setAddOpen} onDone={load} />
      <PasswdSetDialog user={psTarget} pw={psPw} onClose={() => setPsTarget(null)} />
      <Confirm
        open={!!resetConfirm} onOpenChange={(v) => !v && setResetConfirm(null)}
        title={`重置账号 ${resetConfirm} 的密码？`} confirmText="重置"
        onConfirm={() => { if (resetConfirm) void passwdSet(resetConfirm); }}
      >
        将生成新的登录初始密码，原密码即刻失效；其他设备不受影响。新密码仅显示一次，请立即交予发布者妥善保存。
      </Confirm>
      <Confirm
        open={!!confirmAct} onOpenChange={(v) => !v && setConfirmAct(null)}
        title={confirmAct?.title ?? ""} confirmText="确认执行" busy={busy} onConfirm={runConfirm}
      >
        {confirmAct?.body}
      </Confirm>
    </div>
  );
}

function UserAddDialog({ open, onOpenChange, onDone }: { open: boolean; onOpenChange: (v: boolean) => void; onDone: () => void }) {
  const [name, setName] = useState("");
  const [err, setErr] = useState("");
  const [admin, setAdmin] = useState(false);
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<{ registry: string } | null>(null);
  const [pw, setPw] = useState("");

  // 打开即生成随机高强度密码（可改）；连续发放每轮换新值
  useEffect(() => {
    if (open) setPw(randAlnum(16));
  }, [open]);

  async function submit() {
    setErr("");
    if (!NAME_RE.test(name)) {
      setErr(name === "" ? "请输入账号名" : "账号名仅限小写字母、数字与连字符，请修改后重试");
      return;
    }
    if (pw.trim().length < 8) { setErr("密码至少 8 位"); return; }
    setBusy(true);
    try {
      const j = (await api("POST", "/_api/admin/user-add", { user: name.trim(), password: pw.trim(), ...(admin ? { admin: true } : {}) })) as { registry_password?: string; password?: string };
      setResult({
        registry: j.registry_password || j.password || "（响应未携带密码，请联系主机管理员）",
      });
      await onDone();
    } catch (e) {
      toast.error(`发放新账号失败：${(e as { msg?: string }).msg ?? ""}`);
    } finally {
      setBusy(false);
    }
  }

  const copy = (v: string, what: string) => {
    navigator.clipboard?.writeText(v);
    toast.success(`已复制${what}`);
  };

  return (
    <Dialog open={open} onOpenChange={(v) => { onOpenChange(v); if (!v) { setName(""); setErr(""); setAdmin(false); setResult(null); } }}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>发放新账号</DialogTitle>
          <DialogDescription>登录初始密码在表单中设定（默认随机高强度）；登录后可自行修改。</DialogDescription>
        </DialogHeader>
        <FieldGroup>
          <Field data-invalid={!!err}>
            <FieldLabel htmlFor="ad-na-user">账号名</FieldLabel>
            <Input
              id="ad-na-user" className="w-44 font-mono" placeholder="如 carol…"
              value={name} onChange={(e) => setName(e.target.value)} aria-invalid={!!err}
            />
            {err ? <FieldError>{err}</FieldError> : null}
          </Field>
          <Field>
            <FieldLabel htmlFor="ad-na-pw">登录初始密码</FieldLabel>
            <div className="flex items-center gap-2">
              <Input
                id="ad-na-pw" className="w-52 font-mono" autoComplete="off"
                value={pw} onChange={(e) => setPw(e.target.value)}
              />
              <Button
                size="sm" variant="outline" type="button"
                onClick={() => setPw(randAlnum(16))}
              >
                换一个
              </Button>
            </div>
            <FieldDescription>默认随机生成高强度密码，可直接修改；请交予发布者妥善保存。</FieldDescription>
          </Field>
          <Field>
            <label className="flex items-center gap-2 text-sm">
              <input type="checkbox" className="accent-primary" checked={admin} onChange={(e) => setAdmin(e.target.checked)} />
              同时授予管理员（可管理账号与平台状态）
            </label>
          </Field>
          <Button onClick={submit} disabled={busy}>{busy ? "发放中…" : "发放"}</Button>
        </FieldGroup>
        {result && (
          <div className="flex flex-col gap-2 rounded-md border p-3">
            <Alert variant="destructive">
              <ShieldAlertIcon />
              <AlertTitle>发放成功：镜像库密码仅显示一次</AlertTitle>
              <AlertDescription>登录初始密码即表单所设；镜像库密码请立即交予发布者妥善保存，关闭后无法再查。</AlertDescription>
            </Alert>
            <div className="flex flex-wrap items-center gap-2">
              <span className="w-56 shrink-0 text-xs text-muted-foreground">镜像库密码（docker push 推送用）</span>
              <code className="rounded bg-muted px-2 py-0.5 font-mono text-sm">{result.registry}</code>
              <Button size="sm" variant="outline" onClick={() => copy(result.registry, "镜像库密码")}>
                <CopyIcon data-icon="inline-start" />复制密码
              </Button>
            </div>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}

function PasswdSetDialog({ user, pw, onClose }: { user: string | null; pw: string; onClose: () => void }) {
  return (
    <Dialog open={!!user} onOpenChange={(v) => !v && onClose()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader><DialogTitle>重置密码</DialogTitle></DialogHeader>
        <Alert variant="destructive">
          <ShieldAlertIcon />
          <AlertTitle>已为账号 <span className="font-mono">{user}</span> 生成新的登录初始密码</AlertTitle>
          <AlertDescription>仅显示一次，请立即交予发布者妥善保存；原密码即刻失效，其他设备不受影响。</AlertDescription>
        </Alert>
        <div className="flex items-center gap-2">
          <code className="rounded bg-muted px-2 py-1 font-mono text-base">{pw}</code>
          <Button
            size="sm" variant="outline"
            onClick={() => { navigator.clipboard?.writeText(pw); toast.success("已复制密码"); }}
          >
            <CopyIcon data-icon="inline-start" />复制密码
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function AllProjectsPanel() {
  const [rows, setRows] = useState<{ project: string; user: string; status: string; updated: number }[] | null>(null);

  const load = useCallback(async () => {
    try {
      const j = (await api("GET", "/_api/admin/projects")) as { projects?: { project: string; user?: string; status?: string; updated?: number }[] };
      setRows((j.projects || []).map((p) => ({ project: p.project, user: p.user || "—", status: p.status || "", updated: Number(p.updated) || 0 })));
    } catch { setRows([]); }
  }, []);
  const { confirmAct, setConfirmAct, busy, runConfirm } = useConfirmAct(load);
  useEffect(() => { void load(); }, [load]);

  function act(owner: string, project: string, kind: "start" | "stop" | "rm") {
    const CN = { start: "启动", stop: "停止", rm: "删除" } as const;
    const ask = kind === "stop"
      ? `确认停止账号 ${owner} 的项目 ${project}？容器即刻停止、对外不可访问（数据与凭据保留，可随时重新「启动」）。`
      : kind === "rm"
        ? `确认删除账号 ${owner} 的项目 ${project}？容器与路由即刻下线，数据库将一并删除；删除后凭据一并清除，不可恢复。`
        : null;
    setConfirmAct({
      title: `${CN[kind]} ${project}？`,
      body: ask ?? "",
      run: async () => {
        await api("POST", `/_api/${kind}`, { project });
        toast.success(`已${CN[kind]} ${project}（${owner}）`);
      },
    });
  }

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold text-info">全部项目</h2>
        <span className="text-xs text-muted-foreground">全部账号的项目（管理员）</span>
      </div>
      <div className="rounded-lg border">
        <Table>
          <TableHeader>
            <TableRow className="bg-muted/30">
              <TableHead className="w-32">项目</TableHead>
              <TableHead className="w-24">所属账号</TableHead>
              <TableHead className="w-24">状态</TableHead>
              <TableHead className="w-36 text-right">更新</TableHead>
              <TableHead className="text-right">操作</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows === null ? (
              <TableRow><TableCell colSpan={5} className="py-6 text-center text-xs text-muted-foreground">加载中…</TableCell></TableRow>
            ) : rows.map((p) => (
              <TableRow key={p.user + p.project}>
                <TableCell className="font-mono text-xs">{p.project}</TableCell>
                <TableCell className="font-mono text-xs">{p.user}</TableCell>
                <TableCell><StatusBadge status={p.status} /></TableCell>
                <TableCell className="text-right font-mono text-xs text-muted-foreground">{fmtTime(p.updated)}</TableCell>
                <TableCell className="text-right">
                  <div className="flex flex-wrap justify-end gap-1">
                    {p.status === "running"
                      ? <Button size="sm" variant="destructive" onClick={() => act(p.user, p.project, "stop")}>停止</Button>
                      : p.status !== "missing" && <Button size="sm" variant="secondary" onClick={() => act(p.user, p.project, "start")}>启动</Button>}
                    <Button size="sm" variant="destructive" onClick={() => act(p.user, p.project, "rm")}>删除</Button>
                  </div>
                </TableCell>
              </TableRow>
            ))}
            {rows?.length === 0 && (
              <TableRow><TableCell colSpan={5} className="py-6 text-center text-xs text-muted-foreground">（暂无项目）</TableCell></TableRow>
            )}
          </TableBody>
        </Table>
      </div>
      <Confirm
        open={!!confirmAct} onOpenChange={(v) => !v && setConfirmAct(null)}
        title={confirmAct?.title ?? ""} confirmText="确认执行" busy={busy} onConfirm={runConfirm}
      >
        {confirmAct?.body}
      </Confirm>
    </div>
  );
}

function StatusPanel() {
  const [j, setJ] = useState<{
    services?: Record<string, string>;
    disk?: Record<string, { used?: number; total?: number } | string>;
    mem?: { available_mb?: number };
    mem_available_mb?: number;
    containers?: ({ name?: string; status?: string } | string)[];
    timers?: ({ timer?: string; name?: string; next?: string; next_elapse?: string } | string)[];
  } | null>(null);

  const load = useCallback(async () => {
    try { setJ((await api("GET", "/_api/admin/status")) as never); } catch { setJ(null); }
  }, []);

  useEffect(() => {
    void load();
    const id = window.setInterval(load, 30000);
    return () => clearInterval(id);
  }, [load]);

  if (!j) return <div className="flex flex-col gap-3"><h2 className="text-sm font-semibold text-info">平台状态</h2><p className="text-xs text-muted-foreground">（加载失败，将自动重试）</p></div>;

  const svcs = Object.entries(j.services || {});
  const disks = Object.entries(j.disk ?? {});
  const mem = j.mem?.available_mb ?? j.mem_available_mb;
  const ctrs = (j.containers || []).map((c) => (typeof c === "string" ? { name: c, status: "" } : c));
  const timers = (j.timers || []).map((t) => (typeof t === "string" ? { timer: t } : t));

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold text-info">平台状态</h2>
        <span className="text-xs text-muted-foreground">30 秒自动刷新</span>
      </div>
      <div className="flex flex-col gap-4">
        <div>
          <h3 className="mb-2 text-sm font-semibold text-info">服务状态（systemd is-active）</h3>
          <div className="flex flex-wrap gap-4">
            {svcs.length ? svcs.map(([n, s]) => (
              <span key={n} className="flex items-center gap-1.5 text-xs">
                <span className={s === "active" ? "text-ok" : "text-bad"}>●</span>
                <span className="font-mono">{n}</span>
                <span className="text-muted-foreground">{svcCn(s)}</span>
              </span>
            )) : <span className="text-xs text-muted-foreground">（无数据）</span>}
          </div>
        </div>
        <div>
          <h3 className="mb-2 text-sm font-semibold text-info">资源</h3>
          <div className="rounded-lg border">
            <Table>
              <TableBody>
                <TableRow><TableCell>内存可用（MemAvailable）</TableCell><TableCell className="font-mono text-xs">{mem != null ? `${mem} MB` : "—"}</TableCell></TableRow>
                {disks.length ? disks.map(([p, d]) => (
                  <TableRow key={p}>
                    <TableCell className="font-mono text-xs">{p}</TableCell>
                    <TableCell className="font-mono text-xs">{typeof d === "object" && d ? `${fmtGB(d.used)} 已用 / 共 ${fmtGB(d.total)}` : String(d)}</TableCell>
                  </TableRow>
                )) : <TableRow><TableCell className="text-xs text-muted-foreground">（无磁盘数据）</TableCell></TableRow>}
              </TableBody>
            </Table>
          </div>
        </div>
        <div>
          <h3 className="mb-2 text-sm font-semibold text-info">容器</h3>
          <div className="rounded-lg border">
            <Table>
              <TableHeader><TableRow className="bg-muted/30"><TableHead>名称</TableHead><TableHead>状态</TableHead></TableRow></TableHeader>
              <TableBody>
                {ctrs.length ? ctrs.map((c, i) => (
                  <TableRow key={i}>
                    <TableCell className="font-mono text-xs">{c.name ?? ""}</TableCell>
                    <TableCell><StatusBadge status={c.status || ""} /></TableCell>
                  </TableRow>
                )) : <TableRow><TableCell colSpan={2} className="py-4 text-center text-xs text-muted-foreground">（无容器）</TableCell></TableRow>}
              </TableBody>
            </Table>
          </div>
        </div>
        <div>
          <h3 className="mb-2 text-sm font-semibold text-info">定时器（下次触发）</h3>
          <div className="rounded-lg border">
            <Table>
              <TableHeader><TableRow className="bg-muted/30"><TableHead>timer</TableHead><TableHead>下次触发</TableHead></TableRow></TableHeader>
              <TableBody>
                {timers.length ? timers.map((t, i) => (
                  <TableRow key={i}>
                    <TableCell className="font-mono text-xs">{t.timer ?? t.name ?? ""}</TableCell>
                    <TableCell className="font-mono text-xs">{t.next ?? t.next_elapse ?? "—"}</TableCell>
                  </TableRow>
                )) : <TableRow><TableCell colSpan={2} className="py-4 text-center text-xs text-muted-foreground">（无定时器）</TableCell></TableRow>}
              </TableBody>
            </Table>
          </div>
        </div>
      </div>
    </div>
  );
}

// —— 宿主配置：期望状态 + 两段式变更 + Redis 扩容 ——
function HostPanel() {
  const [host, setHost] = useState<HostState | null>(null);
  const [mw, setMw] = useState<Record<string, boolean>>({});
  const [policyEdit, setPolicyEdit] = useState({ disk: "", mem: "", factor: "" });
  const [review, setReview] = useState<{ actions: { kind: string; target?: string; detail?: string; needed?: boolean }[]; impact: { removed_middleware?: string[]; affected_projects?: string[] } } | null>(null);
  const [report, setReport] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [applyBusy, setApplyBusy] = useState(false);
  const [rxOpen, setRxOpen] = useState(false);

  const load = useCallback(async () => {
    try {
      const j = (await api("GET", "/_api/admin/host")) as unknown as HostState;
      setHost(j);
      const mwSet = new Set(Array.isArray(j.host_env?.MIDDLEWARE) ? j.host_env.MIDDLEWARE : String(j.host_env?.MIDDLEWARE ?? "").split(/\s+/).filter(Boolean));
      setMw({ postgres: mwSet.has("postgres"), redis: mwSet.has("redis"), registry: mwSet.has("registry") });
      setPolicyEdit({
        disk: j.policy?.DISK_RESERVE_GB != null ? String(j.policy.DISK_RESERVE_GB) : "",
        mem: j.policy?.RESERVE_MEM_MB != null ? String(j.policy.RESERVE_MEM_MB) : "",
        factor: j.policy?.UPLOAD_SPACE_FACTOR != null ? String(j.policy.UPLOAD_SPACE_FACTOR) : "",
      });
    } catch { setHost(null); }
  }, []);

  useEffect(() => {
    void load();
    const id = window.setInterval(load, 30000);
    return () => clearInterval(id);
  }, [load]);

  function dirty(): boolean {
    if (!host) return false;
    const mwSet = new Set(Array.isArray(host.host_env.MIDDLEWARE) ? host.host_env.MIDDLEWARE : String(host.host_env.MIDDLEWARE ?? "").split(/\s+/).filter(Boolean));
    return MW_NAMES.some((n) => mwSet.has(n) !== !!mw[n])
      || (policyEdit.disk !== "" && policyEdit.disk !== String(host.policy.DISK_RESERVE_GB ?? ""))
      || (policyEdit.mem !== "" && policyEdit.mem !== String(host.policy.RESERVE_MEM_MB ?? ""))
      || (policyEdit.factor !== "" && policyEdit.factor !== String(host.policy.UPLOAD_SPACE_FACTOR ?? ""));
  }

  function postBody(confirm: boolean) {
    const he: Record<string, unknown> = { ...(host?.host_env || {}) };
    delete he.FRP_TOKEN; // 打码值回传会覆盖真实凭据——剔除（FRP_TOML 是文件路径非凭据，可回传）
    he.MIDDLEWARE = MW_NAMES.filter((n) => mw[n]).join(" ");
    const toYesNo = (v: unknown, y: string, n: string) => (v === true || v === y || v === "yes" || v === "true") ? y : n;
    he.CADDY = typeof he.CADDY === "boolean" ? toYesNo(he.CADDY, "yes", "no") : String(he.CADDY ?? "yes");
    he.TUNING = typeof he.TUNING === "boolean" ? toYesNo(he.TUNING, "sysctl", "none") : String(he.TUNING ?? "sysctl");
    he.DEDUP = typeof he.DEDUP === "boolean" ? toYesNo(he.DEDUP, "weekly", "none") : String(he.DEDUP ?? "weekly");
    const policy: Record<string, unknown> = { ...(host?.policy || {}) };
    for (const [v, key] of [[policyEdit.disk, "DISK_RESERVE_GB"], [policyEdit.mem, "RESERVE_MEM_MB"], [policyEdit.factor, "UPLOAD_SPACE_FACTOR"]] as const) {
      if (v !== "" && Number.isFinite(Number(v))) policy[key] = Number(v);
    }
    return { host_env: he, policy, confirm };
  }

  async function preview() {
    if (!host) return;
    setApplyBusy(true);
    try {
      const j = (await api("POST", "/_api/admin/host", postBody(false))) as NonNullable<typeof review>;
      setReview({ actions: j.actions || [], impact: j.impact || {} });
      setReport(null);
    } catch (e) {
      toast.error(`查看变更（预演）失败：${(e as { msg?: string }).msg ?? ""}`);
    } finally { setApplyBusy(false); }
  }

  async function apply() {
    const aff = review?.impact.affected_projects || [];
    if (aff.length && !window.confirm(`此变更将影响 ${aff.length} 个在用项目（${aff.join("、")}）：相关项目的重启/回滚/新发布将失去该中间件支持。确认继续执行？`)) return;
    setApplyBusy(true);
    try {
      const j = (await api("POST", "/_api/admin/host", postBody(true))) as { ok?: boolean; report?: { actions?: { ok?: boolean; kind?: string; target?: string; result?: string; detail?: string }[]; changed?: number; warnings?: string[] } };
      const rep = j.report || {};
      const items = rep.actions || [];
      const lines = items.map((a) => `${a.ok === false ? "● 失败" : "● 成功"} ${kindCn(a.kind ?? "")} ${a.target ?? ""} ${a.result || a.detail || ""}`);
      setReport(`执行报告${j.ok === false ? "（有失败项）" : ""}：完成 ${Number(rep.changed ?? items.length)} 项变更。\n${lines.join("\n")}\n${(rep.warnings || []).join("\n")}`);
      setReview(null);
      await load();
    } catch (e) {
      toast.error(`确认执行失败：${(e as { msg?: string }).msg ?? ""}`);
    } finally { setApplyBusy(false); }
  }

  const pool = host?.redis_pool;
  const poolWarn = !!(pool && pool.total && pool.suggest_expand);
  const he = host?.host_env || {};
  const kv = Object.entries(he).filter(([k]) => k !== "MIDDLEWARE");

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold text-info">主机编排（期望状态 · 两段式变更）</h2>
      </div>

      <div className="flex flex-col gap-3">
        <h3 className="text-sm font-semibold text-info">中间件集合（host.env MIDDLEWARE）</h3>
        <FieldGroup>
          {MW_NAMES.map((k) => (
            <Field key={k} orientation="horizontal">
              <label htmlFor={`mw-${k}`} className="flex items-center gap-2 text-sm">
                <Switch id={`mw-${k}`} checked={!!mw[k]} onCheckedChange={(v) => { setMw((s) => ({ ...s, [k]: v })); setReview(null); }} />
                <span className="font-mono">{k}</span>
              </label>
            </Field>
          ))}
        </FieldGroup>
        <FieldDescription>取消勾选即禁用，重新勾选即启用。修改后走下方「查看变更 → 确认执行」两段式。</FieldDescription>
      </div>

      <Separator />
      <div className="flex flex-col gap-3">
        <h3 className="text-sm font-semibold text-info">主机开关与 FRP（host.env 其余键）</h3>
        <div className="rounded-lg border">
          <Table>
            <TableHeader><TableRow className="bg-muted/30"><TableHead className="w-56">键</TableHead><TableHead>当前值</TableHead></TableRow></TableHeader>
            <TableBody>
              {kv.map(([k, v]) => (
                <TableRow key={k}>
                  <TableCell className="font-mono text-xs">{k}</TableCell>
                  <TableCell className="break-all font-mono text-xs text-muted-foreground">{typeof v === "object" && v !== null ? JSON.stringify(v) : String(v)}</TableCell>
                </TableRow>
              ))}
              {!kv.length && <TableRow><TableCell colSpan={2} className="py-4 text-center text-xs text-muted-foreground">（无其他键）</TableCell></TableRow>}
            </TableBody>
          </Table>
        </div>
      </div>

      <Separator />
      <div className="flex flex-col gap-3">
        <h3 className="text-sm font-semibold text-info">参数值（policy.env）</h3>
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          <Field>
            <FieldLabel>REDIS_DBS</FieldLabel>
            <Input readOnly className="w-24 font-mono" value={host?.policy?.REDIS_DBS != null ? String(host.policy.REDIS_DBS) : ""} />
            <FieldDescription>Redis 逻辑库池上限 · 只读，修改走「扩容」</FieldDescription>
          </Field>
          <Field>
            <FieldLabel>DISK_RESERVE_GB</FieldLabel>
            <Input className="w-24 font-mono" value={policyEdit.disk} onChange={(e) => { setPolicyEdit((s) => ({ ...s, disk: e.target.value })); setReview(null); }} />
            <FieldDescription>磁盘预留</FieldDescription>
          </Field>
          <Field>
            <FieldLabel>RESERVE_MEM_MB</FieldLabel>
            <Input className="w-24 font-mono" value={policyEdit.mem} onChange={(e) => { setPolicyEdit((s) => ({ ...s, mem: e.target.value })); setReview(null); }} />
            <FieldDescription>内存预留</FieldDescription>
          </Field>
          <Field>
            <FieldLabel>UPLOAD_SPACE_FACTOR</FieldLabel>
            <Input className="w-24 font-mono" value={policyEdit.factor} onChange={(e) => { setPolicyEdit((s) => ({ ...s, factor: e.target.value })); setReview(null); }} />
            <FieldDescription>上传空间估算膨胀因子</FieldDescription>
          </Field>
        </div>
      </div>

      <Separator />
      <div className="flex flex-col gap-3">
        <h3 className="text-sm font-semibold text-info">漂移清单与 Redis 池</h3>
        {host?.drift?.length ? host.drift.map((d, i) => (
          <Alert key={i} variant={d.severity === "error" ? "destructive" : "default"}>
            <ShieldAlertIcon />
            <AlertTitle>{d.severity === "error" ? "缺失/停摆" : "漂移"} · <span className="font-mono text-xs">{d.item}</span></AlertTitle>
            <AlertDescription>
              期望 <span className="font-mono">{String(d.expected)}</span>，实际 <span className="font-mono">{String(d.actual)}</span>
              {d.advice ? ` —— 建议：${d.advice}` : ""}
            </AlertDescription>
          </Alert>
        )) : <div className="text-xs text-muted-foreground">（无漂移：期望状态与实际一致）</div>}
        {pool && pool.total ? (
          poolWarn ? (
            <Alert variant="destructive">
              <ShieldAlertIcon />
              <AlertTitle>Redis 逻辑库余量不足（剩 {pool.headroom ?? "?"}/{pool.total}）：建议扩容</AlertTitle>
              <AlertDescription className="flex items-center gap-2">
                <Button size="sm" variant="destructive" onClick={() => setRxOpen(true)}>扩容</Button>
              </AlertDescription>
            </Alert>
          ) : (
            <div className="text-xs text-muted-foreground">
              Redis 逻辑库池：已用 <span className="font-mono">{pool.used ?? "?"}</span> / 共 <span className="font-mono">{pool.total}</span>（余 <span className="font-mono">{pool.headroom ?? "?"}</span>）。
            </div>
          )
        ) : null}
      </div>

      <Separator />
      <div className="flex flex-col gap-3">
        <h3 className="text-sm font-semibold text-info">变更流程（修改 → 查看变更 → 确认执行）</h3>
        <div className="flex flex-wrap items-center gap-2">
          <Button size="sm" disabled={!dirty() || applyBusy} onClick={preview}>{applyBusy ? "预演中…" : "查看变更"}</Button>
          {review && <Button size="sm" variant="destructive" onClick={() => setConfirming(true)}>确认执行</Button>}
          {!dirty() && !review && <span className="text-xs text-muted-foreground">当前无未应用的变更（修改开关或参数后可预演）。</span>}
        </div>
        {review && (
          <div className="flex flex-col gap-2">
            <div className="text-xs text-muted-foreground">
              预演：共 {review.actions.length} 项动作，其中 {review.actions.filter((a) => a.needed !== false).length} 项需执行；尚未写盘，点「确认执行」才会应用。
            </div>
            <div className="rounded-lg border">
              <Table>
                <TableHeader><TableRow className="bg-muted/30"><TableHead className="w-28">动作</TableHead><TableHead className="w-40">目标</TableHead><TableHead>说明</TableHead></TableRow></TableHeader>
                <TableBody>
                  {review.actions.map((a, i) => (
                    <TableRow key={i} className={a.needed === false ? "opacity-55" : ""}>
                      <TableCell className="text-xs">{a.needed === false ? "＝" : "＋"} {kindCn(a.kind ?? "")}</TableCell>
                      <TableCell className="break-all font-mono text-xs">{a.target ?? ""}</TableCell>
                      <TableCell className="break-all text-xs text-muted-foreground">{a.detail ?? ""}</TableCell>
                    </TableRow>
                  ))}
                  {!review.actions.length && <TableRow><TableCell colSpan={3} className="py-4 text-center text-xs text-muted-foreground">（无动作：期望状态已就绪）</TableCell></TableRow>}
                </TableBody>
              </Table>
            </div>
            {!!review.impact.affected_projects?.length && (
              <Alert variant="destructive">
                <ShieldAlertIcon />
                <AlertTitle>红色警示：此变更影响 {review.impact.affected_projects.length} 个在用项目</AlertTitle>
                <AlertDescription className="font-mono">{review.impact.affected_projects.join("、")}——这些项目的重启/回滚/新发布将失去该中间件支持。</AlertDescription>
              </Alert>
            )}
          </div>
        )}
        {report && <pre className="rounded-md border bg-background p-3 font-mono text-xs leading-5 whitespace-pre-wrap">{report}</pre>}
      </div>

      <Confirm
        open={confirming} onOpenChange={(v) => !v && setConfirming(false)}
        title="确认执行以上变更？" confirmText="执行" busy={applyBusy}
        onConfirm={() => { void apply(); setConfirming(false); }}
      >
        将按预演清单变更主机期望状态（写盘并应用）；预演显示的影响在执行后不可自动回退。
      </Confirm>
      <RedisExpandDialog open={rxOpen} onOpenChange={setRxOpen} onDone={load} />
    </div>
  );
}

function RedisExpandDialog({ open, onOpenChange, onDone }: { open: boolean; onOpenChange: (v: boolean) => void; onDone: () => void }) {
  const [n, setN] = useState("");
  const [err, setErr] = useState("");
  const [prompt, setPrompt] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function doPreview() {
    setErr("");
    const v = Number(n);
    if (!Number.isInteger(v) || v < 16 || v > 256) {
      setErr("databases 须为 16–256 的整数（上限受库编号类型承载限制，且需大于当前已分配的最大库编号）");
      setPrompt(null);
      return;
    }
    setBusy(true);
    try {
      const j = (await api("POST", "/_api/admin/host", { redis_databases: v, confirm: false })) as { prompt?: string };
      setPrompt(j.prompt || "（服务端未返回提示文案）");
    } catch (e) {
      toast.error(`获取扩容确认提示失败：${(e as { msg?: string }).msg ?? ""}`);
    } finally { setBusy(false); }
  }

  async function run() {
    setBusy(true);
    try {
      const j = (await api("POST", "/_api/admin/host", { redis_databases: Number(n), confirm: true })) as { databases?: number; restart_secs?: number };
      toast.success(`扩容完成：databases → ${j.databases ?? n}${j.restart_secs != null ? `（实例重启约 ${j.restart_secs} 秒）` : ""}，服务端已自动验证。`);
      onOpenChange(false);
      setPrompt(null); setN("");
      onDone();
    } catch (e) {
      toast.error(`执行扩容失败：${(e as { msg?: string }).msg ?? ""}`);
    } finally { setBusy(false); }
  }

  return (
    <Dialog open={open} onOpenChange={(v) => { onOpenChange(v); if (!v) { setPrompt(null); setErr(""); } }}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Redis 逻辑库扩容</DialogTitle>
          <DialogDescription>新的 databases 值（16–256 的整数，且需大于当前已分配的最大库编号，否则会截断在用库）。</DialogDescription>
        </DialogHeader>
        <FieldGroup>
          <Field data-invalid={!!err}>
            <FieldLabel htmlFor="rx-n">新的 databases 值</FieldLabel>
            <div className="flex items-center gap-2">
              <Input
                id="rx-n" type="number" min={16} max={256} step={1} className="w-28 font-mono"
                placeholder="如 256…" value={n} onChange={(e) => setN(e.target.value)} aria-invalid={!!err}
              />
              <Button size="sm" variant="outline" onClick={doPreview} disabled={busy}>获取确认提示</Button>
              {prompt && <Button size="sm" variant="destructive" onClick={run} disabled={busy}>我已了解，执行扩容</Button>}
            </div>
            {err ? <FieldError>{err}</FieldError> : null}
          </Field>
        </FieldGroup>
        {prompt && <pre className="rounded-md border bg-background p-3 font-mono text-xs leading-5 whitespace-pre-wrap">{prompt}</pre>}
      </DialogContent>
    </Dialog>
  );
}

function EventsPanel() {
  const { events, evConnected } = useApp();
  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold text-info">实时事件</h2>
        <div className="flex items-center gap-3 text-xs text-muted-foreground">
          <span>
            连接状态：<Badge variant={evConnected ? "success" : "secondary"} className="ml-1">{evConnected ? "已连接" : "未连接"}</Badge>
          </span>
          <span>最近 20 条。</span>
        </div>
      </div>
      <ScrollArea className="h-72 rounded-md border bg-background">
        <pre className="p-3 font-mono text-xs leading-6 whitespace-pre-wrap" aria-live="polite">
          {events.length ? events.join("\n") : "（暂无事件）"}
        </pre>
      </ScrollArea>
    </div>
  );
}
