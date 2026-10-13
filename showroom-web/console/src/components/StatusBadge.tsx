import { Badge } from "@/components/ui/badge";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "cn";

/** 状态徽章：●+文字双通道（色盲可辨）；重建中带呼吸动画——语义对齐原版 projState() 状态机；
    非自明状态带提示语（原版 title 语义，E_NAME/E_STATE 等处置指引） */
const MAP: Record<string, { label: string; variant: "success" | "warning" | "danger" | "secondary" | "info"; tip?: string }> = {
  running: { label: "运行中", variant: "success" },
  exited: { label: "已停止", variant: "secondary" },
  paused: { label: "已暂停", variant: "secondary" },
  created: { label: "已创建", variant: "secondary" },
  restarting: { label: "重建中", variant: "warning", tip: "更新发布成功后的滚动窗口" },
  dead: { label: "异常", variant: "danger", tip: "可看日志排查，或点「发布」重新发布" },
  missing: { label: "待发布", variant: "info", tip: "仅镜像无容器：从发布页选择镜像即可拉起" },
  publishing: { label: "发布中…", variant: "info", tip: "发布请求进行中：行操作已禁用" },
};

function BadgeInner({ status }: { status: string }) {
  const s = MAP[status] ?? MAP.exited;
  const breathe = status === "restarting" || status === "publishing";
  return (
    <Badge variant={s.variant} className="min-w-16 justify-center font-normal">
      <span
        aria-hidden
        className={cn("size-1.5 rounded-full bg-current", breathe && "animate-pulse")}
      />
      {s.label}
    </Badge>
  );
}

export function StatusBadge({ status }: { status: string }) {
  const s = MAP[status];
  if (!s?.tip) return <BadgeInner status={status} />;
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="inline-flex cursor-help" tabIndex={0}>
          <BadgeInner status={status} />
        </span>
      </TooltipTrigger>
      <TooltipContent>{s.tip}</TooltipContent>
    </Tooltip>
  );
}
