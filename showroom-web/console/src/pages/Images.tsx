import { useEffect, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Confirm } from "@/components/Confirm";
import { Field, FieldDescription, FieldLabel } from "@/components/ui/field";
import { Progress } from "@/components/ui/progress";
import { Separator } from "@/components/ui/separator";
import {
  Table, TableBody, TableCell, TableHead, TableHeader, TableRow,
} from "@/components/ui/table";
import { FolderOpenIcon, RotateCcwIcon } from "lucide-react";
import { toast } from "sonner";
import { useApp, apiFailToast } from "@/lib/app";
import { api } from "@/lib/api";
import { pemReady } from "@/lib/state";
import { freshUpload, latestUpId, loadUpSession, resumeUploadCore, sealedTotal, uploadPayload } from "@/lib/upload";
import { dataInvalidate } from "@/lib/state";

/** 镜像库：清单删除（内联 GC）+ 上传入库（真加密信封分片链路）+ 断点续传 + 空间条。
 *  上传与项目无关（用户裁决）：registry repo 名由 tar 文件名派生（合法化），无项目输入框。 */
export function Images() {
  const { images, ensureImages, space, refreshSpace, session } = useApp();
  const [delTarget, setDelTarget] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [file, setFile] = useState<File | null>(null);
  const [prog, setProg] = useState<{ cur: number; total: number } | null>(null);
  const [upText, setUpText] = useState("");
  const [resumeId, setResumeId] = useState<string | null>(null);
  const [upBusy, setUpBusy] = useState(false);

  useEffect(() => {
    void refreshSpace();
    setResumeId(latestUpId());
  }, [refreshSpace]);

  async function refreshAll() {
    dataInvalidate("images", "space");
    await Promise.allSettled([ensureImages(true), refreshSpace(true)]);
    setResumeId(latestUpId());
  }

  async function confirmDel() {
    if (!delTarget) return;
    setBusy(true);
    try {
      const j = (await api("POST", "/_api/image-rm", { image: delTarget })) as { freed_est_mb?: number };
      toast.success(`已回收约 ${Number(j.freed_est_mb ?? 0)} MB`);
      setDelTarget(null);
      await refreshAll();
    } catch (e) {
      apiFailToast(e, `删除镜像 ${delTarget}`, refreshSpace);
    } finally {
      setBusy(false);
    }
  }

  /** repo 名派生：tar 文件名去扩展、非法字符→'-'、截 31 位（§11 项目命名）。 */
  function repoOf(f: File): string {
    const base = f.name.replace(/\.tar(\.gz|\.zst)?$/i, "").replace(/[^a-zA-Z0-9-]/g, "-").replace(/^-+/, "");
    return (base || "image").slice(0, 31);
  }

  async function upload() {
    const f = file;
    if (!f) { toast.error("请先选择镜像 tar 文件"); return; }
    if (!pemReady()) { toast.error("尚未完成安全配置：到「设置」完成"); return; }
    setUpBusy(true); setResumeId(null);
    try {
      setUpText("① 压缩中…");
      const { blob, zstd: pre } = await uploadPayload(f, (done, raw) => {
        setUpText(`① 压缩中：${((done / raw) * 100).toFixed(1)}%（${done} / ${raw} 字节）`);
        setProg({ cur: done, total: raw });
      });
      if (pre) setUpText("文件已是压缩格式，直接上传");
      const project = repoOf(f);
      const r = await freshUpload(
        project, f, blob,
        (sent, total) => { setProg({ cur: sent, total }); setUpText(`② 上传中：${sent} / ${total} 字节（${total ? ((sent / total) * 100).toFixed(1) : "0"}%）`); },
        () => void refreshSpace(true),
      );
      setUpText(`已入库 ${project}:${r.tag}，可在「发布」页选「镜像库」源发布。`);
      setProg(null);
      setFile(null);
      await refreshAll();
    } catch (e) {
      setProg(null);
      setUpText("");
      setResumeId(latestUpId());
      apiFailToast(e, "上传入库", refreshSpace);
    } finally {
      setUpBusy(false);
    }
  }

  async function resume() {
    const id = resumeId;
    const rec = id ? loadUpSession(id) : null;
    if (!id || !rec) return;
    const f = file;
    if (!f) { toast.error(`续传需选择同一文件（${rec.name}，${rec.size} 字节）`); return; }
    if (f.size !== rec.size) { toast.error(`续传需选择同一文件（${rec.name}，${rec.size} 字节）`); return; }
    setUpBusy(true);
    try {
      const total = sealedTotal(rec.zsize);
      setProg({ cur: 0, total });
      await resumeUploadCore(id, f, {
        compNote: (done, raw) => { setUpText(`① 重新压缩（断点续传重建产物）：${done} / ${raw} 字节…`); setProg({ cur: done, total: raw }); },
        onRec: (n) => setUpText(`② 断点续传：服务端已收 ${n} 片，重建产物后补传…`),
        upNote: (sent, t) => { setProg({ cur: sent, total: t }); setUpText(`② 上传中：${sent} / ${t} 字节`); },
        onSpaceInvalidate: () => void refreshSpace(true),
      });
      setUpText(`已入库 ${rec.project}:${rec.tag}`);
      setProg(null); setFile(null); setResumeId(null);
      await refreshAll();
    } catch (e) {
      setProg(null); setUpText("");
      apiFailToast(e, "恢复上传", refreshSpace);
    } finally {
      setUpBusy(false);
    }
  }

  const pct = space.totalGb > 0
    ? Math.min(99, ((space.totalGb - space.availGb) / space.totalGb) * 100)
    : 0;

  return (
    <div className="flex flex-col gap-4">
      <h2 className="text-sm font-semibold text-info">
        镜像库 <span className="ml-1 text-xs font-normal text-muted-foreground">（{images.length} 个{session?.admin ? " · 管理员全量" : " · 仅本人上传"}）</span>
      </h2>

      <div className="rounded-lg border">
        <Table>
          <TableHeader>
            <TableRow className="bg-muted/30">
              <TableHead>镜像</TableHead>
              <TableHead className="w-24 text-right">操作</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {images.map((im) => (
              <TableRow key={im.name}>
                <TableCell className="font-mono text-xs">{im.name}</TableCell>
                <TableCell className="text-right">
                  <Button size="sm" variant="destructive" onClick={() => setDelTarget(im.name)}>删除</Button>
                </TableCell>
              </TableRow>
            ))}
            {images.length === 0 && (
              <TableRow><TableCell colSpan={2} className="py-6 text-center text-xs text-muted-foreground">镜像库为空——用下方「上传镜像」入库。</TableCell></TableRow>
            )}
          </TableBody>
        </Table>
      </div>

      <Separator />
      <div className="flex flex-col gap-3">
        <h3 className="text-sm font-semibold text-info">上传镜像</h3>
        <div className="flex flex-wrap items-end gap-3">
          <Field>
            <FieldLabel>镜像 tar 文件</FieldLabel>
            <div className="flex items-center gap-2">
              <label htmlFor="img-file">
                <Button type="button" variant="outline" asChild>
                  <span><FolderOpenIcon data-icon="inline-start" />选择文件</span>
                </Button>
              </label>
              <input
                id="img-file" type="file" className="hidden" accept=".tar"
                onChange={(e) => setFile(e.target.files?.[0] ?? null)}
              />
              <span className="truncate text-xs text-muted-foreground">
                {file ? `${file.name}（${file.size} 字节）` : "未选择文件"}
              </span>
              <Button size="sm" disabled={upBusy || !file} onClick={upload}>上传</Button>
            </div>
            <FieldDescription>上传即入库（镜像名取自 tar 内镜像），不发布；到发布页选「镜像库」源可直接发布。</FieldDescription>
          </Field>
        </div>
        {prog && (
          <div className="flex flex-col gap-1">
            <Progress value={prog.total ? (prog.cur / prog.total) * 100 : 0} />
            <div className="font-mono text-xs text-muted-foreground" aria-live="polite">{upText}</div>
          </div>
        )}
        {!prog && upText && <div className="text-xs text-muted-foreground" aria-live="polite">{upText}</div>}
      </div>

      {resumeId && (
        <>
          <Separator />
          <div className="flex flex-col gap-3">
            <h3 className="text-sm font-semibold text-info">断点续传</h3>
            <div className="flex flex-wrap items-center gap-2">
              <Badge variant="warning">检测到未完成的上传</Badge>
              <Button size="sm" disabled={upBusy} onClick={resume}>
                <RotateCcwIcon data-icon="inline-start" />恢复上次上传
              </Button>
              <span className="text-xs text-muted-foreground">选择同一文件后点击恢复，从中断处继续。</span>
            </div>
          </div>
        </>
      )}

      <Separator />
      <div className="flex flex-col gap-1">
        <div className="text-xs text-muted-foreground" aria-live="polite">
          可用空间：{space.availGb > 0 ? `${space.availGb.toFixed(1)} GB（预留 ${space.reserveGb} GB）` : "空间信息不可用"}
        </div>
        {space.totalGb > 0 && <Progress value={pct} />}
      </div>

      <Confirm
        open={!!delTarget} onOpenChange={(v) => !v && setDelTarget(null)}
        title={`删除镜像 ${delTarget}？`} confirmText="删除" busy={busy} onConfirm={confirmDel}
      >
        正在使用该镜像的已发布项目不受影响（容器继续运行），但回滚/重建将失效。删除即内联 GC，立即回收磁盘空间。
      </Confirm>
    </div>
  );
}
