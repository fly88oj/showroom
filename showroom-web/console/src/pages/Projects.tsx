import { useEffect, useRef, useState } from "react";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { StatusBadge } from "@/components/StatusBadge";
import { Confirm, failMsg } from "@/components/Confirm";
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
} from "@/components/ui/dialog";
import {
  Table, TableBody, TableCell, TableHead, TableHeader, TableRow,
} from "@/components/ui/table";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import {
  ExternalLinkIcon, InfoIcon, MoreHorizontalIcon, ScrollTextIcon,
} from "lucide-react";
import { cn } from "cn";
import { toast } from "sonner";
import { consumeNdjson, api } from "@/lib/api";
import { ENV_VARS } from "@/lib/mock";
import { useApp, fmtTime, externalUrl, isPrivateHost, type Project } from "@/lib/app";

/** 我的项目：状态机行操作 + 更多菜单 + 日志抽屉 + 回滚/环境变量（对齐原版 proj.mjs） */
export function Projects({ base, onOpenSettings }: { base: string; onOpenSettings: () => void }) {
  const { projects, projOp, rollback, setTab, setPubPrefill, refreshProjects, marks } = useApp();
  const [confirm, setConfirm] = useState<{ kind: "stop" | "rm"; p: Project } | null>(null);
  const [rb, setRb] = useState<Project | null>(null); // 回滚对话框
  const [env, setEnv] = useState<Project | null>(null);
  const [logsFor, setLogsFor] = useState<string | null>(null); // 日志 Sheet 项目名
  const [busy, setBusy] = useState(false);
  const [opBusy, setOpBusy] = useState<string | null>(null); // 行内启动执行中
  const [firstLoad, setFirstLoad] = useState(true);

  useEffect(() => {
    (async () => { await refreshProjects(); setFirstLoad(false); })();
  }, [refreshProjects]);

  async function doConfirm() {
    if (!confirm) return;
    setBusy(true);
    try {
      await projOp(confirm.p.project, confirm.kind);
      setConfirm(null);
      // 成功静默：不弹 toast（仅错误才显著）
    } catch { failMsg(confirm.kind === "rm" ? "删除" : "停止"); }
    finally { setBusy(false); }
  }

  function toPub(p: Project) {
    setPubPrefill(p.project);
    setTab("pub");
  }

  async function startProj(name: string) {
    setOpBusy(name);
    try { await projOp(name, "start"); } finally { setOpBusy(null); }
  }

  return (
    <div className="flex flex-col gap-4">
      <h2 className="text-sm font-semibold text-info">项目列表</h2>

      {isPrivateHost() && !base && (
        <Alert>
          <InfoIcon />
          <AlertTitle>正在用内网地址展示 URL</AlertTitle>
          <AlertDescription>
            可在
            <Button variant="link" size="sm" className="h-auto px-1 py-0" onClick={onOpenSettings}>设置</Button>
            中配置「对外访问域名」（公网域名）。
          </AlertDescription>
        </Alert>
      )}

      <div className="rounded-lg border">
        <Table>
          <TableHeader>
            <TableRow className="bg-muted/30">
              <TableHead className="w-32">名称</TableHead>
              <TableHead className="w-28">状态</TableHead>
              <TableHead>对外 URL</TableHead>
              <TableHead className="w-20">版本</TableHead>
              <TableHead className="w-24 text-right">更新</TableHead>
              <TableHead className="w-12">日志</TableHead>
              <TableHead className="w-56 text-right">操作</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {firstLoad ? (
              <TableRow><TableCell colSpan={7} className="py-6 text-center text-xs text-muted-foreground">正在加载…</TableCell></TableRow>
            ) : projects.map((p) => {
              // §4 本地在途标记优先（发布中/上传中/60s 重建中·异常滚动窗口），过期回落服务端 status
              const mk = marks[p.project];
              const effStatus = mk && mk.until > Date.now()
                ? (mk.kind === "rebuilding" ? "restarting" : mk.kind === "failed" ? "dead"
                  : mk.kind === "uploading" ? "uploading" : "publishing")
                : p.status;
              const running = effStatus === "running";
              const dead = effStatus === "dead";
              const missing = effStatus === "missing";
              const mid = effStatus === "exited" || effStatus === "paused" || effStatus === "created";
              const inFlight = effStatus === "restarting" || effStatus === "publishing" || effStatus === "uploading";
              const fullUrl = externalUrl(base, p.project);
              return (
                  <TableRow key={p.project}>
                    <TableCell className="font-mono">{p.project}</TableCell>
                    <TableCell><StatusBadge status={effStatus} /></TableCell>
                    <TableCell>
                      <a
                        href={fullUrl} target="_blank" rel="noopener"
                        className="inline-flex items-center gap-1 text-info hover:underline"
                      >
                        {p.url}
                        <ExternalLinkIcon className="size-3 opacity-60" />
                      </a>
                    </TableCell>
                    <TableCell>
                      <Badge variant="outline" className="font-mono">{p.version.startsWith("v") ? p.version : `v${p.version}`}</Badge>
                    </TableCell>
                    <TableCell className="text-right text-xs text-muted-foreground">{p.updated}</TableCell>
                    <TableCell>
                      <Button
                        size="sm" variant="ghost"
                        aria-label={`查看 ${p.project} 日志`}
                        onClick={() => setLogsFor(p.project)}
                      >
                        <ScrollTextIcon />
                      </Button>
                    </TableCell>
                    <TableCell>
                      <div className="flex flex-wrap items-center justify-end gap-1">
                        {missing ? (
                          <Tooltip>
                            <TooltipTrigger asChild>
                              <Button size="sm" variant="secondary" onClick={() => toPub(p)}>发布</Button>
                            </TooltipTrigger>
                            <TooltipContent>到发布页预填该项目并选择镜像拉起</TooltipContent>
                          </Tooltip>
                        ) : (
                          <Button size="sm" variant="secondary" disabled={inFlight} onClick={() => toPub(p)}>
                            发布
                          </Button>
                        )}
                        {running && (
                          <Button size="sm" variant="destructive" onClick={() => setConfirm({ kind: "stop", p })}>
                            停止
                          </Button>
                        )}
                        {mid && (
                          <Button
                            size="sm" variant="secondary"
                            disabled={opBusy === p.project} onClick={() => startProj(p.project)}
                          >
                            {opBusy === p.project ? "启动中…" : "启动"}
                          </Button>
                        )}
                        {(running || mid) && (
                          <DropdownMenu>
                            <DropdownMenuTrigger asChild>
                              <Button size="sm" variant="ghost">更多<MoreHorizontalIcon data-icon="inline-end" /></Button>
                            </DropdownMenuTrigger>
                            <DropdownMenuContent align="start">
                              <DropdownMenuItem onSelect={() => setRb(p)}>回滚…</DropdownMenuItem>
                              <DropdownMenuItem onSelect={() => setEnv(p)}>环境变量</DropdownMenuItem>
                              <DropdownMenuItem onSelect={() => setTab("db")}>数据</DropdownMenuItem>
                              <DropdownMenuSeparator />
                              <DropdownMenuItem variant="destructive" onSelect={() => setConfirm({ kind: "rm", p })}>
                                删除
                              </DropdownMenuItem>
                            </DropdownMenuContent>
                          </DropdownMenu>
                        )}
                        {dead && (
                          <Button size="sm" variant="destructive" onClick={() => setConfirm({ kind: "rm", p })}>
                            删除
                          </Button>
                        )}
                      </div>
                    </TableCell>
                  </TableRow>
              );
            })}
            {!firstLoad && projects.length === 0 && (
              <TableRow><TableCell colSpan={7} className="py-6 text-center text-xs text-muted-foreground">尚无项目——到「发布」页发布第一个 Demo。</TableCell></TableRow>
            )}
          </TableBody>
        </Table>
      </div>

      <Confirm
        open={!!confirm} onOpenChange={(v) => !v && setConfirm(null)}
        title={confirm?.kind === "rm" ? `删除项目 ${confirm?.p.project}？` : `停止项目 ${confirm?.p.project}？`}
        confirmText={confirm?.kind === "rm" ? "删除" : "停止"} busy={busy} onConfirm={doConfirm}
      >
        {confirm?.kind === "rm"
          ? "容器与路由即刻下线，数据库将一并删除，如需保留请先在数据页备份下载 .dump；删除后凭据一并清除，不可恢复。"
          : "容器即刻停止、对外不可访问（数据与凭据保留，可随时重新「启动」）。"}
      </Confirm>

      <LogsDialog project={logsFor} onClose={() => setLogsFor(null)} />
      <RollbackDialog project={rb} onClose={() => setRb(null)} onRollback={rollback} />
      <EnvDialog project={env} onClose={() => setEnv(null)} />
    </div>
  );
}

/** 日志弹窗（居中 Modal + 明确关闭按钮）：§10.4 ndjson 实时流（tail 100 起），1000 行环形。 */
function LogsDialog({ project, onClose }: { project: string | null; onClose: () => void }) {
  const [lines, setLines] = useState<string[]>([]);
  const streamAc = useRef<AbortController | null>(null);
  useEffect(() => {
    if (!project) return;
    setLines([]);
    let alive = true;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const start = () => {
      if (!alive) return;
      const ac = new AbortController();
      streamAc.current = ac;
      consumeNdjson(
        `/_api/logs-stream?project=${encodeURIComponent(project)}&tail=100`,
        (j) => {
          const line = typeof j.line === "string" ? j.line : JSON.stringify(j);
          setLines((ls) => [...ls.slice(-999), line]);
        },
        ac.signal,
      ).catch(() => {
        if (!alive) return;
        timer = setTimeout(start, 5000); // §10.4 断开 5 秒重连（弹窗关闭即停）
      });
    };
    start();
    return () => { alive = false; streamAc.current?.abort(); if (timer) clearTimeout(timer); };
  }, [project]);
  return (
    <Dialog open={!!project} onOpenChange={(v) => !v && onClose()}>
      <DialogContent className="flex h-[72vh] max-w-3xl flex-col gap-2">
        <DialogHeader className="pb-0">
          <DialogTitle className="flex items-center gap-2 font-mono text-base">
            {project}
            <Badge variant="info">LIVE</Badge>
          </DialogTitle>
          <DialogDescription>实时日志（ndjson 流 · 断开 5 秒自动重连）</DialogDescription>
        </DialogHeader>
        <ScrollArea className="min-h-0 flex-1 rounded-md border bg-background">
          <pre className="p-3 font-mono text-xs leading-5 whitespace-pre-wrap">
            {lines.length ? lines.join("\n") : "（暂无日志，等待输出…）"}
          </pre>
        </ScrollArea>
        <DialogFooter className="pt-0">
          <Button variant="outline" onClick={onClose}>关闭</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** 回滚对话框：版本列表（当前高亮/镜像已删灰置）→ 选择即确认（写明版本与镜像） */
function RollbackDialog({
  project, onClose, onRollback,
}: {
  project: Project | null; onClose: () => void;
  onRollback: (name: string, v: string) => Promise<void>;
}) {
  const [pending, setPending] = useState<{ v: string; image: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [rows, setRows] = useState<{ version: string; image: string; time: number; imagePresent?: boolean }[]>([]);
  const [cur, setCur] = useState<string | null>(null);

  useEffect(() => {
    if (!project) return;
    setRows([]);
    let alive = true;
    (async () => {
      try {
        const j = (await api("GET", `/_api/versions?project=${encodeURIComponent(project.project)}`)) as {
          current?: string | number; versions?: { version: string; image: string; time: number; imagePresent?: boolean }[];
        };
        if (!alive) return;
        setCur(String(j.current ?? ""));
        setRows(j.versions || []);
      } catch {
        if (alive) setRows([]);
      }
    })();
    return () => { alive = false; };
  }, [project?.project]); // eslint-disable-line react-hooks/exhaustive-deps

  async function go() {
    if (!project || !pending) return;
    setBusy(true);
    try {
      await onRollback(project.project, pending.v);
      toast.success(`已回滚 ${project.project} → ${pending.v}（部署重建中）`);
      setPending(null);
      onClose();
    } finally { setBusy(false); }
  }

  return (
    <>
      <Dialog open={!!project} onOpenChange={(v) => !v && onClose()}>
        <DialogContent className="sm:max-w-xl">
          <DialogHeader>
            <DialogTitle>回滚 · <span className="font-mono">{project?.project}</span></DialogTitle>
            <DialogDescription>
              选择要回滚到的版本：将用该版本发布时的镜像重新部署，当前版本随即切换（此后新发布继续向前编号）。
            </DialogDescription>
          </DialogHeader>
          <div className="rounded-lg border">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="w-20">版本</TableHead>
                  <TableHead>镜像</TableHead>
                  <TableHead className="w-36 text-right">发布时间</TableHead>
                  <TableHead className="w-28 text-right">操作</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {rows.map((v) => {
                  const isCur = v.version === cur;
                  const gone = v.imagePresent === false || v.imagePresent == null; // 缺镜像或状态未知均不可回滚
                  return (
                    <TableRow key={v.version} className={cn(isCur && "bg-muted/50", !isCur && gone && "opacity-40")}>
                      <TableCell><Badge variant="outline" className="font-mono">{v.version.startsWith("v") ? v.version : `v${v.version}`}</Badge></TableCell>
                      <TableCell className="font-mono text-xs">{v.image}{gone && !isCur && (v.imagePresent === false ? "（镜像已删除）" : "（镜像状态未知）")}</TableCell>
                      <TableCell className="text-right text-xs text-muted-foreground">{fmtTime(v.time)}</TableCell>
                      <TableCell>
                        {isCur ? <span className="text-xs text-muted-foreground">当前版本</span> : (
                          <Button
                            size="sm" variant="secondary" disabled={gone}
                            onClick={() => setPending({ v: v.version, image: v.image })}
                          >
                            回滚到此
                          </Button>
                        )}
                      </TableCell>
                    </TableRow>
                  );
                })}
                {rows.length === 0 && (
                  <TableRow><TableCell colSpan={4} className="py-6 text-center text-xs text-muted-foreground">暂无发布历史（该项目在本功能上线前发布，或仅发布过一个版本）。</TableCell></TableRow>
                )}
              </TableBody>
            </Table>
          </div>
        </DialogContent>
      </Dialog>
      <Confirm
        open={!!pending} onOpenChange={(v) => !v && setPending(null)}
        title={`确认从版本 ${cur || "?"} 回滚到版本 ${pending?.v}？`}
        confirmText="回滚" busy={busy} onConfirm={go}
      >
        {`将使用镜像 ${pending?.image} 重新部署，确认执行。`}
      </Confirm>
    </>
  );
}

/** 环境变量模态：键/当前值/用途（§6） */
function EnvDialog({ project, onClose }: { project: Project | null; onClose: () => void }) {
  return (
    <Dialog open={!!project} onOpenChange={(v) => !v && onClose()}>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>环境变量 · <span className="font-mono">{project?.project}</span></DialogTitle>
          <DialogDescription>发布时自动注入（<span className="font-mono">showroom env</span> 同）。</DialogDescription>
        </DialogHeader>
        <div className="rounded-lg border">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="w-56">键</TableHead>
                  <TableHead>当前值</TableHead>
                  <TableHead>用途</TableHead>
                </TableRow>
              </TableHeader>
            <TableBody>
              {ENV_VARS.map((v) => (
                <TableRow key={v.key}>
                  <TableCell className="font-mono text-xs">{v.key}</TableCell>
                  <TableCell className="max-w-56 truncate font-mono text-xs">{v.value}</TableCell>
                  <TableCell className="text-xs text-muted-foreground">{v.desc}</TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      </DialogContent>
    </Dialog>
  );
}
