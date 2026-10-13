import {
  AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent,
  AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { cn } from "cn";

/** 通用危险操作确认：红色确认按钮 + 写明影响（对齐原版 confirm 文案纪律） */
export function Confirm({
  open, onOpenChange, title, children, confirmText = "确认执行",
  onConfirm, danger = true, busy,
}: {
  open: boolean;
  onOpenChange: (v: boolean) => void;
  title: string;
  children: React.ReactNode;
  confirmText?: string;
  onConfirm: () => Promise<void> | void;
  danger?: boolean;
  busy?: boolean;
}) {
  return (
    <AlertDialog open={open} onOpenChange={onOpenChange}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{title}</AlertDialogTitle>
          <AlertDialogDescription asChild>
            <div className="whitespace-pre-line">{children}</div>
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>取消</AlertDialogCancel>
          <AlertDialogAction
            className={cn(danger && "bg-destructive text-white hover:bg-destructive/85")}
            disabled={busy}
            onClick={(e) => {
              e.preventDefault(); // 保持对话框打开直到异步完成
              onConfirm();
            }}
          >
            {busy ? "执行中…" : confirmText}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
