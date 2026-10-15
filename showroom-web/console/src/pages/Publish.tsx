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
import { freshUpload } from "@/lib/upload";
import { uploadPayload } from "@/lib/envelope";

type Phase = "idle" | "compress" | "upload" | "publish" | "done" | "fail";
type StepState = "wait" | "run" | "ok" | "skip" | "fail";
const PHASE_CN = { compress: "压缩", upload: "上传", publish: "发布" } as const;

// —— 统一服务行：主服务与附加服务共用（同一镜像源/同一字段/同一变量交互） ——

interface SvcRow {
  name: string;
  src: "tar" | "registry";
  file: File | null;
  image: string;           // registry 模式：用户选择的完整引用；tar 模式：上传后回填
  cport: string;
  mem: string;
  cpu: string;
  env: string;             // `K=V;K2=V2`（wire 格式，组件内部行级编辑）
  resolved: boolean;       // tar 上传完成后置 true（image 已回填）
}

interface EnvRow { k: string; v: string }

// —— 发布：统一 ServiceCard → 真部署流（①压缩 ②上传（含附加服务顺序）③发布 ④完成） ——

export function Publish({
  base, prefill, onConsumed, onOpenSettings,
}: {
  base: string; prefill: string | null; onConsumed: () => void; onOpenSettings: () => void;
}) {
  const { projects, images, session, publishDone, space, refreshSpace, markRow } = useApp();
  const [project, setProject] = useState("");
  const [projErr, setProjErr] = useState("");
  const [projOpen, setProjOpen] = useState(false);
  // 统一服务列表：第一个=主服务（isMain），其余=附加
  const [svcs, setSvcs] = useState<SvcRow[]>([
    { name: "", src: "tar", file: null, image: "", cport: "8080", mem: "256", cpu: "1", env: "", resolved: false },
  ]);
  // 自定义环境变量行（plan-20）：KEY/VALUE——注入容器；同键可覆写平台默认
  const [uenvs, setUenvs] = useState<EnvRow[]>([]);
  const [imgErr, setImgErr] = useState("");
  const [imgErrIdx, setImgErrIdx] = useState<number>(-1);

  const [phase, setPhase] = useState<Phase>("idle");
  const [, setFailAt] = useState<keyof typeof PHASE_CN | null>(null);
  const [stepState, setStepState] = useState<StepState[]>(["wait", "wait", "wait", "wait"]);
  const [prog, setProg] = useState({ cur: 0, total: 1 });
  const [text, setText] = useState("（未开始）");
  const [doneUrl, setDoneUrl] = useState("");
  const deployRef = useRef<{
    files: { file: File; blob: Blob | null; svcIdx: number }[];
    uploadId: string | null;
    target?: string;
    resolvedImages: string[];
  }>({ files: [], uploadId: null, resolvedImages: [] });

  const main = svcs[0];
  const extras = svcs.slice(1);
  const mainName = project.trim() || main.name.trim();
  const existing = useMemo(() => projects.find((p) => p.project === mainName), [projects, mainName]);
  const name = project.trim();
  const urlChip = name ? externalUrl(base, name) : "（清空则自动生成随机名）";
  const nextV = existing ? (parseInt(String(existing.version).replace(/^v/, ""), 10) || 0) + 1 : 1;
  const memChanged = !!existing?.mem && String(existing.mem) !== main.mem;
  const cportChanged = !!existing?.cport && String(existing.cport) !== main.cport;
  const busy = phase === "compress" || phase === "upload" || phase === "publish";
  const myImages = images
    .map((i) => i.name)
    .filter((s) => s.startsWith(`${session?.user ?? ""}/`))
    .map((s) => ({ value: s, label: s }));

  useEffect(() => {
    if (prefill) {
      setProject(prefill);
      const hit = projects.find((p) => p.project === prefill);
      if (hit?.mem) setSvcs((a) => a.map((s, i) => (i === 0 ? { ...s, mem: String(hit.mem) } : s)));
      if (hit?.cport) setSvcs((a) => a.map((s, i) => (i === 0 ? { ...s, cport: String(hit.cport) } : s)));
      onConsumed();
    }
  }, [prefill, projects, onConsumed]);

  useEffect(() => { void refreshSpace(); }, [refreshSpace]);

  const setStep = (i: number, s: StepState) =>
    setStepState((ss) => ss.map((v, k) => (k === i ? s : v)));

  function resetBar() {
    setPhase("idle"); setFailAt(null); setProg({ cur: 0, total: 1 });
    setText("（未开始）"); setDoneUrl("");
    setStepState(["wait", "wait", "wait", "wait"]);
  }

  function updateSvc(idx: number, patch: Partial<SvcRow>) {
    setSvcs((a) => a.map((s, i) => (i === idx ? { ...s, ...patch } : s)));
  }

  function addSvc() {
    setSvcs((a) => [...a, { name: "", src: "registry", file: null, image: "", cport: "", mem: "128m", cpu: "0.5", env: "", resolved: false }]);
  }

  async function doPublish(project2: string, mainTag: string, svcImages: string[]) {
    setPhase("publish");
    setStep(2, "run");
    setText("③ 发布中…");
    const body: Record<string, unknown> = { project: project2, tag: mainTag, container_port: parseInt(main.cport, 10) || 8080 };
    const memv = main.mem.trim();
    if (memv && /^\d+$/.test(memv)) body.mem = `${memv}m`;
    if (uenvs.filter((u) => u.k.trim()).length) {
      const m: Record<string, string> = {};
      for (const u of uenvs.filter((u) => u.k.trim())) m[u.k.trim()] = u.v;
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
    const validExtras = extras.filter((v) => v.name.trim() && parseInt(v.cport, 10) > 0 && (svcImages[extras.indexOf(v) + 1] || v.image.trim()));
    if (validExtras.length) {
      body.services = validExtras.map((v) => {
        const origIdx = extras.indexOf(v) + 1;
        return {
          name: v.name.trim(),
          image: svcImages[origIdx] || v.image.trim(),
          cport: parseInt(v.cport, 10),
          mem: v.mem.trim() || "128m",
          cpu: parseFloat(v.cpu) || 0.5,
          env: parsePairs(v.env),
        };
      });
    }
    await api("POST", "/_api/publish", body);
    try {
      localStorage.setItem(`sr.conf.${project2}`, JSON.stringify({ tag: mainTag, cport: parseInt(main.cport, 10) || 8080, ts: Date.now() }));
    } catch { /* 存储不可写忽略 */ }
  }

  /** 部署主流程：统一处理主服务 + 附加服务的 tar 上传（顺序）→ 发布。 */
  async function deployRun(mode: "fresh" | "resume" | "publish", projectName: string) {
    const d = deployRef.current;
    let phaseNow: keyof typeof PHASE_CN = "compress";
    if (name) markRow(name, "uploading", 10 * 60 * 1000);
    try {
      // ① 压缩（所有 tar 源服务）
      if (mode === "fresh") {
        const tarEntries = d.files;
        if (tarEntries.length > 0) {
          phaseNow = "compress";
          setPhase("compress"); setStep(0, "run");
          let totalRaw = 0, totalDone = 0;
          for (const te of tarEntries) {
            const svcName = te.svcIdx === 0 ? "主服务" : `服务 ${svcs[te.svcIdx]?.name || te.svcIdx}`;
            setText(`① 压缩中：${svcName}…`);
            const { blob } = await uploadPayload(te.file, (done, raw) => {
              setText(`① 压缩中：${svcName} ${((done / raw) * 100).toFixed(1)}%`);
              setProg({ cur: totalDone + done, total: totalRaw + raw });
            });
            te.blob = blob;
            totalRaw += te.file.size;
            totalDone += te.file.size;
          }
          setStep(0, "ok");
          setText(`压缩完成：${tarEntries.length} 个镜像`);
        } else {
          setStep(0, "skip"); setStep(1, "skip");
          setText("全部服务使用镜像库：跳过压缩与上传");
        }
      }

      // ② 上传（所有 tar 源服务，顺序）
      if (d.files.length > 0 && mode !== "publish") {
        phaseNow = "upload";
        markRow(projectName, "uploading", 10 * 60 * 1000);
        setPhase("upload"); setStep(1, "run");
        for (const te of d.files) {
          const svcName = te.svcIdx === 0 ? "主服务" : `服务 ${svcs[te.svcIdx]?.name || te.svcIdx}`;
          const upNote = (sent: number, total: number) => {
            setProg({ cur: sent, total });
            setText(`② 上传中：${svcName} 已传 ${sent} / ${total} 字节（${total ? ((sent / total) * 100).toFixed(1) : "0"}%）`);
          };
          const importProject = te.svcIdx === 0 ? projectName : `${projectName}-${svcs[te.svcIdx]?.name || te.svcIdx}`;
          const r = await freshUpload(importProject, te.file, te.blob as Blob, upNote, () => void refreshSpace(true));
          // 上传成功：回填 resolvedImage（registry 引用：user/{importProject}:{tag}）
          const user = session?.user ?? "";
          d.resolvedImages[te.svcIdx] = `${user}/${importProject.toLowerCase()}:${r.tag}`;
          if (te.svcIdx === 0) d.resolvedImages[0] = r.tag; // 主服务：tag 传给 publish 的 tag 字段
        }
        setStep(1, "ok");
        setText(`上传完成：${d.files.length} 个镜像已导入。`);
      }

      // ③ 发布
      phaseNow = "publish";
      markRow(projectName, "publishing", 10 * 60 * 1000);
      const mainTag = d.resolvedImages[0] || main.image.trim().split(":").pop() || "";
      await doPublish(projectName, mainTag, d.resolvedImages);
      setStep(2, "ok"); setStep(3, "ok");
      setProg({ cur: 1, total: 1 });
      const eurl = externalUrl(base, projectName);
      setDoneUrl(eurl);
      setText("④ 完成：发布成功。");
      setPhase("done");
      markRow(projectName, "rebuilding", 60 * 1000);
      deployRef.current = { files: [], uploadId: null, resolvedImages: [] };
      setSvcs((a) => a.map((s) => ({ ...s, file: null, resolved: false })));
      await publishDone(name);
    } catch (e) {
      const err = e as { error?: string; msg?: string };
      setFailAt(phaseNow);
      setPhase("fail");
      if (phaseNow !== "compress") markRow(projectName, "failed", 60 * 1000);
      setStep(phaseNow === "compress" ? 0 : phaseNow === "upload" ? 1 : 2, "fail");
      setText(`「${PHASE_CN[phaseNow]}」步骤失败：${err.msg || err.error || "网络异常"}；可点「重试」再次执行。`);
      apiFailToast(e, `发布（${PHASE_CN[phaseNow]}）`, refreshSpace);
    }
  }

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    resetBar();
    setProjErr(""); setImgErr(""); setImgErrIdx(-1);
    const d = deployRef.current;
    d.files = []; d.resolvedImages = [];

    let project2 = name;
    if (!project2) {
      project2 = randAlnum(8);
      setProject(project2);
    }
    const bad = checkProj(project2);
    if (bad) { setProjErr(bad); return; }

    // 收集各服务的镜像
    let hasTar = false;
    for (let i = 0; i < svcs.length; i++) {
      const s = svcs[i];
      if (i === 0) continue; // 主服务走 deployRef
      if (s.src === "tar") {
        if (s.file) {
          if (!s.name.trim()) { setImgErr(`附加服务「${i}」缺名称（tar 上传需名称组成镜像名）`); setImgErrIdx(i); return; }
          d.files.push({ file: s.file, blob: null, svcIdx: i });
          hasTar = true;
        }
      } else {
        if (!s.image.trim()) { setImgErr(`附加服务「${s.name || i}」缺镜像引用`); setImgErrIdx(i); return; }
        d.resolvedImages[i] = s.image.trim();
      }
    }
    // 主服务镜像
    if (main.src === "tar") {
      if (main.file) {
        d.files.unshift({ file: main.file, blob: null, svcIdx: 0 });
        hasTar = true;
      } else if (!main.image.trim()) {
        setImgErr("主服务未选文件也未选镜像库引用"); setImgErrIdx(0);
        return;
      } else {
        d.resolvedImages[0] = main.image.trim().split(":").pop() || "";
      }
    } else {
      if (!main.image.trim()) {
        setImgErr("主服务未选择镜像"); setImgErrIdx(0);
        return;
      }
      d.resolvedImages[0] = main.image.trim().split(":").pop() || "";
    }

    if (hasTar && !pemReady()) {
      toast.error("尚未完成安全配置", { description: "登录后通常已自动配置；可到「设置」完成" });
      onOpenSettings();
      return;
    }
    d.target = project2;
    await deployRun("fresh", project2);
  }

  function retry() {
    const pn = deployRef.current.target || name;
    void deployRun("fresh", pn);
  }

  const STEPS = ["① 压缩", "② 上传", "③ 发布", "④ 完成"];

  return (
    <div className="flex flex-col gap-4">
      <h2 className="text-sm font-semibold text-info">发布</h2>

      <form onSubmit={submit} noValidate>
        <FieldGroup>
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
            <FieldLabel>版本</FieldLabel>
            <span aria-live="polite" className="font-mono text-xs text-muted-foreground">
              将发布为 v{nextV}
            </span>
            <FieldDescription>版本由平台自动分配，单调递增；回滚后继续向前编号。</FieldDescription>
          </Field>

          <Separator />
          {(memChanged || cportChanged) && <Badge variant="warning" className="w-fit">与上次发布不同</Badge>}

          {/* —— 主服务（统一 ServiceCard，isMain）—— */}
          <ServiceCard
            isMain
            value={main}
            onChange={(nv) => updateSvc(0, nv)}
            images={myImages}
            imageError={imgErrIdx === 0 ? imgErr : ""}
          />

          {/* —— 全体变量 —— */}
          <div className="flex flex-col gap-2">
            <span className="text-sm font-medium">自定义环境变量（全体容器）</span>
            <p className="text-xs text-muted-foreground">注入每个容器；同键覆写平台默认值；值支持 <span className="font-mono">${'{'}VAR{'}'}</span> 引用平台凭据；配置随版本走。</p>
            <EnvRows rows={uenvs} onChange={setUenvs} />
          </div>

          <Separator />

          {/* —— 附加服务（同一 ServiceCard）—— */}
          <div className="flex flex-col gap-2">
            <div className="flex items-center gap-2">
              <span className="text-sm font-medium">附加服务（多容器）</span>
              <Button type="button" variant="outline" size="sm" onClick={addSvc}>+ 添加服务</Button>
            </div>
            <p className="text-xs text-muted-foreground">与主服务同一表单体验；容器互相以 SHOWROOM_SVC_&lt;名称&gt;_HOST/_PORT 发现；回滚随版本整体恢复。</p>
            {extras.map((v, i) => (
              <ServiceCard
                key={i + 1}
                value={v}
                onChange={(nv) => updateSvc(i + 1, nv)}
                onRemove={() => setSvcs((a) => a.filter((_, j) => j !== i + 1))}
                images={myImages}
                imageError={imgErrIdx === i + 1 ? imgErr : ""}
              />
            ))}
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
                <RotateCcwIcon data-icon="inline-start" />重试
              </Button>
            )}
          </div>
          <Progress value={prog.total ? (prog.cur / prog.total) * 100 : 0} />
          <div className="text-xs text-muted-foreground" aria-live="polite">{text}</div>
          {phase === "done" && (
            <div className="flex flex-wrap items-center gap-2">
              <a href={doneUrl} target="_blank" rel="noopener" className="text-base font-mono font-semibold text-info hover:underline">
                {doneUrl}
              </a>
              <Button size="sm" variant="outline" onClick={() => { navigator.clipboard?.writeText(doneUrl); toast.success("已复制访问地址"); }}>
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
          <li>主服务与附加服务同一表单：选「上传镜像」（自动压缩→上传）或「镜像库」（直接发布）。</li>
          <li>版本由平台自动分配；回滚时主服务与附加服务的配置一起恢复。</li>
          <li>附加服务仅内网（不出公网端口），与主服务同网络互访。</li>
        </ol>
      </div>
    </div>
  );
}

// —— 通用组件：环境变量行（全体/服务专属共用——同一交互形状） ——

function EnvRows({ rows, onChange }: {
  rows: EnvRow[];
  onChange: (rows: EnvRow[]) => void;
}) {
  return (
    <div className="flex flex-col gap-2">
      {rows.map((u, i) => (
        <div key={i} className="grid gap-2 sm:grid-cols-[1fr_3fr_auto]">
          <Input placeholder="KEY" value={u.k} onChange={(e) => onChange(rows.map((x, j) => (j === i ? { ...x, k: e.target.value } : x)))} />
          <Input placeholder="value" value={u.v} onChange={(e) => onChange(rows.map((x, j) => (j === i ? { ...x, v: e.target.value } : x)))} />
          <Button type="button" variant="ghost" size="sm" onClick={() => onChange(rows.filter((_, j) => j !== i))}>移除</Button>
        </div>
      ))}
      <Button type="button" variant="outline" size="sm" className="w-fit" onClick={() => onChange([...rows, { k: "", v: "" }])}>+ 变量</Button>
    </div>
  );
}

// —— 通用组件：服务卡片（主/附加统一——同一镜像源/同一字段/同一变量交互，plan-22 用户裁决） ——

function ServiceCard({ isMain = false, value: v, onChange, onRemove, images, imageError }: {
  isMain?: boolean;
  value: SvcRow;
  onChange: (v: SvcRow) => void;
  onRemove?: () => void;
  images: { value: string; label: string }[];
  imageError?: string;
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
    <div className={cn("flex flex-col gap-3 rounded-lg border p-3", isMain && "border-info/30")}>
      <div className="flex items-center gap-2">
        <span className={cn("text-sm font-medium", isMain ? "text-info" : "")}>{isMain ? "主服务" : "附加服务"}</span>
        {onRemove && (
          <Button type="button" variant="ghost" size="sm" onClick={onRemove}>移除</Button>
        )}
      </div>
      <div className="grid gap-3 sm:grid-cols-2">
        <Field>
          <FieldLabel>镜像源</FieldLabel>
          <ToggleGroup
            type="single" value={v.src} onValueChange={(val) => val && onChange({ ...v, src: val as "tar" | "registry" })}
            className="justify-start"
          >
            <ToggleGroupItem value="tar">上传镜像</ToggleGroupItem>
            <ToggleGroupItem value="registry">镜像库</ToggleGroupItem>
          </ToggleGroup>
        </Field>
        <Field>
          <FieldLabel>{isMain ? "项目端口（容器内）" : "端口（容器内）"}</FieldLabel>
          <Input className="w-24" type="number" min={1} max={65535} placeholder="8080"
            value={v.cport} onChange={(e) => onChange({ ...v, cport: e.target.value })}
          />
        </Field>
      </div>
      {v.src === "tar" ? (
        <Field>
          <FieldLabel>镜像 tar 文件</FieldLabel>
          <div className="flex items-center gap-2">
            <label>
              <Button type="button" variant="outline" asChild>
                <span><FolderOpenIcon data-icon="inline-start" />选择文件</span>
              </Button>
            </label>
            <input type="file" className="hidden" accept=".tar"
              onChange={(e) => onChange({ ...v, file: e.target.files?.[0] ?? null, resolved: false })}
            />
            <span className="truncate text-xs text-muted-foreground">
              {v.file ? `${v.file.name}（${v.file.size} 字节）` : "未选择文件（不选则要求镜像已入镜像库）"}
            </span>
          </div>
        </Field>
      ) : (
        <Field data-invalid={!!imageError}>
          <FieldLabel>从镜像库选择</FieldLabel>
          <Combobox
            value={v.image} onValueChange={(val) => onChange({ ...v, image: val })}
            className="w-56" ariaLabel={isMain ? "主服务镜像" : "附加服务镜像"}
            placeholder="从本人上传的镜像中选择…"
            searchPlaceholder="输入过滤…"
            options={images}
          />
          {imageError ? <FieldError>{imageError}</FieldError> : <FieldDescription>只显示本人上传的镜像。</FieldDescription>}
        </Field>
      )}
      <div className="grid gap-3 sm:grid-cols-2">
        <Field>
          <FieldLabel>{isMain ? "内存需求（MiB）" : "内存"}</FieldLabel>
          <Input className="w-24" placeholder="256" value={v.mem} onChange={(e) => onChange({ ...v, mem: e.target.value })} />
        </Field>
        <Field>
          <FieldLabel>CPU</FieldLabel>
          <Input className="w-16" placeholder="1" value={v.cpu} onChange={(e) => onChange({ ...v, cpu: e.target.value })} />
        </Field>
      </div>
      {!isMain && (
        <div className="flex flex-col gap-1.5">
          <span className="text-xs text-muted-foreground">本服务专属变量（只注入此容器；同键覆盖全体变量）</span>
          <EnvRows rows={envRows} onChange={syncEnv} />
        </div>
      )}
    </div>
  );
}
