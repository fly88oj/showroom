import { useEffect, useMemo, useRef, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Combobox } from "@/components/Combobox";
import { Command, CommandGroup, CommandInput, CommandItem, CommandList } from "@/components/ui/command";
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Progress } from "@/components/ui/progress";
import { Separator } from "@/components/ui/separator";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import {
  CheckIcon, ChevronDownIcon, CircleAlertIcon, CopyIcon, FolderOpenIcon, RotateCcwIcon,
} from "lucide-react";
import { cn } from "cn";
import { toast } from "sonner";
import { api } from "@/lib/api";
import { useApp, apiFailToast, externalUrl } from "@/lib/app";
import { checkProj, pemReady } from "@/lib/state";
import { randAlnum } from "@/lib/bytes";
import { freshUpload, resumeUploadCore } from "@/lib/upload";
import { uploadPayload } from "@/lib/envelope";

type Phase = "idle" | "compress" | "upload" | "publish" | "done" | "fail";
type StepState = "wait" | "run" | "ok" | "skip" | "fail";
const PHASE_CN = { compress: "压缩", upload: "上传", publish: "发布" } as const;

/** 发布：单表单双镜像源 → 真部署流（①压缩 ②上传（断点续传）③发布 ④完成） */
export function Publish({
  base, prefill, onConsumed, onOpenSettings,
}: {
  base: string; prefill: string | null; onConsumed: () => void; onOpenSettings: () => void;
}) {
  const { projects, images, session, publishDone, space, refreshSpace, markRow } = useApp();
  const [project, setProject] = useState("");
  // 附加服务行（plan-19）：name/image/cport/mem/cpu
  const [svcs, setSvcs] = useState<{ name: string; image: string; cport: string; mem: string; cpu: string; env: string }[]>([]);
  // 自定义环境变量行（plan-20）：KEY/VALUE——注入容器；同键可覆写平台默认
  const [uenvs, setUenvs] = useState<{ k: string; v: string }[]>([]);
  const [projErr, setProjErr] = useState("");
  const [projOpen, setProjOpen] = useState(false);
  const [src, setSrc] = useState<"tar" | "registry">("tar");
  const [file, setFile] = useState<File | null>(null);
  const [image, setImage] = useState("");
  const [imgErr, setImgErr] = useState("");
  const [mem, setMem] = useState("256");
  const [cport, setCport] = useState("8080");

  const [phase, setPhase] = useState<Phase>("idle");
  const [failAt, setFailAt] = useState<keyof typeof PHASE_CN | null>(null);
  const [stepState, setStepState] = useState<StepState[]>(["wait", "wait", "wait", "wait"]);
  const [prog, setProg] = useState({ cur: 0, total: 1 });
  const [text, setText] = useState("（未开始）");
  const [doneUrl, setDoneUrl] = useState("");
  const deployRef = useRef<{ file: File | null; blob: Blob | null; tag: string; uploadId: string | null; target?: string }>({ file: null, blob: null, tag: "", uploadId: null });

  useEffect(() => {
    if (prefill) {
      setProject(prefill);
      const hit = projects.find((p) => p.project === prefill);
      if (hit?.mem) setMem(String(hit.mem));
      if (hit?.cport) setCport(String(hit.cport));
      onConsumed();
    }
  }, [prefill, projects, onConsumed]);

  useEffect(() => { void refreshSpace(); }, [refreshSpace]);

  const existing = useMemo(() => projects.find((p) => p.project === project.trim()), [projects, project]);
  const name = project.trim();
  const urlChip = name ? externalUrl(base, name) : "（清空则自动生成随机名）";
  const nextV = existing ? (parseInt(String(existing.version).replace(/^v/, ""), 10) || 0) + 1 : 1;
  const memChanged = !!existing?.mem && String(existing.mem) !== mem;
  const cportChanged = !!existing?.cport && String(existing.cport) !== cport;
  const busy = phase === "compress" || phase === "upload" || phase === "publish";

  const setStep = (i: number, s: StepState) =>
    setStepState((ss) => ss.map((v, k) => (k === i ? s : v)));

  function resetBar() {
    setPhase("idle"); setFailAt(null); setProg({ cur: 0, total: 1 });
    setText("（未开始）"); setDoneUrl("");
    setStepState(["wait", "wait", "wait", "wait"]);
  }

  async function doPublish(project2: string, tag: string) {
    setPhase("publish");
    setStep(2, "run");
    setText("③ 发布中…");
    const body: Record<string, unknown> = { project: project2, tag, container_port: parseInt(cport, 10) || 8080 };
    const validUenvs = uenvs.filter((u) => u.k.trim());
    if (validUenvs.length) {
      const m: Record<string, string> = {};
      for (const u of validUenvs) m[u.k.trim()] = u.v; // 同键后值胜出
      body.env = m;
    }
    const parsePairs = (txt: string): Record<string, string> => {
      const m: Record<string, string> = {};
      for (const part of txt.split(";")) {
        const kv = part.split("=");
        if (kv.length >= 2 && kv[0].trim()) m[kv[0].trim()] = kv.slice(1).join("=");
      }
      return m;
    };
    const validSvcs = svcs.filter((v) => v.name.trim() && v.image.trim() && parseInt(v.cport, 10) > 0);
    if (validSvcs.length) {
      body.services = validSvcs.map((v) => ({
        name: v.name.trim(), image: v.image.trim(),
        cport: parseInt(v.cport, 10), mem: v.mem.trim() || "128m", cpu: parseFloat(v.cpu) || 0.5,
        env: parsePairs(v.env),
      }));
    }
    const memv = mem.trim();
    if (memv && /^\d+$/.test(memv)) body.mem = `${memv}m`; // C4：裸数字按 MiB（补 m 后缀，服务端字节语义）
    await api("POST", "/_api/publish", body);
    try {
      localStorage.setItem(`sr.conf.${project2}`, JSON.stringify({ tag, cport: parseInt(cport, 10) || 8080, ts: Date.now() }));
    } catch { /* 存储不可写忽略 */ }
  }

  /** 部署主流程：mode = fresh / resume（断点续传）/ publish（仅重发发布）。 */
  async function deployRun(mode: "fresh" | "resume" | "publish", projectName: string) {
    const d = deployRef.current;
    let phaseNow: keyof typeof PHASE_CN = "compress";
    if (name) markRow(name, "uploading", 10 * 60 * 1000); // 在途兜底标记（成功/失败各按终态改写）
    try {
      if (mode === "fresh") {
        if (d.file) {
          phaseNow = "compress";
          setPhase("compress"); setStep(0, "run");
          setText("① 压缩中：读取文件…");
          const { blob, zstd: pre } = await uploadPayload(d.file, (done, raw) => {
            setText(`① 压缩中：${((done / raw) * 100).toFixed(1)}%（${done} / ${raw} 字节）`);
            setProg({ cur: done, total: raw });
          });
          d.blob = blob;
          setStep(0, "ok");
          setText(pre ? "压缩完成：文件已是压缩格式，直接上传" : `压缩完成：${blob.size} / ${d.file.size} 字节`);
        } else {
          setStep(0, "skip"); setStep(1, "skip");
          setText("未选镜像文件：跳过压缩与上传，直接发布（要求镜像已入镜像库）");
        }
      }
      if (d.file && mode !== "publish") {
        phaseNow = "upload";
        markRow(projectName, "uploading", 10 * 60 * 1000);
        setPhase("upload"); setStep(1, "run");
        const upNote = (sent: number, total: number) => {
          setProg({ cur: sent, total });
          setText(`② 上传中：已传 ${sent} / 压缩后总长 ${total} 字节（${total ? ((sent / total) * 100).toFixed(1) : "0"}%）`);
        };
        if (mode === "resume" && d.uploadId) {
          await resumeUploadCore(d.uploadId, d.file, {
            compNote: (done, raw) => { setStep(0, "run"); setText(`① 重新压缩（断点续传重建产物）：${done} / ${raw} 字节…`); setProg({ cur: done, total: raw }); },
            onRec: (rec) => setText(`② 上传中：断点续传（服务端已收 ${rec} 片），重新压缩后补传…`),
            upNote,
            onSpaceInvalidate: () => void refreshSpace(true),
          });
          setStep(0, "ok");
        } else {
          const r = await freshUpload(projectName, d.file, d.blob as Blob, upNote, () => void refreshSpace(true));
          d.uploadId = r.uploadId;
          d.tag = r.tag;
        }
        d.uploadId = null;
        setStep(1, "ok");
        setText("上传完成：镜像已导入镜像库。");
      }
      phaseNow = "publish";
      markRow(projectName, "publishing", 10 * 60 * 1000);
      await doPublish(projectName, d.tag);
      setStep(2, "ok"); setStep(3, "ok");
      setProg({ cur: 1, total: 1 });
      const eurl = externalUrl(base, projectName);
      setDoneUrl(eurl);
      setText("④ 完成：发布成功。");
      setPhase("done");
      markRow(projectName, "rebuilding", 60 * 1000); // §4 「重建中」60s 滚动窗口
      deployRef.current = { file: null, blob: null, tag: "", uploadId: null };
      setFile(null);
      await publishDone(name);
    } catch (e) {
      const err = e as { error?: string; msg?: string };
      setFailAt(phaseNow);
      setPhase("fail");
      if (phaseNow !== "compress") markRow(projectName, "failed", 60 * 1000); // §4 「异常」滚动窗口
      setStep(phaseNow === "compress" ? 0 : phaseNow === "upload" ? 1 : 2, "fail");
      setText(
        `「${PHASE_CN[phaseNow]}」步骤失败：${err.msg || err.error || "网络异常"}`
        + (phaseNow === "upload" && deployRef.current.uploadId ? "；点「重试」将从断点继续。" : "；可点「重试」再次执行。"),
      );
      apiFailToast(e, `发布（${PHASE_CN[phaseNow]}）`, refreshSpace);
    }
  }

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    resetBar();
    setProjErr(""); setImgErr("");
    const d = deployRef.current;
    d.file = null; d.blob = null; d.uploadId = null; d.tag = "";
    if (src === "registry") {
      const m = image.trim().match(/^[^/\s]+\/([^/\s:]+):(\S+)$/);
      if (!m) {
        setImgErr("请先在「镜像库」源选择镜像（格式 user/project:tag），或切换回「上传镜像」源");
        return;
      }
      d.tag = m[2];
    } else {
      d.file = file;
      d.tag = "";
    }
    let project2 = name;
    if (!project2) {
      // §12.1：项目名为空 = 自动生成随机 8 位（crypto 拒绝采样）
      project2 = randAlnum(8);
      setProject(project2);
    }
    const bad = checkProj(project2);
    if (bad) {
      setProjErr(bad);
      return;
    }
    if (src === "tar" && file) {
      if (!pemReady()) {
        toast.error("尚未完成安全配置", { description: "登录后通常已自动配置；可到「设置」完成" });
        onOpenSettings();
        return;
      }
    }
    deployRef.current.target = project2;
    await deployRun("fresh", project2);
  }

  function retry() {
    const pn = deployRef.current.target || name;
    if (failAt === "upload" && deployRef.current.uploadId) void deployRun("resume", pn);
    else if (failAt === "publish") void deployRun("publish", pn);
    else void deployRun("fresh", pn);
  }

  const STEPS = ["① 压缩", "② 上传", "③ 发布", "④ 完成"];

  return (
    <div className="flex flex-col gap-4">
      <h2 className="text-sm font-semibold text-info">发布</h2>

      <form onSubmit={submit} noValidate>
        <FieldGroup>
          <div className="grid gap-4 sm:grid-cols-2">
            <Field data-invalid={!!projErr}>
              <FieldLabel htmlFor="pub-project">项目</FieldLabel>
              <Popover open={projOpen} onOpenChange={setProjOpen}>
                <div className="flex items-center gap-2">
                  <Input
                    id="pub-project" className="w-44" placeholder="输入新名，或选择已有项目…"
                    value={project} aria-invalid={!!projErr}
                    onChange={(e) => { setProject(e.target.value); setProjErr(""); }}
                  />
                  <PopoverTrigger asChild>
                    <Button type="button" variant="outline" size="icon" aria-label="选择已有项目">
                      <ChevronDownIcon />
                    </Button>
                  </PopoverTrigger>
                  <span aria-live="polite" className="truncate font-mono text-xs text-muted-foreground">
                    {urlChip}
                  </span>
                </div>
                <PopoverContent className="w-44 p-0" align="start">
                  <Command>
                    <CommandInput placeholder="输入过滤…" />
                    <CommandList>
                      <CommandGroup>
                        {projects.map((p) => (
                          <CommandItem
                            key={p.project} value={p.project} className="font-mono text-xs"
                            onSelect={(v) => { setProject(v); setProjOpen(false); }}
                          >
                            {p.project}
                          </CommandItem>
                        ))}
                      </CommandGroup>
                    </CommandList>
                  </Command>
                </PopoverContent>
              </Popover>
              {projErr ? <FieldError>{projErr}</FieldError> : (
                <FieldDescription>可输入新项目名发布，或从已有项目选择；清空后发布将自动生成随机名。</FieldDescription>
              )}
            </Field>

            <Field>
              <FieldLabel>镜像源</FieldLabel>
              <ToggleGroup
                type="single" value={src} onValueChange={(v) => v && setSrc(v as "tar" | "registry")}
                className="justify-start"
              >
                <ToggleGroupItem value="tar">上传镜像</ToggleGroupItem>
                <ToggleGroupItem value="registry">镜像库</ToggleGroupItem>
              </ToggleGroup>
              <FieldDescription>上传：自动压缩 → 上传 → 发布；镜像库：①② 自动跳过直接发布。</FieldDescription>
            </Field>
          </div>

          {src === "tar" ? (
            <Field>
              <FieldLabel>镜像 tar 文件</FieldLabel>
              <div className="flex items-center gap-2">
                <label htmlFor="pub-file">
                  <Button type="button" variant="outline" asChild>
                    <span><FolderOpenIcon data-icon="inline-start" />选择文件</span>
                  </Button>
                </label>
                <input
                  id="pub-file" type="file" className="hidden" accept=".tar"
                  onChange={(e) => setFile(e.target.files?.[0] ?? null)}
                />
                <span className="truncate text-xs text-muted-foreground">
                  {file ? `${file.name}（${file.size} 字节）` : "未选择文件"}
                </span>
              </div>
              <FieldDescription>不选则直接发布，要求镜像已入镜像库；选择后自动压缩并上传再发布。</FieldDescription>
            </Field>
          ) : (
            <Field data-invalid={!!imgErr}>
              <FieldLabel>源镜像</FieldLabel>
              <Combobox
                value={image} onValueChange={(v) => { setImage(v); setImgErr(""); }}
                className="w-56" ariaLabel="源镜像"
                placeholder="从本人上传的镜像中选择…"
                searchPlaceholder="输入前缀过滤，如 alice/demo:…"
                options={images
                  .map((i) => i.name)
                  .filter((s) => s.startsWith(`${session?.user ?? ""}/`))
                  .map((s) => ({ value: s, label: s }))}
              />
              {imgErr ? <FieldError>{imgErr}</FieldError> : <FieldDescription>只显示本人上传的镜像。</FieldDescription>}
            </Field>
          )}

          <Field>
            <FieldLabel>版本</FieldLabel>
            <span aria-live="polite" className="font-mono text-xs text-muted-foreground">
              将发布为 v{nextV}
            </span>
            <FieldDescription>版本由平台自动分配，单调递增；回滚后继续向前编号。</FieldDescription>
          </Field>

          <Separator />
          <div className="flex flex-col gap-3">
            {(memChanged || cportChanged) && <Badge variant="warning" className="w-fit">与上次发布不同</Badge>}
              <div className="grid gap-4 sm:grid-cols-2">
                <Field>
                  <FieldLabel htmlFor="pub-mem">内存需求（MiB）</FieldLabel>
                  <Input id="pub-mem" className="w-24" value={mem} onChange={(e) => setMem(e.target.value)} />
                  {memChanged && <FieldDescription className="text-warn">与上次发布不同</FieldDescription>}
                </Field>
                <Field>
                  <FieldLabel htmlFor="pub-cport">项目端口（容器内）</FieldLabel>
                  <Input
                    id="pub-cport" className="w-24" type="number" min={1} max={65535}
                    value={cport} onChange={(e) => setCport(e.target.value)}
                  />
                  {cportChanged && <FieldDescription className="text-warn">与上次发布不同</FieldDescription>}
                </Field>
              </div>

              <Separator />
              <div className="flex flex-col gap-2">
                <div className="flex items-center gap-2">
                  <span className="text-sm font-medium">自定义环境变量（全体容器）</span>
                  <Button type="button" variant="outline" size="sm" onClick={() => setUenvs((a) => [...a, { k: "", v: "" }])}>+ 添加变量</Button>
                </div>
                <p className="text-xs text-muted-foreground">任意应用变量（如 LOG_LEVEL / TIMEZONE）注入每个容器；同键覆写平台默认值；值支持 <span className="font-mono">${'{'}VAR{'}'}</span> 引用平台凭据（如 <span className="font-mono">${'{'}DATABASE_URL{'}'}</span>）；配置随版本走（回滚恢复）。</p>
                <EnvRows rows={uenvs} onChange={setUenvs} />
              </div>

              <Separator />
              <div className="flex flex-col gap-2">
                <div className="flex items-center gap-2">
                  <span className="text-sm font-medium">附加服务（多容器）</span>
                  <Button
                    type="button" variant="outline" size="sm"
                    onClick={() => setSvcs((v) => [...v, { name: "", image: "", cport: "", mem: "", cpu: "", env: "" }])}
                  >+ 添加服务</Button>
                </div>
                <p className="text-xs text-muted-foreground">镜像须已在「镜像库」；容器互相以 SHOWROOM_SVC_&lt;名称&gt;_HOST/_PORT 发现；回滚时附加服务随版本一起恢复。</p>
                {svcs.map((v, i) => (
                  <ServiceCard
                    key={i}
                    value={v}
                    onChange={(nv) => setSvcs((a) => a.map((x, j) => (j === i ? nv : x)))}
                    onRemove={() => setSvcs((a) => a.filter((_, j) => j !== i))}
                  />
                ))}
              </div>
          </div>

          <div className="flex items-center gap-3">
            <Button type="submit" disabled={busy}>
              {busy ? (phase === "compress" ? "压缩中…" : phase === "upload" ? "上传中…" : "发布中…") : "发布"}
            </Button>
            {!busy && (
              <span className="text-xs text-muted-foreground" aria-live="polite">
                可用空间：{space.availGb > 0 ? `${space.availGb.toFixed(1)} GB（预留 ${space.reserveGb} GB）` : "…"}
              </span>
            )}
          </div>
        </FieldGroup>
      </form>

      {phase !== "idle" && (
        <div className="flex flex-col gap-2">
          <div className="flex flex-wrap items-center gap-2">
            {STEPS.map((s, i) => (
              <Badge
                key={s}
                variant={
                  stepState[i] === "ok" ? "success"
                  : stepState[i] === "run" ? "info"
                  : stepState[i] === "fail" ? "danger"
                  : stepState[i] === "skip" ? "outline" : "secondary"
                }
                className={cn(stepState[i] === "run" && "animate-pulse")}
              >
                {stepState[i] === "ok" && <CheckIcon />}
                {stepState[i] === "fail" && <CircleAlertIcon />}
                {s}
                {stepState[i] === "skip" && "（跳过）"}
              </Badge>
            ))}
            {phase === "fail" && (
              <Button size="sm" variant="secondary" onClick={retry}>
                <RotateCcwIcon data-icon="inline-start" />
                {failAt === "upload" && deployRef.current.uploadId ? "重试（断点续传）" : "重试"}
              </Button>
            )}
          </div>
          <Progress value={prog.total ? (prog.cur / prog.total) * 100 : 0} />
          <div className="text-xs text-muted-foreground" aria-live="polite">{text}</div>
          {phase === "done" && (
            <div className="flex flex-wrap items-center gap-2">
              <a
                href={doneUrl} target="_blank" rel="noopener"
                className="text-base font-mono font-semibold text-info hover:underline"
              >
                {doneUrl}
              </a>
              <Button
                size="sm" variant="outline"
                onClick={() => { navigator.clipboard?.writeText(doneUrl); toast.success("已复制访问地址"); }}
              >
                <CopyIcon data-icon="inline-start" />复制地址
              </Button>
            </div>
          )}
        </div>
      )}

      <Separator />
      <div className="flex flex-col gap-2">
        <h3 className="text-sm font-semibold text-info">发布说明</h3>
        <ol className="list-decimal pl-5 text-xs leading-6 text-muted-foreground">
          <li>选「上传镜像」→ 自动压缩 → 上传 → 发布 → 展示访问地址（恒为「访问地址 /&lt;项目名&gt;/」形式）。</li>
          <li>选「镜像库」→ 直接发布已入库镜像（①②两段自动跳过）。</li>
          <li>任一步失败状态条标红，可点「重试」；只入库不发布请到「镜像库」页上传。</li>
        </ol>
      </div>
    </div>
  );
}


// —— 通用组件：环境变量行（全体/服务专属共用——同一交互形状，plan-21 用户裁决） ——

interface EnvRow { k: string; v: string }

function EnvRows({ rows, onChange }: {
  rows: EnvRow[];
  onChange: (rows: EnvRow[]) => void;
}) {
  return (
    <div className="flex flex-col gap-2">
      {rows.map((u, i) => (
        <div key={i} className="grid gap-2 sm:grid-cols-[1fr_3fr_auto]">
          <Input value={u.k} onChange={(e) => onChange(rows.map((x, j) => (j === i ? { ...x, k: e.target.value } : x)))} />
          <Input value={u.v} onChange={(e) => onChange(rows.map((x, j) => (j === i ? { ...x, v: e.target.value } : x)))} />
          <Button type="button" variant="ghost" size="sm" onClick={() => onChange(rows.filter((_, j) => j !== i))}>移除</Button>
        </div>
      ))}
      <Button type="button" variant="outline" size="sm" className="w-fit" onClick={() => onChange([...rows, { k: "", v: "" }])}>+ 变量</Button>
    </div>
  );
}

// —— 通用组件：附加服务卡片（名称/镜像/端口/资源/专属环境变量一体——添加=整卡添加） ——

interface SvcRow { name: string; image: string; cport: string; mem: string; cpu: string; env: string }

function ServiceCard({ value: v, onChange, onRemove }: {
  value: SvcRow;
  onChange: (v: SvcRow) => void;
  onRemove: () => void;
}) {
  const [envRows, setEnvRows] = useState<EnvRow[]>(() =>
    v.env ? v.env.split(";").filter((p) => p.trim()).map((p) => {
      const eq = p.indexOf("=");
      return eq >= 0 ? { k: p.slice(0, eq), v: p.slice(eq + 1) } : { k: p, v: "" };
    }) : [],
  );
  const syncEnv = (rows: EnvRow[]) => {
    setEnvRows(rows);
    onChange({ ...v, env: rows.filter((r) => r.k.trim()).map((r) => `${r.k.trim()}=${r.v}`).join(";") });
  };
  return (
    <div className="flex flex-col gap-2 rounded-lg border p-3">
      <div className="grid gap-2 sm:grid-cols-[1fr_2fr_5rem_5rem_5rem_auto]">
        <Input placeholder="名称" value={v.name} onChange={(e) => onChange({ ...v, name: e.target.value })} />
        <Input placeholder="user/api:v2" value={v.image} onChange={(e) => onChange({ ...v, image: e.target.value })} />
        <Input placeholder="端口" type="number" value={v.cport} onChange={(e) => onChange({ ...v, cport: e.target.value })} />
        <Input placeholder="128m" value={v.mem} onChange={(e) => onChange({ ...v, mem: e.target.value })} />
        <Input placeholder="0.5" value={v.cpu} onChange={(e) => onChange({ ...v, cpu: e.target.value })} />
        <Button type="button" variant="ghost" size="sm" onClick={onRemove}>移除</Button>
      </div>
      <div className="flex flex-col gap-1.5">
        <span className="text-xs text-muted-foreground">本服务专属变量（只注入此容器；同键覆盖全体变量）</span>
        <EnvRows rows={envRows} onChange={syncEnv} />
      </div>
    </div>
  );
}
